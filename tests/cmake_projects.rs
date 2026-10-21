// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! The CMake modules' functions that need a configured project:
//! `sccion_wrap_automoc` and `sccion_cached_tool`.
//!
//! The projects have no languages, so no compiler is involved. The tool the
//! cache stands in front of is a CMake script that writes its output and
//! counts how often it ran, which is how a test tells a build from a restore.
//! Skipped, with a note, where there is no `cmake` (or, for the trees git
//! lists, no `git`).

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const COMPILER_CACHE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/cmake/SccionCompilerCache.cmake"
);
const TOOL_CACHE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/cmake/SccionToolCache.cmake");

fn have(program: &str) -> bool {
    let found = which::which(program).is_ok();
    if !found {
        eprintln!("no {program} on PATH; skipping");
    }
    found
}

fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

/// What CMake and the build said, with line wrapping undone.
fn said(out: &Output) -> String {
    text(out).split_whitespace().collect::<Vec<_>>().join(" ")
}

fn cmake(args: &[&str], vars: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new("cmake");
    cmd.args(args)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("SCCION_TOOL_CACHE_DIR");
    for (k, v) in vars {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

// ---------------------------------------------------------------------------
// sccion_wrap_automoc

fn automoc_project(dir: &Path, launcher: &Path, real_moc: &Path) -> Output {
    let src = dir.join("src");
    fs::create_dir_all(&src).unwrap();
    fs::write(
        src.join("CMakeLists.txt"),
        format!(
            "cmake_minimum_required(VERSION 3.17)\n\
             project(t NONE)\n\
             include(\"{COMPILER_CACHE}\")\n\
             sccion_compiler_cache()\n\
             add_executable(Fake::moc IMPORTED)\n\
             set_target_properties(Fake::moc PROPERTIES IMPORTED_LOCATION \"{moc}\")\n\
             add_custom_target(app)\n\
             add_custom_target(lib)\n\
             sccion_wrap_automoc(app lib MOC Fake::moc)\n\
             get_target_property(app_moc app AUTOMOC_EXECUTABLE)\n\
             get_target_property(lib_moc lib AUTOMOC_EXECUTABLE)\n\
             message(STATUS \"AUTOMOC app=[${{app_moc}}] lib=[${{lib_moc}}]\")\n",
            moc = slash(real_moc)
        ),
    )
    .unwrap();
    let build = dir.join("build");
    cmake(
        &[
            "-S",
            &slash(&src),
            "-B",
            &slash(&build),
            &format!("-DCMAKE_CXX_COMPILER_LAUNCHER={}", slash(launcher)),
        ],
        &[("XDG_DATA_HOME", &slash(&dir.join("data")))],
    )
}

fn automoc_of(out: &Output, target: &str) -> String {
    let all = said(out);
    let start = all
        .find(&format!("{target}=["))
        .unwrap_or_else(|| panic!("{all}"))
        + target.len()
        + 2;
    all[start..].split(']').next().unwrap().to_owned()
}

/// The documented moc setup in one call: a copy of sccion named moc in the
/// build directory, the wraps file beside it naming the real moc, and the copy
/// as each target's AUTOMOC_EXECUTABLE.
#[test]
fn automoc_runs_moc_through_sccion() {
    if !have("cmake") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let sccion = bin.join(format!("sccion{}", env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_sccion"), &sccion).unwrap();
    let real_moc = dir
        .path()
        .join("qt")
        .join(format!("moc{}", env::consts::EXE_SUFFIX));
    fs::create_dir_all(real_moc.parent().unwrap()).unwrap();
    fs::write(&real_moc, b"").unwrap();

    let out = automoc_project(dir.path(), &sccion, &real_moc);
    assert!(out.status.success(), "{}", text(&out));
    let standin = dir
        .path()
        .join("build/sccion-wraps")
        .join(format!("moc{}", env::consts::EXE_SUFFIX));
    assert_eq!(automoc_of(&out, "app"), slash(&standin), "{}", text(&out));
    assert_eq!(automoc_of(&out, "lib"), slash(&standin), "{}", text(&out));
    assert_eq!(
        fs::read(&standin).unwrap(),
        fs::read(&sccion).unwrap(),
        "the stand-in is this sccion"
    );
    let wraps =
        fs::read_to_string(dir.path().join("build/sccion-wraps/sccion-wraps.toml")).unwrap();
    assert_eq!(wraps, format!("[wraps]\nmoc = '{}'\n", slash(&real_moc)));
}

/// sccache and ccache do not cache moc, so with either AUTOMOC is left as it
/// was.
#[test]
fn automoc_is_left_alone_without_sccion() {
    if !have("cmake") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let ccache = dir
        .path()
        .join(format!("ccache{}", env::consts::EXE_SUFFIX));
    fs::write(&ccache, b"").unwrap();
    let out = automoc_project(dir.path(), &ccache, &dir.path().join("moc"));
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        automoc_of(&out, "app"),
        "app_moc-NOTFOUND",
        "{}",
        text(&out)
    );
    assert!(!dir.path().join("build/sccion-wraps").exists());
}

// ---------------------------------------------------------------------------
// sccion_cached_tool

/// A checkout of a project with one cached tool: `pkg/` is its source tree
/// and `pkg.lock` its lockfile. The tool's build appends a line to `counter`.
struct Checkout {
    src: PathBuf,
    build: PathBuf,
}

struct ToolWorld {
    dir: tempfile::TempDir,
}

impl ToolWorld {
    fn new(with_git: bool) -> Option<ToolWorld> {
        if !have("cmake") || (with_git && !have("git")) {
            return None;
        }
        Some(ToolWorld {
            dir: tempfile::tempdir().unwrap(),
        })
    }

    fn counter(&self) -> PathBuf {
        self.dir.path().join("builds.txt")
    }

    fn store(&self) -> PathBuf {
        self.dir.path().join("store")
    }

    fn builds(&self) -> usize {
        fs::read_to_string(self.counter())
            .map(|t| t.lines().count())
            .unwrap_or(0)
    }

    fn checkout(&self, name: &str, git: bool) -> Checkout {
        let src = self.dir.path().join(name);
        fs::create_dir_all(src.join("pkg")).unwrap();
        fs::write(src.join("pkg/input.txt"), "one\n").unwrap();
        fs::write(src.join("pkg.lock"), "pinned 1\n").unwrap();
        fs::write(
            src.join("build_tool.cmake"),
            "file(APPEND \"${COUNTER}\" \"built\\n\")\n\
             if(DEFINED ENV{SLOW_TOOL})\n\
               execute_process(COMMAND \"${CMAKE_COMMAND}\" -E sleep 3)\n\
             endif()\n\
             file(READ \"${IN}\" text)\n\
             file(WRITE \"${OUT}\" \"built from: ${text}\")\n",
        )
        .unwrap();
        fs::write(
            src.join("CMakeLists.txt"),
            format!(
                "cmake_minimum_required(VERSION 3.21)\n\
                 project(t NONE)\n\
                 include(\"{TOOL_CACHE}\")\n\
                 sccion_cached_tool(tool ALL\n\
                   OUTPUTS \"${{CMAKE_BINARY_DIR}}/out/tool.txt\"\n\
                   COMMAND \"${{CMAKE_COMMAND}}\" \"-DIN=${{CMAKE_SOURCE_DIR}}/pkg/input.txt\"\n\
                     \"-DOUT=${{CMAKE_BINARY_DIR}}/out/tool.txt\" \"-DCOUNTER=${{COUNTER}}\"\n\
                     -P \"${{CMAKE_SOURCE_DIR}}/build_tool.cmake\"\n\
                   SOURCE_TREES pkg\n\
                   INPUT_FILES pkg.lock\n\
                   TOOLS \"\\\"${{CMAKE_COMMAND}}\\\" --version\"\n\
                   ENVIRONMENT TOOL_FLAVOUR)\n"
            ),
        )
        .unwrap();
        if git {
            for args in [
                &["init", "-q"][..],
                &["add", "-A"],
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-q",
                    "-m",
                    "x",
                ],
            ] {
                assert!(
                    Command::new("git")
                        .args(args)
                        .current_dir(&src)
                        .status()
                        .unwrap()
                        .success()
                );
            }
        }
        let build = self.dir.path().join(format!("{name}-build"));
        Checkout { src, build }
    }

    fn configure(&self, c: &Checkout, defines: &[&str]) {
        let mut args = vec![
            "-S".to_owned(),
            slash(&c.src),
            "-B".to_owned(),
            slash(&c.build),
            format!("-DCOUNTER={}", slash(&self.counter())),
        ];
        args.extend(defines.iter().map(|d| d.to_string()));
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = cmake(&args, &[]);
        assert!(out.status.success(), "{}", text(&out));
    }

    fn build(&self, c: &Checkout, vars: &[(&str, &str)]) -> Output {
        let store = slash(&self.store());
        let mut all = vec![("SCCION_TOOL_CACHE_DIR", store.as_str())];
        all.extend_from_slice(vars);
        let out = cmake(&["--build", &slash(&c.build)], &all);
        assert!(out.status.success(), "{}", text(&out));
        out
    }

    fn output(&self, c: &Checkout) -> String {
        fs::read_to_string(c.build.join("out/tool.txt")).unwrap()
    }
}

#[test]
fn a_second_checkout_restores_instead_of_building() {
    let Some(w) = ToolWorld::new(true) else {
        return;
    };
    let a = w.checkout("a", true);
    w.configure(&a, &[]);
    let out = w.build(&a, &[]);
    assert_eq!(w.builds(), 1);
    assert!(said(&out).contains("miss tool"), "{}", text(&out));
    assert_eq!(w.output(&a), "built from: one\n");

    // Nothing changed: nothing built, nothing restored, nothing said.
    let out = w.build(&a, &[]);
    assert_eq!(w.builds(), 1);
    assert!(!said(&out).contains("tool cache:"), "{}", text(&out));

    let b = w.checkout("b", true);
    w.configure(&b, &[]);
    let out = w.build(&b, &[]);
    assert_eq!(w.builds(), 1, "{}\n{}", text(&out), key_diff(&a, &b));
    assert!(said(&out).contains("hit tool"), "{}", text(&out));
    assert_eq!(w.output(&b), "built from: one\n");
}

/// The key lines one checkout has and the other does not, for a failure to
/// show.
fn key_diff(a: &Checkout, b: &Checkout) -> String {
    let read = |c: &Checkout| {
        fs::read_to_string(c.build.join("sccion-tools/tool/key.txt")).unwrap_or_default()
    };
    let (ka, kb) = (read(a), read(b));
    let only = |x: &str, y: &str| {
        x.lines()
            .filter(|l| !y.lines().any(|m| m == *l))
            .map(|l| format!("  {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "only in a:\n{}\nonly in b:\n{}",
        only(&ka, &kb),
        only(&kb, &ka)
    )
}

#[test]
fn each_part_of_the_key_is_seen() {
    let Some(w) = ToolWorld::new(true) else {
        return;
    };
    let a = w.checkout("a", true);
    w.configure(&a, &[]);
    w.build(&a, &[]);
    assert_eq!(w.builds(), 1);

    // A modified source, and the miss says which.
    fs::write(a.src.join("pkg/input.txt"), "two\n").unwrap();
    let out = w.build(&a, &[]);
    assert_eq!(w.builds(), 2);
    assert!(
        said(&out).contains("changed: sources (<SRC>/pkg/input.txt)"),
        "{}",
        text(&out)
    );
    assert_eq!(w.output(&a), "built from: two\n");

    // Back as it was: the first entry is still there.
    fs::write(a.src.join("pkg/input.txt"), "one\n").unwrap();
    let out = w.build(&a, &[]);
    assert_eq!(w.builds(), 2);
    assert!(said(&out).contains("hit tool"), "{}", text(&out));

    // An untracked file in the tree: SwiftPM, say, would compile it.
    fs::write(a.src.join("pkg/new.txt"), "new\n").unwrap();
    w.build(&a, &[]);
    assert_eq!(w.builds(), 3);
    fs::remove_file(a.src.join("pkg/new.txt")).unwrap();

    // The lockfile.
    fs::write(a.src.join("pkg.lock"), "pinned 2\n").unwrap();
    w.build(&a, &[]);
    assert_eq!(w.builds(), 4);
    fs::write(a.src.join("pkg.lock"), "pinned 1\n").unwrap();

    // An environment variable the tool listed.
    let out = w.build(&a, &[("TOOL_FLAVOUR", "sharp")]);
    assert_eq!(w.builds(), 5);
    assert!(said(&out).contains("changed: env"), "{}", text(&out));

    // Build output inside the tree is not an input, if git ignores it.
    fs::write(a.src.join(".gitignore"), "pkg/.build/\n").unwrap();
    w.build(&a, &[]);
    let before = w.builds();
    fs::create_dir_all(a.src.join("pkg/.build")).unwrap();
    fs::write(a.src.join("pkg/.build/artifact"), "x").unwrap();
    w.build(&a, &[]);
    assert_eq!(w.builds(), before);
}

/// A tracked symlink to a directory that the build points elsewhere in each
/// checkout -- into its own build directory -- does not split the key between
/// checkouts. Sourcetrail commits one, and every checkout but the first missed.
#[cfg(unix)]
#[test]
fn a_rewritten_symlink_to_a_directory_does_not_split_checkouts() {
    let Some(w) = ToolWorld::new(true) else {
        return;
    };
    let make = |name: &str| {
        let c = w.checkout(name, false);
        std::os::unix::fs::symlink("/nowhere", c.src.join("pkg/generated")).unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "-A"],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "x",
            ],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&c.src)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        // What the build does: point it at this checkout's own directory.
        let own = w.dir.path().join(format!("{name}-generated"));
        fs::create_dir_all(&own).unwrap();
        fs::remove_file(c.src.join("pkg/generated")).unwrap();
        std::os::unix::fs::symlink(&own, c.src.join("pkg/generated")).unwrap();
        c
    };
    let a = make("a");
    w.configure(&a, &[]);
    w.build(&a, &[]);
    let b = make("b");
    w.configure(&b, &[]);
    let out = w.build(&b, &[]);
    assert_eq!(w.builds(), 1, "{}\n{}", text(&out), key_diff(&a, &b));
}

/// Two checkouts after the same key at once build it once: the second waits
/// for the first and restores what it stored.
#[test]
fn concurrent_builds_of_one_key_build_once() {
    let Some(w) = ToolWorld::new(true) else {
        return;
    };
    let (a, b) = (w.checkout("a", true), w.checkout("b", true));
    w.configure(&a, &[]);
    w.configure(&b, &[]);
    let store = slash(&w.store());
    let spawn = |c: &Checkout| {
        Command::new("cmake")
            .args(["--build", &slash(&c.build)])
            .env("SCCION_TOOL_CACHE_DIR", &store)
            .env("SLOW_TOOL", "1")
            .output()
    };
    let (ra, rb) = std::thread::scope(|s| {
        let ha = s.spawn(|| spawn(&a).unwrap());
        let hb = s.spawn(|| spawn(&b).unwrap());
        (ha.join().unwrap(), hb.join().unwrap())
    });
    assert!(
        ra.status.success() && rb.status.success(),
        "{}\n{}",
        text(&ra),
        text(&rb)
    );
    assert_eq!(w.builds(), 1, "{}\n{}", text(&ra), text(&rb));
    assert_eq!(w.output(&a), w.output(&b));
    let hits = [&ra, &rb]
        .iter()
        .filter(|o| said(o).contains("hit tool"))
        .count();
    assert_eq!(hits, 1, "{}\n{}", text(&ra), text(&rb));
}

#[test]
fn off_builds_every_time() {
    let Some(w) = ToolWorld::new(true) else {
        return;
    };
    let a = w.checkout("a", true);
    w.configure(&a, &[]);
    w.build(&a, &[]);
    let b = w.checkout("b", true);
    w.configure(&b, &["-DSCCION_TOOL_CACHE=OFF"]);
    w.build(&b, &[]);
    assert_eq!(w.builds(), 2);
}

/// An entry whose stored file no longer matches what was recorded is not
/// served: it is built again and replaced.
#[test]
fn a_damaged_entry_is_rebuilt() {
    let Some(w) = ToolWorld::new(true) else {
        return;
    };
    let a = w.checkout("a", true);
    w.configure(&a, &[]);
    w.build(&a, &[]);
    let tool_dir = w.store().join("tool");
    for e in fs::read_dir(&tool_dir).unwrap() {
        let files = e.unwrap().path().join("files");
        if files.is_dir() {
            for f in fs::read_dir(files).unwrap() {
                fs::write(f.unwrap().path(), "tampered").unwrap();
            }
        }
    }
    let b = w.checkout("b", true);
    w.configure(&b, &[]);
    let out = w.build(&b, &[]);
    assert_eq!(w.builds(), 2);
    assert!(said(&out).contains("is damaged"), "{}", text(&out));
    assert_eq!(w.output(&b), "built from: one\n");
}

/// Outside a git checkout the tree is walked instead.
#[test]
fn trees_outside_git_are_walked() {
    let Some(w) = ToolWorld::new(false) else {
        return;
    };
    let a = w.checkout("a", false);
    w.configure(&a, &[]);
    w.build(&a, &[]);
    let b = w.checkout("b", false);
    w.configure(&b, &[]);
    let out = w.build(&b, &[]);
    assert_eq!(w.builds(), 1, "{}", text(&out));
    fs::write(b.src.join("pkg/input.txt"), "two\n").unwrap();
    w.build(&b, &[]);
    assert_eq!(w.builds(), 2);
    assert_eq!(w.output(&b), "built from: two\n");
}

/// A key names paths in the checkout relative to it, and only at a path
/// boundary: a checkout at `/w/b` must not turn `/w/builds.txt` into
/// `<SRC>uilds.txt`, which made a second checkout miss.
#[test]
fn checkout_roots_are_replaced_only_at_path_boundaries() {
    if !have("cmake") {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("t.cmake");
    fs::write(
        &script,
        format!(
            "include(\"{TOOL_CACHE}\")\n\
             set(SCCION_TOOL_SOURCE_ROOT \"/w/v1.2+b\")\n\
             set(SCCION_TOOL_BINARY_ROOT \"/w/v1.2+b/.build\")\n\
             foreach(t IN ITEMS \"/w/v1.2+b/pkg/x\" \"/w/v1.2+bx/y\" \"/w/v1x2+b/z\" \
             \"-DIN=/w/v1.2+b\" \"/w/v1.2+b/.build/out\" \"/w/v1.2+b;/w/v1.2+b/.build\")\n\
               _sccion_tool_portable(r \"${{t}}\")\n\
               message(\"[${{t}}] -> [${{r}}]\")\n\
             endforeach()\n"
        ),
    )
    .unwrap();
    let out = cmake(&["-P", &slash(&script)], &[]);
    assert!(out.status.success(), "{}", text(&out));
    for expected in [
        "[/w/v1.2+b/pkg/x] -> [<SRC>/pkg/x]",
        "[/w/v1.2+bx/y] -> [/w/v1.2+bx/y]",
        "[/w/v1x2+b/z] -> [/w/v1x2+b/z]",
        "[-DIN=/w/v1.2+b] -> [-DIN=<SRC>]",
        "[/w/v1.2+b/.build/out] -> [<BIN>/out]",
        "[/w/v1.2+b;/w/v1.2+b/.build] -> [<SRC>;<BIN>]",
    ] {
        assert!(text(&out).contains(expected), "{expected}\n{}", text(&out));
    }
}
