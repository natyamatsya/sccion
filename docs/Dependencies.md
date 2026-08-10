# Keying on what the compiler read

To reuse an object, sccion has to know every file the compilation read. There
are two ways to find that out, and which one is used decides what a cache miss
costs.

**Preprocess first.** Run the preprocessor, hash the result, and recover the
include list from the preprocessed text. This works for every compiler, and it
is what sccion does by default. The cost is an extra compiler run on every miss.

**Compile and ask.** Compile the translation unit and have the compiler report
what it opened while it was doing the work it was going to do anyway. This
removes the preprocessor run entirely, and needs a compiler that can report its
inputs — currently MSVC, through `/sourceDependencies`.

The second is [`use_dependency_keying`](Local.md#preprocessor-cache-mode), which
also documents when it pays off and when it does not; the trade is real in both
directions and depends on whether you build one checkout repeatedly or many
fresh ones.

Compilers that cannot report their inputs preprocess as before, as do
distributed compilations, which need preprocessed source to send.

## Dependency files

Both routes end at the same place: a manifest listing every input and its
digest, stored under a key derived from the compilation, and consulted before
anything is compiled.

Make-style dependency files (`-MD`, `/showIncludes` output, moc's
`--output-dep-file`) and MSVC's `/sourceDependencies` JSON are parsed into that
one representation. A build that already asks for a dependency file keeps it:
sccion uses the caller's rather than adding a second, because two
`/sourceDependencies` flags mean `cl` writes only the last, and the file the
build asked for would never appear.

**Manifests are checkout-independent.** The paths in a manifest key, and the
include paths recorded inside it, are normalized against the declared checkout
root like everything else — otherwise a second checkout matches no manifest and
preprocesses everything it could have skipped, and a manifest that *was* shared
would be validated against the other checkout's files, which is a wrong-object
hit rather than a miss. See [Sharing one cache between checkouts](Checkouts.md).

## Qt: moc

`moc` is cached as a compiler in its own right, keyed the same way a C
compilation is, including the same dependency-file machinery and the same
checkout normalization. On a Qt build this is around a quarter of all
compilations — 669 of 2557 on one project — and took its warm build from 225 s
to 148 s.

`rcc` output is not compiled by sccion but is compiled *through* it, and it has
a caveat worth reading before expecting two checkouts to share:
[Generated sources](Checkouts.md#generated-sources).

## When the answer is "nothing"

A compilation that opens no files at all is a normal thing — a Qt `qrc_*.cpp` is
a byte array with no `#include` in it — and it is cacheable. An empty report and
an unreadable one must not be confused, and conflating them made every such unit
permanently uncacheable on one real build: 59 translation units recompiled every
time, a 17.6% warm regression that no unit test caught.

The two are now distinct both in behaviour and in what gets reported.
`explain_miss` names `dependency_report_unreadable` as its own reason, so a
compilation that cannot be cached says which of the two happened rather than
leaving it to be inferred. See [the MCP surface](MCP.md#explain_miss-and-the-classification).
