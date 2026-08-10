// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Guards the compiler flags that make an object independent of where the tree
//! is checked out.
//!
//! Two checkouts can only share cache entries if the objects are
//! interchangeable, and with `/Z7` they are not by default: the debug info
//! carries absolute source paths, so an object reused in another checkout
//! points a debugger at a tree that may not even exist. `/d1trimfile` plus
//! `/Brepro` fixes that.
//!
//! `/d1trimfile` is undocumented, so nothing guarantees it keeps working across
//! a Visual Studio upgrade -- and the failure is silent. This test is what
//! turns that into a red build rather than a debugging session that quietly
//! opens the wrong file.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Compile `src` into `obj`, and return the object's bytes.
///
/// The object is written *outside* the source root on purpose. The one
/// absolute path these flags cannot remove is the object's own `/Fo` path, so
/// keeping it elsewhere lets the assertion be the clean one -- the source root
/// must not appear at all.
fn compile(cl: &Path, src_root: &Path, obj: &Path, trim: bool) -> Option<Vec<u8>> {
    let mut cmd = Command::new(cl);
    cmd.args(["/nologo", "/c", "/O2", "/Z7"]);
    if trim {
        cmd.arg(format!("/d1trimfile:{}\\", src_root.display()));
        cmd.arg("/Brepro");
    }
    cmd.arg(src_root.join("u.cpp"));
    cmd.arg(format!("/Fo{}", obj.display()));
    cmd.current_dir(src_root);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        eprintln!(
            "cl failed: {}",
            String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    fs::read(obj).ok()
}

fn occurrences(haystack: &[u8], needle: &str) -> usize {
    let n = needle.as_bytes();
    if n.is_empty() || haystack.len() < n.len() {
        return 0;
    }
    haystack.windows(n.len()).filter(|w| *w == n).count()
}

#[test]
fn msvc_trims_paths_out_of_debug_info() {
    let cl = match which::which("cl.exe") {
        Ok(cl) => cl,
        Err(_) => {
            // Skipping so that `cargo test` works off a developer prompt, but a
            // skip that reports success is how a guard stops guarding. Anywhere
            // this must actually run, set SCCACHE_TEST_REQUIRE_MSVC and the
            // absent toolchain becomes a failure instead of a green tick.
            // Non-empty, not merely present: clearing a variable often leaves it
            // set to the empty string, and treating that as "required" turns an
            // ordinary developer machine into a failing build.
            if std::env::var_os("SCCACHE_TEST_REQUIRE_MSVC").is_some_and(|v| !v.is_empty()) {
                panic!(
                    "SCCACHE_TEST_REQUIRE_MSVC is set but cl.exe is not on PATH; \
                     run from a Visual Studio developer prompt"
                );
            }
            eprintln!("skipping: cl.exe is not on PATH (run from a developer prompt)");
            return;
        }
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    // Distinctive, and equal length, so one cannot accidentally substring the
    // other and mask a failure.
    let (a, b) = (root.join("wt-aaa"), root.join("wt-bbb"));
    let objdir = root.join("objs");
    fs::create_dir_all(&objdir).unwrap();
    for d in [&a, &b] {
        fs::create_dir_all(d).unwrap();
        // No includes: this must compile without INCLUDE being set up.
        fs::write(d.join("u.cpp"), "int f(int x){ return x + 1; }\n").unwrap();
    }

    let obj = |name: &str| -> PathBuf { objdir.join(name) };

    // Negative control. Without the flags the source root is embedded, which is
    // what makes cross-checkout reuse wrong -- and it proves this test is
    // exercising the flags rather than asserting something vacuous.
    let plain_a = compile(&cl, &a, &obj("plain_a.obj"), false).expect("compile a");
    assert!(
        occurrences(&plain_a, "wt-aaa") > 0,
        "without /d1trimfile the source root should be embedded; if it is not, \
         this test can no longer detect the regression it exists for"
    );

    // With the flags, neither object may mention its own source root...
    let trim_a = compile(&cl, &a, &obj("trim_a.obj"), true).expect("compile a trimmed");
    let trim_b = compile(&cl, &b, &obj("trim_b.obj"), true).expect("compile b trimmed");
    assert_eq!(
        occurrences(&trim_a, "wt-aaa"),
        0,
        "/d1trimfile + /Brepro should leave no absolute source path in the object"
    );
    // ...and, the property that actually matters, neither may mention the other's.
    assert_eq!(
        occurrences(&trim_a, "wt-bbb"),
        0,
        "object must not reference the other checkout"
    );
    assert_eq!(occurrences(&trim_b, "wt-aaa"), 0);
    assert_eq!(occurrences(&trim_b, "wt-bbb"), 0);

    // The source must still be recorded, or the debugger has nothing to resolve.
    assert!(
        occurrences(&trim_a, "u.cpp") > 0,
        "the source file name must survive trimming, relative"
    );
}
