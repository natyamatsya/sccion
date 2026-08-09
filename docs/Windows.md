# MSVC and Windows builds

Everything here is about making a Windows C++ build cacheable at all. The
failures this documents are not crashes: each one produces a build that succeeds
while caching far less than it appears to, or nothing.

## What the build has to look like

**Debug information must go into the object, not into a shared PDB.** With `cl`,
`/Zi` gives every translation unit in a target the *same* PDB, which two
compilations cannot both produce from cache, so sccache refuses them as
`shared_pdb`. `/Z7` puts the debug information in the object and is cacheable.
Most CMake projects get `/Zi` by default and cache one object per target until
this is changed. `clang-cl` treats `/Zi` as `/Z7` and does not have the problem.

**`/Brepro` removes the COFF timestamp**, which is what makes the same source
compile to the same bytes twice — and it is what lets a build's objects be
compared at all. It is not sufficient on its own: on one real build only one
sampled object in eight reproduced byte for byte even with `/Brepro` set, so
something else in the toolchain still varies. That caps how much
[`SCCACHE_VERIFY_HITS`](Configuration.md) can check, and it is a reason to treat
"cannot be verified" as an ordinary answer rather than a warning.

`sccache --check-env` reports both, along with the rest of the machine's
configuration. See [Measuring and checking](Benchmarks.md#checking-the-machine).

## Flags

Unknown `/`-form switches are treated as flags, not as input files. The `-` form
of the same switch always worked; the `/` form did not, and a single unrecognized
switch made the compilation look like it had two inputs and fall out of caching
entirely. On one real build `/dynamicdeopt` made **100% of the units
uncacheable** this way.

`/dynamicdeopt` also writes a companion `.alt.obj` beside the object. Both are
stored and both are restored: a hit that produced only the object would leave a
stale companion next to a fresh one, which the linker then uses.

## Precompiled headers

`/Yc` and `/Yu` compilations are cached, including the `.pch` itself. On a
1,898-unit build these were 528 units that previously could not be cached at
all.

A PCH is kept **inside the checkout that built it**, and deliberately cannot be
shared with another checkout — `cl` accepts a foreign `.pch` and then fails every
`/Yu` unit with C2011. See [Sharing one cache between checkouts](Checkouts.md).

## Preprocessor cache mode

[Preprocessor cache mode](Local.md#preprocessor-cache-mode) works for MSVC.
Three things had to be true before it could hit at all, and each of them failed
silently:

- MSVC passed `-EP` unconditionally, which emits no `#line` directives, so the
  include list the mode depends on could never be recovered. It now uses `-E`
  when the mode is active.
- `#line` detection compared a four-byte slice against a five-byte literal, so
  it never matched — for any compiler, not just MSVC.
- Cache keys were built with forward slashes while the index rebuilt at daemon
  startup used backslashes, so **the preprocessor cache never hit again after a
  restart**. Since the daemon idles out, that meant it was effectively dead in
  ordinary use.

`--check-env` reports the mode as unavailable rather than enabled when the
backend cannot support it, which is the state that previously reported itself as
working.

## Where the cache lives

Put the cache on the same volume as the build tree. Materializing a hit copies
the object out of the cache, and a copy across volumes cannot use the
filesystem's block cloning — on ReFS and on Dev Drive, a hit that clones is
close to free, and one that copies is not.

`--check-env` reports the volume of each, the filesystem, and whether the
antivirus exclusions that dominate small-file I/O on Windows are in place. It
reports and never changes anything: a build tool that reconfigures antivirus is
indistinguishable from malware, and behaviour that varies with what was detected
gives a hit on one machine and a miss on another.
