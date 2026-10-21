# SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
# SPDX-License-Identifier: Apache-2.0
#
# Cache what an external build produces -- the binary a SwiftPM, cargo or zig
# build makes for a CMake project -- under a key computed from everything that
# goes into it, and restore it instead of building when the key has been built
# before, in this checkout or any other on the machine.
#
#   include(SccionToolCache)
#   sccion_cached_tool(<name> [ALL]
#       OUTPUTS <file>...
#       COMMAND <command> [<arg>...]
#       [WORKING_DIRECTORY <dir>]
#       [COMMAND_ENVIRONMENT <var>=<value>...]
#       [SOURCE_TREES <dir>...]
#       [INPUT_FILES <file>...]
#       [TOOLS <command line>...]
#       [ENVIRONMENT <var>...]
#       [RECIPE_VERSION <n>]
#       [DEPENDS <target-or-file>...]
#       [COMMENT <text>])
#
# A compiler cache sees compilations. Some builds it cannot see into at all --
# SwiftPM runs swiftc with no launcher -- and others it can only partly serve:
# a Rust build still links, and runs its build scripts and proc-macros, on
# every hit. So this caches the product instead. Measured on one project, the
# Swift helper binary was 227 s of a fresh checkout's build whose C++ came
# entirely from the compiler cache.
#
# The key is a SHA-256 over:
#
# - every file of each SOURCE_TREE as git sees it -- tracked, and untracked
#   unless ignored -- by path relative to the project, so two checkouts share
#   entries and build output inside a tree (.build, target, .zig-cache) does
#   not count. Outside a git checkout the tree is walked instead, leaving out
#   the usual build directories;
# - each INPUT_FILE: lockfiles, generated sources;
# - the output of each TOOLS command line: `swift --version`, `rustc -vV`;
# - the value of each ENVIRONMENT variable;
# - the COMMAND, WORKING_DIRECTORY and COMMAND_ENVIRONMENT, with the source and
#   binary directories written relative, and RECIPE_VERSION, which is how a
#   change to the rule that none of the above shows is told apart.
#
# A tree the build reads but the key leaves out is the one way an entry can be
# served for the wrong inputs, so list every local dependency's tree: the
# package's own, and each path dependency's.
#
# The target runs on every build. It is cheap when nothing changed -- a few git
# listings and tool queries, and nothing restored when this build directory
# already holds what the key names. Entries live in SCCION_TOOL_CACHE_DIR (the
# environment variable, else the cache variable, else
# $XDG_CACHE_HOME/sccion-tools, ~/.cache/sccion-tools or
# %LOCALAPPDATA%/sccion-tools), the newest SCCION_TOOL_CACHE_KEEP (5) of each
# tool kept. Two builds after the same key at once build it once: the second
# waits for the first and restores. SCCION_TOOL_CACHE=OFF builds every time.
#
# The file is also its own build-time driver, run as `cmake -P` with a
# manifest the configure writes; a project vendors it alone.

include_guard(GLOBAL)

# The sccion release this copy of the module comes from.
set(SCCION_CMAKE_MODULE_VERSION "0.1.4")

# What an entry holds, and how a key is spelled. A change to either makes every
# key new.
set(_SCCION_TOOL_FORMAT 1)

if(NOT CMAKE_SCRIPT_MODE_FILE)
	option(SCCION_TOOL_CACHE "Restore external build products from sccion's tool cache" ON)
	set(SCCION_TOOL_CACHE_DIR "" CACHE PATH
		"Where the tool cache keeps its entries; empty for the default")
	set(SCCION_TOOL_CACHE_KEEP 5 CACHE STRING
		"How many entries the tool cache keeps for each tool")
endif()

# A list as a bracket argument, which CMake reads back unchanged.
function(_sccion_tool_bracket out value)
	set(${out} "[==[${value}]==]" PARENT_SCOPE)
endfunction()

function(sccion_cached_tool name)
	cmake_parse_arguments(PARSE_ARGV 1 arg "ALL"
		"WORKING_DIRECTORY;RECIPE_VERSION;COMMENT"
		"OUTPUTS;COMMAND;COMMAND_ENVIRONMENT;SOURCE_TREES;INPUT_FILES;TOOLS;ENVIRONMENT;DEPENDS")
	if(NOT arg_OUTPUTS OR NOT arg_COMMAND)
		message(FATAL_ERROR "sccion_cached_tool(${name}): OUTPUTS and COMMAND are required")
	endif()
	if(NOT arg_WORKING_DIRECTORY)
		set(arg_WORKING_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}")
	endif()
	if(NOT arg_RECIPE_VERSION)
		set(arg_RECIPE_VERSION 1)
	endif()
	set(trees "")
	foreach(tree IN LISTS arg_SOURCE_TREES)
		get_filename_component(tree "${tree}" ABSOLUTE BASE_DIR "${CMAKE_CURRENT_SOURCE_DIR}")
		list(APPEND trees "${tree}")
	endforeach()
	set(files "")
	foreach(file IN LISTS arg_INPUT_FILES)
		get_filename_component(file "${file}" ABSOLUTE BASE_DIR "${CMAKE_CURRENT_SOURCE_DIR}")
		list(APPEND files "${file}")
	endforeach()

	set(state "${CMAKE_BINARY_DIR}/sccion-tools/${name}")
	set(content "# Written by sccion_cached_tool(${name}); read by its build step.\n")
	foreach(field IN ITEMS
			"NAME=${name}"
			"OUTPUTS=${arg_OUTPUTS}"
			"COMMAND=${arg_COMMAND}"
			"WORKING_DIRECTORY=${arg_WORKING_DIRECTORY}"
			"COMMAND_ENVIRONMENT=${arg_COMMAND_ENVIRONMENT}"
			"SOURCE_TREES=${trees}"
			"INPUT_FILES=${files}"
			"TOOLS=${arg_TOOLS}"
			"ENVIRONMENT=${arg_ENVIRONMENT}"
			"RECIPE_VERSION=${arg_RECIPE_VERSION}"
			"SOURCE_ROOT=${CMAKE_SOURCE_DIR}"
			"BINARY_ROOT=${CMAKE_BINARY_DIR}"
			"STATE_DIR=${state}"
			"STORE=${SCCION_TOOL_CACHE_DIR}"
			"ENABLED=${SCCION_TOOL_CACHE}"
			"KEEP=${SCCION_TOOL_CACHE_KEEP}"
			"HOST=${CMAKE_HOST_SYSTEM_NAME}-${CMAKE_HOST_SYSTEM_PROCESSOR}")
		string(FIND "${field}" "=" eq)
		string(SUBSTRING "${field}" 0 ${eq} key)
		math(EXPR start "${eq} + 1")
		string(SUBSTRING "${field}" ${start} -1 value)
		_sccion_tool_bracket(value "${value}")
		string(APPEND content "set(SCCION_TOOL_${key} ${value})\n")
	endforeach()
	# GENERATE, so generator expressions in the command -- a tool target's
	# $<TARGET_FILE:...> -- are what the build step reads.
	file(GENERATE OUTPUT "${state}/manifest.cmake" CONTENT "${content}")

	if(arg_ALL)
		set(all ALL)
	else()
		set(all "")
	endif()
	if(NOT arg_COMMENT)
		set(arg_COMMENT "Building ${name}, or restoring it from the tool cache")
	endif()
	add_custom_target(${name} ${all}
		COMMAND "${CMAKE_COMMAND}" "-DSCCION_TOOL_MANIFEST=${state}/manifest.cmake"
			-P "${CMAKE_CURRENT_FUNCTION_LIST_FILE}"
		BYPRODUCTS ${arg_OUTPUTS}
		DEPENDS ${arg_DEPENDS}
		COMMENT "${arg_COMMENT}"
		VERBATIM)
endfunction()

# ---------------------------------------------------------------------------
# The build step.

# `text` with the source and binary directories written as <SRC> and <BIN>,
# so that a key does not name the checkout it was computed in. Only where a
# directory ends at a path boundary: a checkout at /w/b must not turn
# /w/builds.txt into <SRC>uilds.txt.
function(_sccion_tool_portable out text)
	_sccion_tool_replace_root(text "${text}" "${SCCION_TOOL_BINARY_ROOT}" "<BIN>")
	_sccion_tool_replace_root(text "${text}" "${SCCION_TOOL_SOURCE_ROOT}" "<SRC>")
	set(${out} "${text}" PARENT_SCOPE)
endfunction()

# By hand rather than by regex, which would need `root` escaped first.
function(_sccion_tool_replace_root out text root token)
	set(result "")
	string(LENGTH "${root}" length)
	while(length GREATER 0)
		string(FIND "${text}" "${root}" at)
		if(at EQUAL -1)
			break()
		endif()
		math(EXPR end "${at} + ${length}")
		string(SUBSTRING "${text}" 0 ${at} before)
		string(SUBSTRING "${text}" ${end} 1 next)
		if(next STREQUAL "" OR next STREQUAL "/" OR next STREQUAL ";" OR next STREQUAL " "
				OR next STREQUAL "\"" OR next STREQUAL "=")
			string(APPEND result "${before}${token}")
		else()
			string(APPEND result "${before}${root}")
		endif()
		string(SUBSTRING "${text}" ${end} -1 text)
	endwhile()
	set(${out} "${result}${text}" PARENT_SCOPE)
endfunction()

# Key lines for one source tree: `file <tree>/<path> <hash>` for each file.
function(_sccion_tool_tree out tree)
	_sccion_tool_portable(name "${tree}")
	if(NOT IS_DIRECTORY "${tree}")
		message(FATAL_ERROR "sccion tool cache: ${SCCION_TOOL_NAME}: no source tree at ${tree}")
	endif()
	set(lines "")
	execute_process(
		COMMAND git -c core.quotePath=false -C "${tree}" ls-files -s -- .
		OUTPUT_VARIABLE staged
		RESULT_VARIABLE failed
		ERROR_QUIET)
	if(NOT failed)
		# What git already knows the contents of, by blob id: no hashing.
		string(REGEX REPLACE "[0-7]+ ([0-9a-f]+) [0-9]\t([^\n]*)" "file ${name}/\\2 git:\\1" staged "${staged}")
		string(REPLACE "\n" ";" staged "${staged}")
		list(APPEND lines ${staged})
		# What differs from it, by content: modified, deleted and untracked
		# files. A modified file keeps its index line too, which costs a hit
		# between two checkouts that staged differently, never a wrong one.
		execute_process(
			COMMAND git -c core.quotePath=false -C "${tree}" ls-files -m -o --exclude-standard -- .
			OUTPUT_VARIABLE changed
			ERROR_QUIET)
		string(REPLACE "\n" ";" changed "${changed}")
		foreach(path IN LISTS changed)
			if(path STREQUAL "")
				continue()
			elseif(IS_DIRECTORY "${tree}/${path}")
				# A symlink to a directory -- one a build rewrites, say, to point
				# into its own build directory. What it points to is not hashed
				# through it, so it counts as git has it; list that directory's
				# files as INPUT_FILES or the directory as a tree of its own.
				continue()
			elseif(EXISTS "${tree}/${path}")
				file(SHA256 "${tree}/${path}" hash)
				list(APPEND lines "worktree ${name}/${path} sha256:${hash}")
			else()
				list(APPEND lines "worktree ${name}/${path} deleted")
			endif()
		endforeach()
	else()
		file(GLOB_RECURSE paths LIST_DIRECTORIES false RELATIVE "${tree}" "${tree}/*")
		list(FILTER paths EXCLUDE REGEX
			"(^|/)(\\.git|\\.build|\\.swiftpm|target|\\.zig-cache|zig-cache|zig-out|node_modules)/")
		foreach(path IN LISTS paths)
			file(SHA256 "${tree}/${path}" hash)
			list(APPEND lines "file ${name}/${path} sha256:${hash}")
		endforeach()
	endif()
	list(FILTER lines EXCLUDE REGEX "^$")
	set(${out} "${lines}" PARENT_SCOPE)
endfunction()

# The key's lines, in a fixed order.
function(_sccion_tool_key_lines out)
	set(lines
		"format ${_SCCION_TOOL_FORMAT}"
		"recipe ${SCCION_TOOL_RECIPE_VERSION}"
		"host ${SCCION_TOOL_HOST}")
	foreach(field IN ITEMS OUTPUTS COMMAND WORKING_DIRECTORY COMMAND_ENVIRONMENT)
		_sccion_tool_portable(value "${SCCION_TOOL_${field}}")
		string(REPLACE ";" " " value "${value}")
		string(TOLOWER "${field}" label)
		list(APPEND lines "${label} ${value}")
	endforeach()
	foreach(var IN LISTS SCCION_TOOL_ENVIRONMENT)
		if(DEFINED ENV{${var}})
			list(APPEND lines "env ${var}=$ENV{${var}}")
		else()
			list(APPEND lines "env ${var} unset")
		endif()
	endforeach()
	foreach(tool IN LISTS SCCION_TOOL_TOOLS)
		separate_arguments(argv UNIX_COMMAND "${tool}")
		execute_process(
			COMMAND ${argv}
			OUTPUT_VARIABLE said
			ERROR_VARIABLE also
			RESULT_VARIABLE failed)
		if(failed)
			message(FATAL_ERROR "sccion tool cache: ${SCCION_TOOL_NAME}: `${tool}` failed: ${also}")
		endif()
		string(SHA256 said "${said}${also}")
		list(APPEND lines "tool ${tool} ${said}")
	endforeach()
	set(files "")
	foreach(file IN LISTS SCCION_TOOL_INPUT_FILES)
		if(NOT EXISTS "${file}")
			message(FATAL_ERROR "sccion tool cache: ${SCCION_TOOL_NAME}: no input file at ${file}")
		endif()
		file(SHA256 "${file}" hash)
		_sccion_tool_portable(name "${file}")
		list(APPEND files "input ${name} sha256:${hash}")
	endforeach()
	list(SORT files)
	list(APPEND lines ${files})
	foreach(tree IN LISTS SCCION_TOOL_SOURCE_TREES)
		_sccion_tool_tree(tree_lines "${tree}")
		list(SORT tree_lines)
		list(APPEND lines ${tree_lines})
	endforeach()
	set(${out} "${lines}" PARENT_SCOPE)
endfunction()

# Which kinds of key line differ between two keys' texts, for a miss to say
# why: the first changed input by name, the rest by kind.
function(_sccion_tool_explain out before after)
	string(REPLACE "\n" ";" before "${before}")
	string(REPLACE "\n" ";" after "${after}")
	set(gone ${before})
	list(REMOVE_ITEM gone ${after})
	set(new ${after})
	list(REMOVE_ITEM new ${before})
	set(kinds "")
	set(first "")
	foreach(line IN LISTS gone new)
		string(REGEX MATCH "^[a-z_]+" kind "${line}")
		if(kind STREQUAL "file" OR kind STREQUAL "worktree" OR kind STREQUAL "input")
			string(REGEX REPLACE "^[a-z_]+ ([^ ]+) .*" "\\1" path "${line}")
			if(NOT first)
				set(first " (${path})")
			endif()
			set(kind "sources")
		endif()
		list(APPEND kinds "${kind}")
	endforeach()
	list(REMOVE_DUPLICATES kinds)
	list(JOIN kinds ", " kinds)
	set(${out} "${kinds}${first}" PARENT_SCOPE)
endfunction()

function(_sccion_tool_store_dir out)
	if(DEFINED ENV{SCCION_TOOL_CACHE_DIR} AND NOT "$ENV{SCCION_TOOL_CACHE_DIR}" STREQUAL "")
		set(dir "$ENV{SCCION_TOOL_CACHE_DIR}")
	elseif(SCCION_TOOL_STORE)
		set(dir "${SCCION_TOOL_STORE}")
	elseif(CMAKE_HOST_WIN32)
		file(TO_CMAKE_PATH "$ENV{LOCALAPPDATA}/sccion-tools" dir)
	elseif(DEFINED ENV{XDG_CACHE_HOME} AND NOT "$ENV{XDG_CACHE_HOME}" STREQUAL "")
		set(dir "$ENV{XDG_CACHE_HOME}/sccion-tools")
	else()
		set(dir "$ENV{HOME}/.cache/sccion-tools")
	endif()
	set(${out} "${dir}" PARENT_SCOPE)
endfunction()

function(_sccion_tool_build)
	set(argv "${CMAKE_COMMAND}" -E env ${SCCION_TOOL_COMMAND_ENVIRONMENT} -- ${SCCION_TOOL_COMMAND})
	execute_process(
		COMMAND ${argv}
		WORKING_DIRECTORY "${SCCION_TOOL_WORKING_DIRECTORY}"
		RESULT_VARIABLE failed)
	if(failed)
		message(FATAL_ERROR "sccion tool cache: ${SCCION_TOOL_NAME}: the build failed (${failed})")
	endif()
	foreach(output IN LISTS SCCION_TOOL_OUTPUTS)
		if(NOT EXISTS "${output}")
			message(FATAL_ERROR
				"sccion tool cache: ${SCCION_TOOL_NAME}: the build succeeded but did not write ${output}")
		endif()
	endforeach()
endfunction()

function(_sccion_tool_run)
	_sccion_tool_key_lines(lines)
	list(JOIN lines "\n" text)
	string(SHA256 key "${text}")
	string(SUBSTRING "${key}" 0 12 short)
	set(state "${SCCION_TOOL_STATE_DIR}")

	# This build directory already holds what the key names.
	set(present TRUE)
	foreach(output IN LISTS SCCION_TOOL_OUTPUTS)
		if(NOT EXISTS "${output}")
			set(present FALSE)
		endif()
	endforeach()
	if(present AND EXISTS "${state}/key")
		file(READ "${state}/key" last)
		if(last STREQUAL key)
			return()
		endif()
	endif()

	if(NOT SCCION_TOOL_ENABLED)
		_sccion_tool_build()
		file(WRITE "${state}/key" "${key}")
		file(WRITE "${state}/key.txt" "${text}")
		return()
	endif()

	_sccion_tool_store_dir(store)
	set(tool_dir "${store}/${SCCION_TOOL_NAME}")
	set(entry "${tool_dir}/${key}")
	file(MAKE_DIRECTORY "${tool_dir}")
	# One build per key: a second build after the same key waits here, then
	# finds the entry the first one stored.
	file(LOCK "${tool_dir}/${key}.lock" GUARD PROCESS TIMEOUT 7200 RESULT_VARIABLE lock_failed)
	if(lock_failed)
		message(WARNING "sccion tool cache: ${SCCION_TOOL_NAME}: could not lock ${key}: ${lock_failed}")
	endif()

	if(EXISTS "${entry}/entry.cmake")
		include("${entry}/entry.cmake")
		set(intact TRUE)
		set(index 0)
		foreach(output IN LISTS SCCION_TOOL_OUTPUTS)
			list(GET _entry_hashes ${index} want)
			get_filename_component(base "${output}" NAME)
			set(stored "${entry}/files/${index}-${base}")
			if(EXISTS "${stored}")
				file(SHA256 "${stored}" got)
			else()
				set(got "")
			endif()
			if(NOT got STREQUAL want)
				set(intact FALSE)
			endif()
			math(EXPR index "${index} + 1")
		endforeach()
		if(intact)
			set(index 0)
			foreach(output IN LISTS SCCION_TOOL_OUTPUTS)
				get_filename_component(base "${output}" NAME)
				get_filename_component(parent "${output}" DIRECTORY)
				file(MAKE_DIRECTORY "${parent}")
				# -E copy keeps the file mode, so an executable stays one.
				execute_process(
					COMMAND "${CMAKE_COMMAND}" -E copy "${entry}/files/${index}-${base}" "${output}"
					RESULT_VARIABLE copy_failed)
				if(copy_failed)
					message(FATAL_ERROR "sccion tool cache: ${SCCION_TOOL_NAME}: could not restore ${output}")
				endif()
				math(EXPR index "${index} + 1")
			endforeach()
			file(TOUCH_NOCREATE "${entry}/entry.cmake")
			message(STATUS
				"sccion tool cache: hit ${SCCION_TOOL_NAME} ${short} (stored ${_entry_stored} from ${_entry_from})")
			file(WRITE "${state}/key" "${key}")
			file(WRITE "${state}/key.txt" "${text}")
			return()
		endif()
		message(WARNING "sccion tool cache: ${SCCION_TOOL_NAME}: entry ${short} is damaged; rebuilding it")
		file(REMOVE_RECURSE "${entry}")
	endif()

	if(EXISTS "${state}/key.txt")
		file(READ "${state}/key.txt" before)
		_sccion_tool_explain(why "${before}" "${text}")
		message(STATUS "sccion tool cache: miss ${SCCION_TOOL_NAME} ${short} -- changed: ${why}")
	else()
		message(STATUS "sccion tool cache: miss ${SCCION_TOOL_NAME} ${short} -- not built with these inputs before")
	endif()
	_sccion_tool_build()

	# Stored complete or not at all: assembled aside, then renamed into place.
	string(RANDOM LENGTH 12 nonce)
	set(partial "${tool_dir}/.partial-${nonce}")
	file(MAKE_DIRECTORY "${partial}/files")
	set(hashes "")
	set(index 0)
	foreach(output IN LISTS SCCION_TOOL_OUTPUTS)
		get_filename_component(base "${output}" NAME)
		execute_process(COMMAND "${CMAKE_COMMAND}" -E copy "${output}" "${partial}/files/${index}-${base}")
		file(SHA256 "${output}" hash)
		list(APPEND hashes "${hash}")
		math(EXPR index "${index} + 1")
	endforeach()
	string(TIMESTAMP now "%Y-%m-%d %H:%M" UTC)
	file(WRITE "${partial}/entry.cmake"
		"set(_entry_hashes [==[${hashes}]==])\n"
		"set(_entry_stored [==[${now} UTC]==])\n"
		"set(_entry_from [==[${SCCION_TOOL_SOURCE_ROOT}]==])\n")
	file(WRITE "${partial}/key.txt" "${text}")
	file(RENAME "${partial}" "${entry}" RESULT rename_failed)
	if(rename_failed)
		file(REMOVE_RECURSE "${partial}")
	endif()
	file(WRITE "${state}/key" "${key}")
	file(WRITE "${state}/key.txt" "${text}")

	# The newest few of each tool.
	file(GLOB entries LIST_DIRECTORIES true "${tool_dir}/*")
	list(FILTER entries INCLUDE REGEX "/[0-9a-f]+$")
	list(LENGTH entries count)
	if(count GREATER SCCION_TOOL_KEEP)
		set(dated "")
		foreach(e IN LISTS entries)
			file(TIMESTAMP "${e}/entry.cmake" when "%Y%m%d%H%M%S")
			list(APPEND dated "${when} ${e}")
		endforeach()
		list(SORT dated ORDER DESCENDING)
		list(SUBLIST dated ${SCCION_TOOL_KEEP} -1 old)
		foreach(line IN LISTS old)
			string(REGEX REPLACE "^[0-9]+ " "" e "${line}")
			# Its lock file stays: a build waiting on it may hold it.
			if(NOT e STREQUAL entry)
				file(REMOVE_RECURSE "${e}")
			endif()
		endforeach()
	endif()
endfunction()

if(CMAKE_SCRIPT_MODE_FILE AND DEFINED SCCION_TOOL_MANIFEST)
	include("${SCCION_TOOL_MANIFEST}")
	_sccion_tool_run()
endif()
