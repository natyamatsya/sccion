# A CMake module that chooses the cache

`cmake/SccionCompilerCache.cmake` puts a compiler cache in front of a CMake
project's compilers without asking anything of the people who build it:

- **sccion** when it is installed; **sccache** or **ccache** when one of those
  is instead; nothing, and a line saying so, when there is none.
- **Rust built through Corrosion** goes through the same cache, as cargo's
  `RUSTC_WRAPPER`, when that cache can take rustc (sccion and sccache can,
  ccache cannot).
- **MSVC** gets `/Z7`, without which a Debug build caches nothing (see
  [MSVC and Windows builds](Windows.md)).
- **A launcher left behind** in a build directory -- a path into a checkout that
  no longer exists -- is replaced instead of failing every compile.
- **moc**, which no launcher reaches, runs through sccion with
  `sccion_wrap_automoc` (below).

A second file, `cmake/SccionToolCache.cmake`, caches whole products of builds a
compiler cache cannot see into, such as a binary SwiftPM builds (below).

It is a single file with no dependency on sccion itself, so it is meant to be
committed into the project: a machine without sccion still configures, with
whichever cache it has.

## Using it

Copy the file into the project, say as `cmake/SccionCompilerCache.cmake` (and
`cmake/SccionToolCache.cmake`, if you use it). Both come in every release archive
under `cmake/`, and the installed sccion writes the copies matching its version
on request:

```sh
sccion --cmake-module-dir    # prints the directory; the files are in it
```

Then, near the top of the top-level `CMakeLists.txt`, before any target:

```cmake
include(cmake/SccionCompilerCache.cmake)
sccion_compiler_cache()
```

and after each `corrosion_import_crate`, if the project builds Rust:

```cmake
corrosion_import_crate(MANIFEST_PATH rust/Cargo.toml IMPORTED_CRATES rust_targets)
sccion_cache_rust_crates(${rust_targets})
```

Pass every target the import made: each has a cargo build of its own.

## Options

`sccion_compiler_cache([OPTION <name>] [CANDIDATES <program>...])`

- `OPTION` names the cache variable that switches it, `SCCION_COMPILER_CACHE` by
  default. `AUTO` uses a cache when one is installed, `ON` requires one -- a CI
  job that measures the cache should fail rather than quietly build uncached --
  and `OFF` builds cold without uninstalling anything. A project that had its
  own option before keeps its name: `sccion_compiler_cache(OPTION
  MYPROJECT_COMPILER_CACHE)`.
- `CANDIDATES` lists the programs to look for, in order of preference:
  `sccion sccache ccache` by default.

A launcher the build already sets -- `CMAKE_CXX_COMPILER_LAUNCHER` from a user
preset, a toolchain file or the command line -- is kept as long as it exists. A
`RUSTC_WRAPPER` the environment sets, as the sccion GitHub Action does, is left
to cargo.

## What Rust caching covers

Library crates compiled without incremental: cargo builds registry dependencies
that way in every profile, and with the same paths in every checkout, so those
are shared between checkouts. A crate of the project itself keys on its own
checkout's path. Binaries, proc-macros and build scripts are linked by rustc and
never cached. Measured on one project's two Rust targets, built from scratch: 160
s uncached, 56 s from a warm cache. See [Rust](Rust.md).

## Running moc through sccion

A build system does not send moc through a compiler launcher -- `AUTOMOC` runs
the executable it is given -- so sccion has to *be* that executable (see
[Keying on what the compiler read](Dependencies.md)). One line per target does
the whole setup:

```cmake
find_package(Qt6 REQUIRED COMPONENTS Core)
sccion_wrap_automoc(myapp mylib)
```

It puts a copy of the sccion in use (a hard link where the filesystem allows)
in the build directory under moc's name, writes the `sccion-wraps.toml` beside
it naming the real moc -- `Qt6::moc`, else `Qt5::moc`, or `MOC <target-or-path>`
-- and makes the copy each target's `AUTOMOC_EXECUTABLE`. With sccache, ccache or
no cache, which do not cache moc, it does nothing.

## Caching what external builds produce

`cmake/SccionToolCache.cmake` is a second, separate file, for the builds a
compiler cache cannot see into: a binary SwiftPM builds (swiftc runs with no
launcher), or one cargo or zig builds, where even a warm compiler cache still
links and runs build scripts. It caches the product instead, keyed on everything
that goes into it, and restores it -- in this checkout or any other on the
machine -- when the key has been built before.

```cmake
include(cmake/SccionToolCache.cmake)
sccion_cached_tool(my_swift_tool ALL
	OUTPUTS "${CMAKE_BINARY_DIR}/swift/release/my_swift_tool"
	COMMAND swift build --configuration release
		--package-path "${CMAKE_SOURCE_DIR}/tools/swift"
		--build-path "${CMAKE_BINARY_DIR}/swift"
	SOURCE_TREES tools/swift third_party/some-path-dependency
	INPUT_FILES ${GENERATED_SOURCES}
	TOOLS "swift --version" "xcrun --show-sdk-version"
	ENVIRONMENT SDKROOT DEVELOPER_DIR MACOSX_DEPLOYMENT_TARGET
	DEPENDS ${GENERATED_SOURCES})
```

The key is a SHA-256 over:

- every file of each `SOURCE_TREES` directory as git sees it -- tracked, and
  untracked unless ignored -- by path relative to the project, so checkouts
  share entries and build output inside a tree (`.build`, `target`,
  `.zig-cache`) does not count. Outside git the tree is walked, leaving out the
  usual build directories. A symlink to a directory counts as git has it;
- each `INPUT_FILES` file: lockfiles, generated sources;
- the output of each `TOOLS` command line;
- each `ENVIRONMENT` variable's value;
- the command, its working directory and `COMMAND_ENVIRONMENT`, with the source
  and binary directories written relative, and `RECIPE_VERSION`: bump it when the
  rule changes in a way none of the rest shows.

**List every local dependency's tree.** A tree the build reads but the key
leaves out is the one way an entry can be served for the wrong inputs. Remote
dependencies are covered by their lockfile, as long as they are pinned.

The target runs on every build, and costs a few git listings and tool queries
when nothing changed. A miss says what changed since this build directory's last
key; a hit says where the entry came from. Two builds after the same key at once
build it once: the second waits for the first and restores. Each stored file is
checked against its recorded hash before it is served.

Entries go to `SCCION_TOOL_CACHE_DIR` -- the environment variable, else the
CMake cache variable, else `$XDG_CACHE_HOME/sccion-tools`,
`~/.cache/sccion-tools` or `%LOCALAPPDATA%/sccion-tools` -- keeping the newest
`SCCION_TOOL_CACHE_KEEP` (5) per tool. `-DSCCION_TOOL_CACHE=OFF` builds every
time.

## Keeping the copy current

The copy is pinned, like any vendored file, and changes only when the project
updates it. When the installed sccion ships a different module, configuring says
so and names the directory holding it; refreshing is a copy from there. The file
records the release it came from in `SCCION_CMAKE_MODULE_VERSION`.
