// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! A copy of sccion under a tool's name, where the PATH has no such tool.
//!
//! It used to wrap itself: every compile, and the probe that detects the
//! compiler, started it again under the same name, at about 100 new processes
//! a second. It has to fail at once, and say why, on stderr -- its stdout is
//! the wrapped tool's.

use assert_cmd::Command;
use predicates::prelude::*;
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;

#[test]
fn masquerade_without_the_real_tool_fails_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir
        .path()
        .join(format!("sccion-masquerade-tool{}", env::consts::EXE_SUFFIX));
    let sccion = Path::new(env!("CARGO_BIN_EXE_sccion"));
    if fs::hard_link(sccion, &fake).is_err() {
        fs::copy(sccion, &fake).unwrap();
    }

    Command::new(&fake)
        .arg("--version")
        .env("PATH", dir.path())
        .env_remove("SCCION_MASQUERADING")
        // Generous for a slow CI host, and short of what a loop would spawn.
        .timeout(Duration::from_secs(30))
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "but the PATH has no `sccion-masquerade-tool",
        ));
}

#[test]
fn wraps_file_beside_the_copy_is_read() {
    // The copy is started by its full path, the way a build system starts it,
    // and must find the file beside itself rather than in its working
    // directory. Naming a file that does not exist proves the file was read
    // without needing a real tool to run.
    let dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let fake = dir
        .path()
        .join(format!("sccion-masquerade-tool{}", env::consts::EXE_SUFFIX));
    let sccion = Path::new(env!("CARGO_BIN_EXE_sccion"));
    if fs::hard_link(sccion, &fake).is_err() {
        fs::copy(sccion, &fake).unwrap();
    }
    fs::write(
        dir.path().join("sccion-wraps.toml"),
        "[wraps]\nsccion-masquerade-tool = 'no-such-tool'\n",
    )
    .unwrap();

    Command::new(&fake)
        .arg("--version")
        .current_dir(elsewhere.path())
        .env("PATH", elsewhere.path())
        .env_remove("SCCION_MASQUERADING")
        .timeout(Duration::from_secs(30))
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("there is no file at"));
}
