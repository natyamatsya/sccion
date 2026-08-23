// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! Confirms that debug information survives a cache hit taken from a *different
//! checkout* of the same tree.
//!
//! This is the property that decides whether cross-checkout cache sharing is
//! safe, and it is not something the build's exit status can tell you: an
//! object with the wrong paths links fine, runs fine, and only misleads you
//! later, in a debugger, one file at a time.
//!
//! Reading the PDB is a stronger check than stepping through a debugger by
//! hand, because it can assert the thing a human would have to take on trust —
//! that every source file the PDB names resolves inside *this* checkout, and
//! that its contents are the ones the code was actually compiled from. MSVC
//! records a checksum per source file for exactly this purpose; a debugger uses
//! it to decide whether a file on disk matches, and so do we.

#![cfg(windows)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use md5::Digest as _;
use pdb::FallibleIterator;

/// Which digest MSVC recorded for a source file, and its bytes.
///
/// Not fixed to one algorithm: the default has moved between toolset versions
/// (this one does not use MD5), and hard-coding the wrong one produces a test
/// that compares nothing.
#[derive(Debug)]
enum SourceHash {
    Md5(Vec<u8>),
    Sha1(Vec<u8>),
    Sha256(Vec<u8>),
    None,
}

impl SourceHash {
    fn matches(&self, contents: &[u8]) -> bool {
        match self {
            SourceHash::Md5(want) => md5::Md5::digest(contents).to_vec() == *want,
            SourceHash::Sha1(want) => sha1::Sha1::digest(contents).to_vec() == *want,
            SourceHash::Sha256(want) => sha2::Sha256::digest(contents).to_vec() == *want,
            SourceHash::None => false,
        }
    }

    fn is_none(&self) -> bool {
        matches!(self, SourceHash::None)
    }
}

fn require_msvc(tool: &str) -> Option<PathBuf> {
    match which::which(tool) {
        Ok(p) => Some(p),
        Err(_) => {
            // A skip that reports success is how a guard stops guarding.
            if std::env::var_os("SCCACHE_TEST_REQUIRE_MSVC").is_some_and(|v| !v.is_empty()) {
                panic!("SCCACHE_TEST_REQUIRE_MSVC is set but {tool} is not on PATH");
            }
            eprintln!("skipping: {tool} is not on PATH (run from a developer prompt)");
            None
        }
    }
}

fn run(cmd: &mut Command, what: &str) {
    let out = cmd.output().unwrap_or_else(|e| panic!("{what}: {e}"));
    assert!(
        out.status.success(),
        "{what} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Every source file the PDB names, as a path plus the checksum recorded for it.
fn pdb_source_files(pdb_path: &Path) -> Vec<(String, SourceHash)> {
    let file = fs::File::open(pdb_path).expect("open pdb");
    let mut pdb = pdb::PDB::open(file).expect("parse pdb");
    let strings = pdb.string_table().expect("string table");
    let dbi = pdb.debug_information().expect("debug information");

    let mut found = Vec::new();
    let mut modules = dbi.modules().expect("modules");
    while let Some(module) = modules.next().expect("module") {
        let Some(info) = pdb.module_info(&module).expect("module info") else {
            continue;
        };
        let program = info.line_program().expect("line program");
        let mut files = program.files();
        while let Some(f) = files.next().expect("file") {
            let name = f
                .name
                .to_string_lossy(&strings)
                .expect("file name")
                .to_string();
            let sum = match f.checksum {
                pdb::FileChecksum::Md5(b) => SourceHash::Md5(b.to_vec()),
                pdb::FileChecksum::Sha1(b) => SourceHash::Sha1(b.to_vec()),
                pdb::FileChecksum::Sha256(b) => SourceHash::Sha256(b.to_vec()),
                pdb::FileChecksum::None => SourceHash::None,
            };
            found.push((name, sum));
        }
    }
    found
}

/// Audit a real PDB, pointed at by the environment.
///
/// The synthetic test below proves the mechanism. This one is for pointing at
/// an actual product build before trusting cross-checkout sharing on it:
///
/// ```text
/// SCCACHE_TEST_PDB=D:\...\build\...\RelWithDebInfo\product.pdb
/// SCCACHE_TEST_PDB_ROOT=D:\src\product
/// ```
///
/// Only files that resolve under the root are checked. A product PDB names the
/// SDK, the CRT and every vendored dependency, and those legitimately live
/// elsewhere; the question here is whether *our* sources are addressed in a way
/// that survives being compiled in a different checkout.
#[test]
fn real_pdb_addresses_project_sources_portably() {
    let (Some(pdb_path), Some(root)) = (
        std::env::var_os("SCCACHE_TEST_PDB"),
        std::env::var_os("SCCACHE_TEST_PDB_ROOT"),
    ) else {
        eprintln!("skipping: set SCCACHE_TEST_PDB and SCCACHE_TEST_PDB_ROOT to audit a real build");
        return;
    };
    let root = PathBuf::from(root);
    let files = pdb_source_files(Path::new(&pdb_path));
    assert!(!files.is_empty(), "PDB named no source files at all");

    let mut checked = 0usize;
    let mut absolute_into_root = Vec::new();
    for (name, checksum) in &files {
        let p = Path::new(name);
        // An absolute path *inside* the tree is the failure this is looking
        // for: it pins the object to this checkout.
        if p.is_absolute() {
            if p.starts_with(&root) {
                absolute_into_root.push(name.clone());
            }
            continue;
        }
        let local = root.join(name);
        if !local.is_file() {
            continue; // not ours
        }
        checked += 1;
        assert!(
            checksum.matches(&fs::read(&local).unwrap()),
            "{name} on disk does not match what the PDB says was compiled"
        );
    }

    assert!(
        absolute_into_root.is_empty(),
        "{} source paths are absolute inside the checkout, so objects built here \
         cannot be reused by another checkout; first few: {:?}",
        absolute_into_root.len(),
        absolute_into_root.iter().take(5).collect::<Vec<_>>()
    );
    assert!(
        checked > 0,
        "no project sources resolved under {}; is SCCACHE_TEST_PDB_ROOT right?",
        root.display()
    );
    eprintln!("verified {checked} project sources in {pdb_path:?}");
}

#[test]
fn debug_info_from_another_checkout_still_resolves_locally() {
    let (Some(cl), Some(link)) = (require_msvc("cl.exe"), require_msvc("link.exe")) else {
        return;
    };

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let (a, b) = (root.join("wt-aaa"), root.join("wt-bbb"));
    for d in [&a, &b] {
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(
            d.join("src").join("m.cpp"),
            "int helper(int x){ return x * 2; }\nint main(){ return helper(21) - 42; }\n",
        )
        .unwrap();
    }

    // Compile in checkout A, with the flags that make the object portable.
    let mut c = Command::new(&cl);
    c.args(["/nologo", "/c", "/O2", "/Z7", "/EHsc"])
        .arg(format!("/d1trimfile:{}\\", a.display()))
        .arg("/Brepro")
        .arg("src\\m.cpp")
        .arg("/Fom.obj")
        .current_dir(&a);
    run(&mut c, "compile in A");

    // Hand that object to checkout B and link it there. This is exactly what a
    // cross-checkout cache hit does.
    fs::copy(a.join("m.obj"), b.join("m.obj")).unwrap();
    let mut l = Command::new(&link);
    l.args([
        "/nologo",
        "/DEBUG",
        "/INCREMENTAL:NO",
        "/OUT:m.exe",
        "m.obj",
    ])
    .current_dir(&b);
    run(&mut l, "link in B");

    // Only our own translation unit. A PDB also names the sources of whatever
    // it statically linked -- the CRT contributes paths like
    // `minkernel\crts\ucrt\src\...`, which are relative but resolve against
    // Microsoft's build tree, not this one. Matching loosely on a suffix picks
    // those up too: `initterm.cpp` ends with `m.cpp`.
    let files = pdb_source_files(&b.join("m.pdb"));
    let ours: Vec<_> = files
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("src\\m.cpp"))
        .collect();
    // Only the plausibly-ours entries on failure. A PDB names every SDK and CRT
    // header it pulled in, and dumping all of them buries the one line that
    // says what went wrong under twenty kilobytes that never varies.
    let candidates: Vec<_> = files
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| n.ends_with("m.cpp") && !n.contains("\\crt"))
        .take(5)
        .collect();
    assert!(
        !ours.is_empty(),
        "the PDB should name the source as `src\\m.cpp`, relative; nearest entries \
         were {candidates:?} (of {} files total)",
        files.len()
    );

    for (name, checksum) in ours {
        // It must not point into the checkout the object came from.
        assert!(
            !name.contains("wt-aaa"),
            "PDB references the originating checkout: {name}"
        );
        assert!(
            !Path::new(name).is_absolute(),
            "PDB records an absolute source path, so it cannot resolve in another \
             checkout: {name}"
        );

        // It must resolve against *this* checkout...
        let local = b.join(name);
        assert!(
            local.is_file(),
            "PDB path {name} does not resolve inside the checkout using it \
             (looked for {})",
            local.display()
        );

        // ...and be the same bytes the code was compiled from. This is the part
        // that a human stepping through a debugger would have to take on trust,
        // and it is what stops a stale or subtly different file being shown
        // beside the right line numbers.
        assert!(
            !checksum.is_none(),
            "no source checksum recorded for {name}; the comparison below would be \
             vacuous"
        );
        assert!(
            checksum.matches(&fs::read(&local).unwrap()),
            "source {name} in this checkout does not match what the cached object \
             was compiled from (recorded {checksum:?})"
        );
    }

    // The linked program must also actually work.
    let out = Command::new(b.join("m.exe"))
        .current_dir(&b)
        .output()
        .expect("run m.exe");
    assert_eq!(out.status.code(), Some(0), "linked program should return 0");
}
