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

It is a single file with no dependency on sccion itself, so it is meant to be
committed into the project: a machine without sccion still configures, with
whichever cache it has.

## Using it

Copy the file into the project, say as `cmake/SccionCompilerCache.cmake`. It
comes in every release archive under `cmake/`, and the installed sccion writes
the copy matching its version on request:

```sh
sccion --cmake-module-dir    # prints the directory; the file is in it
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

## Keeping the copy current

The copy is pinned, like any vendored file, and changes only when the project
updates it. When the installed sccion ships a different module, configuring says
so and names the directory holding it; refreshing is a copy from there. The file
records the release it came from in `SCCION_CMAKE_MODULE_VERSION`.
