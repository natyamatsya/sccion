// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! `cmake/SccionCompilerCache.cmake`, checked by running a CMake script that
//! includes it.
//!
//! A script costs a fraction of a second and needs no compiler or generator.
//! Programs are looked for only in a directory of the
//! test's own -- fake ones, and a copy of this sccion where one has to run --
//! so whatever compiler caches the machine has installed cannot answer for
//! them. Skipped, with a note, where there is no `cmake`.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

const MODULE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/cmake/SccionCompilerCache.cmake"
);

struct Project {
    dir: tempfile::TempDir,
}

impl Project {
    fn new() -> Option<Project> {
        if which::which("cmake").is_err() {
            eprintln!("no cmake on PATH; skipping");
            return None;
        }
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("bin")).unwrap();
        Some(Project { dir })
    }

    fn bin(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    /// A program the module can find, named `stem`. Never run.
    fn fake(&self, stem: &str) -> PathBuf {
        let path = self
            .bin()
            .join(format!("{stem}{}", env::consts::EXE_SUFFIX));
        fs::write(&path, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// This sccion, under its own name: the module runs it to ask where its
    /// copy of the module is.
    fn sccion(&self) -> PathBuf {
        let path = self
            .bin()
            .join(format!("sccion{}", env::consts::EXE_SUFFIX));
        fs::copy(env!("CARGO_BIN_EXE_sccion"), &path).unwrap();
        path
    }

    /// Run a CMake script that includes `module` and runs `body`, with
    /// `defines` before it and `vars` in the environment.
    ///
    /// Script mode rather than a configured project: it needs no generator,
    /// so no build program has to be findable while every system search path
    /// is turned off.
    fn configure(
        &self,
        module: &Path,
        body: &str,
        defines: &[&str],
        vars: &[(&str, &str)],
    ) -> Output {
        let module = module.to_string_lossy().replace('\\', "/");
        let script = self.dir.path().join("t.cmake");
        fs::write(
            &script,
            format!(
                "include(\"{module}\")\n\
                 {body}\n\
                 message(STATUS \"RESULT launcher=[${{CMAKE_CXX_COMPILER_LAUNCHER}}] \
                 rustc=[${{SCCION_RUSTC_WRAPPER}}]\")\n"
            ),
        )
        .unwrap();
        let data = self.dir.path().join("data");
        let mut cmd = std::process::Command::new("cmake");
        cmd.arg(format!(
            "-DCMAKE_PROGRAM_PATH={}",
            self.bin().to_string_lossy().replace('\\', "/")
        ))
        .arg("-DCMAKE_FIND_USE_SYSTEM_ENVIRONMENT_PATH=OFF")
        .arg("-DCMAKE_FIND_USE_CMAKE_SYSTEM_PATH=OFF")
        .args(defines)
        .arg("-P")
        .arg(&script)
        .env_remove("RUSTC_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        // Where a copied sccion writes its module: the test's, not the
        // machine's.
        .env("XDG_DATA_HOME", &data)
        .env("APPDATA", &data);
        for (k, v) in vars {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr)
}

/// What CMake said, with its line wrapping undone: it breaks long messages
/// wherever they reach its line width.
fn said(out: &Output) -> String {
    text(out).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The launcher and wrapper a configure ended with.
fn result(out: &Output) -> (String, String) {
    let all = text(out);
    let line = all
        .lines()
        .find(|l| l.contains("RESULT launcher="))
        .unwrap_or_else(|| panic!("no result line:\n{all}"));
    let field = |name: &str| {
        let start = line.find(&format!("{name}=[")).unwrap() + name.len() + 2;
        line[start..].split(']').next().unwrap().to_owned()
    };
    (field("launcher"), field("rustc"))
}

fn same(a: &str, b: &Path) -> bool {
    Path::new(a).canonicalize().ok() == b.canonicalize().ok()
}

#[test]
fn sccion_is_preferred_and_takes_rustc_too() {
    let Some(p) = Project::new() else { return };
    p.fake("ccache");
    p.fake("sccache");
    let sccion = p.sccion();
    let out = p.configure(Path::new(MODULE), "sccion_compiler_cache()", &[], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let (launcher, rustc) = result(&out);
    assert!(same(&launcher, &sccion), "{}", text(&out));
    assert!(same(&rustc, &sccion), "{}", text(&out));
    // Its own module, so nothing to say about another one.
    assert!(!said(&out).contains("ships a different"), "{}", text(&out));
}

/// The fallback: a contributor with only ccache gets ccache for C and C++,
/// and cargo is left alone, since ccache cannot wrap rustc.
#[test]
fn ccache_is_used_but_not_for_rustc() {
    let Some(p) = Project::new() else { return };
    let ccache = p.fake("ccache");
    let out = p.configure(Path::new(MODULE), "sccion_compiler_cache()", &[], &[]);
    assert!(out.status.success(), "{}", text(&out));
    let (launcher, rustc) = result(&out);
    assert!(same(&launcher, &ccache), "{}", text(&out));
    assert_eq!(rustc, "");
}

#[test]
fn sccache_takes_rustc_too() {
    let Some(p) = Project::new() else { return };
    let sccache = p.fake("sccache");
    let out = p.configure(Path::new(MODULE), "sccion_compiler_cache()", &[], &[]);
    let (launcher, rustc) = result(&out);
    assert!(
        same(&launcher, &sccache) && same(&rustc, &sccache),
        "{}",
        text(&out)
    );
}

#[test]
fn none_installed_builds_uncached_unless_one_is_required() {
    let Some(p) = Project::new() else { return };
    let out = p.configure(Path::new(MODULE), "sccion_compiler_cache()", &[], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(result(&out), (String::new(), String::new()));
    assert!(said(&out).contains("none found"), "{}", text(&out));

    let out = p.configure(
        Path::new(MODULE),
        "sccion_compiler_cache()",
        &["-DSCCION_COMPILER_CACHE=ON"],
        &[],
    );
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        said(&out).contains("SCCION_COMPILER_CACHE=ON but no compiler cache"),
        "{}",
        text(&out)
    );
}

/// A project keeps its own option name: Sourcetrail's CI passes
/// `-DSOURCETRAIL_COMPILER_CACHE=ON`.
#[test]
fn the_option_can_be_named_by_the_project() {
    let Some(p) = Project::new() else { return };
    p.fake("ccache");
    let body = "sccion_compiler_cache(OPTION PROJ_CACHE)";
    let out = p.configure(Path::new(MODULE), body, &["-DPROJ_CACHE=OFF"], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(result(&out).0, "");
    assert!(
        said(&out).contains("disabled (PROJ_CACHE=OFF)"),
        "{}",
        text(&out)
    );
}

/// CI's sccion action exports RUSTC_WRAPPER itself; cargo keeps that one.
#[test]
fn a_rustc_wrapper_from_the_environment_is_kept() {
    let Some(p) = Project::new() else { return };
    let sccion = p.sccion();
    let out = p.configure(
        Path::new(MODULE),
        "sccion_compiler_cache()",
        &[],
        &[("RUSTC_WRAPPER", "elsewhere")],
    );
    let (launcher, rustc) = result(&out);
    assert!(same(&launcher, &sccion), "{}", text(&out));
    assert_eq!(rustc, "");
    assert!(
        said(&out).contains("cargo keeps the RUSTC_WRAPPER"),
        "{}",
        text(&out)
    );
}

/// A launcher path that no longer exists -- left in a build directory by a
/// checkout that is gone -- is replaced, not used.
#[test]
fn a_launcher_that_no_longer_exists_is_replaced() {
    let Some(p) = Project::new() else { return };
    let ccache = p.fake("ccache");
    let gone = p.dir.path().join("gone").join("sccion");
    let out = p.configure(
        Path::new(MODULE),
        "sccion_compiler_cache()",
        &[&format!(
            "-DCMAKE_CXX_COMPILER_LAUNCHER={}",
            gone.to_string_lossy().replace('\\', "/")
        )],
        &[],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(said(&out).contains("does not exist"), "{}", text(&out));
    assert!(same(&result(&out).0, &ccache), "{}", text(&out));
}

/// A project whose committed copy differs from the installed sccion's hears
/// about it, and where to find the other one.
#[test]
fn a_different_installed_module_is_pointed_out() {
    let Some(p) = Project::new() else { return };
    p.sccion();
    let older = p.dir.path().join("SccionCompilerCache.cmake");
    fs::write(
        &older,
        fs::read_to_string(MODULE).unwrap() + "# a project's older copy\n",
    )
    .unwrap();
    let out = p.configure(&older, "sccion_compiler_cache()", &[], &[]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        said(&out).contains("ships a different SccionCompilerCache.cmake"),
        "{}",
        text(&out)
    );
}

#[test]
fn rust_crates_need_corrosion_only_when_there_is_a_wrapper() {
    let Some(p) = Project::new() else { return };
    // No cache that takes rustc: nothing to set, so no Corrosion needed.
    p.fake("ccache");
    let body = "sccion_compiler_cache()\nsccion_cache_rust_crates(some_crate)";
    let out = p.configure(Path::new(MODULE), body, &[], &[]);
    assert!(out.status.success(), "{}", text(&out));

    p.fake("sccache");
    let out = p.configure(Path::new(MODULE), body, &[], &[]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        said(&out).contains("include Corrosion before calling it"),
        "{}",
        text(&out)
    );
}
