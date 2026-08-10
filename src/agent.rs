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

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// What a caller sets to name itself.
pub const AGENT_ID_VAR: &str = "SCCACHE_AGENT_ID";

/// Who to attribute a compilation in `cwd` to.
///
/// **Deliberately not basedir-stripped.** Everywhere else a recorded path is
/// made relative to its basedir so that two checkouts of one project produce
/// comparable records. Here the opposite is wanted: the whole purpose is to
/// tell two checkouts apart, and stripping would collapse every one of them to
/// the same empty string -- precisely in the case this exists to serve, since
/// a committed `.sccache.toml` makes the checkout root a basedir.
pub fn identity(env_vars: &[(OsString, OsString)], cwd: &Path) -> Option<String> {
    let named = env_vars
        .iter()
        .find(|(k, _)| k == OsStr::new(AGENT_ID_VAR))
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

/// One spelling of a path, whatever separators the caller happened to use.
///
/// Windows accepts both, and which one arrives here depends on how the
/// compilation was invoked -- a build system passes `D:\x`, someone asking a
/// question about it passes `D:/x`. These are the same directory, and every
/// consumer of this module compares the results as *strings*: as an identity it
/// would split one checkout into two buckets, and as a filter it would match
/// nothing at all.
///
/// `afff1eb` fixed this exact shape once already, where preprocessor cache keys
/// were built with forward slashes and the on-startup index rebuild used
/// backslashes. Comparing `Path`s instead would fix the filter but not the
/// identity, which has to be one string.
pub fn canonical(path: &Path) -> String {
    let s = path.to_string_lossy();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s.into_owned()
    }
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

    /// An empty value is how a shell says "unset" without unsetting.
    #[test]
    fn an_empty_value_is_not_an_identity() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(identity(&env(&[(AGENT_ID_VAR, "")]), t.path()), None);
        assert_eq!(identity(&[], t.path()), None, "and neither is nowhere");
    }
}
