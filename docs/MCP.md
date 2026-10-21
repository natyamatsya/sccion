# `sccion-mcp` — asking sccion questions from an agent

`sccion-mcp` is a Model Context Protocol server that answers questions about a
local sccion daemon: why a compilation missed, how the cache is doing, whether
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

**It is stdio only.** Everything sccion does is same-machine. The remote
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
# in the sccion config file
mcp = false
```
```
SCCION_MCP=false     # outranks the config file, so a CI job needs no edit
```

Read by the server at startup like the cache settings, so restart it with
`sccion --stop-server`. `sccion-mcp` refuses to start too, which turns "every
tool call fails" into one line an operator can act on.

**"May this checkout be answered for?"** — the repository's decision, in its
committed `.sccion.toml`:

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
sccion-mcp
```

It ships in each release archive beside `sccion`; from a checkout,
`cargo install --locked --path crates/sccion-mcp`.

Reads line-delimited JSON-RPC on stdin, writes responses on stdout, logs to
stderr under `SCCION_LOG`. It needs a running daemon and will not start one:
starting a daemon in order to ask it for statistics reports the statistics of
the daemon just started, which is a confident zero rather than an answer.

The adapter keeps nothing of its own: every answer is fetched from the daemon
when it is asked for, and the daemon is the state. A client on a revision that
still has a session does get one, because that is what its protocol says — the
SDK holds it, no answer depends on it, and a restarted process starts clean.

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
| `explain_key` | What this compilation's cache key was computed from |
| `compare_keys` | Which part of the key differs between two workspaces |
| `uncacheable_summary` | Everything here that can never be cached, by reason |

Resources: `sccion://environment` and `sccion://stats/{handle}`.

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
anything a caller supplies. Nothing else scopes it — not even a protocol session,
where the client's revision has one, since the daemon that mints handles knows
nothing about connections: it lives as long as the daemon does.

### `explain_miss` and the classification

The `classification` on each record is the payload:

- `uncacheable:<reason>` — **stop**. `time_macros`,
  `dependency_report_unreadable`, `shared_pdb` and the rest are permanent
  properties of the compilation as invoked. Retrying it unchanged reaches the
  same verdict. The reason is usually actionable: a source that expands
  `__DATE__`, a flag sccion cannot model.
- anything else (`cold`, `read_error`, `timed_out`, …) — may well succeed next
  time.

Records also carry the *input* path, not just the output basename. Three
concurrent `mocs_compilation.cpp.obj` compilations cannot be told apart by their
output; they can by their input.

### Maintaining the cache itself

The tools above answer "why did *my build* miss". These answer "why is the
*cache* behaving like that", which is a different job and mostly a different
person — someone changing sccion rather than someone using it.

`compare_keys` is the one to reach for first. Two checkouts of a project that
should share entries and do not is the commonest correctness question there is,
and a key is one opaque string, so comparing two of them says only that they
differ. This says *which part* differs:

```
compare_keys(handle_a, handle_b, "src/foo.c")
  -> sameKey: false, differingComponents: ["inputs"]
```

Both builds must have run with `SCCION_TRACE_COMPONENTS=1`. The answer is a
list rather than a single name because an empty list with two different keys is
itself informative: the difference is in something not broken out as a
component.

Records are history, not state, so the reply carries each side's sequence number
and classification. Only misses are recorded by default, so a workspace whose
last build *hit* offers its previous record and the comparison quietly spans two
moments; `SCCION_TRACE_ALL=1` records hits too and removes the ambiguity.

`uncacheable_summary` groups the permanent failures by reason with counts and
examples. "141 permanent misses" is a fact about a build, and reading it out of
141 separate records is work a caller should not have to do.

## Protocol revision

The protocol is **`rmcp`'s**, the official Rust SDK. This crate implements none
of it, which is a deliberate reversal: it used to carry a hand-written envelope
built to 2026-07-28 alone, and that is precisely how it came to be unreachable
from every editor in the field, all of which open with the `initialize` that
revision removed. A second implementation of a specification that revises
quarterly is a maintenance cost that buys a way to be wrong on your own.

What the SDK brings:

- **Both lifecycles, negotiated per connection.** A client that opens with
  `initialize` gets a session; one that speaks 2026-07-28 sends no handshake at
  all, opening with `server/discover` or any request carrying the required
  `_meta`. Neither is configured — the client chooses, and one binary answers.
- **`server/discover`** reports every revision the SDK implements
  (`2024-11-05` through `2026-07-28`), so discovery describes the server that is
  actually answering.
- **The envelope**: `resultType` on every result, `ttlMs` and `cacheScope` on
  the discovery and list results, and the `_meta` validation the newer lifecycle
  requires. The next revision is a version bump here rather than a diff to an
  envelope of our own.

What this repository still decides, in `server.rs`, because none of it is the
protocol's business:

- **Every tool reads.** `readOnlyHint` and `destructiveHint: false` on all
  eight, asserted by a test. No tool can evict an entry, change configuration or
  start a compilation.
- **A handle is a capability**, so a tool that cannot resolve one refuses rather
  than falling back to the machine or to the adapter's own directory.
- **`cacheScope: private`** on anything keyed by a handle — the statistics
  resource — because a result that describes one caller's build must not be
  served into another's context. `public` is for the environment report alone,
  which is the same answer for everybody.
- **A stable tool list**, sorted by name, so a client that caches it is not
  re-fetching over a reordering.
- **Two invented error codes**, `-32001` (unknown handle) and `-32002` (no
  daemon), both inside the implementation-defined `-32000`..=`-32019` range;
  `-32020`..=`-32099` is reserved for the specification and nothing here may use
  it. Every other failure reuses a code the SDK already assigns.

**One thing to know about tool results.** `CallToolResult` has no
`ttlMs`/`cacheScope` in the 2026-07-28 schema — they belong to the discovery and
list results — so a tool answer carries no cache marking. The previous adapter
put one there anyway, which was a field the specification does not define. The
`private` intent survives in what the tools refuse rather than on the wire; the
statistics *resource*, which does carry the field, is the one to read when a
caller wants it stated.

## Not implemented, deliberately

- **`estimate_build`** — projecting what a change will cost. Not worth the
  machinery until the miss records are being read by something.
- **`request_promotion`** — promoting entries into a shared cache tier. This is
  the one mutating tool the design contemplates, and it must not exist before
  the trust boundary that makes shared writes safe. Its validation token is
  minted outside MCP entirely, by whatever attests that a build was good.
