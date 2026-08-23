# Measuring and checking

Most of what looks like an sccion performance problem is a configuration
problem, and most of what looks like a speedup is a measurement problem. This
page is about telling them apart before changing anything.

## Read the counters before the clock

`sccion --show-stats` first, wall clock second. A build that got slower after a
change is a different question from a build that stopped hitting the cache, and
only the counters distinguish them. **A changed hit count is a correctness
result, not a statistic**: it means a cache key moved, and no timing comparison
across a moved key is meaningful.

Two counts worth reading together: cache hits against compile requests, and
preprocessor cache hits against misses. The second can be zero while the first
looks healthy, which is the signature of
[preprocessor cache mode being structurally unable to hit](Windows.md#preprocessor-cache-mode).

Distrust a result that looks too good. A rebuild is only as complete as the
outputs deleted to provoke it — deleting objects reissues the object compiles
and nothing else, so a project with a code generator will report a perfect hit
rate for a third of its work never having been rebuilt at all.

## The benchmark harness

`scripts/bench` runs sccion against a CMake + Ninja MSVC build in three
scenarios — cold, fully cached, and one header touched — and writes wall clock
and sccion's own counters to JSON so runs can be diffed across commits. It can
drive a generated project or a real build tree, sweep compilers and compression
levels, and repeat runs to report median and spread rather than one sample.

```powershell
cargo build --release -p sccion-bench
.\target\release\sccion-bench.exe
```

See [`scripts/bench/README.md`](../scripts/bench/README.md) for the scenarios,
the flags, and how to A/B two sccion binaries — in particular that both must be
named `sccion.exe` in separate directories, because sccion decides what it is
from `argv[0]` and a renamed binary treats `--stop-server` as a compilation.

The harness excludes the link step by default. Linking is a large share of a
warm build's CPU and sccion schedules none of it, so including it dilutes every
measurement of the part that changed.

## Profiling a whole build

A build's time is not spent in one process. In a warm build the compile step can
be under 40% of the CPU, with the rest in code generators, the linker and a long
tail of custom commands — none of which a profiler attached to sccion would
ever see.

`scripts/profile-warm-build.ps1` captures an ETW trace across every process the
build spawns, bracketing exactly one build. It must be run elevated; ETW CPU
sampling has no unprivileged equivalent.

Build the binary under test with the profiling profile, which is `release` with
debug information kept, so the trace resolves symbols:

```
cargo build --profile profiling -p sccion
```

## A worked example: twelve times faster, and it did not matter

rkyv was measured against bincode on mirror types of `PreprocessorCacheEntry`,
release with LTO, median of eleven to fifteen batches after a warmup. It won
every comparison it was asked for and still lost.

| includes | bincode decode | rkyv access (validated) | encode | on disk |
| --- | --- | --- | --- | --- |
| 500 | 73.2 us | 6.1 us (11.9x) | 2.0x slower | +18.1% |
| 10 000 | 1 011 us | 147 us (6.9x) | 2.5x slower | +13.4% |

Because the denominator was elsewhere. `result_matches` did one
`symlink_metadata` per include — 12.2 to 12.5 microseconds warm against the real
Windows Kits and MSVC header trees, about 6.1 ms for 500 includes — so decoding
was **1.2% of a lookup** and winning it twelve times over saved 67 microseconds.
Validation is almost the whole of rkyv's read (136 of 147 us at the cap) and
cannot be skipped for bytes off disk, so the unvalidated figure is not one to
plan against.

Three costs sat on top of that 1%: `IncludeEntry::path` is an `OsString`, which
rkyv cannot archive; the hits that re-hash rewrite a digest in place, and
archived data cannot grow, so those need a full deserialize (only 1.1–1.5x
better); and rkyv is a far larger dependency surface than a frozen bincode.

**That 1.2% is now about 14%** — see [the lookup path](#the-lookup-path), which
took the syscalls out. The conclusion still holds, but on the two structural
costs rather than on the denominator. If the encoder is revisited, this is the
paragraph to re-measure first.

That 14% is a Windows figure, because the change that produced it is. The same
decode is about 6% of a macOS lookup — see [macOS](#macos), where a separate
change moved that denominator too. Three numbers for one unchanged encoder is
the point: the encoder was never the variable.

### The part that was worth fixing

The same measurement found a real cost elsewhere. `StoragePutRaw` and
`StorageGetRaw` carry whole zip entries between client and daemon, and `Vec<u8>`
through serde is a *sequence* of `u8` — the encoder went round the serializer
once per byte, eight million times for an 8 MiB object, to move bytes that were
already contiguous. `serde_bytes` asks for `serialize_bytes`, which bincode
implements as the same length prefix plus one `write_all`: identical bytes,
which is why it could be done to a live protocol.

| payload | encode | decode |
| --- | --- | --- |
| 1 MiB | 562 → 299 us | 952 → 134 us |
| 8 MiB | 4 400 → 889 us | 8 332 → 870 us |

About 11 ms off an 8 MiB round trip. rkyv encoded the same 8 MiB in 894 us;
`serde_bytes` does it in 889, with no new format. A zero-copy encoder always
looks enormous next to a copy that did not need to exist.

### Where bincode ended up

Its last release is a tombstone, so the crate is vendored in `crates/bincode`
and the format is pinned by `util::bincode_format` and `protocol::wire_format`.

- **wincode** reproduces the format exactly — on `OsString`, `Option`, maps,
  enums, `Result`, empty vectors — and reads bincode's bytes. It is also ~2.4x
  slower to encode. The destination when bincode must go, not a reason to go.
- **bincode 2.0.1** under `config::legacy()` also matches, is slower still, and
  is just as unmaintained.
- **Vendoring cost nothing.** Renamed, migrated 2015 → 2024 and stripped of six
  `unsafe` blocks, it is never slower and decodes a full-size entry 15–20%
  faster: `from_ne_bytes` is a load the compiler sees through where
  `copy_nonoverlapping` into a zeroed local is a memcpy it must reason about.

Two measurement notes, which is what this page is for. Cross-binary comparisons
are invalid — ten encoders in sequence each meet a different allocator and cache
state, and bincode's decode column swung 40% between runs there, against a few
per cent for two implementations alternating in one binary. And first runs lie:
wincode measured 1.16x on encode cold and 0.37–0.42x once warm.

## The lookup path

Five changes to `result_matches`, each measured on its own against the real
`lookup_result_digest`. The harness is `lookup_cost` in
`src/compiler/preprocessor_cache.rs`, `#[ignore]`d:

```
cargo test --release --lib lookup_cost -- --ignored --nocapture
```

Release matters; a debug build measures rustc.

| shape | before | after | |
| --- | --- | --- | --- |
| 200 includes | 2.70 ms | 0.54 ms | 5.0x |
| 496 includes | 6.70 ms | 1.08 ms | 6.2x |
| 2 000 includes | 30.49 ms | 4.36 ms | 7.0x |
| 7 results miss late | 54.11 ms | 1.47 ms | 36.8x |
| 3 basedirs | 13.55 ms | 2.82 ms | 4.8x |
| entry on disk, 496 | 128 843 B | 109 003 B | −15.4% |

In order: resolve and stat in one call rather than `exists` then `stat`; one
stat per file per *lookup* rather than per result, which made the cost flat in
the number of results tried where it had been linear; the stats concurrently in
batches that start at eight and double as the entry proves itself; on Windows,
one directory scan in place of a stat per file in it; and digests stored as 32
bytes rather than 64 hex characters.

**What it cost.** A directory scan pays for the whole directory, so it wins on
density and loses without it: at 13% density it takes 3.20 ms where per-file
stats take about 2.7. Real translation units are dense — an `include` directory
largely gets pulled in wholesale — but a project shaped otherwise pays about 18%
here. A cap on how much of a directory to read was tried and removed: stopping
part-way pays most of the enumeration *and* the fallback stats, and measured
worse at both densities. Windows only; elsewhere `DirEntry::metadata` still
stats per file, and with more than one basedir resolution *is* the stat.

Read-ahead rather than a barrier, because the loop still stops at the first
include that does not match. Stat'ing the whole entry up front measured 0.54 ms
for a stale *first* include where the lazy loop settles it in 0.01 ms; the
ramp brings that to 0.07 ms.

**The order mattered more than any single change.** Storing digests as bytes
would have been noise at the start, when the entry was ~1% of a lookup; after
the syscalls came down it was ~14%. And the first benchmark run showed extra
results and extra basedirs costing *nothing* — both readings were wrong.
Basedirs are stored normalised (forward slashes, lowercase, trailing separator)
and a raw Windows path silently fails to strip, leaving every stored path
absolute and the code under test unreached; and extra results really are free
when the newest one hits, so the cost needs a deliberately late miss to appear.
Implementing from that first run would have optimised two paths the benchmark
was not reaching.

## macOS

Everything above was measured on Windows, which is where the work was done. The
numbers below are the same harnesses on macOS, and they are here because two of
the conclusions do not carry across.

Apple M2 Max, 12 cores, 32 GiB, macOS 27.0, APFS on the internal SSD,
case-folding, Spotlight indexing on. Release builds; `--test-threads 1`, because
`cargo test` runs the stopwatches concurrently by default and three of them on
one machine measure each other.

| shape | before | after | |
| --- | --- | --- | --- |
| 200 includes | 2.45 ms | 0.43 ms | 5.7x |
| 496 includes | 4.87 ms | 0.89 ms | 5.5x |
| 2 000 includes | 19.36 ms | 3.46 ms | 5.6x |
| 7 results, miss late | 5.72 ms | 1.30 ms | 4.4x |
| 3 basedirs | 5.07 ms | 1.71 ms | 3.0x |
| miss on the first include | — | 0.05 ms | |
| entry on disk, 496 | — | 79 819 B, decode 0.05 ms | |

Do not read these against the Windows table. Different machine, and the rule at
the top of this page applies to itself: only the counters compare across
machines, never the clock.

The change is worth its own paragraph, because the first reading of these
numbers was wrong in a way this page exists to catch. A lookup here measured as
96–99% syscalls, which looked like a filesystem the code could do nothing about.
It was not. `prefetch_stats` spawned one blocking task per include, and on APFS
the `stat` costs 2.08 us against 7.8 us of `spawn_blocking` around it — the pool
was costing four times what it bought, and statting every include inline on one
thread beat it 4.7x. Sixty-four includes per task fixed it.

**"96% syscalls" was true and still misleading.** The syscalls were real; almost
none of the time was in the kernel. A profile that stops at "it is I/O-bound" is
a profile that has not been read.

What is left is a lookup that is no longer syscall-bound at all:

```
   496 includes: lookup 0.92 = stats 0.63 + rest 0.28 (31%)  | decode 0.06
  2000 includes: lookup 3.61 = stats 2.50 + rest 1.11 (31%)  | decode 0.23
```

Entry decode is ~6% of a lookup here now, against ~1.2% before the change and
the ~14% this page records for Windows. Same encoder throughout; the denominator
moved twice.

Chunk size is fixed rather than grouped by directory, which was measured:
grouping buys nothing — shuffling the list so no task shares a parent changes
nothing, so there is no locality to capture — and costs 47% on a lopsided tree,
where one directory of 400 headers becomes one task while eleven threads idle.
The Windows path groups by directory for an unrelated reason: there a scan *is*
the metadata.

`file_stat_matches` is **off by default**, and now worth 8–9x rather than the
2.5–2.7x it was before: 0.44 ms against 3.58 at 200 includes, 0.91 against 8.32
at 496. Taking the stats down made the content-digest fallback the dominant cost
when it is reached. The default is the slow path.

**Block cloning does not exist here.** `blockclone/` is entirely
`#[cfg(windows)]`, so every cache hit on macOS copies its bytes; `hit_cost`
reports `cloned 0` on every row. APFS would take it — `clonefile` is a metadata
operation and does not scale with size:

| size | `clonefile` | read/write copy | |
| --- | --- | --- | --- |
| 256 KiB | 0.163 ms | 0.198 ms | 1.2x |
| 2 MiB | 0.164 ms | 0.654 ms | 4.0x |
| 32 MiB | 0.163 ms | 8.859 ms | 54x |

The harness is `clone_versus_copy` in `src/blockclone/mod.rs`, beside the gap it
describes:

```
cargo test --release --lib clone_versus_copy -- --ignored --nocapture
```

Best of 50, in-process for a reason. Measure it with `cp -c` against `cp`
instead and you get 31 ms against 32 ms, which is two process spawns and no
information.

That is not 64x off a cache hit, and the gap should be sized before anyone acts
on it: delivering a 2 MiB uncompressed object costs 2.48 ms today, about seven
times the raw copy of the same bytes, so most of that time is not byte movement
and cloning would not touch it. What the table says is that the byte movement
itself can be made free, as it already is on Windows.

Reading the divan suite (`cargo bench`): `strip_basedirs_multiple` reports a
median of 17.6 us and a **slowest of 44 ms**, reproducibly. That is the one-time
case-folding probe — `strip_basedirs` asks `basedir_folds_case`, which probes the
filesystem once per basedir and caches the answer in `FOLDING`. The benchmark's
basedirs do not exist, so the probe walks to the volume root. Read the median;
the mean is one cold sample smeared across a hundred.

## Checking the machine

```
sccion --check-env
```

Reports the environment sccion is about to run in: where the cache is and
whether it shares a volume with the build tree, the filesystem and whether it
supports block cloning, antivirus exclusions, and whether the compiler flags in
use are cacheable at all.

The failures it exists to catch are not crashes. They are configurations that
work perfectly while quietly costing most of what sccion could have saved — a
cache on a different volume from the build tree, `/Zi` instead of `/Z7` so
almost nothing is cacheable, preprocessor cache mode reporting itself enabled
while structurally unable to hit. Each produces a build that succeeds and
numbers that mean nothing.

It runs out of band, never on the compile path, and it **reports without
changing anything**. No writes to antivirus preferences, no filesystem
reconfiguration. Besides needing elevation, a build tool that reconfigures
antivirus is indistinguishable from malware — and behaviour that varied with
what was detected would give a hit on one machine and a miss on another, which
is the least debuggable kind of bug.
