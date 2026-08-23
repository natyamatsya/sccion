// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
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
pub const VERIFY_HITS_VAR: &str = "SCCION_VERIFY_HITS";

/// How often to verify, from the compilation's environment.
///
/// `None` when the build did not ask, which is the normal case. Values outside
/// `0.0..=1.0` are clamped rather than refused: a diagnostic setting must not
/// be able to fail a build.
pub fn rate(env_vars: &[(OsString, OsString)]) -> Option<f64> {
    let raw = env_vars
        .iter()
        .find(|(k, _)| crate::envvar::matches(k, VERIFY_HITS_VAR))
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

/// Why this compilation was never going to reproduce, when that is known.
///
/// A compilation that does not reproduce its own output cannot be verified, and
/// the advice that used to be given — build with `/Brepro` — is useless to a
/// build that already does. On one real Windows build **every** sampled hit came
/// back unverifiable with `/Brepro` already set, and the cause was a different
/// flag entirely.
///
/// `/dynamicdeopt` emits a `.pmd_optdep` section holding per-compilation
/// identifiers. Measured: the same source compiled twice with the flag produces
/// objects that differ in that section and in the size of the file; with the
/// flag removed and nothing else changed, the two objects are byte-identical.
/// The COFF timestamp differs too, but only as a consequence — under `/Brepro`
/// it is a hash of the contents.
///
/// Naming the flag turns "the compiler is not deterministic" into something the
/// reader can act on. It does not change the verdict: an unverifiable
/// compilation stays unverifiable.
pub fn known_reason_for_irreproducibility(arguments: &[OsString]) -> Option<&'static str> {
    let has = |needle: &str| {
        arguments
            .iter()
            .any(|a| a.to_string_lossy().to_lowercase().starts_with(needle))
    };
    if has("/dynamicdeopt") || has("-dynamicdeopt") {
        return Some(
            "/dynamicdeopt writes per-compilation identifiers into .pmd_optdep, so the \
             object cannot reproduce byte for byte. Verification needs a build without it",
        );
    }
    // The remaining cause is COFF's, so it is MSVC's and clang-cl's. Given to
    // every compilation regardless, this told a macOS build to set `/Brepro` --
    // advice for an object format it does not emit, about a timestamp its
    // objects do not carry, naming a flag its compiler does not accept.
    //
    // Recognised from the command line rather than from the host, because a
    // clang-cl compilation produces a COFF object and wants the same advice
    // wherever it runs. These spellings are MSVC's and are not paths anything
    // would pass to a POSIX driver.
    const MSVC_MARKERS: [&str; 7] = ["/Fo", "/Fd", "/Fp", "/Z7", "/Zi", "/nologo", "/EH"];
    let msvc = arguments.iter().any(|a| {
        let a = a.to_string_lossy();
        MSVC_MARKERS.iter().any(|m| a.starts_with(m))
    });
    if msvc && !has("/brepro") && !has("-brepro") {
        return Some(
            "the object carries a COFF timestamp; build with /Brepro to make \
             SCCION_VERIFY_HITS meaningful",
        );
    }
    None
}

/// The prefix-map flags, which are what makes an object independent of where
/// it was built.
const PREFIX_MAP_FLAGS: [&str; 3] = [
    "-ffile-prefix-map=",
    "-fdebug-prefix-map=",
    "-fmacro-prefix-map=",
];

/// Why a byte difference on a verified hit might not be the cache's fault.
///
/// `None` when a difference is conclusive, which is when the accusation may be
/// made. The accusation is this module's product, so it is worth being exact
/// about when it holds.
///
/// The reasoning it guards is: recompile, and if the compiler reproduces its
/// own output then a difference from the cache is the cache's. That is sound
/// within one checkout and **unsound across two**. Basedirs deliberately make
/// keys less specific so that a second checkout hits, and the object it is
/// served was built somewhere else and embeds that somewhere else's paths --
/// a documented, accepted property, not a wrong answer. Measured on two
/// checkouts of one fixture: without a prefix-map, the hit in the second
/// checkout was reported as `THE CACHE RETURNED THE WRONG OBJECT`, and a
/// `WrongHit` record was written. Every sampled hit in that build would have
/// said the same.
///
/// The remedy restores the accusation rather than weakening it: with
/// `-ffile-prefix-map=<checkout>=.` the two checkouts' objects are
/// byte-identical -- measured -- so a difference means what it says again.
///
/// Coverage is checked rather than assumed. A prefix-map for some other
/// directory does not make the object checkout-independent, so the flag only
/// counts when the path it rewrites *is* a basedir -- which is exactly the
/// question basedir stripping already answers: strip the flag, and if the path
/// it mapped is gone, it was one.
pub fn cross_checkout_ambiguity(
    arguments: &[OsString],
    basedirs: &[Vec<u8>],
) -> Option<&'static str> {
    // No basedirs, no sharing across paths, nothing to be ambiguous about.
    if basedirs.is_empty() {
        return None;
    }
    let mapped_away = arguments.iter().any(|arg| {
        let bytes = arg.as_encoded_bytes();
        let Some(flag) = PREFIX_MAP_FLAGS
            .iter()
            .find(|f| bytes.starts_with(f.as_bytes()))
        else {
            return false;
        };
        // `-ffile-prefix-map=/a/proj=.` becomes `-ffile-prefix-map==.` when
        // `/a/proj` is a basedir, and is untouched when it is not.
        let stripped = crate::util::strip_basedirs_in_arg(bytes, basedirs);
        stripped.starts_with(flag.as_bytes()) && stripped.get(flag.len()) == Some(&b'=')
    });
    if mapped_away {
        return None;
    }
    Some(
        "objects embed the absolute path of the checkout they were built in, and basedirs \
         let this hit come from a different checkout, so a difference here cannot tell a \
         wrong object from another checkout's paths. Build with \
         -ffile-prefix-map=<checkout>=. -- measured to make two checkouts' objects \
         byte-identical, which is what gives this check its meaning back",
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

    /// The advice has to fit the build. Telling a build that already sets
    /// `/Brepro` to set `/Brepro` is what this replaced, and it was given on
    /// every sampled hit of a real build whose objects could not reproduce for
    /// an entirely different reason.
    #[test]
    fn an_unverifiable_compilation_names_the_flag_responsible() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();

        let with_deopt = known_reason_for_irreproducibility(&args(&[
            "/c",
            "/Z7",
            "/Brepro",
            "/dynamicdeopt",
            "a.cpp",
        ]))
        .expect("the flag is present, so the cause is known");
        assert!(with_deopt.contains(".pmd_optdep"), "{with_deopt}");

        // /Brepro missing is the other knowable cause, and still worth saying.
        let no_brepro = known_reason_for_irreproducibility(&args(&["/c", "/Z7", "a.cpp"]))
            .expect("no /Brepro is a knowable cause");
        assert!(no_brepro.contains("/Brepro"), "{no_brepro}");

        // Both accounted for: nothing to add beyond the generic verdict.
        assert_eq!(
            known_reason_for_irreproducibility(&args(&["/c", "/Z7", "/Brepro", "a.cpp"])),
            None
        );
    }

    /// The advice must fit the compiler too, not just the build.
    #[test]
    fn a_clang_build_is_not_told_to_set_an_msvc_flag() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(
            known_reason_for_irreproducibility(&args(&["-c", "-g", "a.cpp", "-o", "a.o"])),
            None,
            "there is no COFF timestamp on a Mach-O object and no /Brepro to set"
        );
        // And the MSVC case still answers, which is what `/Fo` distinguishes.
        assert!(
            known_reason_for_irreproducibility(&args(&["/c", "/Z7", "/Foa.obj", "a.cpp"]))
                .is_some_and(|r| r.contains("/Brepro"))
        );
    }

    /// The accusation is this module's product, so it has to hold where it is
    /// made. Across two checkouts it does not: the object the cache serves was
    /// built elsewhere and embeds those paths on purpose.
    #[test]
    fn a_second_checkouts_paths_are_not_an_accusation() {
        let args = |v: &[&str]| v.iter().map(OsString::from).collect::<Vec<_>>();
        let basedirs = vec![b"/a/proj/".to_vec()];

        assert_eq!(
            cross_checkout_ambiguity(&args(&["-c", "/a/proj/src/lib.cpp"]), &[]),
            None,
            "without basedirs nothing shares across paths, so a difference is conclusive"
        );

        assert!(
            cross_checkout_ambiguity(&args(&["-c", "/a/proj/src/lib.cpp"]), &basedirs)
                .is_some_and(|r| r.contains("-ffile-prefix-map")),
            "with basedirs and nothing rewriting paths, it cannot tell the two apart"
        );

        assert_eq!(
            cross_checkout_ambiguity(
                &args(&["-ffile-prefix-map=/a/proj=.", "-c", "/a/proj/src/lib.cpp"]),
                &basedirs
            ),
            None,
            "the objects are path-independent, so the accusation holds again"
        );

        // Coverage is checked, not assumed: a map for somewhere else leaves the
        // checkout's own paths in the object.
        assert!(
            cross_checkout_ambiguity(
                &args(&["-ffile-prefix-map=/opt/sdk=.", "-c", "/a/proj/src/lib.cpp"]),
                &basedirs
            )
            .is_some(),
            "a prefix-map for another directory does not make this object portable"
        );
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
