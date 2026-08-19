// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Who asked for a compilation.
//!
//! One sccache daemon serves every build on the machine, and when several are
//! running at once its statistics are a sum with no addends. "2557 hits" does
//! not say whether one caller got all of them and another none, which is the
//! first question anyone asks when several concurrent builds share a cache.
//!
//! The identity is read from the environment of **each compilation**, not the
//! daemon's -- the daemon is started by whichever build got there first and
//! outlives it, so anything read from its environment describes a shell that
//! may be long gone.
//!
//! When nothing says, the checkout root stands in. That fallback is the useful
//! half: concurrent builds are usually concurrent *checkouts*, so grouping by
//! checkout gives correct per-caller numbers without the callers having to
//! cooperate, or even know sccache is there.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// What a caller sets to name itself.
pub const AGENT_ID_VAR: &str = "SCCION_AGENT_ID";

/// Who to attribute a compilation in `cwd` to.
///
/// **Deliberately not basedir-stripped.** Everywhere else a recorded path is
/// made relative to its basedir so that two checkouts of one project produce
/// comparable records. Here the opposite is wanted: the whole purpose is to
/// tell two checkouts apart, and stripping would collapse every one of them to
/// the same empty string -- precisely in the case this exists to serve, since
/// a committed `.sccion.toml` makes the checkout root a basedir.
pub fn identity(env_vars: &[(OsString, OsString)], cwd: &Path) -> Option<String> {
    let named = env_vars
        .iter()
        .find(|(k, _)| crate::envvar::matches(k, AGENT_ID_VAR))
        .map(|(_, v)| v.to_string_lossy().into_owned())
        .filter(|v| !v.is_empty());
    named.or_else(|| workspace(cwd))
}

/// The checkout a compilation in `cwd` belongs to, absolute, for the same
/// reason as above.
pub fn workspace(cwd: &Path) -> Option<String> {
    crate::projectconf::discover(cwd)
        .vcs_root
        .as_deref()
        .map(canonical)
}

/// One spelling of a path, whatever spelling the caller happened to use.
///
/// Every consumer of this module compares the results as *strings*: as an
/// identity a second spelling splits one checkout into two buckets, and as a
/// filter it matches nothing at all. Three ways a caller reaches one directory
/// by more than one name, and all three have been seen:
///
/// - **Separators.** Windows accepts both, and which arrives here depends on
///   how the compilation was invoked -- a build system passes `D:\x`, someone
///   asking a question about it passes `D:/x`. `afff1eb` fixed this exact shape
///   once already, where preprocessor cache keys were built with forward
///   slashes and the on-startup index rebuild used backslashes.
/// - **Case**, on a volume that folds it. Measured: one checkout reached as
///   `/Users/.../Sourcetrail-TS` and `/users/.../sourcetrail-ts` registered as
///   two workspaces, so `--show-stats` split one build's figures in half.
/// - **Symlinks.** `/tmp/w` and `/private/tmp/w` are one directory on macOS,
///   and a worktree reached through a convenience link is the same tree as the
///   one reached through its real path.
///
/// [`canonical_path`] answers all three by asking the filesystem, which is the
/// only thing that knows. Comparing `Path`s instead would fix the separators
/// and neither of the others, and would not fix the identity at all -- that has
/// to be one string.
pub fn canonical(path: &Path) -> String {
    let resolved = canonical_path(path);
    let s = resolved.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
}

/// The filesystem's own name for a directory.
///
/// `fs::canonicalize` is `realpath` on Unix and `GetFinalPathNameByHandle` on
/// Windows, and both answer with the name as *stored* -- which is what makes
/// this correct case on a folding volume without lowercasing anything. A
/// lowercased identity would be one string per directory too, and it would be a
/// lie: `--show-stats` and `describe_workspace` print this, and printing a
/// spelling that is not on disk sends someone `cd`-ing to a path their shell
/// completes differently.
///
/// Falls back to the path as given when the call fails, which is what happens
/// for a checkout that has since been deleted. A handle outlives what it names,
/// and reporting the last known name beats reporting nothing.
///
/// Not cached, unlike [`crate::volume::folds_case`]. What a volume does with
/// case is fixed while it is mounted; what a path resolves to is not -- a
/// directory can be renamed or replaced under a running daemon, and a cached
/// answer would then name a tree that is no longer there.
///
/// Measured at 10.9 us per call on a warm cache, for a path six components
/// deep. That is not free -- it is the same order as the `projectconf::discover`
/// walk it sits next to, not a rounding error against it -- but a compilation
/// is four orders of magnitude larger, and the pair of calls on the compile
/// path costs around 0.02% of one.
pub fn canonical_path(path: &Path) -> PathBuf {
    let Ok(resolved) = std::fs::canonicalize(path) else {
        return path.to_path_buf();
    };
    strip_verbatim(resolved)
}

/// Windows canonicalizes to the verbatim form, which is an implementation
/// detail of the call and not a path anyone typed or wants to read.
#[cfg(windows)]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => match s.strip_prefix(r"\\?\") {
            Some(rest) => PathBuf::from(rest),
            None => path,
        },
    }
}

#[cfg(not(windows))]
fn strip_verbatim(path: PathBuf) -> PathBuf {
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .collect()
    }

    #[test]
    fn what_the_caller_says_wins() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(
            identity(&env(&[(AGENT_ID_VAR, "agent-7")]), t.path()),
            Some("agent-7".into())
        );
    }

    /// The point of the fallback: N agents in N worktrees are attributed
    /// correctly without any of them having been told to set anything.
    #[test]
    fn without_one_the_checkout_stands_in() {
        let t = tempfile::tempdir().unwrap();
        let wt = t.path().join("agent-7");
        let deep = wt.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();
        // A committed project file, which is the normal case and the one that
        // makes the checkout root a basedir.
        std::fs::write(wt.join(crate::projectconf::PROJECT_CONF_NAME), "").unwrap();

        let got = identity(&[], &deep).expect("a checkout is an identity");
        assert!(
            got.ends_with("agent-7"),
            "the identity has to distinguish checkouts, so it must not be \
             basedir-stripped: {got:?}"
        );
    }

    /// The same checkout reached by two spellings is one workspace.
    ///
    /// A build system hands over `D:\wt\build` and someone asking a question
    /// about it hands over `D:/wt/src`. Windows accepts both, every consumer
    /// compares the answers as strings, and the two spellings had produced two
    /// different strings -- so a query scoped to the workspace matched none of
    /// its own records, and reported "no misses" for a build that had just
    /// missed hundreds of times. Found by running two agents against a real
    /// build; the unit tests all used one spelling and were happy.
    #[test]
    fn one_checkout_is_one_identity_however_it_is_spelled() {
        let t = tempfile::tempdir().unwrap();
        let wt = t.path().join("wt");
        std::fs::create_dir_all(wt.join("build")).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();

        // Two directories inside one checkout are one workspace everywhere. On
        // Windows the callers also disagree about the separator, which is the
        // sharper form of the same hazard and the one that shipped broken --
        // mangling separators off Windows only produces a path that names
        // nothing, so the fixture would be testing the fixture.
        let (a, b) = if cfg!(windows) {
            (
                PathBuf::from(wt.join("src").to_string_lossy().replace('\\', "/")),
                PathBuf::from(wt.join("build").to_string_lossy().replace('/', "\\")),
            )
        } else {
            (wt.join("src"), wt.join("build"))
        };

        assert_eq!(
            workspace(&a),
            workspace(&b),
            "the same checkout, spelled two ways, is one workspace"
        );
    }

    /// The case half of the same hazard, measured rather than assumed.
    ///
    /// One checkout reached as `/Users/.../Sourcetrail-TS` and
    /// `/users/.../sourcetrail-ts` registered as two workspaces, so a build's
    /// figures were split between them. As with basedir matching, whether the
    /// two spellings *are* one directory is the volume's answer to give and not
    /// this test's -- so it asks, and asserts the opposite where the volume
    /// says they are two.
    #[test]
    fn one_checkout_is_one_identity_however_it_is_cased() {
        let t = tempfile::tempdir().unwrap();
        let wt = t.path().join("Worktree");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();

        let other = t.path().join("wORKTREE").join("src");
        if crate::volume::folds_case(&wt) {
            assert_eq!(
                workspace(&other),
                workspace(&wt.join("src")),
                "this volume folds, so both spellings reach the same checkout"
            );
            assert!(
                workspace(&wt).is_some_and(|w| w.ends_with("Worktree")),
                "the identity is the name on disk, not a lowercased one: \
                 {:?}",
                workspace(&wt)
            );
        } else {
            assert_eq!(
                workspace(&other),
                None,
                "this volume does not fold, so that path names nothing"
            );
        }
    }

    /// A worktree reached through a convenience link is the same worktree.
    ///
    /// macOS makes this unavoidable rather than exotic: `/tmp` is a symlink to
    /// `/private/tmp`, so anything built under a temporary directory has two
    /// names before anyone has done anything unusual.
    #[cfg(unix)]
    #[test]
    fn a_link_to_a_checkout_is_not_a_second_checkout() {
        let t = tempfile::tempdir().unwrap();
        let wt = t.path().join("wt");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), "gitdir: elsewhere").unwrap();

        let link = t.path().join("link");
        std::os::unix::fs::symlink(&wt, &link).unwrap();

        assert_eq!(
            workspace(&link.join("src")),
            workspace(&wt.join("src")),
            "one directory under two names is one workspace"
        );
    }

    /// A handle outlives what it names, and the last known name beats nothing.
    #[test]
    fn a_path_that_is_gone_still_has_a_name() {
        let t = tempfile::tempdir().unwrap();
        let gone = t.path().join("deleted");
        assert_eq!(canonical_path(&gone), gone);
    }

    /// An empty value is how a shell says "unset" without unsetting.
    #[test]
    fn an_empty_value_is_not_an_identity() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(identity(&env(&[(AGENT_ID_VAR, "")]), t.path()), None);
        assert_eq!(identity(&[], t.path()), None, "and neither is nowhere");
    }
}
