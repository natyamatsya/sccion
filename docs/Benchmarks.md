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

rkyv was evaluated as a replacement for bincode, which encodes the preprocessor
cache on disk and every client/daemon message, and whose final release is a
`compile_error!`. The measurement is worth keeping because of how the answer
came out: the candidate won every comparison it was asked for and still lost.

Mirror types of `PreprocessorCacheEntry` and `IncludeEntry` — blake3 hex digests
at their real 64-character width, Windows include paths, both `Option<Timestamp>`
arms — encoded with bincode 1 and with rkyv 0.8, release with LTO, median of
eleven to fifteen batches after a warmup. `path` was a `String` on both sides,
because rkyv has no `OsString` and the comparison has to be of encoders rather
than of one encoder against a field it cannot represent.

| includes | bincode encode | rkyv encode | bincode decode + walk | rkyv access + walk | on disk |
| --- | --- | --- | --- | --- | --- |
| 50 | 0.5 us | 1.2 us | 3.6 us | 0.6 us (6.5x) | +18.7% |
| 500 | 4.2 us | 8.4 us | 73.2 us | 6.1 us (11.9x) | +18.1% |
| 4 000 | 40.5 us | 168.7 us | 482 us | 58.9 us (8.2x) | +13.4% |
| 10 000 | 302 us | 757 us | 1 011 us | 147 us (6.9x) | +13.4% |

Six to twelve times faster to read, two to four times slower to write, and
thirteen to nineteen percent larger. Validation is almost the whole of the read —
136 of 147 microseconds at the cap — and it cannot be skipped for bytes that came
off disk, so the unvalidated figure, ninety to a hundred and thirty times, is not
a number to plan against.

Then the denominator. `result_matches` does one `symlink_metadata` per include
before it compares anything, and against the real Windows Kits and MSVC header
trees on the development machine that costs **12.2 to 12.5 microseconds per
stat, warm — about 6.1 milliseconds for 500 includes**. So for a typical
translation unit the decode is 73 microseconds inside a 6.2 millisecond lookup:
**1.2% of the path**. Winning it twelve times over saves 67 microseconds, near
one percent, which is the same conclusion
[profiling a whole build](#profiling-a-whole-build) reaches by a different route
and the module's own comment reaches by reading the code: the cost here is
`open()` latency through the filter stack, not anything the CPU is doing.

Three structural costs sit on top of a one percent win. `IncludeEntry::path` is
an `OsString`, which rkyv cannot archive, so the field would have to become bytes
and answer for lossless WTF-16 on Windows. `lookup_result_digest` takes
`&mut self` and rewrites `include.digest` in place when it re-hashes, and
archived data cannot grow a string — so exactly the hits that update an entry
would need a full deserialize, which measured only 1.1 to 1.5 times better than
bincode. And rkyv is a far larger dependency surface than a bincode that has
stopped moving.

**The conclusion is to stay on bincode 1 for the cache.** The encoder will have
to be replaced eventually, and the test that says whether a candidate is a
drop-in is `bincode_format` in `src/util.rs`, which pins the current encoding
byte for byte.

### The part that was worth fixing

The same measurement found a real cost somewhere else. In client-side mode
`StoragePutRaw` and `StorageGetRaw` carry whole zip entries between client and
daemon, and an 8 MiB object cost 4.4 milliseconds to encode and 8.3 to decode.
Not because of bincode: because `Vec<u8>` through serde is a *sequence* of
`u8`, so the encoder went round the serializer once per byte, eight million
times, to move bytes that were already contiguous.

`serde_bytes` asks for `serialize_bytes`, which bincode implements as the same
length prefix followed by one `write_all` — identical bytes on the wire, which
is why it could be done to a live protocol at all, and why `wire_format` in
`src/protocol.rs` now asserts those bytes against a recording made before the
change.

| payload | encode before | after | decode before | after |
| --- | --- | --- | --- | --- |
| 64 KiB | 43.1 us | 7.9 us (5.5x) | 51.6 us | 4.7 us (10.9x) |
| 1 MiB | 562 us | 299 us (1.9x) | 952 us | 134 us (7.1x) |
| 8 MiB | 4 400 us | 889 us (5.0x) | 8 332 us | 870 us (9.6x) |
| 32 MiB | 17 811 us | 3 583 us (5.0x) | 33 701 us | 3 621 us (9.3x) |

About 11 milliseconds off the round trip for an 8 MiB object. rkyv encoded the
same 8 MiB in 894 microseconds; `serde_bytes` does it in 889, with no new
dependency and no change of format. rkyv still wins the decode outright by
borrowing instead of copying — but it is now winning against one memcpy at
9.6 GB/s, not against work that should never have happened.

**That is the shape of finding worth looking for.** A zero-copy encoder always
looks enormous next to a copy that did not need to exist, and the ratio says
nothing about the encoder. Find the denominator first; the answer here was to
stop copying, and it cost one annotation.

### The candidate that actually is a drop-in

wincode, with `serde-wincode`, is the alternative bincode's own farewell points
at. Measured against the pinned bytes, it passes the part that matters: the
encoding is **identical** — on primitives, on `OsString`, on `Option` in both
arms, on `BTreeMap` and `HashMap`, on enums in unit, struct and boxed form, on
`Result<Option<Vec<u8>>, String>`, on empty strings and empty vectors — and it
reads bytes bincode wrote. A cache would survive the swap, and so would a
daemon mid-flight.

`OsString` is worth naming, because it is what `IncludeEntry::path` is and what
rkyv cannot represent at all.

What it is not is faster. Both encoders measured in one binary, three runs, on
the same data:

| entry | encode | decode |
| --- | --- | --- |
| 50 includes | 0.46-0.47x | 0.79-0.85x |
| 500 includes | 0.37-0.39x | 1.45-1.53x |
| 10 000 includes | 0.42x | 0.92-0.98x |

Encoding is consistently around two and a half times slower; decoding is a wash
outside the middle size. (A first run showed encode at 1.16x and was simply
wrong — cold. It is quoted here as a reminder to run the thing three times.)

So wincode is the destination rather than the reason to leave. When bincode has
to go, this is the move that costs no format change, no version bump and no
cold cache; it is not an argument for going today.

### What vendoring cost

bincode is now vendored in `crates/bincode`, renamed, migrated from edition
2015 to 2024, and stripped of its six `unsafe` blocks --
`copy_nonoverlapping` into a zeroed integer became `from_ne_bytes`, a transmute
to `[u8; N]` became `to_ne_bytes`, and the float pointer-casts became
`f32::from_bits`. Those are load and store paths, so they are exactly where a
regression would hide.

Both crates were measured in one binary, three runs. First the part that has to
hold: the bytes are **identical** on every integer width, on floats including
negative zero, infinity and a NaN with a payload, on the preprocessor cache
entry and on a `serde_bytes` blob -- and each crate reads the other's output.

| shape | encode | decode |
| --- | --- | --- |
| 50 includes | 1.05-1.08x | 1.03-1.05x |
| 500 includes | 0.98-1.02x | 1.08-1.10x |
| 10 000 includes | 1.01-1.10x | **1.15-1.20x** |
| 1 MiB blob | 1.01-1.24x | 1.00x |
| 8 MiB blob | 1.00x | 1.00x |

Above 1.00x means the vendored crate is faster. It is never slower, and it
decodes a full-size cache entry 15 to 20 per cent quicker -- `from_ne_bytes` is
a load the compiler can see through, where `copy_nonoverlapping` into a zeroed
local is a memcpy it has to reason about. The blob path is unchanged because it
is already one `memcpy` and nothing else.

So the modernisation was free, and the byte-for-byte checks say it was safe.

One measurement note, since this page is about those: the nine-encoder sweep
above cannot answer this question. Run ten encoders in sequence and each one
meets a different allocator and cache state, and bincode's decode column swings
by forty per cent between runs. Two implementations alternating in one binary
is stable to a few per cent. Comparisons are only as good as what is held
still.

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
