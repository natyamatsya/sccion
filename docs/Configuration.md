# Available Configuration Options

## file

```toml
# If specified, wait this long for the server to start up.
server_startup_timeout_ms = 10000

# Whether the out-of-band MCP query surface may be used on this machine
# (see docs/MCP.md). Enabled by default; set it to false on a build
# server, where there is no agent to serve. SCCACHE_MCP overrides it.
mcp = true

# Base directories to strip from source paths during cache key
# computation.
#
# Similar to ccache's CCACHE_BASEDIR, but supports multiple paths.
#
# 'basedirs' enables cache hits across different absolute root
# paths when compiling the same source code, such as between
# parallel checkouts of the same project, Git worktrees, or different
# users in a shared environment.
# When multiple matching paths are provided, the longest prefix
# is used.
#
# Path matching is case-insensitive on Windows and case-sensitive on other OSes.
#
# Example:
#   basedir = ["/home/user/project"] results in the path prefix rewrite:
#   "/home/user/project/src/main.c" -> "src/main.c"
basedirs = ["/home/user/project"]
# basedirs = ["/home/user/project", "/home/user/workspace"]

[dist]
# where to find the scheduler
scheduler_url = "http://1.2.3.4:10600"
# a set of prepackaged toolchains
toolchains = []
# the maximum size of the toolchain cache in bytes
toolchain_cache_size = 5368709120
cache_dir = "/home/user/.cache/sccache-dist-client"

[dist.auth]
type = "token"
token = "secrettoken"

# Multi-level cache configuration
# Define cache levels in order (fast to slow).
# Each level must be separately configured below.
# See docs/MultiLevel.md for details.
[cache.multilevel]
chain = ["disk", "redis", "s3"]
write_error_policy = "l0"  # Optional: ignore, l0 (default), or all

#[cache.azure]
# Azure Storage connection string (see <https://docs.azure.cn/en-us/storage/common/storage-configure-connection-string>)
connection_string = "BlobEndpoint=https://example.blob.core.windows.net/;SharedAccessSignature=..."
# Name of container
container = "my_container_name"
# Optional string to prepend to each blob storage key
key_prefix = ""

# How cache entry payloads are stored: "zstd" (default) or "none". Applies to
# whichever backend is in use, which is why it is not under [cache.*].
# Overridden by SCCACHE_CACHE_COMPRESSION.
#
# NOTE: top-level settings must appear before any [table] header. In TOML a key
# written below one belongs to that table, and sccache will reject it.
compression = "zstd"
# The zstd level, when compressing. Overridden by SCCACHE_CACHE_ZSTD_LEVEL.
zstd_level = 3

[cache.disk]
dir = "/tmp/.cache/sccache"
size = 7516192768 # 7 GiBytes

# See the local docs on more explanations about this mode
[cache.disk.preprocessor_cache_mode]
# Whether to use the preprocessor cache mode
use_preprocessor_cache_mode = true
# Whether to use file times to check for changes
file_stat_matches = true
# Whether to also use ctime (file status change) time to check for changes
use_ctime_for_stat = true
# Whether to ignore `__TIME__` when caching
ignore_time_macros = false
# Whether to skip (meaning not cache, only hash) system headers
skip_system_headers = false
# Whether hash the current working directory
hash_working_directory = true
# Whether to learn a compilation's inputs by compiling and asking, rather than
# by preprocessing first
use_dependency_keying = true

[cache.gcs]
# optional oauth url
oauth_url = "..."
# optional deprecated url
deprecated_url = "..."
rw_mode = "READ_ONLY"
# rw_mode = "READ_WRITE"
cred_path = "/psst/secret/cred"
bucket = "bucket"
key_prefix = "prefix"

[cache.gha]
url = "http://localhost"
token = "secret"
cache_to = "sccache-latest"
cache_from = "sccache-"

[cache.memcached]
# Deprecated alias for `endpoint`
# url = "127.0.0.1:11211"
endpoint = "tcp://127.0.0.1:11211"
# Username and password for authentication
username = "user"
password = "passwd"
# Entry expiration time in seconds. Default is 86400 (24 hours)
expiration = 3600
key_prefix = "/custom/prefix/if/need"

[cache.redis]
# Deprecated, use `endpoint` instead
url = "redis://user:passwd@1.2.3.4:6379/?db=1"
## Refer to the `opendal` documentation for more information about Redis endpoint
# Single-node endpoint. Mutually exclusive with `cluster_endpoints`
endpoint = "redis://127.0.0.1:6379"
# Multiple-node list of endpoints (cluster mode). Mutually exclusive with `endpoint`
cluster_endpoints = "redis://10.0.0.1:6379,redis://10.0.0.2:6379"
username = "user"
password = "passwd"
# Database number to use. Default is 0
db = 1
# Entry expiration time in seconds. Default is 0 (never expire)
expiration = 3600
key_prefix = "/custom/prefix/if/need"

[cache.s3]
bucket = "name"
endpoint = "s3-us-east-1.amazonaws.com"
use_ssl = true
key_prefix = "s3prefix"
# SSE-S3 with an S3-managed key (AES256).
server_side_encryption = false
# SSE-KMS with the AWS-managed KMS key (aws/s3).
server_side_encryption_aws_kms = false
# SSE-KMS with a customer-managed KMS key (takes precedence when set).
# server_side_encryption_kms_key_id = "arn:aws:kms:us-east-1:111:key/abc"

[cache.webdav]
endpoint = "http://192.168.10.42:80/some/webdav.php"
key_prefix = "/custom/webdav/subfolder/if/need"
# Basic HTTP authentication credentials.
username = "alice"
password = "secret12"
# Mutually exclusive with username & password. Bearer token value
token = "token123"

[cache.oss]
bucket = "name"
endpoint = "oss-us-east-1.aliyuncs.com"
key_prefix = "ossprefix"
no_credentials = true

[cache.cos]
bucket = "name"
endpoint = "cos.na-siliconvalley.myqcloud.com"
key_prefix = "cosprefix"
```

sccache looks for its configuration file at the path indicated by env variable `SCCACHE_CONF`.

If no such env variable is set, sccache looks at default locations as below:
- Linux: `~/.config/sccache/config`
- macOS: `~/Library/Application Support/Mozilla.sccache/config`
- Windows: `%APPDATA%\Mozilla\sccache\config\config`

The latest `cache.XXX` entries may be found here: https://github.com/mozilla/sccache/blob/ffe3070f77ef3301c8ff718316e4ab017ec83042/src/config.rs#L300.

## env

Whatever is set by a file based configuration, it is overruled by the env
configuration variables

Note that some env variables may need sccache server restart to take effect.

### misc

* `SCCACHE_ALLOW_CORE_DUMPS` to enable core dumps by the server
* `SCCACHE_CONF` configuration file path
* `SCCACHE_BASEDIRS` base directory (or directories) to strip from paths for cache key computation. This is similar to ccache's `CCACHE_BASEDIR` and enables cache hits across different absolute paths when compiling the same source code. Multiple directories can be separated by `;` on Windows hosts and by `:` on any other operating system. When multiple directories are specified, the longest matching prefix is used. Path matching is **case-insensitive** on Windows and **case-sensitive** on other operating systems. Environment variable takes precedence over file configuration. Only absolute paths are supported; relative paths will cause an error and prevent the server from start.
* `SCCACHE_CACHED_CONF`
* `SCCACHE_IDLE_TIMEOUT` how long the local daemon process waits for more client requests before exiting, in seconds. Set to `0` to run sccache permanently
* `SCCACHE_STARTUP_NOTIFY` specify a path to a socket which will be used for server completion notification
* `SCCACHE_MAX_FRAME_LENGTH` how much data can be transferred between client and server
* `SCCACHE_NO_DAEMON` set to `1` to disable putting the server to the background
* `SCCACHE_CLIENT_SIDE` set to `1` to run the compile in the client process and use the daemon only as a gateway to the cache storage (see [the architecture doc](Architecture.md#client-side-mode-sccache_client_side)). This is the recommended mode and is expected to become the only supported configuration in the future. Ignored when `SCCACHE_ERROR_LOG` or distributed compilation is in use.
* `SCCACHE_CACHE_MULTIARCH` to disable caching of multi architecture builds.
* `SCCACHE_CACHE_ZSTD_LEVEL` to set zstd compression level of cache. the range is `1-22` and default is `3`.
* `SCCACHE_CACHE_COMPRESSION` to disable compression entirely: set it to `none`
  (or `off`/`false`/`0`) to store cache entries verbatim. Anything else, or unset,
  keeps zstd at `SCCACHE_CACHE_ZSTD_LEVEL`.

  This trades cache capacity for a cheaper cache hit. Measured on a 1.4 MiB MSVC
  `/Z7` object: the entry grows from 263 KiB to 1,458 KiB (5.5x), while the
  server-side work to materialize a hit falls from 5.1 ms to 3.0 ms and
  end-to-end hit latency from 27.0 ms to 24.5 ms. Whether that shows up in a
  build's wall clock depends on whether materialization is on its critical path —
  at high `-j` it usually is not.

  Both settings are read by the **server**, once, at startup: change either and
  restart it with `sccache --stop-server`. Entries record their own encoding, so
  a cache may hold a mix, and one written by an older sccache still reads
  correctly — changing this does not invalidate anything.
  - For example, in `10`, it have about 0.9x size with about 1.6x time than default `3` (tested with compiling sccache code)
  - This option will only applied to newly compressed cache and don't affect existing cache.
  - If you want to be apply to all cache, you should reset cache and make new cache.
* `SCCACHE_LOG_MILLIS` when set (to any value), enables millisecond precision timestamps in log output instead of the default second precision.
* `SCCACHE_MCP` set to `false` (or `off`/`0`) disables the [MCP query
  surface](MCP.md) on this machine. Enabled by default, so a developer machine
  needs no configuration.

  **Turn it off on a build server.** The surface exists so that an agent can ask
  why a build missed; where there is no agent it is only a way to read build
  state with nobody to serve. The environment variable outranks the `mcp` key in
  the config file, so a CI job can disable it without editing machine
  configuration.

  Read by the **server** at startup, like the cache settings: change it and
  restart with `sccache --stop-server`. `sccache-mcp` also refuses to start, so
  the failure is one clear line rather than an error per tool call.
* `SCCACHE_AGENT_ID` names the caller, so `--show-stats` can break its numbers
  down per caller instead of reporting one sum with no addends. One daemon
  serves every build on the machine, and "2557 hits" does not say whether one
  build got all of them and another none.

  Read from each compilation's environment, so it takes effect in the shell that
  sets it without restarting the daemon. When unset, the checkout root stands
  in — concurrent builds are usually concurrent checkouts, so the breakdown is
  correct without the callers having to set anything. The per-caller section is
  printed only when there is more than nothing to say. At most 64 callers are
  tracked separately; the rest share an `<other>` bucket, so the totals stay
  right even when the detail is lost.
* `SCCACHE_MISS_LOG` path to a file every cache miss is appended to, one
  tab-separated record per line. `--show-stats` counts misses but cannot say
  which translation units or why, and `SCCACHE_LOG=debug` can say but only
  inside a trace of every compilation — large enough, and slow enough, to change
  what is being measured.

  Unlike most settings here this is read from the environment of **each
  compilation**, not the server's, so it takes effect in the shell that sets it
  without restarting the daemon and without that shell having to be the one that
  started it. The daemon always keeps the last few thousand records in memory
  regardless; this is for when you want all of them.

  The columns, in order. New fields are appended, so a one-liner reading a given
  column keeps reading the same thing:

  | # | Column | Notes |
  |---|---|---|
  | 1 | classification | `cold`, `read_error`, `timed_out`, `forced_recache`, `forced_no_cache`, or `uncacheable:<reason>` |
  | 2 | preprocessor cache mode | `direct-hit`, `direct-miss`, `direct-off` |
  | 3 | cache key | `-` when the compilation was refused before one could be computed |
  | 4 | output | usually a basename; the compiler's own name when the arguments were never parsed |
  | 5 | input | basedir-relative; `-` when the compilation has no single input |
  | 6 | language | as `--show-stats` names it |
  | 7 | workspace | the checkout root, basedir-relative |
  | 8 | agent | see `SCCACHE_AGENT_ID` |
  | 9 | command digest | of the basedir-stripped arguments |
  | 10 | sequence | the order the daemon saw them in |

  `uncacheable:` is the one that says *stop*: `time_macros`,
  `dependency_report_unreadable`, `shared_pdb` and the rest are permanent
  properties of the compilation as invoked, so retrying it unchanged reaches the
  same verdict. Every other classification may well succeed next time.
* `SCCACHE_ERROR_LOG` path to a file where sccache will log errors
* `SCCACHE_LOG` log level, accepting standard env_logger values, see [env_logger documentation](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for details

### cache configs

#### multi-level cache

Multi-level caching enables hierarchical cache storage with automatic backfill. See the [Multi-Level Cache documentation](MultiLevel.md) for detailed information.

* `SCCACHE_MULTILEVEL_CHAIN` comma-separated list of cache backend names to use in hierarchy (e.g., `disk,redis,s3`)
  - Order matters: left-to-right is fast-to-slow (L0, L1, L2, ...)
  - Valid names: `disk`, `redis`, `memcached`, `s3`, `gcs`, `azure`, `gha`, `webdav`, `oss`, `cos`
  - Each level must be separately configured with its own environment variables
  - If not set, sccache uses single-level mode (legacy behavior)
* `SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY` controls error handling on cache writes (default: `l0`)
  - `ignore` - never fail on write errors, log warnings only (most permissive)
  - `l0` - fail only if L0 (first level) write fails (default, balances reliability and performance)
  - `all` - fail if any read-write level fails (most strict)
  - Read-only levels are always skipped and never cause failures

**Basic example**:
```bash
export SCCACHE_MULTILEVEL_CHAIN="disk,redis,s3"
export SCCACHE_DIR="/tmp/cache"              # for disk level
export SCCACHE_REDIS_ENDPOINT="redis://..."  # for redis level
export SCCACHE_BUCKET="my-bucket"            # for s3 level
```

**Write policy examples**:
```bash
# Default: Fail only if disk write fails
export SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY="l0"

# Best effort: Never fail on cache writes
export SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY="ignore"

# Strict: Fail if any level write fails
export SCCACHE_MULTILEVEL_WRITE_ERROR_POLICY="all"
```

#### disk (local)

* `SCCACHE_DIR` local on disk artifact cache directory
* `SCCACHE_CACHE_SIZE` maximum size of the local on disk cache i.e. `2G` - default is 10G
* `SCCACHE_DIRECT` enable/disable preprocessor caching (see [the local doc](Local.md))
* `SCCACHE_LOCAL_RW_MODE` the mode that the cache will operate in (`READ_ONLY` or `READ_WRITE`)

#### s3 compatible

* `SCCACHE_BUCKET` s3 bucket to be used
* `SCCACHE_ENDPOINT` s3 endpoint
* `SCCACHE_REGION` s3 region, required if using AWS S3
* `SCCACHE_S3_USE_SSL` s3 endpoint requires TLS, set this to `true`
* `SCCACHE_S3_KEY_PREFIX` s3 key prefix (optional)
* `SCCACHE_S3_RW_MODE` allows to use s3 backend in read-only mode if set to `READ_ONLY`

The endpoint used then becomes `${SCCACHE_BUCKET}.s3-{SCCACHE_REGION}.amazonaws.com`.
If you are not using the default endpoint and `SCCACHE_REGION` is undefined, it
will default to `us-east-1`.

#### cloudflare r2

* `SCCACHE_BUCKET` is the name of your R2 bucket.
* `SCCACHE_ENDPOINT` must follow the format of `https://<ACCOUNT_ID>.r2.cloudflarestorage.com`. Note that the `https://` must be included. Your account ID can be found [here](https://developers.cloudflare.com/fundamentals/get-started/basic-tasks/find-account-and-zone-ids/).
* `SCCACHE_REGION` should be set to `auto`.
* `SCCACHE_S3_KEY_PREFIX` s3 key prefix (optional).

#### redis

* `SCCACHE_REDIS` full redis url, including auth and access token/passwd (deprecated).
* `SCCACHE_REDIS_ENDPOINT` redis url without auth and access token/passwd - single node configuration.
* `SCCACHE_REDIS_CLUSTER_ENDPOINTS` redis cluster urls, separated by comma - shared cluster configuration.
* `SCCACHE_REDIS_USERNAME` redis username (optional).
* `SCCACHE_REDIS_PASSWORD` redis password (optional).
* `SCCACHE_REDIS_DB` redis database (optional, default is 0).
* `SCCACHE_REDIS_EXPIRATION` / `SCCACHE_REDIS_TTL` ttl for redis cache, don't set for default behavior.
* `SCCACHE_REDIS_KEY_PREFIX` key prefix (optional).
* `SCCACHE_REDIS_RW_MODE` allows to use redis backend in read-only mode if set to `READ_ONLY`

The full url appears then as `redis://user:passwd@1.2.3.4:6379/?db=1`.

#### memcached

* `SCCACHE_MEMCACHED` is a deprecated alias for `SCCACHE_MEMCACHED_ENDPOINT`.
* `SCCACHE_MEMCACHED_ENDPOINT` memcached url.
* `SCCACHE_MEMCACHED_USERNAME` memcached username (optional).
* `SCCACHE_MEMCACHED_PASSWORD` memcached password (optional).
* `SCCACHE_MEMCACHED_EXPIRATION` ttl for memcached cache, don't set for default behavior.
* `SCCACHE_MEMCACHED_KEY_PREFIX` key prefix (optional).
* `SCCACHE_MEMCACHED_RW_MODE` allows to use memcached backend in read-only mode if set to `READ_ONLY`

#### gcs

* `SCCACHE_GCS_BUCKET`
* `SCCACHE_GCS_CREDENTIALS_URL`
* `SCCACHE_GCS_KEY_PATH`
* `SCCACHE_GCS_RW_MODE`

#### azure

* `SCCACHE_AZURE_CONNECTION_STRING`
* `SCCACHE_AZURE_BLOB_CONTAINER`
* `SCCACHE_AZURE_KEY_PREFIX`
* `SCCACHE_AZURE_RW_MODE`

#### gha

* `SCCACHE_GHA_CACHE_URL` / `ACTIONS_RESULTS_URL` GitHub Actions cache API URL
* `SCCACHE_GHA_RUNTIME_TOKEN` / `ACTIONS_RUNTIME_TOKEN` GitHub Actions access token
* `SCCACHE_GHA_CACHE_TO` cache key to write
* `SCCACHE_GHA_CACHE_FROM` comma separated list of cache keys to read from
* `SCCACHE_GHA_RW_MODE` allows to use GHA cache backend in read-only mode if set to `READ_ONLY`

#### webdav

* `SCCACHE_WEBDAV_ENDPOINT` a webdav service endpoint to store cache, such as `http://127.0.0.1:8080/my/webdav.php`.
* `SCCACHE_WEBDAV_KEY_PREFIX` specify the key prefix (subfolder) of cache (optional).
* `SCCACHE_WEBDAV_USERNAME` a username to authenticate with webdav service (optional).
* `SCCACHE_WEBDAV_PASSWORD` a password to authenticate with webdav service (optional).
* `SCCACHE_WEBDAV_TOKEN` a token to authenticate with webdav service (optional) - may be used instead of login & password.
* `SCCACHE_WEBDAV_RW_MODE` allows to use webdav backend in read-only mode if set to `READ_ONLY`

#### OSS

* `SCCACHE_OSS_BUCKET`
* `SCCACHE_OSS_ENDPOINT`
* `SCCACHE_OSS_KEY_PREFIX`
* `ALIBABA_CLOUD_ACCESS_KEY_ID`
* `ALIBABA_CLOUD_ACCESS_KEY_SECRET`
* `SCCACHE_OSS_NO_CREDENTIALS`
* `SCCACHE_OSS_RW_MODE`

#### Tencent Cloud Object Storage (COS)

* `SCCACHE_COS_BUCKET`
* `SCCACHE_COS_ENDPOINT`
* `SCCACHE_COS_KEY_PREFIX`
* `TENCENTCLOUD_SECRET_ID`
* `TENCENTCLOUD_SECRET_KEY`
* `SCCACHE_COS_RW_MODE`

## Checking the environment

`sccache --check-env` reports the environment sccache will run in and exits. It
runs nothing on the compile path, changes nothing, and never adapts sccache's
behaviour to what it finds — behaviour that varies with the detected environment
produces a cache hit on one machine and a miss on another.

It reports where the cache is, whether it shares a volume with the working
directory, the filesystem and whether it supports block cloning, Dev Drive trust
state, the storage encoding in effect, preprocessor cache mode and its settings,
and Defender's real-time protection and process exclusions.

Findings are graded: an **error** means results would be meaningless, a
**warning** means it works but leaves performance unclaimed, and **info** is worth
knowing. The exit status is non-zero only for errors, so a CI step can gate on it
without failing over advice.

Anything it cannot determine is reported as unknown rather than guessed — Dev
Drive state needs elevation, and reporting an unelevated failure as "not a Dev
Drive" would silently disable the check.

`--stats-format=json` emits the whole report for tooling.

## Per-invocation cost on Windows

Building with `--no-default-features` drops the cache backends the local-disk case
never uses (OpenDAL's nine services, reqwest, reqsign, hyper), taking the binary
from about 19 MiB to 7 MiB.

Measured effect, 150 serial cache-hit invocations against a 1 MiB `/Z7` object:

```
                              full     slim
process-creation floor        8.96     9.02 ms   (cmd /c exit 0)
sccache startup above floor   1.43     0.70 ms
config + connect + IPC        5.07     5.06 ms
compile-specific              9.42     9.78 ms
```

The slim build halves sccache's own startup, but that is 0.7 ms of a ~24 ms
invocation. The dominant term is the **~9 ms this machine charges to create any
process at all** — `cmd /c exit 0` costs the same — and nothing inside sccache can
go below it. Only not spawning a process per translation unit can, which means
build-system integration rather than a change here.

So build slim for the smaller deployment and the DLL imports it removes, but do
not expect it to change build times.
