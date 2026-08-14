# Available Configuration Options

> Every `SCCION_*` variable on this page is also read under its `SCCACHE_*`
> name, and means the same thing. `SCCION_` wins where both are set, and a
> disagreement between the two is logged rather than settled quietly. The
> configuration file's own location is unchanged from sccache. The project
> file, `.sccion.toml`, is this fork's own and is read under that name only.

## file

```toml
# If specified, wait this long for the server to start up.
server_startup_timeout_ms = 10000

# Whether the out-of-band MCP query surface may be used on this machine
# (see docs/MCP.md). Enabled by default; set it to false on a build
# server, where there is no agent to serve. SCCION_MCP overrides it.
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
cache_dir = "/home/user/.cache/sccion-dist-client"

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
# Overridden by SCCION_CACHE_COMPRESSION.
#
# NOTE: top-level settings must appear before any [table] header. In TOML a key
# written below one belongs to that table, and sccion will reject it.
compression = "zstd"
# The zstd level, when compressing. Overridden by SCCION_CACHE_ZSTD_LEVEL.
zstd_level = 3

[cache.disk]
dir = "/tmp/.cache/sccion"
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
cache_to = "sccion-latest"
cache_from = "sccion-"

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

sccion looks for its configuration file at the path indicated by env variable `SCCION_CONF`.

If no such env variable is set, sccion looks at default locations as below:
- Linux and macOS: `~/.config/sccion/config`
- Windows: `%APPDATA%\sccion\config`

If none of those exists, the locations sccache used are read as well —
`~/.config/sccache/config`, `~/Library/Application Support/Mozilla.sccache/config`
(and the `~/Library/Preferences/Mozilla.sccache/config` that predated it), or
`%APPDATA%\Mozilla\sccache\config\config` — so a configuration file written
years ago is still found.

The latest `cache.XXX` entries may be found here: https://github.com/mozilla/sccache/blob/ffe3070f77ef3301c8ff718316e4ab017ec83042/src/config.rs#L300.

## env

Whatever is set by a file based configuration, it is overruled by the env
configuration variables

Note that some env variables may need sccion server restart to take effect.

### misc

* `SCCION_ALLOW_CORE_DUMPS` to enable core dumps by the server
* `SCCION_CONF` configuration file path
* `SCCION_BASEDIRS` base directory (or directories) to strip from paths for cache key computation. This is similar to ccache's `CCACHE_BASEDIR` and enables cache hits across different absolute paths when compiling the same source code. Multiple directories can be separated by `;` on Windows hosts and by `:` on any other operating system. When multiple directories are specified, the longest matching prefix is used. Path matching is **case-insensitive** on Windows and **case-sensitive** on other operating systems. Environment variable takes precedence over file configuration. Only absolute paths are supported; relative paths will cause an error and prevent the server from start.
* `SCCION_CACHED_CONF`
* `SCCION_IDLE_TIMEOUT` how long the local daemon process waits for more client requests before exiting, in seconds. Set to `0` to run sccion permanently
* `SCCION_STARTUP_NOTIFY` specify a path to a socket which will be used for server completion notification
* `SCCION_MAX_FRAME_LENGTH` how much data can be transferred between client and server
* `SCCION_NO_DAEMON` set to `1` to disable putting the server to the background
* `SCCION_CLIENT_SIDE` set to `1` to run the compile in the client process and use the daemon only as a gateway to the cache storage (see [the architecture doc](Architecture.md#client-side-mode-sccion_client_side)). This is the recommended mode and is expected to become the only supported configuration in the future. Ignored when `SCCION_ERROR_LOG` or distributed compilation is in use.
* `SCCION_CACHE_MULTIARCH` to disable caching of multi architecture builds.
* `SCCION_CACHE_ZSTD_LEVEL` to set zstd compression level of cache. the range is `1-22` and default is `3`.
* `SCCION_CACHE_COMPRESSION` to disable compression entirely: set it to `none`
  (or `off`/`false`/`0`) to store cache entries verbatim. Anything else, or unset,
  keeps zstd at `SCCION_CACHE_ZSTD_LEVEL`.

  Per object this trades cache capacity for a cheaper cache hit. Measured on a
  1.4 MiB MSVC `/Z7` object: the entry grows from 263 KiB to 1,458 KiB (5.5x),
  while the server-side work to materialize a hit falls from 5.1 ms to 3.0 ms and
  end-to-end hit latency from 27.0 ms to 24.5 ms.

  **Across a whole build that trade does not survive, and it inverts.** On a
  2557-unit MSVC + Qt + CMake project, each arm populated from cold into its own
  cache and then rebuilt warm at 2557/2557 hits:

  | | cold | warm | cache |
  |---|---|---|---|
  | `none` | 472.3 s | **121.0 s** | **7,358 MB** |
  | zstd 3 | 471.8 s | **108.1 s** | **1,996 MB** |

  Compression is 10.7% *faster* warm and 3.69x smaller, and costs nothing on the
  cold side. A warm build reads the whole cache, and 7.4 GB of reads costs more
  than 2.0 GB plus the decompression — so the per-object saving is real and the
  build-level effect is the opposite sign.

  Under `none` on a volume that supports it, cache hits materialize by
  filesystem block cloning — the object's extents are shared out of the entry
  instead of written (`--show-stats` reports it as "Objects block-cloned"; a
  compressed blob shares no extents with what it expands to, which is why this
  exists only under `none`). Measured on the same build, cloning recovers most
  of the copying penalty but not all of it:

  | | warm | cache |
  |---|---|---|
  | `none`, copied | 121.0 s | 7,358 MB |
  | `none`, cloned | 112.9 s | 7,372 MB |
  | zstd 3 | **108.1 s** | **1,996 MB** |

  So prefer the default. `none` is for measurements that need byte-stable
  on-disk entries, and it now costs less than it used to.

  Both settings are read by the **server**, once, at startup: change either and
  restart it with `sccion --stop-server`. Entries record their own encoding, so
  a cache may hold a mix, and one written by an older sccion still reads
  correctly — changing this does not invalidate anything.
  - For example, in `10`, it have about 0.9x size with about 1.6x time than default `3` (tested with compiling sccion code)
  - This option will only applied to newly compressed cache and don't affect existing cache.
  - If you want to be apply to all cache, you should reset cache and make new cache.
* `SCCION_LOG_MILLIS` when set (to any value), enables millisecond precision timestamps in log output instead of the default second precision.
* `SCCION_MCP` set to `false` (or `off`/`0`) disables the [MCP query
  surface](MCP.md) on this machine. Enabled by default, so a developer machine
  needs no configuration.

  **Turn it off on a build server.** The surface exists so that an agent can ask
  why a build missed; where there is no agent it is only a way to read build
  state with nobody to serve. The environment variable outranks the `mcp` key in
  the config file, so a CI job can disable it without editing machine
  configuration.

  Read by the **server** at startup, like the cache settings: change it and
  restart with `sccion --stop-server`. `sccion-mcp` also refuses to start, so
  the failure is one clear line rather than an error per tool call.
* `SCCION_AGENT_ID` names the caller, so `--show-stats` can break its numbers
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
* `SCCION_VERIFY_HITS` set to a fraction (`0.02` for one hit in fifty) recompiles
  that share of cache *hits* and compares the result with what the cache served.
  Off by default.

  This is the only check for a **wrong hit**. Every other diagnostic looks at
  misses, because a miss costs time; a wrong hit costs correctness, and a short
  dependency list produces one rather than a miss. Sampling rather than checking
  everything is the point — verifying every hit means compiling everything,
  which is what the cache exists to avoid.

  **The build always keeps what the cache gave it**, whatever the check finds.
  A diagnostic that substituted a freshly built object would make a build depend
  on whether sampling happened to select it.

  A mismatch is not reported until sccion has established that the compiler
  reproduces *its own* output, by compiling a second time. On Windows a COFF
  object carries a timestamp unless the build asks for reproducibility, so both
  MSVC and clang fail that test by default and every sampled hit would otherwise
  look wrong. When they fail it sccion says so and accuses nothing; build with
  `/Brepro` (MSVC) or `-Brepro` (clang-cl) to make the check meaningful.

  Mismatches are recorded with the classification `wrong_hit` and reported over
  [MCP](MCP.md) like any other finding.
* `SCCION_TRACE_COMPONENTS` set to `1` records what each cache key was computed
  *from* — compiler, arguments, extra files, environment, working directory and
  the inputs — each as its own digest. A key is one opaque string, so two keys
  either match or do not; these say *which part* differs, which is the question
  when two checkouts of one project stop sharing entries.

  Off by default because it walks everything that goes into a key a second time,
  including the preprocessed text. Read per compilation, so one build can be
  traced without slowing others sharing the machine. Query it with the
  [`explain_key` and `compare_keys` tools](MCP.md).

  A key taken from a preprocessor cache entry was never computed from parts, so
  it has no components however much tracing is on; `SCCION_DIRECT=false` forces
  one to be computed.
* `SCCION_TRACE_ALL` set to `1` records cache *hits* as well as misses. Off by
  default: a warm build hits thousands of times and would push everything
  explanatory out of the daemon's bounded ring.

  On, the records are a full `(input, key, outcome)` trace of a build, which is
  the whole of a key-stability check — capture one, change sccion, capture
  another, and diff the keys. Any key that moved is either the point of the
  change or a bug in it.
* `SCCION_MISS_LOG` path to a file every cache miss is appended to, one
  tab-separated record per line. `--show-stats` counts misses but cannot say
  which translation units or why, and `SCCION_LOG=debug` can say but only
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
  | 1 | classification | `cold`, `read_error`, `timed_out`, `forced_recache`, `forced_no_cache`, `uncacheable:<reason>`, `hit` under `SCCION_TRACE_ALL`, or `wrong_hit` |
  | 2 | preprocessor cache mode | `direct-hit`, `direct-miss`, `direct-off` |
  | 3 | cache key | `-` when the compilation was refused before one could be computed |
  | 4 | output | usually a basename; the compiler's own name when the arguments were never parsed |
  | 5 | input | basedir-relative; `-` when the compilation has no single input |
  | 6 | language | as `--show-stats` names it |
  | 7 | workspace | the checkout root, basedir-relative |
  | 8 | agent | see `SCCION_AGENT_ID` |
  | 9 | command digest | of the basedir-stripped arguments |
  | 10 | sequence | the order the daemon saw them in |

  `uncacheable:` is the one that says *stop*: `time_macros`,
  `dependency_report_unreadable`, `shared_pdb` and the rest are permanent
  properties of the compilation as invoked, so retrying it unchanged reaches the
  same verdict. Every other classification may well succeed next time.
* `SCCION_ERROR_LOG` path to a file where sccion will log errors
* `SCCION_LOG` log level, accepting standard env_logger values, see [env_logger documentation](https://docs.rs/env_logger/latest/env_logger/#enabling-logging) for details

### cache configs

#### multi-level cache

Multi-level caching enables hierarchical cache storage with automatic backfill. See the [Multi-Level Cache documentation](MultiLevel.md) for detailed information.

* `SCCION_MULTILEVEL_CHAIN` comma-separated list of cache backend names to use in hierarchy (e.g., `disk,redis,s3`)
  - Order matters: left-to-right is fast-to-slow (L0, L1, L2, ...)
  - Valid names: `disk`, `redis`, `memcached`, `s3`, `gcs`, `azure`, `gha`, `webdav`, `oss`, `cos`
  - Each level must be separately configured with its own environment variables
  - If not set, sccion uses single-level mode (legacy behavior)
* `SCCION_MULTILEVEL_WRITE_ERROR_POLICY` controls error handling on cache writes (default: `l0`)
  - `ignore` - never fail on write errors, log warnings only (most permissive)
  - `l0` - fail only if L0 (first level) write fails (default, balances reliability and performance)
  - `all` - fail if any read-write level fails (most strict)
  - Read-only levels are always skipped and never cause failures

**Basic example**:
```bash
export SCCION_MULTILEVEL_CHAIN="disk,redis,s3"
export SCCION_DIR="/tmp/cache"              # for disk level
export SCCION_REDIS_ENDPOINT="redis://..."  # for redis level
export SCCION_BUCKET="my-bucket"            # for s3 level
```

**Write policy examples**:
```bash
# Default: Fail only if disk write fails
export SCCION_MULTILEVEL_WRITE_ERROR_POLICY="l0"

# Best effort: Never fail on cache writes
export SCCION_MULTILEVEL_WRITE_ERROR_POLICY="ignore"

# Strict: Fail if any level write fails
export SCCION_MULTILEVEL_WRITE_ERROR_POLICY="all"
```

#### disk (local)

* `SCCION_DIR` local on disk artifact cache directory
* `SCCION_CACHE_SIZE` maximum size of the local on disk cache i.e. `2G` - default is 10G
* `SCCION_DIRECT` enable/disable preprocessor caching (see [the local doc](Local.md))
* `SCCION_LOCAL_RW_MODE` the mode that the cache will operate in (`READ_ONLY` or `READ_WRITE`)

#### s3 compatible

* `SCCION_BUCKET` s3 bucket to be used
* `SCCION_ENDPOINT` s3 endpoint
* `SCCION_REGION` s3 region, required if using AWS S3
* `SCCION_S3_USE_SSL` s3 endpoint requires TLS, set this to `true`
* `SCCION_S3_KEY_PREFIX` s3 key prefix (optional)
* `SCCION_S3_RW_MODE` allows to use s3 backend in read-only mode if set to `READ_ONLY`

The endpoint used then becomes `${SCCION_BUCKET}.s3-{SCCION_REGION}.amazonaws.com`.
If you are not using the default endpoint and `SCCION_REGION` is undefined, it
will default to `us-east-1`.

#### cloudflare r2

* `SCCION_BUCKET` is the name of your R2 bucket.
* `SCCION_ENDPOINT` must follow the format of `https://<ACCOUNT_ID>.r2.cloudflarestorage.com`. Note that the `https://` must be included. Your account ID can be found [here](https://developers.cloudflare.com/fundamentals/get-started/basic-tasks/find-account-and-zone-ids/).
* `SCCION_REGION` should be set to `auto`.
* `SCCION_S3_KEY_PREFIX` s3 key prefix (optional).

#### redis

* `SCCION_REDIS` full redis url, including auth and access token/passwd (deprecated).
* `SCCION_REDIS_ENDPOINT` redis url without auth and access token/passwd - single node configuration.
* `SCCION_REDIS_CLUSTER_ENDPOINTS` redis cluster urls, separated by comma - shared cluster configuration.
* `SCCION_REDIS_USERNAME` redis username (optional).
* `SCCION_REDIS_PASSWORD` redis password (optional).
* `SCCION_REDIS_DB` redis database (optional, default is 0).
* `SCCION_REDIS_EXPIRATION` / `SCCION_REDIS_TTL` ttl for redis cache, don't set for default behavior.
* `SCCION_REDIS_KEY_PREFIX` key prefix (optional).
* `SCCION_REDIS_RW_MODE` allows to use redis backend in read-only mode if set to `READ_ONLY`

The full url appears then as `redis://user:passwd@1.2.3.4:6379/?db=1`.

#### memcached

* `SCCION_MEMCACHED` is a deprecated alias for `SCCION_MEMCACHED_ENDPOINT`.
* `SCCION_MEMCACHED_ENDPOINT` memcached url.
* `SCCION_MEMCACHED_USERNAME` memcached username (optional).
* `SCCION_MEMCACHED_PASSWORD` memcached password (optional).
* `SCCION_MEMCACHED_EXPIRATION` ttl for memcached cache, don't set for default behavior.
* `SCCION_MEMCACHED_KEY_PREFIX` key prefix (optional).
* `SCCION_MEMCACHED_RW_MODE` allows to use memcached backend in read-only mode if set to `READ_ONLY`

#### gcs

* `SCCION_GCS_BUCKET`
* `SCCION_GCS_CREDENTIALS_URL`
* `SCCION_GCS_KEY_PATH`
* `SCCION_GCS_RW_MODE`

#### azure

* `SCCION_AZURE_CONNECTION_STRING`
* `SCCION_AZURE_BLOB_CONTAINER`
* `SCCION_AZURE_KEY_PREFIX`
* `SCCION_AZURE_RW_MODE`

#### gha

* `SCCION_GHA_CACHE_URL` / `ACTIONS_RESULTS_URL` GitHub Actions cache API URL
* `SCCION_GHA_RUNTIME_TOKEN` / `ACTIONS_RUNTIME_TOKEN` GitHub Actions access token
* `SCCION_GHA_CACHE_TO` cache key to write
* `SCCION_GHA_CACHE_FROM` comma separated list of cache keys to read from
* `SCCION_GHA_RW_MODE` allows to use GHA cache backend in read-only mode if set to `READ_ONLY`

#### webdav

* `SCCION_WEBDAV_ENDPOINT` a webdav service endpoint to store cache, such as `http://127.0.0.1:8080/my/webdav.php`.
* `SCCION_WEBDAV_KEY_PREFIX` specify the key prefix (subfolder) of cache (optional).
* `SCCION_WEBDAV_USERNAME` a username to authenticate with webdav service (optional).
* `SCCION_WEBDAV_PASSWORD` a password to authenticate with webdav service (optional).
* `SCCION_WEBDAV_TOKEN` a token to authenticate with webdav service (optional) - may be used instead of login & password.
* `SCCION_WEBDAV_RW_MODE` allows to use webdav backend in read-only mode if set to `READ_ONLY`

#### OSS

* `SCCION_OSS_BUCKET`
* `SCCION_OSS_ENDPOINT`
* `SCCION_OSS_KEY_PREFIX`
* `ALIBABA_CLOUD_ACCESS_KEY_ID`
* `ALIBABA_CLOUD_ACCESS_KEY_SECRET`
* `SCCION_OSS_NO_CREDENTIALS`
* `SCCION_OSS_RW_MODE`

#### Tencent Cloud Object Storage (COS)

* `SCCION_COS_BUCKET`
* `SCCION_COS_ENDPOINT`
* `SCCION_COS_KEY_PREFIX`
* `TENCENTCLOUD_SECRET_ID`
* `TENCENTCLOUD_SECRET_KEY`
* `SCCION_COS_RW_MODE`

## Checking the environment

`sccion --check-env` reports the environment sccion will run in and exits. It
runs nothing on the compile path, changes nothing, and never adapts sccion's
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
sccion startup above floor   1.43     0.70 ms
config + connect + IPC        5.07     5.06 ms
compile-specific              9.42     9.78 ms
```

The slim build halves sccion's own startup, but that is 0.7 ms of a ~24 ms
invocation. The dominant term is the **~9 ms this machine charges to create any
process at all** — `cmd /c exit 0` costs the same — and nothing inside sccion can
go below it. Only not spawning a process per translation unit can, which means
build-system integration rather than a change here.

So build slim for the smaller deployment and the DLL imports it removes, but do
not expect it to change build times.
