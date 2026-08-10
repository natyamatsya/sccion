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
