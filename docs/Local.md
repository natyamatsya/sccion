# Local

sccache defaults to using local disk storage. You can set the `SCCACHE_DIR` environment variable to change the disk cache location. By default it will use a sensible location for the current platform: `~/.cache/sccache` on Linux, `%LOCALAPPDATA%\Mozilla\sccache` on Windows, and `~/Library/Caches/Mozilla.sccache` on MacOS.

The default cache size is 10 gigabytes. To change this, set `SCCACHE_CACHE_SIZE`, for example `SCCACHE_CACHE_SIZE="1G"`.

The local storage only supports a single sccache server at a time. Multiple concurrent servers will race and cause spurious build failures.

## Preprocessor cache mode

This is inspired by [ccache's direct mode](https://ccache.dev/manual/3.7.9.html#_the_direct_mode) and works roughly the same.
It adds a cache that allows to skip preprocessing when compiling C/C++. This can make it much faster to return compilation results
from cache since preprocessing is a major expense for these.

Preprocessor cache mode is controlled by a configuration option which is true by default, as well as additional conditions described below.

To ensure that the cached preprocessor results for a source file correspond to the un-preprocessed inputs, sccache needs
to remember, among other things, all files included by the source file. sccache also needs to recognize
when "external factors" may change the results, such as system time if the `__TIME__` macro is used
in a source file. How conservative sccache is about some of these external factors is configurable, see below.

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
  not exist at the time. sccache does not know about such files, so it cannot invalidate the result if the header file later exists.
- A macro such as `__TIME__` (etc) is used in the source code and `ignore_time_macros` is enabled
- There are other external factors influencing the preprocessing result that sccache does not know about

Configuration options and their default values:

- `use_preprocessor_cache_mode`: `true`. Whether to use preprocessor cache mode. This can be overridden for an sccache invocation by setting the environment variable `SCCACHE_DIRECT` to `true`/`on`/`1` or `false`/`off`/`0`.
- `file_stat_matches`: `false`. If false, only compare header files by hashing their contents. If true, will use size + ctime + mtime to check whether a file has changed. See other flags below for more control over this behavior.
- `use_ctime_for_stat`: `true`. If true, uses the ctime (file status change on UNIX, creation time on Windows) to check that a file has/hasn't changed. Can be useful to disable when backdating modification times in a controlled manner.

- `ignore_time_macros`: `false`. If true, ignore `__DATE__`, `__TIME__` and `__TIMESTAMP__` being present in the source code. Will speed up preprocessor cache mode, but can produce stale results.

- `skip_system_headers`: `false`. If true, the preprocessor cache will only add the paths of included system headers to the cache key but ignore the headers' contents.

- `hash_working_directory`: `true`. If true, will add the current working directory to the cache key to distinguish two compilations from different directories.

- `use_dependency_keying`: `false`. If true, a compilation whose inputs are not already known is *compiled* and asked what it read, instead of being preprocessed so the same list can be recovered from the preprocessed text. This removes an entire compiler run from every preprocessor cache miss — on a reference MSVC build, 285ms of the 681ms such a miss costs. Only compilers that can report their inputs while compiling do this (currently MSVC, via `/sourceDependencies`); the rest preprocess as before, as do distributed compilations, which need preprocessed source.

  The trade is that a miss now costs a compile rather than a preprocess, so a compilation whose *manifest* is missing but whose *object* is already in the cache pays the difference instead of getting a cheap hit. Measure before enabling it on a build that shares a cache between checkouts.

  Whether it pays off is a property of the project rather than of the machine, so it can also be set in a committed `.sccache.toml` (see [Project configuration](#project-configuration)). It is asked for in order of how specific the answer is: the environment variable `SCCACHE_DEPEND` (`true`/`on`/`1` or `false`/`off`/`0`) for this invocation, then the project file, then this setting.

See where to write the config in [the configuration doc](Configuration.md).

## Project configuration

Some settings depend on the project being built rather than on the machine building it. Those can be set in a `.sccache.toml` committed to the repository, which sccache looks for the way tools look for `.clang-format`: by walking up from the directory a compilation runs in, stopping once it has looked in the checkout root (a directory or file named `.git`, `.jj` or `.hg`).

The point of finding it by walking up is that the file does not have to name a path. **The directory holding the file is the checkout root**, so one committed file is correct in every checkout of the project, including worktrees that do not exist yet.

```toml
# Every path under this file's directory is normalized out of cache keys, so
# two checkouts of this project can share cache entries. The file's own
# directory is always included, so an empty file is the common case.
basedirs = ["some/generated/tree"]

# See "Preprocessor cache mode" above. Absent means the project has no opinion
# and the machine-wide setting stands.
use_dependency_keying = true
```

Only these two keys are honoured, and the file is ignored (with a warning) if it contains anything else. It travels with the repository, so anything it can set is something a repository can set: the test a key has to pass is that the worst a hostile repository could do with it is make its own builds slower. Settings that could redirect a build — where the cache lives, what the compiler is — are deliberately not available here, for the same reason `.clang-format` cannot run a command. `basedirs` entries must be relative to the file; absolute ones are ignored.

A malformed file does not fail the build. The checkout root is still used as a basedir, and a warning is logged.

## Read-only cache mode

By default, the local cache operates in read/write mode. The `SCCACHE_LOCAL_RW_MODE` environment variable can be set to `READ_ONLY` (or `READ_WRITE`) to modify this behavior.

You can use read-only mode to prevent sccache from writing new cache items to the disk. This can be useful, for example, if you want to use items that have already been cached, but not add new ones to the cache. 

Note that this feature is only effective if you already have items in your cache. Using this option on an empty cache will cause sccache to simply do nothing, just add overhead.
