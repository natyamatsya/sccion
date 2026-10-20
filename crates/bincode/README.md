# bincode 1.3.3, vendored and migrated to edition 2024

Upstream: <https://github.com/servo/bincode>, MIT, `LICENSE.md` unchanged.
Upstream's own readme is kept as `upstream-readme.md`.

## Why this is here

bincode's latest release is 3.0.0, and 3.0.0 is a tombstone: its `lib.rs` is a
single `compile_error!` and its readme says development has ceased and nothing
further will be published.

This encoding is not an implementation detail here. It is the preprocessor cache
on disk and every message between client and daemon, so changing it means a
`FORMAT_VERSION` bump, a `PROTOCOL_VERSION` bump, and a cold cache for everyone.

Every alternative was measured (see `docs/Benchmarks.md`). wincode reproduces the
format exactly but encodes about two and a half times slower. bincode 2.0.1 under
`config::legacy()` also reproduces it, and is slower still, and is just as
unmaintained. Nothing was faster than what is already here.

So the problem is *supply*, not format and not speed: an upstream that cannot be
patched if serde or rustc moves underneath it. Copying 3 770 lines with one
dependency answers supply directly. Every other option answered it with a
migration.

## What guards it

Not a diff against the published crate — see below. Three test suites:

- **Upstream's own**, kept and passing: 4 unit, 42 integration, 4 doc.
- **`util::bincode_format`**, which pins the encoding of every shape this project
  stores, byte for byte, against a recording made from published 1.3.3.
- **`protocol::wire_format`**, which does the same for the blob-carrying messages.

Any change here must leave all three green. If it cannot, it is a format change,
and it needs the version bumps that implies.

## How it diverges from published 1.3.3

Originally this crate was kept close to what was published, so that a diff
against 1.3.3 would show only deliberate changes. **That property is gone**: the
crate has been migrated from edition 2015 to edition 2024, which touched every
file. The tests above replaced it as the safety net, which is the stronger
guarantee anyway — they check behaviour rather than text.

Packaging:

- **`Cargo.toml`** — rewritten. The package is `sccion-bincode` so nothing can
  confuse it with the crates.io crate and `cargo tree` says where the code came
  from; the root manifest imports it as `bincode`, which is why no call site
  changed. `publish = false`.
- **`src/lib.rs`** — dropped `#![crate_name = "bincode"]` and two `#![crate_type]`
  attributes. The first contradicts the rename and rustc rejects the mismatch;
  all three are the deprecated way of saying what `Cargo.toml` says.
- **`src/lib.rs`, `src/config/mod.rs`** — doc examples say `sccion_bincode::`, so
  the doctests compile.
- **`tests/test.rs`** — `extern crate sccion_bincode as bincode;`, and two stray
  semicolons removed.
- **`readme.md`** → `upstream-readme.md`.

Edition migration, 2015 → 2018 → 2021 → 2024, one hop at a time with the tests
run at each:

- `cargo fix --edition` for each hop: crate-relative paths became `crate::`,
  `dyn` on trait objects, and the rest of the mechanical work.
- The shims `cargo fix` leaves behind were then removed rather than kept:
  twelve `r#try!(...)` became `...?` (which is what `try!` expanded to), and
  `expr_2021` fragment specifiers went back to `expr`. These are internal macros
  with fixed call sites, so the wider 2024 `expr` matcher costs nothing.
- `cargo fmt` reformatted to the 2024 style edition, because `.rustfmt.toml`
  applies repo-wide and the pre-commit hook would have done it anyway.

Lints are allowed rather than fixed, rustc's and clippy's both; one of clippy's
is deny-by-default and fires on a test that round-trips 3.14159.

## Two things that were known, and are now done

- **`unsafe` is gone, and forbidden.** The six blocks in `src/byteorder.rs` were
  all pre-1.32 workarounds -- one cited rust-lang/rust#22776 as the reason it
  could not be written safely. `from_ne_bytes`/`to_ne_bytes` and
  `f32::from_bits`/`to_bits` replace them exactly, and the crate now carries
  `#![forbid(unsafe_code)]` so a seventh cannot appear quietly.
- **Network reads are bounded.** `IoReader::fill_buffer` in `src/de/read.rs`
  still sizes its buffer from a length field in the payload before reading it,
  and the default limit is still `Infinite` -- that is upstream behaviour and is
  left alone. The two places in `src/dist/http/` that decode from the
  network -- `read_bincode` in `handler.rs` and `run_job` in `server.rs` --
  pass a limit instead. `util::bincode_limits` in the main crate is
  the test that a limit refuses before allocating rather than after.

Note for anyone adding a limit elsewhere: `bincode::config()` is deprecated and
`options()` is **not** a drop-in for it. `DefaultOptions` is varint and
reject-trailing; the free functions, and therefore everything already written,
are fixint and allow-trailing. Spell out
`DefaultOptions::new().with_fixint_encoding().allow_trailing_bytes()`.
