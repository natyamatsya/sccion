![License](https://img.shields.io/badge/license-Apache--2.0-blue)
[![Upstream](https://img.shields.io/badge/forked%20from-mozilla%2Fsccache-lightgrey)](https://github.com/mozilla/sccache)

sccion - Shared Compilation Cache
=================================

**sccion** is a compiler cache: a wrapper you put in front of your compiler that
avoids compiling anything twice, storing results on [local disk](docs/Local.md)
or in one of [several cloud storage backends](#storage-options). Multi-level
caching with automatic backfill is supported for hierarchical cache
architectures (see [Multi-Level Cache](docs/MultiLevel.md)).

It is a fork of Mozilla's [sccache](https://github.com/mozilla/sccache) — see
[Lineage](#lineage) — and a *scion* is a cutting grafted onto established
rootstock. The rootstock is the cache: one store that persists while checkouts
are cut from it and thrown away. Most of what this fork adds serves that shape,
which is the shape of a build once agents work in their own worktrees. Two
checkouts should hit each other's entries; a repository should state the facts
that decide its own cache keys once, in a committed file; and a build that
misses anyway should be answerable about *that* tree.

sccion includes support for caching the compilation of Assembler, C/C++ code,
[Rust](docs/Rust.md), as well as NVIDIA's CUDA using
[nvcc](https://docs.nvidia.com/cuda/cuda-compiler-driver-nvcc/index.html), and
[clang](https://llvm.org/docs/CompileCudaWithLLVM.html), [AMD's ROCm
HIP](https://rocm.docs.amd.com/projects/HIP/en/latest/index.html).

sccion also provides [icecream](https://github.com/icecc/icecream)-style
distributed compilation (automatic packaging of local toolchains) for all
supported compilers (including Rust). The distributed compilation system
includes several security features that icecream lacks such as authentication,
transport layer encryption, and sandboxed compiler execution on build servers.
See [the distributed quickstart](docs/DistributedQuickstart.md) guide for more
information.

> **`SCCACHE_*` still works.** The documentation below spells every setting
> `SCCION_*`, and every one of them is also read under its `SCCACHE_*` name,
> meaning exactly the same thing. `SCCION_` wins if both are set, and a
> disagreement between them is logged rather than settled in silence. The two
> variables naming the configuration file itself -- `SCCACHE_CONF` and
> `SCCACHE_CACHED_CONF` -- keep their names outright, being read before there is
> any configuration to say otherwise; everything else, the cache directory
> included, answers to both spellings. The one thing to rename is the project
> file, `.sccache.toml` -> `.sccion.toml`: it never existed in sccache, so there
> is no older setup for it to be compatible with.

---

Table of Contents (ToC)
=======================

* [Installation](#installation)
* [Usage](#usage)
* [Build Requirements](#build-requirements)
* [Build](#build)
* [Separating caches between invocations](#separating-caches-between-invocations)
* [Overwriting the cache](#overwriting-the-cache)
* [Debugging](#debugging)
* [MSVC and Windows builds](docs/Windows.md)
* [Keying on what the compiler read](docs/Dependencies.md)
* [Sharing one cache between checkouts](docs/Checkouts.md)
* [Measuring and checking](docs/Benchmarks.md)
* [Asking sccion why it missed, from an agent](docs/MCP.md)
* [Interaction with GNU `make` jobserver](#interaction-with-gnu-make-jobserver)
* [Known Caveats](#known-caveats)
* [Lineage](#lineage)
* [Storage Options](#storage-options)
  * [Multi-Level (Hierarchical Caching)](docs/MultiLevel.md)
  * [Local](docs/Local.md)
  * [S3](docs/S3.md)
  * [R2](docs/S3.md#R2)
  * [Redis](docs/Redis.md)
  * [Memcached](docs/Memcached.md)
  * [Google Cloud Storage](docs/Gcs.md)
  * [Azure](docs/Azure.md)
  * [GitHub Actions](docs/GHA.md)
  * [WebDAV (Ccache/Bazel/Gradle compatible)](docs/Webdav.md)
  * [Alibaba OSS](docs/OSS.md)
  * [Tencent Cloud Object Storage](docs/COS.md)

---

## Installation

**Build it from a checkout.** sccion is not published to crates.io and has no
prebuilt binaries or package-manager entries; anything you install under the
name `sccache` is upstream, not this.

```bash
git clone https://github.com/natyamatsya/sccion
cd sccion
cargo build --release          # target/release/sccion
```

See [Build Requirements](#build-requirements) for what the optional features
need, and [Build](#build) for the feature flags.

### With Nix

The flake in this repository builds it too:

```bash
nix build          # result/bin/sccion
nix run . -- --help
```

### If you want the upstream instead

Packaged builds of Mozilla's sccache are on [its releases
page](https://github.com/mozilla/sccache/releases/latest), in Homebrew,
MacPorts, scoop, winget, nixpkgs and crates.io. They are a different program
with a different name, and this project has no bearing on them.

---

Usage
-----

Running sccion is like running ccache: prefix your compilation commands with it, like so:

```bash
sccion gcc -o foo.o -c foo.c
```

In a repository, run `sccion --init` once and commit what it writes. That file's
*location* is its content: the directory holding it becomes the root every
compilation's paths are recorded relative to, so two checkouts of the project
share cache entries instead of each warming its own. Measured on this repository
across two worktrees of one commit: 559 of 561 compilations shared, 288 s down to
18 s. See [Sharing one cache between checkouts](docs/Checkouts.md).

The commands take dashes -- `--init`, `--check-env`, `--show-stats` -- because
everything that is not a flag is passed through to be compiled, and a subcommand
would shadow any compiler that happened to share its name.

If you want to use sccion for caching Rust builds you can define `build.rustc-wrapper` in the
[cargo configuration file](https://doc.rust-lang.org/cargo/reference/config.html).  For example, you can set it globally
in `$HOME/.cargo/config.toml` by adding:

```toml
[build]
rustc-wrapper = "/path/to/sccion"
```

Alternatively you can use the environment variable `RUSTC_WRAPPER`:

```bash
export RUSTC_WRAPPER=/path/to/sccion
cargo build
```

sccion supports gcc, clang, MSVC, rustc, [NVCC](https://docs.nvidia.com/cuda/cuda-compiler-driver-nvcc/index.html), [NVC++](https://docs.nvidia.com/hpc-sdk//compilers/hpc-compilers-user-guide/index.html), [hipcc](https://rocm.docs.amd.com/projects/HIP/en/latest/index.html), and [Wind River's diab compiler](https://www.windriver.com/products/development-tools/#diab_compiler). Both gcc and msvc support Response Files, read more about their implementation [here](docs/ResponseFiles.md).

If you don't [specify otherwise](#storage-options), sccion will use a local disk cache.

sccion works using a client-server model, where the server runs locally on the same machine as the client. The client-server model allows the server to be more efficient by keeping some state in memory. The sccion command will spawn a server process if one is not already running, or you can run `sccion --start-server` to start the background server process without performing any compilation.

By default sccion server will listen on `127.0.0.1:4226`, you can specify environment variable `SCCION_SERVER_PORT` to use a different port or `SCCION_SERVER_UDS` to listen on unix domain socket. Abstract unix socket is also supported as long as the path is escaped following the [format](https://doc.rust-lang.org/std/ascii/fn.escape_default.html). For example:

```
% env SCCION_SERVER_UDS=$HOME/sccion.sock sccion --start-server # unix socket
% env SCCION_SERVER_UDS=\\x00sccion.sock sccion --start-server # abstract unix socket
```

You can run `sccion --stop-server` to terminate the server. It will also terminate after (by default) 10 minutes of inactivity.

Running `sccion --show-stats` will print a summary of cache statistics.

Some notes about using `sccion` with [Jenkins](https://jenkins.io) are [here](docs/Jenkins.md).

To use sccion with cmake, provide the following command line arguments to cmake 3.4 or newer:

```
-DCMAKE_C_COMPILER_LAUNCHER=sccion
-DCMAKE_CXX_COMPILER_LAUNCHER=sccion
```

The process for using sccion with MSVC and cmake, depends on which version of cmake you're using. **For versions of cmake 3.24 and earlier**, to generate PDB files for debugging with MSVC, you can use the [`/Z7` option](https://docs.microsoft.com/en-us/cpp/build/reference/z7-zi-zi-debug-information-format?view=msvc-160). Alternatively, the `/Zi` option together with `/Fd` can work if `/Fd` names a different PDB file name for each object file created. It will not work if `/FS` is also passed: that flag is the build system stating that several compilations write the one PDB, so sccion declines to cache them rather than capture a file the others are still writing. CMake with Ninja passes `/FS` for exactly that reason, which makes `/Z7` the only option there. Note that CMake sets `/Zi` by default, so if you use CMake, you can use `/Z7` by adding code like this in your CMakeLists.txt:

```cmake
if(CMAKE_BUILD_TYPE STREQUAL "Debug")
  string(REPLACE "/Zi" "/Z7" CMAKE_CXX_FLAGS_DEBUG "${CMAKE_CXX_FLAGS_DEBUG}")
  string(REPLACE "/Zi" "/Z7" CMAKE_C_FLAGS_DEBUG "${CMAKE_C_FLAGS_DEBUG}")
elseif(CMAKE_BUILD_TYPE STREQUAL "Release")
  string(REPLACE "/Zi" "/Z7" CMAKE_CXX_FLAGS_RELEASE "${CMAKE_CXX_FLAGS_RELEASE}")
  string(REPLACE "/Zi" "/Z7" CMAKE_C_FLAGS_RELEASE "${CMAKE_C_FLAGS_RELEASE}")
elseif(CMAKE_BUILD_TYPE STREQUAL "RelWithDebInfo")
  string(REPLACE "/Zi" "/Z7" CMAKE_CXX_FLAGS_RELWITHDEBINFO "${CMAKE_CXX_FLAGS_RELWITHDEBINFO}")
  string(REPLACE "/Zi" "/Z7" CMAKE_C_FLAGS_RELWITHDEBINFO "${CMAKE_C_FLAGS_RELWITHDEBINFO}")
endif()
```

By default, sccion will fail your build if it fails to successfully communicate with its associated server. To have sccion instead gracefully failover to the local compiler without stopping, set the environment variable `SCCION_IGNORE_SERVER_IO_ERROR=1`.

**For versions of cmake 3.25 and later**, to compile with MSVC, you have to use the new `CMAKE_MSVC_DEBUG_INFORMATION_FORMAT` option, meant to configure the `-Z7` flag.  Additionally, you must set the cmake policy number 0141 to the NEW setting:
```cmake
set(CMAKE_MSVC_DEBUG_INFORMATION_FORMAT "$<$<CONFIG:Debug,RelWithDebInfo>:Embedded>")
cmake_policy(SET CMP0141 NEW)
```

Example configuration where we automatically look for `sccion` in the `PATH`:
```cmake
find_program(SCCACHE sccion REQUIRED)

set(CMAKE_C_COMPILER_LAUNCHER ${SCCACHE})
set(CMAKE_CXX_COMPILER_LAUNCHER ${SCCACHE})
set(CMAKE_MSVC_DEBUG_INFORMATION_FORMAT "$<$<CONFIG:Debug,RelWithDebInfo>:Embedded>")
cmake_policy(SET CMP0141 NEW)
```

Alternatively, if configuring cmake with MSVC on the command line, assuming that sccion is on the default search path:

```
cmake -DCMAKE_C_COMPILER_LAUNCHER=sccion -DCMAKE_CXX_COMPILER_LAUNCHER=sccion -DCMAKE_MSVC_DEBUG_INFORMATION_FORMAT=Embedded -DCMAKE_POLICY_CMP0141=NEW [...]
```

And you can build code as usual without any additional flags in the command line, which is useful for IDEs.


---

Build Requirements
------------------

sccion is a [Rust](https://www.rust-lang.org/) program. Building it requires `cargo` (and thus`rustc`). sccion currently requires **Rust 1.91.0**. We recommend you install Rust via [Rustup](https://rustup.rs/).

Build
-----

If you are building sccion for non-development purposes make sure you use `cargo build --release` to get optimized binaries:

```bash
cargo build --release [--no-default-features --features=s3|redis|gcs|memcached|azure|gha|webdav|oss|cos]
```

The list of features can be found in the `Cargo.toml` file, `[features]` section.

By default, `sccion` builds with support for all storage backends, but individual backends may be disabled by resetting the list of features and enabling all the other backends. Refer the [Cargo Documentation](http://doc.crates.io/manifest.html#the-features-section) for details on how to select features with Cargo.

### Building portable binaries

When building with the `dist-server` feature, `sccion` will depend on OpenSSL, which can be an annoyance if you want to distribute portable binaries. It is possible to statically link against OpenSSL using the `vendored-openssl` feature.

#### Linux

Build with `cargo` and use `ldd` to check that the resulting binary does not depend on OpenSSL anymore.

#### macOS

Build with `cargo` and use `otool -L` to check that the resulting binary does not depend on OpenSSL anymore.

#### Windows

On Windows, the binary might also depend on a few MSVC CRT DLLs that are not available on older Windows versions.

It is possible to statically link against the CRT using a `.cargo/config.toml` file with the following contents.

```toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-Ctarget-feature=+crt-static"]
```

Build with `cargo` and use `dumpbin /dependents` to check that the resulting binary does not depend on MSVC CRT DLLs anymore.

When statically linking with OpenSSL, you will need Perl available in your `$PATH`.

---

Separating caches between invocations
-------------------------------------

In situations where several different compilation invocations
should not reuse the cached results from each other,
one can set `SCCION_C_CUSTOM_CACHE_BUSTER` to a unique value
that'll be mixed into the hash.
`MACOSX_DEPLOYMENT_TARGET` and `IPHONEOS_DEPLOYMENT_TARGET` variables
already exhibit such reuse-suppression behaviour.
There are currently no such variables for compiling Rust.

---

Overwriting the cache
---------------------

In situations where the cache contains broken build artifacts, it can be necessary to overwrite the contents in the cache. That can be achieved by setting the `SCCION_RECACHE` environment variable.

---

Debugging
---------

You can set the `SCCION_ERROR_LOG` environment variable to a path and set `SCCION_LOG` to get the server process to redirect its logging there (including the output of unhandled panics, since the server sets `RUST_BACKTRACE=1` internally).

    SCCION_ERROR_LOG=/tmp/sccion_log.txt SCCION_LOG=debug sccion

You can also set these environment variables for your build system, for example

    SCCION_ERROR_LOG=/tmp/sccion_log.txt SCCION_LOG=debug cmake --build /path/to/cmake/build/directory

Alternatively, if you are compiling locally, you can run the server manually in foreground mode by running `SCCION_START_SERVER=1 SCCION_NO_DAEMON=1 sccion`, and send logging to stderr by setting the [`SCCION_LOG` environment variable](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for example. This method is not suitable for CI services because you need to compile in another shell at the same time.

    SCCION_LOG=debug SCCION_START_SERVER=1 SCCION_NO_DAEMON=1 sccion

---

Interaction with GNU `make` jobserver
-------------------------------------

sccion provides support for a [GNU make jobserver](https://www.gnu.org/software/make/manual/html_node/Job-Slots.html). When the server is started from a process that provides a jobserver, sccion will use that jobserver and provide it to any processes it spawns. (If you are running sccion from a GNU make recipe, you will need to prefix the command with `+` to get this behavior.) If the sccion server is started without a jobserver present it will create its own with the number of slots equal to the number of available CPU cores.

This is most useful when using sccion for Rust compilation, as rustc supports using a jobserver for parallel codegen, so this ensures that rustc will not overwhelm the system with codegen tasks. Cargo implements its own jobserver ([see the information on `NUM_JOBS` in the cargo documentation](https://doc.rust-lang.org/stable/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-build-scripts)) for rustc to use, so using sccion for Rust compilation in cargo via `RUSTC_WRAPPER` should do the right thing automatically.

---

Normalizing Paths with `SCCION_BASEDIRS`
-----------------------------------------

By default, sccion requires absolute paths to match for cache hits. To enable cache sharing across different build directories, you can set `SCCION_BASEDIRS` to strip a base directory from paths before hashing:

```bash
export SCCION_BASEDIRS=/home/user/project
```

You can also specify multiple base directories by separating them by `;` on Windows hosts and by `:` on any other operating system. When multiple directories are provided, the longest matching prefix is used:

```bash
export SCCION_BASEDIRS="/home/user/project:/home/user/workspace"
```

Path matching is **case-insensitive** on Windows and **case-sensitive** on other operating systems.

This is similar to ccache's `CCACHE_BASEDIR` and helps when:
* Building the same project from different directories
* Sharing cache between CI jobs with different checkout paths
* Multiple developers working with different username paths
* Working with multiple project checkouts simultaneously

**Note:** Only absolute paths are supported. Relative paths will prevent server from starting.

You can also configure this in the sccion config file:

```toml
# Single directory
basedirs = ["/home/user/project"]

# Or multiple directories
basedirs = ["/home/user/project", "/home/user/workspace"]
```

---

Known Caveats
-------------

### General

* By default, absolute paths to files must match to get a cache hit. To work around this, use `SCCION_BASEDIRS` (see above) to normalize paths before hashing.

### Rust

* Crates that invoke the system linker cannot be cached. This includes `bin`, `dylib`, `cdylib`, and `proc-macro` crates. You may be able to improve compilation time of large `bin` crates by converting them to a `lib` crate with a thin `bin` wrapper.
* Incrementally compiled crates cannot be cached. By default, in the debug profile Cargo will use incremental compilation for workspace members and path dependencies. [You can disable incremental compilation.](https://doc.rust-lang.org/cargo/reference/profiles.html#incremental)

[More details on Rust caveats](/docs/Rust.md)

### C++20 Modules

sccion has partial support for C++20 named modules when using **Clang**. The following flags are supported:

* `-fmodule-file=<path>` and `-fmodule-file=<name>=<path>` - importing precompiled module interfaces
* `-fmodule-output=<path>` - generating module interface output alongside object files
* `--precompile` - compiling module interface units
* `-fmodules-reduced-bmi` - generating reduced BMI files

The following module-related flags are **not supported** and will bypass the cache:

* `-fmodules` and `-fcxx-modules` - Clang header modules (not C++20 named modules)
* `-fprebuilt-implicit-modules` and `-fprebuilt-module-path` - implicit module discovery

**GCC** and **MSVC** C++20 modules are not yet supported. Compilations using `-fmodules-ts` (GCC) or `/interface`, `/ifcOutput`, etc. (MSVC) will bypass the cache.

### User Agent

* Requests sent to your storage option of choice will have a user agent header indicating the current sccion version, e.g. `sccion/0.17.0`.

Lineage
-------

sccion is a fork of [sccache](https://github.com/mozilla/sccache), originally
written by Ted Mielczarek and maintained by Mozilla and the sccache
contributors. The full history is preserved in this repository's git log,
beginning at `115016e` — nothing was squashed, so every upstream commit remains
attributable to whoever wrote it.

**Licence.** sccache is licensed under the Apache License, Version 2.0, and
sccion is distributed under the same licence. A copy is in [LICENSE](LICENSE).
You may obtain a copy at <http://www.apache.org/licenses/LICENSE-2.0>. Unless
required by applicable law or agreed to in writing, software distributed under
the Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS
OF ANY KIND, either express or implied.

**Statement of changes** (Apache-2.0 §4(b)). Files modified in this fork carry a
`SPDX-FileCopyrightText` line naming the fork's contributors alongside the
sccache contributors. In summary, sccion adds:

* **Dependency keying** — cache keys derived from the compiler's own dependency
  report rather than from preprocessed source. See
  [docs/Dependencies.md](docs/Dependencies.md).
* **Project configuration** — a committed `.sccion.toml`, discovered by walking
  up from the source like `.clang-format`, that lets a repository declare the
  facts about itself that affect its own cache keys. See
  [docs/Checkouts.md](docs/Checkouts.md).
* **Sharing one cache between checkouts** — basedir rewriting so two working
  copies of the same repository, or an agent's worktree and the tree it was cut
  from, hit each other's entries. See [docs/Checkouts.md](docs/Checkouts.md).
* **An MCP query surface** — `sccion-mcp`, a read-only adapter that lets an
  agent ask why *its* build missed. Questions are scoped by a workspace handle,
  so an answer is about one checkout and is never served to another. See
  [docs/MCP.md](docs/MCP.md).
* **`--check-env`** — a report on the environment a build would run in, and
  which configuration layer decided each setting.
* **Windows and MSVC work** — see [docs/Windows.md](docs/Windows.md).
* **Measurement** — see [docs/Benchmarks.md](docs/Benchmarks.md).

**Names.** The binaries are `sccion`, `sccion-dist` and `sccion-mcp`. Settings
are spelled `SCCION_*` and are read under `SCCACHE_*` as well. The cache and the
configuration file are `~/.cache/sccion` and `~/.config/sccion/config` on Linux
and macOS alike, and under `%LOCALAPPDATA%` and `%APPDATA%` on Windows: the
convention a command-line program follows, rather than the per-platform one that
would put a cache you manage from a terminal in `~/Library/Caches`. Wherever
sccache already left either of them, that path keeps being read instead — a
rename may not strand a warm cache, nor a configuration file written years ago.
The project file is `.sccion.toml` with no fallback, because it is this fork's
own invention and has no upstream spelling to honour. sccion
is not affiliated with or endorsed by Mozilla, and the Mozilla name and the
sccache trade marks are used here only to identify the work this one is derived
from.

Storage Options
---------------

* [Local](docs/Local.md)
* [S3](docs/S3.md)
* [R2](docs/S3.md#R2)
* [Redis](docs/Redis.md)
* [Memcached](docs/Memcached.md)
* [Google Cloud Storage](docs/Gcs.md)
* [Azure](docs/Azure.md)
* [GitHub Actions](docs/GHA.md)
* [WebDAV (Ccache/Bazel/Gradle compatible)](docs/Webdav.md)
* [Alibaba OSS](docs/OSS.md)
* [Tencent Cloud Object Storage](docs/COS.md)
