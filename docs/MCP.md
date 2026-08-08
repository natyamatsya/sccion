# `sccache-mcp` — asking sccache questions from an agent

`sccache-mcp` is a Model Context Protocol server that answers questions about a
local sccache daemon: why a compilation missed, how the cache is doing, whether
the machine is set up to cache at all.

It exists because an agent's only way to learn about the build today is to parse
CLI output, which is brittle, unversioned, and gives no way to ask a follow-up
question. And because the answer an agent most needs is one the counters cannot
give: **is this worth retrying**. A cold miss will hit once the cache warms; a
compilation that can never be cached will not, however many times it is tried.

## What it is not

**Nothing here is on the compile path.** The compiler wrapper is unchanged — a
native binary on a local pipe — and no part of a compilation goes through
JSON-RPC. These are questions asked *between* builds.

**It is read-only.** No tool can evict an entry, change configuration, or start
a compilation. That is a deliberate boundary, not an accident of what has been
built so far: a tool surface exposed to an agent must not be able to poison or
clear a cache that other builds depend on.

**It is stdio only.** Everything sccache does is same-machine. The remote
transport would import an authorization surface that is itself mid-migration,
for no gain. Under stdio, a leaked workspace handle is confined to the process
tree that could already talk to the daemon.

## Turning it off

Enabled by default, so a developer machine needs no configuration. There are two
switches, and they answer different questions.

**"Does this machine offer the surface at all?"** — the operator's decision, and
the one to use on a build server, where there is no agent to serve and the
surface is only a way to read build state:

```toml
# in the sccache config file
mcp = false
```
```
SCCACHE_MCP=false     # outranks the config file, so a CI job needs no edit
```

Read by the server at startup like the cache settings, so restart it with
`sccache --stop-server`. `sccache-mcp` refuses to start too, which turns "every
tool call fails" into one line an operator can act on.

**"May this checkout be answered for?"** — the repository's decision, in its
committed `.sccache.toml`:

```toml
expose_over_mcp = false
```

**A project may decline and may not grant.** `true` is ignored rather than
honoured. Every other key in that file shapes cache keys, where the rule is that
the worst a hostile repository can do is make its own builds slower; this one
governs whether build state can be read, so it only passes that rule one way. A
repository that could switch the surface on would be a way to make a machine
answer questions about itself by being cloned onto it.

Both are enforced in one place — the daemon, when a workspace is registered.
Every other tool needs a handle, so refusing there closes all of them, and the
daemon rather than the stateless adapter is the authority on what the machine
allows.

## Running it

```
sccache-mcp
```

Reads line-delimited JSON-RPC on stdin, writes responses on stdout, logs to
stderr under `SCCACHE_LOG`. It needs a running daemon and will not start one:
starting a daemon in order to ask it for statistics reports the statistics of
the daemon just started, which is a confident zero rather than an answer.

The adapter holds no state and can be restarted between any two requests. The
sccache daemon is the state.

## Tools

Every tool but `register_workspace` and `check_environment` takes a
`workspace_handle`.

| Tool | Answers |
|---|---|
| `register_workspace` | Which checkout is this directory in, and what handle names it |
| `explain_miss` | Why recent compilations here were not served from cache |
| `get_stats` | Hits, misses, and the breakdown per caller |
| `get_cache_health` | Cache size against its limit |
| `check_environment` | Is this machine set up to cache at all |

Resources: `sccache://environment` and `sccache://stats/{handle}`.

### Handles

A handle names a checkout. `register_workspace` accepts any directory inside
one and resolves upward, so an agent can pass its working directory without
knowing where the checkout starts. Registering the same checkout twice returns
the same handle.

A directory outside any checkout is **refused**. Minting a handle for it would
produce one that no record could ever match, and every question asked with it
would come back empty — which reads as "nothing missed" rather than "that is not
a workspace".

A handle is a capability. Holding one grants read access to that workspace's
build state, so it is minted by the daemon, unguessable, and never derived from
anything a caller supplies. There are no sessions in this protocol revision, so
nothing else scopes it: it lives as long as the daemon does.

### `explain_miss` and the classification

The `classification` on each record is the payload:

- `uncacheable:<reason>` — **stop**. `time_macros`,
  `dependency_report_unreadable`, `shared_pdb` and the rest are permanent
  properties of the compilation as invoked. Retrying it unchanged reaches the
  same verdict. The reason is usually actionable: a source that expands
  `__DATE__`, a flag sccache cannot model.
- anything else (`cold`, `read_error`, `timed_out`, …) — may well succeed next
  time.

Records also carry the *input* path, not just the output basename. Three
concurrent `mocs_compilation.cpp.obj` compilations cannot be told apart by their
output; they can by their input.

## Protocol revision

Built to **2026-07-28**, which removed protocol-level sessions and the
`initialize` handshake. The consequences are visible in the interface:

- There is no connection setup. Every request carries its own
  `_meta.io.modelcontextprotocol/protocolVersion`; every result carries
  `serverInfo` and a `resultType`.
- `server/discover` is implemented and is the one endpoint that cannot be
  skipped. It is exempt from the version check, since it is how a client finds
  out what to speak.
- Cross-call state is a server-minted handle passed as an ordinary tool
  argument, which is what `register_workspace` is.
- `ttlMs` and `cacheScope` are on every list and read result. Anything keyed by
  a handle is `private` — marking it shareable would let one caller's build state
  be served into another's context.
- There is no logging, ping, or subscription surface. Those were removed or
  deprecated; the migration for logging is stderr, which is where this logs.
- Adapter error codes stay in `-32000`..=`-32019`; `-32020`..=`-32099` is
  reserved for the specification.

Every one of those is asserted by a test in `crates/sccache-mcp`, and the rules
live in one file (`envelope.rs`) so the next revision is a diff there rather
than an audit of the whole adapter.

**One thing is unverified.** The specification names an
`UnsupportedProtocolVersionError` but its spec-assigned numeric code was not
available when this was written. An implementation-defined code (`-32003`) is
used deliberately in the meantime: a wrong guess there can never collide with a
specification assignment, whereas an invented number in the reserved range
could. This is the item to re-check against the published specification.

## Not implemented, deliberately

- **`estimate_build`** — projecting what a change will cost. Not worth the
  machinery until the miss records are being read by something.
- **`request_promotion`** — promoting entries into a shared cache tier. This is
  the one mutating tool the design contemplates, and it must not exist before
  the trust boundary that makes shared writes safe. Its validation token is
  minted outside MCP entirely, by whatever attests that a build was good.
