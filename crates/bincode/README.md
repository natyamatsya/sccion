# bincode 1.3.3, vendored

Upstream: <https://github.com/servo/bincode>, MIT, `LICENSE.md` unchanged.
Upstream's own readme is kept as `upstream-readme.md`.

## Why this is here

bincode's latest release is 3.0.0, and 3.0.0 is a tombstone: its `lib.rs` is a
single `compile_error!` and its readme says development has ceased and nothing
further will be published.

This encoding is not an implementation detail here. It is the preprocessor cache
on disk and every message between client and daemon, so changing it means a
`FORMAT_VERSION` bump, a `PROTOCOL_VERSION` bump, and a cold cache for everyone.
Two tests pin it: `util::bincode_format` and `protocol::wire_format`.

Every alternative was measured (see `docs/Benchmarks.md`). wincode reproduces the
format exactly but encodes about two and a half times slower. bincode 2.0.1 under
`config::legacy()` also reproduces it, and is slower still, and is just as
unmaintained. Nothing was faster than what is already here.

So the problem is *supply*, not format and not speed: an upstream that cannot be
patched if serde or rustc moves underneath it. Copying 3 770 frozen lines with one
dependency answers supply directly. Every other option answered it with a
migration.

## What was changed

Everything not listed here is byte-identical to the published 1.3.3.

- **`Cargo.toml`** — rewritten. The package is `sccion-bincode` so nothing can
  confuse it with the crates.io crate and `cargo tree` says where the code came
  from; the root manifest imports it as `bincode`, which is why no call site
  changed. `publish = false`. Lints allowed rather than fixed, so a diff against
  1.3.3 shows only deliberate changes.
- **`src/lib.rs`** — dropped `#![crate_name = "bincode"]` and two `#![crate_type]`
  attributes. The first contradicts the rename and rustc rejects the mismatch;
  all three are the deprecated way of saying what `Cargo.toml` says.
- **`src/lib.rs`, `src/config/mod.rs`** — eight doc-example lines say
  `sccion_bincode::` instead of `bincode::`, so the doctests compile.
- **`tests/test.rs`** — `extern crate sccion_bincode as bincode;`, so the rest of
  upstream's suite stays verbatim.
- **`readme.md`** → `upstream-readme.md`, to make room for this file.

Upstream's tests all still pass: 4 unit, 42 integration, 4 doc.

## Updating it

Don't, casually. The reason this is vendored is that its output is a format
already written to disk. Any change that touches `src/ser`, `src/de`,
`src/config` or `src/byteorder` must leave `util::bincode_format` and
`protocol::wire_format` passing, and if it cannot, it is a format change and
needs the version bumps that implies.
