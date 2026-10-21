# Sharing one cache between checkouts

Two checkouts of the same project — a second clone, a `git worktree`, an agent's
scratch copy — compile the same sources and normally share nothing, because the
absolute path each was checked out to reaches the cache key through include
flags, the working directory and the preprocessed text.

Declaring a checkout root fixes that. Every path under it is normalized out of
the key, so the same source compiled in two places produces the same key and the
second build starts warm.

Measured on a 2557-unit MSVC + Qt + CMake build, a fresh worktree building
against a cache another worktree filled: **2029 of 2557 hits, 79.35%** — 250 s
against 530 s for the build that filled it. What the remaining 528 are is the
subject of most of this page.

## Turning it on

```bash
sccion --init      # writes .sccion.toml at the top of the checkout
git add .sccion.toml
```

The file it writes is comments and nothing else, which is the common case: the
directory holding it is the root, so an empty file already does the whole job.
Every setting it carries is listed in it, commented out. Writing it by hand is
equally fine:

```toml
# Empty is the common case -- the file's own directory is the checkout root.
```

`--init` never edits a file that is already there, and it declines when one
further up the tree already governs the directory: a second file deeper down
would move the root and strand every key computed against the first.

That is the whole configuration. sccion finds the file by walking up from each
compilation the way tools find `.clang-format`, and **the directory holding it
supplies the root**, so one committed file is correct in every checkout of the
project, including ones that do not exist yet. See
[Project configuration](Local.md#project-configuration) for the other keys it
carries.

Nothing needs setting in the environment, and nothing needs to agree between
machines.

## What it requires of the build

Sharing objects between checkouts is only correct if the objects do not carry
the path they were built from. A compiler records that path in debug
information, and a restored object is byte-for-byte what the other checkout
produced.

For MSVC, that means `/Z7` (or `/Zi` where the compiler emits debug info into
the object) together with `/d1trimfile:<checkout root>`, which removes the
absolute prefix, and `/Brepro` for reproducible output. For GCC and Clang it
means `-fdebug-prefix-map`. Without one of those, the objects are still correct
to *run* — but a debugger opening one from the other checkout will look for
sources under a path that does not exist on this machine, and a build system
that does this needs a source root configured for its debugger and its crash
dumps.

`hash_working_directory` exists for builds that have not done this: it keeps the
compilation's directory in the key so two directories cannot share an entry. See
[Local.md](Local.md#preprocessor-cache-mode). Declaring a checkout root asserts
that the trade above has been made.

## What a hit replays

A hit hands the build more than the object: what the compiler printed, and the
dependency files it wrote. Those name every header by absolute path -- MSVC's
`/showIncludes` lines, a `-MD` file, moc's `--output-dep-file` and
`--output-json` -- and the build system takes them as the compilation's
dependencies. Replayed as the first checkout wrote them, the second checkout
would depend on the first one's headers, and an edit to a header there would
rebuild nothing. Measured before this was handled: 1,853 objects of a second
checkout depended on the first one's headers rather than their own. After it, a
declaration added to a header in the second checkout rebuilt exactly the 16
objects that include it, and reran its moc.

So sccion stores them with the checkout root taken out wherever a path starts
with it, and a hit puts back the root of the checkout it serves, in the
separator style the text used (`/`, `\`, or `\\` as JSON escapes it). The build
system receives what a compile in that checkout would have printed.

A compilation's checkout root is the declared root that contains its working
directory -- the `.sccion.toml` directory above it, or the entry of
`SCCACHE_BASEDIRS` it is under when that lists every checkout. A root that does
not contain the working directory is some other tree, and paths under it are
replayed as they stand; so is everything a compilation prints when its working
directory is under no root at all, a build directory outside the checkout
being the usual case.

A root spelled with escaped spaces (`\ ` in a Make-style dependency file) is not
recognized, so a checkout whose path contains a space keeps absolute paths in
its dependency output.

No compiler option does this instead. `-ffile-prefix-map` and its
`-fdebug-`/`-fmacro-` siblings, and MSVC's `/d1trimfile`, rewrite `__FILE__`,
debug information and coverage data, not dependency output, and MSVC's
`/sourceDependencies` writes the same absolute paths as JSON.

## What is deliberately not shared

**Precompiled headers stay inside the checkout that built them.** A `.pch`
restored from another checkout is not rejected by `cl` — it is accepted, and
then every `/Yu` translation unit that uses it fails with a wall of C2011 type
redefinitions, because the compiler can no longer match the force-included
header against the one baked into the PCH and processes it a second time on top.
The restored file is byte-identical to the one the other checkout produced; the
failure is in how `cl` uses it, not in what was stored.

So compilations that create or consume a PCH keep their absolute paths in the
key, which confines them to one checkout. Everything else still shares.

On the build measured above that is 528 of 2557 units — **every one of the
misses that remain**. It is the price of the feature, not a defect in it, and it
does not amortize across checkouts the way object entries do.

## Generated sources

A generator that embeds anything checkout-specific in its output defeats sharing
for whatever compiles that output, and sccion is right to miss: the inputs
really are different bytes.

Qt's `rcc` is the case worth knowing. It writes each resource file's
last-modified time into the generated `qrc_*.cpp`, so two checkouts made seconds
apart produce different generated source for identical resources. `rcc` honours
`SOURCE_DATE_EPOCH` and overrides every embedded timestamp with it:

```
SOURCE_DATE_EPOCH=1600000000
```

On the build above that is worth 42 units — the difference between 77.71% and
the 79.35% quoted at the top.

Setting it is not enough on its own if the generated files already exist. CMake's
`AUTORCC` and `AUTOMOC` track a *stamp* file rather than the generated sources,
so deleting `qrc_*.cpp` or `moc_*.cpp` does not make them regenerate, and the
change appears to do nothing. Touch the `.qrc` inputs, or the generator
executable, to make the custom command re-run.

## When a checkout is not sharing

`SCCION_TRACE_COMPONENTS=1` breaks every computed key into its parts —
compiler, arguments, extra files, environment, working directory, inputs — and
records them with the miss. `compare_keys` then names which part differs between
two checkouts, rather than leaving the whole key as one opaque digest. See
[the MCP surface](MCP.md#maintaining-the-cache-itself); `SCCION_MISS_LOG` gives
the same records as a file for a build that wants all of them.

The component that differs is usually the answer on its own. `arguments` means a
flag still carries an absolute path. `inputs` means the sources genuinely differ
— a generated file, as above. `working_directory` means `hash_working_directory`
is on and no root is declared.
