# Sccion high level architecture

This schema shows at a high level how sccion decides whether to compile or to
reuse a cached result.


```mermaid
  flowchart LR
      id1[[Environment variables]] --> hash
      id2[[Compiler binary]] --> hash
      id3[[Compiler arguments]] --> hash
      id5[[Files]] --> |  | hash
      Compile --> Upload
      Storage[(Storage)] --> | yes | Download
      hash([hash]) --> | exists? | Storage
      Storage --> | no | Compile
      Upload --> Storage

      classDef input fill:#4fc3f7,stroke:#01579b,color:#000
      classDef store fill:#ffb74d,stroke:#e65100,color:#000
      classDef action fill:#81c784,stroke:#1b5e20,color:#000
      class id1,id2,id3,id5 input
      class Storage store
      class hash,Compile,Upload,Download action
```


For more details about how hash generation works, see [the caching documentation](Caching.md).


## Direct mode (preprocessor cache)

For C/C++, sccion can also cache the *preprocessor* result, so that a cache
lookup can skip preprocessing entirely. This is inspired by
[ccache's direct mode](https://ccache.dev/manual/3.7.9.html#_the_direct_mode)
and is described in the [local storage doc](Local.md).

Before computing the object-cache key, sccion looks up a **preprocessor cache
entry** keyed on the source file (path + contents) and the preprocessor
arguments. That entry records every file included by the source. If the entry
exists and all of those included files are unchanged, sccion reuses it and
never runs the preprocessor; otherwise it preprocesses normally and stores a
fresh entry. This step is shown as the blue region in the diagrams below and is
enabled by default (it is skipped for non-C/C++, when disabled, or when certain
flags such as `-Wp,*` / `-Xpreprocessor` are present).


## Execution modes

The caching logic above is the same regardless of *where* it runs. What differs
is which process actually runs the compiler and talks to the storage backend.

sccion is split in two:

- a short-lived **CLI process**, spawned once per compiler invocation (e.g. by
  `make -jN`), and
- a long-lived **daemon** (the background server), which holds the storage
  backend and the accumulated statistics.

The two communicate over a local IPC connection. There are two ways to divide
the work between them.

### Server-side mode (default)

The CLI forwards the whole compilation to the daemon. The daemon runs the
direct-mode preprocessor cache lookup, computes the hash, looks up the object
cache, runs the compiler on a miss, and stores the result. It writes the output
files (e.g. the object file) to disk itself; only the output streams (stdout /
stderr) and the exit code travel back to the CLI.

```mermaid
  sequenceDiagram
      participant B as Build (make -jN)
      participant C as sccion CLI
      participant D as sccion daemon
      participant S as Storage backend
      B->>C: sccion cc -c foo.c
      C->>D: Compile request (IPC)
      rect rgba(33, 150, 243, 0.18)
          note over D,S: Direct mode — preprocessor cache (C/C++)
          D->>S: lookup preprocessor entry (source + args)
          alt entry found & included files unchanged
              S-->>D: manifest → reuse, skip preprocessing
          else miss / stale / disabled
              D->>D: run preprocessor
              D->>S: store preprocessor entry
          end
      end
      rect rgba(76, 175, 80, 0.18)
          note over D,S: Object cache
          D->>S: lookup object (hash)
          alt cache hit
              S-->>D: cached object
          else cache miss
              D->>D: run compiler
              D->>S: store object
          end
      end
      D-->>C: CompileFinished (stdout / stderr / exit code)
      C-->>B: exit code + outputs
```

### Client-side mode (`SCCION_CLIENT_SIDE`)

The compile pipeline runs **in the CLI process itself**; the daemon is used only
as a shared gateway to the storage backend (and as a place to aggregate stats).

On startup the CLI performs a one-shot `StorageHandshake` to fetch the cache
metadata (cache mode, max size, basedirs, preprocessor-cache config). It then
runs the same pipeline locally — including direct mode — and forwards each
individual cache operation to the daemon over IPC. On the client side this is
implemented by `IpcStorage`, which implements the same `Storage` trait as every
other backend, so the rest of the compile pipeline is unchanged.

```mermaid
  sequenceDiagram
      participant B as Build (make -jN)
      participant C as sccion CLI (IpcStorage)
      participant D as sccion daemon
      participant S as Storage backend
      B->>C: sccion cc -c foo.c
      C->>D: StorageHandshake (IPC)
      D-->>C: cache metadata (mode, max_size, basedirs, …)
      rect rgba(33, 150, 243, 0.18)
          note over C,S: Direct mode — preprocessor cache (C/C++)
          C->>D: StorageGetPreprocessorEntry (key)
          D->>S: lookup
          alt entry found & included files unchanged
              D-->>C: manifest → reuse, skip preprocessing
          else miss / stale / disabled
              D-->>C: miss
              C->>C: run preprocessor
              C->>D: StoragePutPreprocessorEntry (key, bytes)
              D->>S: store
          end
      end
      rect rgba(76, 175, 80, 0.18)
          note over C,S: Object cache
          C->>C: compute hash
          C->>D: StorageGetPath / StorageGetRaw (key)
          D->>S: lookup
          alt cache hit
              S-->>D: path / bytes
              D-->>C: path / bytes
          else cache miss
              D-->>C: miss
              C->>C: run compiler
              C->>D: StoragePutRaw (key, bytes)
              D->>S: store
          end
      end
      C->>D: RecordStats (delta)
      C-->>B: exit code + outputs
```

`StorageGetPath` lets the CLI read a cached entry straight off disk when the
backend exposes a local path; for backends that don't (S3, Redis, …) the CLI
falls back to fetching the raw bytes with `StorageGetRaw`. Direct-mode entries
are exchanged with `StorageGetPreprocessorEntry` / `StoragePutPreprocessorEntry`.
Because each CLI process accumulates its own statistics, it flushes them to the
daemon with `RecordStats` before exiting.

Client-side mode is enabled with the `SCCION_CLIENT_SIDE` environment variable
(or the `client_side_mode` config key). It is expected to become the only
supported configuration in the future, with server-side mode eventually
removed. It is currently mutually exclusive with:

- **error logging** (`SCCION_ERROR_LOG`): the client always logs to stderr, and
  multiple concurrent CLI processes would race on the log file, and
- **distributed compilation** (a configured scheduler URL).

If either is in use, the setting is ignored and sccion stays in server-side
mode.

### Client-side mode and MSVC

Client-side mode is also ignored for MSVC-family drivers (`cl.exe`,
`clang-cl.exe`), where it is a pessimization rather than a win.

The daemon caches compiler information — for MSVC, the `/showIncludes` prefix
and the compiler version — in a map that lives for the life of the process. In
client-side mode that map lives in the CLI process instead, and each CLI process
handles exactly one translation unit, so the map starts empty every time.
Repopulating it runs the compiler twice: once for `detect_c_compiler`'s probe and
once for `detect_showincludes_prefix`. A client-side cache **hit** therefore costs
three processes, against one for the server-side path.

This is decided from the executable name, because determining it properly would
mean running the compiler — the very cost being avoided. A misjudgement is
harmless in both directions: a false positive just uses the faster server-side
path, and a false negative leaves behaviour unchanged.

The restriction is expected to be lifted once the daemon can answer compiler
information over the wire, letting the client reconstitute the descriptor without
spawning anything. Because the fallback happens once per translation unit, it is
reported at `debug` level rather than on stderr; `sccion --check-env` surfaces it
as a finding.
