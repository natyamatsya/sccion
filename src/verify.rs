// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Checking that a cache *hit* was the right answer.
//!
//! Every other diagnostic in this codebase looks at misses, because a miss is
//! what costs time. A wrong hit costs correctness, and nothing else looks for
//! it. The dependency-file machinery is built to avoid exactly this — "a short
//! dependency list causes a wrong *hit*, not a miss", and every path through it
//! refuses to cache rather than guessing — but nothing verifies that the
//! machinery works. This does, by sampling.
//!
//! **The rule this must not break: report, never adapt.** After comparing, the
//! build gets back precisely the files the cache gave it, whatever was found.
//! A diagnostic that quietly substitutes a freshly compiled object would make
//! builds depend on whether sampling happened to select them, and would hide
//! the very defect it exists to surface. `checkenv.rs` states the same rule for
//! the environment report; it matters more here, because this one runs on the
//! hit path of a real build.
//!
//! It is off unless asked for, and sampled rather than exhaustive: verifying
//! every hit means compiling everything, which is the thing the cache exists to
//! avoid. A rate of a few per cent over a large build reaches every part of the
//! machinery soon enough, and costs a few per cent.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// The rate at which cache hits are verified, as a fraction.
pub const VERIFY_HITS_VAR: &str = "SCCACHE_VERIFY_HITS";

/// How often to verify, from the compilation's environment.
///
/// `None` when the build did not ask, which is the normal case. Values outside
/// `0.0..=1.0` are clamped rather than refused: a diagnostic setting must not
/// be able to fail a build.
pub fn rate(env_vars: &[(OsString, OsString)]) -> Option<f64> {
    let raw = env_vars
        .iter()
        .find(|(k, _)| k == OsStr::new(VERIFY_HITS_VAR))
        .map(|(_, v)| v.to_string_lossy().into_owned())?;
    match raw.trim().parse::<f64>() {
        Ok(r) if r > 0.0 => Some(r.min(1.0)),
        Ok(_) => None,
        Err(_) => {
            warn!("{VERIFY_HITS_VAR}: not a number: {raw:?}");
            None
        }
    }
}

/// Whether this particular hit is the one to check.
pub fn sample(rate: f64) -> bool {
    rate >= 1.0 || rand::random::<f64>() < rate
}

/// What the cache put on disk, kept so it can be put back.
///
/// Holds the bytes rather than a copy on disk: an object is a few hundred KiB,
/// only a sampled fraction of hits reach here, and a temporary file is one more
/// thing to clean up when a compilation fails half way.
pub struct Restored {
    files: Vec<(PathBuf, Option<Vec<u8>>)>,
}

impl Restored {
    /// Read every output the cache wrote, before anything overwrites it.
    ///
    /// A path that does not exist is recorded as absent rather than skipped, so
    /// that [`Restored::put_back`] can remove a file the recompile created and
    /// leave the tree exactly as the cache left it.
    pub fn take(paths: impl IntoIterator<Item = PathBuf>) -> Restored {
        let files = paths
            .into_iter()
            .map(|p| {
                let contents = std::fs::read(&p).ok();
                (p, contents)
            })
            .collect();
        Restored { files }
    }

    /// The outputs whose bytes the compiler did not reproduce.
    ///
    /// A difference here is the serious one: the cache answered a question with
    /// something the compiler would not have produced.
    pub fn differing(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .filter(|(path, cached)| {
                let fresh = std::fs::read(path).ok();
                fresh.as_deref() != cached.as_deref()
            })
            .map(|(path, _)| path.clone())
            .collect()
    }

    /// Put the cache's answer back, exactly as it was.
    ///
    /// Errors are logged and swallowed: this runs after a build has already
    /// been told its compilation succeeded, and a diagnostic must not be able
    /// to turn that into a failure.
    pub fn put_back(self) {
        for (path, cached) in self.files {
            let result = match cached {
                Some(bytes) => std::fs::write(&path, bytes),
                // It did not exist before the recompile, so it must not now.
                None => match std::fs::remove_file(&path) {
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    other => other,
                },
            };
            if let Err(e) = result {
                warn!(
                    "{VERIFY_HITS_VAR}: could not restore {}: {e}",
                    path.display()
                );
            }
        }
    }
}

/// Whether `path` is one worth comparing.
///
/// Dependency files are excluded. They legitimately differ between a cache hit
/// and a fresh compile — a hit may not restore one at all, and when it does the
/// paths inside describe whichever compilation first produced the entry. That
/// is a known and deliberate property, so comparing them would report a
/// mismatch on every sampled hit and drown the one that matters.
pub fn worth_comparing(path: &Path) -> bool {
    !matches!(
        path.extension().and_then(OsStr::to_str),
        Some("d") | Some("json")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rate_is_read_from_the_compilations_environment() {
        let env = |v: &str| vec![(OsString::from(VERIFY_HITS_VAR), OsString::from(v))];
        assert_eq!(rate(&env("0.05")), Some(0.05));
        assert_eq!(rate(&env("1")), Some(1.0));
        assert_eq!(rate(&env("2")), Some(1.0), "clamped, not refused");
        assert_eq!(rate(&env("0")), None);
        assert_eq!(rate(&env("nonsense")), None, "and never fails the build");
        assert_eq!(rate(&[]), None);
    }

    /// The whole point: after verifying, the build must hold exactly what the
    /// cache gave it. Anything else makes a build depend on whether sampling
    /// selected it.
    #[test]
    fn the_cache_s_answer_is_put_back_whatever_was_found() {
        let t = tempfile::tempdir().unwrap();
        let obj = t.path().join("a.o");
        let created = t.path().join("created-by-recompile.o");
        std::fs::write(&obj, b"from the cache").unwrap();

        let restored = Restored::take([obj.clone(), created.clone()]);

        // Stand in for the recompile: it overwrites one output and creates one
        // the cache had not restored.
        std::fs::write(&obj, b"from the compiler").unwrap();
        std::fs::write(&created, b"new").unwrap();

        let differing = restored.differing();
        assert_eq!(differing.len(), 2, "both differ from what the cache held");

        restored.put_back();
        assert_eq!(std::fs::read(&obj).unwrap(), b"from the cache");
        assert!(
            !created.exists(),
            "a file the cache never wrote must not survive the check"
        );
    }

    #[test]
    fn an_identical_recompile_reports_nothing() {
        let t = tempfile::tempdir().unwrap();
        let obj = t.path().join("a.o");
        std::fs::write(&obj, b"same bytes").unwrap();

        let restored = Restored::take([obj.clone()]);
        std::fs::write(&obj, b"same bytes").unwrap();

        assert!(restored.differing().is_empty());
        restored.put_back();
        assert_eq!(std::fs::read(&obj).unwrap(), b"same bytes");
    }

    /// Dependency files differ between a hit and a fresh compile by design, so
    /// comparing them would report a mismatch every time and bury the real one.
    #[test]
    fn dependency_files_are_left_out_of_the_comparison() {
        assert!(worth_comparing(Path::new("a.obj")));
        assert!(worth_comparing(Path::new("a.o")));
        assert!(!worth_comparing(Path::new("a.d")));
        assert!(!worth_comparing(Path::new("a.json")));
    }
}
