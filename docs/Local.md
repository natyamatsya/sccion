# Local

sccion defaults to using local disk storage. You can set the `SCCION_DIR` environment variable to change the disk cache location. By default it uses `~/.cache/sccion` on Linux **and macOS**, and `%LOCALAPPDATA%\sccion` on Windows.

macOS gets the XDG location rather than `~/Library/Caches` deliberately. A compiler cache is something you `du`, watch, and delete when an experiment goes wrong, and each of those is worse when the directory is somewhere the shell does not go by habit. It also makes one instruction — "clear `~/.cache/sccion`" — true on both Unixes.

**If sccache already left a cache on this machine**, that directory keeps being used instead: `~/.cache/sccache`, `~/Library/Caches/Mozilla.sccache`, or `%LOCALAPPDATA%\Mozilla\sccache`, whichever the platform has. Moving the default out from under a warm cache would report no error at all — only a build that suddenly misses everything, which is the failure this program exists to make visible. Delete the old directory to migrate; the next build starts filling the new one, cold.

The default cache size is 10 gigabytes. To change this, set `SCCION_CACHE_SIZE`, for example `SCCION_CACHE_SIZE="1G"`.

The local storage only supports a single sccion server at a time. Multiple concurrent servers will race and cause spurious build failures.

## Preprocessor cache mode

This is inspired by [ccache's direct mode](https://ccache.dev/manual/3.7.9.html#_the_direct_mode) and works roughly the same.
It adds a cache that allows to skip preprocessing when compiling C/C++. This can make it much faster to return compilation results
from cache since preprocessing is a major expense for these.

Preprocessor cache mode is controlled by a configuration option which is true by default, as well as additional conditions described below.

To ensure that the cached preprocessor results for a source file correspond to the un-preprocessed inputs, sccion needs
to remember, among other things, all files included by the source file. sccion also needs to recognize
when "external factors" may change the results, such as system time if the `__TIME__` macro is used
in a source file. How conservative sccion is about some of these external factors is configurable, see below.

Preprocessor cache mode will be disabled in any of the following cases:

- Not compiling C or C++
- The configuration option is false
- Not using GCC or Clang
- Not using local storage for the cache
- Any of the compiler options `-MP`, `-Xpreprocessor`, `-Wp,` are present
- The modification time of one of the header files is too new (avoids a race condition)
- Certain strings such as `__DATE__`, `__TIME__`, `__TIMESTAMP__` are present in the source code,
  indicating that the preprocessor result may change based on external factors

The preprocessor cache may silently produce stale results in any of the following cases:

- When a source file was compiled and its results were cached, a header file would have been included if it existed, but it did
  not exist at the time. sccion does not know about such files, so it cannot invalidate the result if the header file later exists.
- A macro such as `__TIME__` (etc) is used in the source code and `ignore_time_macros` is enabled
- There are other external factors influencing the preprocessing result that sccion does not know about

Configuration options and their default values:

- `use_preprocessor_cache_mode`: `true`. Whether to use preprocessor cache mode. This can be overridden for an sccion invocation by setting the environment variable `SCCION_DIRECT` to `true`/`on`/`1` or `false`/`off`/`0`.
- `file_stat_matches`: `false`. If false, only compare header files by hashing their contents. If true, will use size + ctime + mtime to check whether a file has changed. See other flags below for more control over this behavior.
- `use_ctime_for_stat`: `true`. If true, uses the ctime (file status change on UNIX, creation time on Windows) to check that a file has/hasn't changed. Can be useful to disable when backdating modification times in a controlled manner.

- `ignore_time_macros`: `false`. If true, ignore `__DATE__`, `__TIME__` and `__TIMESTAMP__` being present in the source code. Will speed up preprocessor cache mode, but can produce stale results.

- `skip_system_headers`: `false`. If true, the preprocessor cache will only add the paths of included system headers to the cache key but ignore the headers' contents.

- `hash_working_directory`: `true`. If true, adds the directory a compilation runs in to the cache key, so that two compilations from different directories do not share an entry. This matters because a compiler records the path it saw in the object's debug information, while nothing else in the key reliably carries it: MSVC's `-EP` emits no paths at all, and a build naming its sources relatively puts none in its arguments either. Setting it to `false` trades that guarantee for hits, and is safe for a build whose objects do not embed absolute paths (`/Z7` with `/d1trimfile`, or `-fdebug-prefix-map`).

  Basedirs apply here as they do elsewhere: the directory is recorded relative to a declared checkout root, so declaring one keeps two checkouts of a project sharing entries. That is what declaring it asserts.

- `use_dependency_keying`: `true`. A compilation whose inputs are not already known is *compiled* and asked what it read, instead of being preprocessed so the same list can be recovered from the preprocessed text. This removes an entire compiler run from every preprocessor cache miss. Only compilers that can report their inputs while compiling do this (currently MSVC, via `/sourceDependencies`); the rest preprocess as before, as do distributed compilations, which need preprocessed source.

  Measured on a 2557-unit MSVC + Qt + CMake build: cold 485.4s → 414.8s, warm 99.5s → 94.8s. The warm side improves because a source file that includes nothing gets a manifest for the first time — sccion does not store one when the include list is empty, so such files re-preprocessed on every build.

  The trade is that a miss now costs a compile rather than a preprocess, so a compilation whose *manifest* is missing but whose *object* is already in the cache pays the difference instead of getting a cheap hit. That is not hypothetical: on the same build, a *second checkout's first build* against a cache the first one filled took 359s with this on and 306s with it off, because a manifest that does not match still leaves the object findable by preprocessing.

  So: leave it on if you build one checkout repeatedly, and set it to `false` — in the config, or in a `.sccion.toml` committed to a project that does otherwise — if you routinely build fresh checkouts or worktrees against a shared cache.

  Whether it pays off is a property of the project rather than of the machine, so it can also be set in a committed `.sccion.toml` (see [Project configuration](#project-configuration)). It is asked for in order of how specific the answer is: the environment variable `SCCION_DEPEND` (`true`/`on`/`1` or `false`/`off`/`0`) for this invocation, then the project file, then this setting.

See where to write the config in [the configuration doc](Configuration.md).

## Project configuration

Some settings depend on the project being built rather than on the machine building it. Those can be set in a `.sccion.toml` committed to the repository, which sccion looks for the way tools look for `.clang-format`: by walking up from the directory a compilation runs in, stopping once it has looked in the checkout root (a directory or file named `.git`, `.jj` or `.hg`).

The point of finding it by walking up is that the file does not have to name a path. **The directory holding the file is the checkout root**, so one committed file is correct in every checkout of the project, including worktrees that do not exist yet.

```toml
# Every path under this file's directory is normalized out of cache keys, so
# two checkouts of this project can share cache entries. The file's own
# directory is always included, so an empty file is the common case.
basedirs = ["some/generated/tree"]

# See "Preprocessor cache mode" above. Absent means the project has no opinion
# and the machine-wide setting stands.
use_dependency_keying = true

# Decline preprocessor cache mode for this project. `true` only means "no
# objection": whether the mode is available at all depends on the cache
# backend, which a checkout does not get to assert. The counterpart of
# SCCION_DIRECT.
use_preprocessor_cache_mode = false

# Files every compilation in this tree depends on but does not include -- a
# toolchain manifest, a generated config header. Their contents go into every
# cache key. The counterpart of SCCION_EXTRAFILES, named by the repository
# that knows about them rather than by whoever remembered to export it.
extra_hash_files = ["toolchain.lock", "generated/buildinfo.h"]

# An arbitrary string folded into every cache key from this tree. Change it to
# invalidate the project's own entries. The counterpart of
# SCCION_C_CUSTOM_CACHE_BUSTER.
custom_cache_buster = "v2"

# Refuse to have this checkout's build state answered for over MCP (see
# docs/MCP.md). Only `false` is honoured: a project may decline, and cannot
# grant. See below for why this one key is one-directional.
expose_over_mcp = false
```

For each of these, the most specific answer wins: the environment variable for this invocation, then the project file, then the machine-wide config. A file that says nothing about a setting leaves it alone — absent is not the same as `false`.

`expose_over_mcp` is the exception, and deliberately so. Every other key here shapes cache keys, where the rule below is satisfied in both directions. This one governs whether build state can be *read*, so it only satisfies it in the refusing direction: a checkout that could switch the MCP surface **on** would be a way to make a machine start answering questions about itself by being cloned onto it. So `false` is honoured and `true` is ignored, leaving the machine's setting standing. Disabling it machine-wide is `SCCION_MCP=false` or `mcp = false` in the config, and that always wins.

Only these keys are honoured, and the file is ignored (with a warning) if it contains anything else. It travels with the repository, so anything it can set is something a repository can set: the test a key has to pass is that the worst a hostile repository could do with it is make its own builds slower or less shared. Settings that could redirect a build — where the cache lives, what the compiler is, which server to talk to — are deliberately not available here, for the same reason `.clang-format` cannot run a command.

Most of sccion's configuration cannot be set here even in principle, quite apart from that rule: one sccion server serves every checkout on the machine and reads its configuration once at startup, so the cache backend, cache size, compression, logging, idle timeout and distributed-build settings are fixed for the server's lifetime. Two checkouts asking for different ones would have no answer.

`basedirs` and `extra_hash_files` entries must be relative to the file and stay under it; absolute paths, and paths containing `..`, are ignored with a warning.

A malformed file does not fail the build. The checkout root is still used as a basedir, and a warning is logged.

## Read-only cache mode

By default, the local cache operates in read/write mode. The `SCCION_LOCAL_RW_MODE` environment variable can be set to `READ_ONLY` (or `READ_WRITE`) to modify this behavior.

You can use read-only mode to prevent sccion from writing new cache items to the disk. This can be useful, for example, if you want to use items that have already been cached, but not add new ones to the cache. 

Note that this feature is only effective if you already have items in your cache. Using this option on an empty cache will cause sccion to simply do nothing, just add overhead.
