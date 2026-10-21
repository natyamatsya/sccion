# SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
# SPDX-License-Identifier: Apache-2.0
#
# Put a compiler cache in front of the compiler, if the machine has one.
#
#   include(SccionCompilerCache)
#   sccion_compiler_cache([OPTION <name>] [CANDIDATES <program>...])
#   sccion_cache_rust_crates(<corrosion-target>...)
#   sccion_wrap_automoc(<target>... [MOC <moc-target-or-path>])
#
# sccion is preferred, and sccache and ccache are honoured: a contributor who
# already runs one should not have to install anything. The file stands alone,
# so a project can commit a copy of it and still configure -- uncached, or with
# sccache or ccache -- on a machine without sccion. `sccion --cmake-module-dir`
# prints where the installed sccion keeps the copy matching its version.
#
# This is deliberately not a preset. A launcher named in a preset is a promise
# that a particular binary exists on whoever's machine reads it: CMake does not
# check, so a missing one configures cleanly and then fails on every single
# compile. Detection here fails the only way that is useful -- by saying, once,
# at configure time, that the build will not be cached.

include_guard(GLOBAL)

# The sccion release this copy of the module comes from.
set(SCCION_CMAKE_MODULE_VERSION "0.1.5")

# The cache that also takes rustc, as cargo's RUSTC_WRAPPER, when `cache` is
# one that can: sccion and sccache can, ccache cannot. Empty otherwise -- and
# when the environment already names a wrapper, which is then the one cargo
# should keep using.
function(_sccion_rustc_wrapper_for out cache)
	set(wrapper "")
	get_filename_component(name "${cache}" NAME_WE)
	if(name STREQUAL "sccion" OR name STREQUAL "sccache")
		if(DEFINED ENV{RUSTC_WRAPPER} OR DEFINED ENV{CARGO_BUILD_RUSTC_WRAPPER})
			message(STATUS "Compiler cache: cargo keeps the RUSTC_WRAPPER the environment sets")
		else()
			set(wrapper "${cache}")
		endif()
	endif()
	set(${out} "${wrapper}" PARENT_SCOPE)
endfunction()

# Say so when the sccion in use ships a different copy of this module than the
# one the project includes: the project's copy is pinned, and refreshing it is
# a decision for whoever maintains the build, not something to do behind their
# back. Silent when they match, and with an sccion too old to ship one.
function(_sccion_note_newer_module cache)
	get_filename_component(name "${cache}" NAME_WE)
	if(NOT name STREQUAL "sccion")
		return()
	endif()
	execute_process(
		COMMAND "${cache}" --cmake-module-dir
		OUTPUT_VARIABLE dir
		ERROR_QUIET
		RESULT_VARIABLE failed
		OUTPUT_STRIP_TRAILING_WHITESPACE)
	if(failed OR NOT EXISTS "${dir}/SccionCompilerCache.cmake")
		return()
	endif()
	file(SHA256 "${dir}/SccionCompilerCache.cmake" theirs)
	file(SHA256 "${CMAKE_CURRENT_FUNCTION_LIST_FILE}" ours)
	if(NOT theirs STREQUAL ours)
		message(STATUS
			"Compiler cache: the installed sccion ships a different SccionCompilerCache.cmake "
			"than this project's (from sccion ${SCCION_CMAKE_MODULE_VERSION}); it is in ${dir}")
	endif()
endfunction()

# Choose the launcher for C and C++, and the RUSTC_WRAPPER for
# sccion_cache_rust_crates().
#
# OPTION names the cache variable that switches it (default
# SCCION_COMPILER_CACHE): AUTO uses a cache when one is installed, ON requires
# one -- what CI wants when the point of the job is to measure the cache -- and
# OFF is how you get a cold build without uninstalling anything. CANDIDATES
# lists the programs to look for, in order of preference (default: sccion
# sccache ccache).
#
# Sets CMAKE_C_COMPILER_LAUNCHER, CMAKE_CXX_COMPILER_LAUNCHER, SCCION_RUSTC_WRAPPER
# and, for MSVC, CMAKE_MSVC_DEBUG_INFORMATION_FORMAT in the caller's scope, and
# records the cache it chose for the functions below, wherever they are called.
function(sccion_compiler_cache)
	cmake_parse_arguments(PARSE_ARGV 0 arg "" "OPTION" "CANDIDATES")
	if(NOT arg_OPTION)
		set(arg_OPTION SCCION_COMPILER_CACHE)
	endif()
	if(NOT arg_CANDIDATES)
		set(arg_CANDIDATES sccion sccache ccache)
	endif()
	set(${arg_OPTION} "AUTO" CACHE STRING
		"Use a compiler cache: AUTO (if one is installed), ON (require one), OFF")
	set_property(CACHE ${arg_OPTION} PROPERTY STRINGS AUTO ON OFF)
	set(mode "${${arg_OPTION}}")
	set(SCCION_RUSTC_WRAPPER "" PARENT_SCOPE)
	set_property(GLOBAL PROPERTY SCCION_COMPILER_CACHE_PROGRAM "")

	if(mode STREQUAL "OFF" OR NOT mode)
		message(STATUS "Compiler cache: disabled (${arg_OPTION}=OFF)")
		return()
	endif()

	# Someone who set the launcher by hand -- in a user preset, a toolchain file
	# or on the command line -- has said what they want, and it may well be a
	# wrapper this does not know about. But only if it is still there: a launcher
	# path outlives the checkout it pointed into, and CMake keeps it in the cache
	# of a build directory that then fails on every compile with a message about
	# a program nobody remembers configuring. A leftover is not a preference.
	set(existing "${CMAKE_CXX_COMPILER_LAUNCHER}")
	if(NOT existing)
		set(existing "${CMAKE_C_COMPILER_LAUNCHER}")
	endif()
	if(existing)
		list(GET existing 0 existing_exe) # it may carry arguments of its own
		set(existing_usable TRUE)
		if(IS_ABSOLUTE "${existing_exe}")
			if(NOT EXISTS "${existing_exe}")
				set(existing_usable FALSE)
			endif()
		else()
			find_program(SCCION_EXISTING_LAUNCHER "${existing_exe}")
			if(SCCION_EXISTING_LAUNCHER)
				set(existing_exe "${SCCION_EXISTING_LAUNCHER}")
			else()
				set(existing_usable FALSE)
			endif()
			unset(SCCION_EXISTING_LAUNCHER CACHE)
		endif()

		if(existing_usable)
			message(STATUS "Compiler cache: using the launcher already set (${existing_exe})")
			set_property(GLOBAL PROPERTY SCCION_COMPILER_CACHE_PROGRAM "${existing_exe}")
			_sccion_rustc_wrapper_for(wrapper "${existing_exe}")
			set(SCCION_RUSTC_WRAPPER "${wrapper}" PARENT_SCOPE)
			_sccion_note_newer_module("${existing_exe}")
			return()
		endif()
		message(WARNING
			"Compiler cache: the configured launcher '${existing_exe}' does not exist. "
			"Every compilation in this build directory would fail with it. Looking for one "
			"that is installed instead; configure with -D ${arg_OPTION}=OFF for "
			"no launcher at all.")
		# Replace it rather than shadow it, or the stale value stays in
		# CMakeCache.txt to mislead the next person who reads it.
		unset(CMAKE_C_COMPILER_LAUNCHER CACHE)
		unset(CMAKE_CXX_COMPILER_LAUNCHER CACHE)
	endif()

	find_program(SCCION_COMPILER_CACHE_EXECUTABLE
		NAMES ${arg_CANDIDATES}
		DOC "Compiler cache to run compilations through")

	if(NOT SCCION_COMPILER_CACHE_EXECUTABLE)
		string(JOIN ", " candidates ${arg_CANDIDATES})
		if(mode STREQUAL "AUTO")
			message(STATUS
				"Compiler cache: none found (looked for ${candidates}); compilations will not be cached")
			return()
		endif()
		message(FATAL_ERROR
			"${arg_OPTION}=ON but no compiler cache was found on PATH "
			"(looked for ${candidates}). Install one, or configure with "
			"-D ${arg_OPTION}=AUTO to build uncached.")
	endif()

	set(CMAKE_C_COMPILER_LAUNCHER   "${SCCION_COMPILER_CACHE_EXECUTABLE}" PARENT_SCOPE)
	set(CMAKE_CXX_COMPILER_LAUNCHER "${SCCION_COMPILER_CACHE_EXECUTABLE}" PARENT_SCOPE)
	message(STATUS "Compiler cache: ${SCCION_COMPILER_CACHE_EXECUTABLE}")
	set_property(GLOBAL PROPERTY SCCION_COMPILER_CACHE_PROGRAM "${SCCION_COMPILER_CACHE_EXECUTABLE}")
	_sccion_rustc_wrapper_for(wrapper "${SCCION_COMPILER_CACHE_EXECUTABLE}")
	set(SCCION_RUSTC_WRAPPER "${wrapper}" PARENT_SCOPE)
	_sccion_note_newer_module("${SCCION_COMPILER_CACHE_EXECUTABLE}")

	# MSVC's default /Zi writes debug info into one PDB shared by every
	# translation unit in a target. A compiler cache cannot serve an object whose
	# debug info lives in a file it did not write, so the whole Debug build
	# silently caches nothing -- sccion classifies exactly this as
	# `uncacheable:shared_pdb`, which is the fastest way to recognise it if it
	# ever comes back. /Z7 puts the debug info in the object, where a cache can
	# carry it. Same information, larger objects, no PDB.
	if(MSVC AND NOT DEFINED CMAKE_MSVC_DEBUG_INFORMATION_FORMAT)
		set(CMAKE_MSVC_DEBUG_INFORMATION_FORMAT "Embedded" PARENT_SCOPE)
		message(STATUS
			"Compiler cache: using /Z7 (embedded debug info) so debug builds are cacheable; "
			"set CMAKE_MSVC_DEBUG_INFORMATION_FORMAT to override")
	endif()

	# A .sccion.toml at the top of the checkout is what lets two checkouts share
	# cache entries -- measured on one project, 288 s against 18 s for a second
	# checkout's build -- and it is one committed file. Worth a line when it is
	# missing, and silence when it is not.
	get_filename_component(cache_name "${SCCION_COMPILER_CACHE_EXECUTABLE}" NAME_WE)
	if(cache_name STREQUAL "sccion" AND NOT EXISTS "${CMAKE_SOURCE_DIR}/.sccion.toml")
		message(STATUS
			"Compiler cache: no .sccion.toml in this checkout -- `sccion --init` writes one, "
			"and it is what lets two checkouts share cache entries")
	endif()
endfunction()

# Run the cargo builds of these Corrosion targets through the compiler cache as
# well, as RUSTC_WRAPPER, when sccion_compiler_cache() chose one that takes
# rustc. Pass every target corrosion_import_crate() made -- its IMPORTED_CRATES
# -- since each has a cargo build of its own.
#
# What rustc caching covers: library crates (rlib, staticlib), compiled without
# incremental. cargo builds registry dependencies that way in every profile, and
# runs them with the same paths in every checkout, so those share between
# checkouts. A crate of the project itself keys on its own checkout's path, and
# binaries, proc-macros and build scripts are linked by rustc and never cached.
# Measured on one project's two Rust targets built from scratch: 160 s
# uncached, 56 s from a warm cache.
function(sccion_cache_rust_crates)
	if(NOT SCCION_RUSTC_WRAPPER)
		return()
	endif()
	if(NOT COMMAND corrosion_set_env_vars)
		message(FATAL_ERROR
			"sccion_cache_rust_crates() sets RUSTC_WRAPPER on Corrosion targets; "
			"include Corrosion before calling it")
	endif()
	foreach(target IN LISTS ARGN)
		corrosion_set_env_vars(${target} "RUSTC_WRAPPER=${SCCION_RUSTC_WRAPPER}")
	endforeach()
	list(JOIN ARGN ", " targets)
	message(STATUS "Compiler cache: cargo builds of ${targets} go through ${SCCION_RUSTC_WRAPPER}")
endfunction()

# Run these targets' AUTOMOC through sccion, which caches moc too.
#
# A build system does not send moc through a compiler launcher -- AUTOMOC runs
# the executable it is given, with nothing in front of it -- so sccion has to
# *be* that executable. This puts a copy of the sccion in use (a hard link
# where the filesystem allows) in the build directory under moc's name, writes
# the sccion-wraps.toml beside it that names the real moc, and makes the copy
# each target's AUTOMOC_EXECUTABLE. The real moc is named at configure time,
# so the PATH the build later runs with plays no part. Measured on one Qt
# project: moc was a quarter of all compilations, and caching it took a warm
# build from 225 s to 148 s.
#
# MOC is the moc to stand in for, a target or a path: by default Qt6::moc, or
# else Qt5::moc. One build directory has one stand-in, so the first call
# decides which moc it is. Nothing happens unless the cache in use is sccion --
# sccache and ccache do not cache moc -- so a machine without sccion builds
# exactly as before.
function(sccion_wrap_automoc)
	cmake_parse_arguments(PARSE_ARGV 0 arg "" "MOC" "")
	get_property(cache GLOBAL PROPERTY SCCION_COMPILER_CACHE_PROGRAM)
	get_filename_component(cache_name "${cache}" NAME_WE)
	if(NOT cache_name STREQUAL "sccion")
		return()
	endif()

	get_property(standin GLOBAL PROPERTY SCCION_AUTOMOC_STANDIN)
	if(NOT standin)
		if(arg_MOC)
			set(moc "${arg_MOC}")
		elseif(TARGET Qt6::moc)
			set(moc Qt6::moc)
		elseif(TARGET Qt5::moc)
			set(moc Qt5::moc)
		else()
			message(WARNING
				"sccion_wrap_automoc(): no Qt6::moc or Qt5::moc target, so moc stays uncached; "
				"find Qt first, or pass MOC <moc>")
			return()
		endif()
		if(TARGET "${moc}")
			set(moc "$<TARGET_FILE:${moc}>")
		endif()

		set(dir "${CMAKE_BINARY_DIR}/sccion-wraps")
		set(standin "${dir}/moc${CMAKE_HOST_EXECUTABLE_SUFFIX}")
		file(MAKE_DIRECTORY "${dir}")
		# Made afresh each configure: a link to the sccion of an earlier one
		# would go on running that sccion after it was upgraded.
		file(REMOVE "${standin}")
		file(CREATE_LINK "${cache}" "${standin}" COPY_ON_ERROR)
		# A literal string, so a Windows path needs no escaping.
		file(GENERATE OUTPUT "${dir}/sccion-wraps.toml" CONTENT "[wraps]\nmoc = '${moc}'\n")
		set_property(GLOBAL PROPERTY SCCION_AUTOMOC_STANDIN "${standin}")
		message(STATUS "Compiler cache: AUTOMOC runs moc through sccion (${standin})")
	endif()
	set_target_properties(${arg_UNPARSED_ARGUMENTS} PROPERTIES AUTOMOC_EXECUTABLE "${standin}")
endfunction()
