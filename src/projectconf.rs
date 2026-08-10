// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Per-checkout configuration, found the way `.clang-format` is found.
//!
//! `basedirs` is what lets two checkouts of one project share cache entries,
//! but the configured form has to name absolute paths and enumerate every
//! checkout. That is exactly wrong for the case it exists to serve: a file
//! committed to the repository would be correct in one checkout and wrong in
//! every other, and worktrees that agents create on demand cannot be enumerated
//! in advance at all.
//!
//! Discovering the file by walking up solves it by not naming a path. The
//! directory holding the file *is* the checkout root, so one committed file is
//! correct in every checkout, including ones that do not exist yet.
//!
//! **What the file may set is deliberately narrow.** It travels with the
//! repository, so anything it can set is something a repository can set.
//! Letting it move the cache directory, or point at a network path, would turn
//! a checkout into a way to redirect another user's build. That is the same
//! reason `.clang-format` cannot run a command.
//!
//! The walk answers a second question on the way, and it would be wasteful not
//! to keep it: **which checkout is this compilation in**. Finding the project
//! file means passing the VCS root, so the root is free, and it is what names a
//! workspace for anything that has to distinguish one concurrent build from
//! another. It is reported whether or not there is a project file, because most
//! repositories have never heard of sccache and still belong to a checkout.
//!
//! The test a key has to pass is that **its blast radius is the declaring
//! repository's own builds**. Nothing here may reach another project's entries,
//! another user's cache, or anything about where a build reads or writes.
//!
//! That test used to be written as "the worst it can do is make its own builds
//! slower", which was too tight and did not describe what was already admitted:
//! `basedirs` makes keys deliberately *less* specific, and a tree that declares
//! a root whose contents are not in fact path-independent gets wrong objects,
//! not slow ones. It is allowed because path-independence is the tree's own
//! property to assert and the consequence lands on the tree that asserted it.
//! `ignore_time_macros` is admitted on exactly that footing.
//!
//! `basedirs` passes: it can only make
//! cache keys less specific to a path. `use_dependency_keying` passes: it
//! chooses whether a compilation's inputs are learned by compiling and asking
//! or by preprocessing first, which changes what the build spends and nothing
//! about where anything is read from or written to. It is here because the
//! choice depends on how the project is built -- how often a compilation misses
//! its manifest while its object is still in the cache -- which is exactly the
//! thing a repository knows and a machine-wide config cannot.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::normalize_basedir;

/// The file name looked for at each level.
pub const PROJECT_CONF_NAME: &str = ".sccion.toml";

/// The version control system a checkout belongs to.
///
/// Which one it is matters beyond ending the walk: a git worktree and a jj
/// workspace are the two ways a checkout appears at a path nobody enumerated in
/// advance, and they identify themselves differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vcs {
    Git,
    Jj,
    Mercurial,
}

impl Vcs {
    /// Whether `dir` is the top of a checkout, and of what kind.
    ///
    /// `.git` is a directory in a clone and a *file* in a worktree, so both are
    /// accepted; a worktree is precisely the case this exists for.
    fn at(dir: &Path) -> Option<Vcs> {
        for (marker, vcs) in [
            (".git", Vcs::Git),
            (".jj", Vcs::Jj),
            (".hg", Vcs::Mercurial),
        ] {
            if dir.join(marker).exists() {
                return Some(vcs);
            }
        }
        None
    }
}

/// The subset of the file that is honoured. Everything else is ignored on
/// purpose -- see the module comment.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConf {
    /// Extra roots, relative to the file. The file's own directory is always
    /// included, so the common case is an empty file.
    #[serde(default)]
    basedirs: Vec<String>,
    /// Whether compilations in this tree should be keyed on what the compiler
    /// reports reading, rather than on preprocessed text. Absent means the
    /// project has no opinion and the machine-wide setting stands.
    #[serde(default)]
    use_dependency_keying: Option<bool>,
    /// Whether preprocessor cache mode may be used here. `false` declines it;
    /// `true` means only "no objection", since whether it is available at all
    /// depends on the cache backend.
    #[serde(default)]
    use_preprocessor_cache_mode: Option<bool>,
    /// Whether `__DATE__`, `__TIME__` and `__TIMESTAMP__` appearing in a file
    /// this tree compiles should be taken at face value.
    ///
    /// The check scans bytes, so it cannot tell a macro that expands from one
    /// merely named -- and toolchain headers name them: libclang's
    /// `Preprocessor.h` has `Ident__DATE__` and a comment, which is enough to
    /// make every translation unit that includes it uncacheable in direct mode.
    /// Whether a tree actually expands them is a fact about the tree, which is
    /// why it is answerable here and not on the machine.
    #[serde(default)]
    ignore_time_macros: Option<bool>,
    /// An arbitrary string folded into every cache key from this tree, so the
    /// project can invalidate its own entries.
    #[serde(default)]
    custom_cache_buster: Option<String>,
    /// Files whose contents belong in every cache key from this tree -- a
    /// toolchain manifest, a generated config header. Relative to the file.
    #[serde(default)]
    extra_hash_files: Vec<String>,
    /// Whether this checkout's build state may be answered for over MCP.
    ///
    /// **Only `false` is honoured.** See [`Discovered::expose_over_mcp`].
    #[serde(default)]
    expose_over_mcp: Option<bool>,
}

/// What a project file at or above a directory contributes.
///
/// Every `Option` is `None` when the file said nothing, which is not the same
/// as saying no: the machine-wide setting has to be able to stand, or a project
/// would be opting out of settings it never mentioned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Discovered {
    /// Roots whose absolute paths must not reach a cache key. Empty when there
    /// is no project file, which is every project that has not opted in.
    pub basedirs: Vec<Vec<u8>>,
    pub use_dependency_keying: Option<bool>,
    pub use_preprocessor_cache_mode: Option<bool>,
    pub ignore_time_macros: Option<bool>,
    pub custom_cache_buster: Option<String>,
    /// Absolute, already resolved against the file's directory.
    pub extra_hash_files: Vec<PathBuf>,
    /// The top of the checkout the compilation is in, and which VCS put it
    /// there. Found whether or not there is a project file -- the walk passes
    /// the root either way, and a compilation in a repository that has never
    /// heard of sccache still belongs to a workspace worth naming.
    pub vcs_root: Option<PathBuf>,
    pub vcs: Option<Vcs>,
    /// The project file these settings were read from, when one was found and
    /// parsed. A report that says a project overrode a setting is only half an
    /// answer without it: the file may be several directories above the one
    /// asked about, and finding it by hand means repeating the walk.
    ///
    /// Stays `None` for a file that exists but could not be read or parsed --
    /// it contributed nothing but the root, and naming it as the source of
    /// settings it did not supply would be worse than silence. The `warn!` on
    /// that path is what says it was there.
    pub source: Option<PathBuf>,
    /// `Some(false)` when this checkout declines to be answered for over MCP.
    ///
    /// **Deliberately one-directional: a project may say no and cannot say
    /// yes.** Everything else here shapes cache keys, where the rule is that
    /// the worst a hostile repository can do is make its own builds slower.
    /// This one is different in kind -- it governs whether build state can be
    /// read -- so it only passes that rule in the refusing direction. A
    /// repository that could *enable* the surface would be a repository that
    /// turns on a way to read a machine's build state by being checked out on
    /// it, which is exactly what the machine-wide setting exists to decide.
    pub expose_over_mcp: Option<bool>,
}

/// Enough of a file to notice that it has been edited.
type Stamp = (Option<std::time::SystemTime>, u64);

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok(), meta.len()))
}

/// How long "there is no project file here" is taken on trust.
///
/// Adding the file is how a project opts in, so the answer that has to be
/// revisited is precisely the negative one -- and it used to be kept for the
/// life of the daemon. That made opting in require a restart, and the symptom
/// was indistinguishable from the file not working: write `.sccion.toml`,
/// rebuild, watch nothing change.
///
/// A few seconds of staleness costs a handful of compilations keyed without the
/// basedir, which is a slower build and never a wrong one. Re-walking is ~10
/// `stat`s, so at this interval a build pays it a few dozen times instead of
/// once per compilation -- the saving the memo exists for is kept, and the trap
/// is not.
const ABSENT_REVALIDATE_AFTER: Duration = Duration::from_secs(2);

/// What was answered for a directory, and how to tell it is still the answer.
#[derive(Clone)]
struct Memo {
    found: Discovered,
    /// The file the answer came from, as it looked when it was read. `None`
    /// when there was no file, which is the case [`ABSENT_REVALIDATE_AFTER`]
    /// exists for.
    source: Option<(PathBuf, Stamp)>,
    /// When this answer was taken. Only consulted for a negative one; a file
    /// that exists is revalidated by its stamp instead, which is exact.
    taken: Instant,
}

/// Discovered settings, keyed by the directory the search started from.
///
/// The walk is a handful of `stat`s, but it would otherwise run on every single
/// compilation, and a build asks the same question a thousand times with the
/// same answer.
static CACHE: LazyLock<Mutex<HashMap<PathBuf, Memo>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Everything a project file at or above `start` contributes.
pub fn discover(start: &Path) -> Discovered {
    if let Some(memo) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(start)
        .cloned()
    {
        // Re-read a file that has been edited. One `stat` on a path already
        // known is nothing next to a compilation, and the alternative is a
        // trap: `custom_cache_buster` exists to be changed, and a build that
        // went on using the old value until someone restarted the server would
        // look exactly like the setting doing nothing.
        match &memo.source {
            Some((path, seen)) if stamp(path).as_ref() != Some(seen) => {}
            // Nothing was found last time. Look again, but not on every
            // compilation -- see `ABSENT_REVALIDATE_AFTER`.
            None if memo.taken.elapsed() >= ABSENT_REVALIDATE_AFTER => {}
            _ => return memo.found,
        }
    }
    let (found, source) = walk(start);
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).insert(
        start.to_path_buf(),
        Memo {
            found: found.clone(),
            source: source.and_then(|p| stamp(&p).map(|s| (p, s))),
            taken: Instant::now(),
        },
    );
    found
}

/// Basedirs contributed by a project file at or above `start`.
pub fn discover_basedirs(start: &Path) -> Vec<Vec<u8>> {
    discover(start).basedirs
}

/// The settings in force at `start`, and the file they came from.
fn walk(start: &Path) -> (Discovered, Option<PathBuf>) {
    let mut conf: Option<(Discovered, PathBuf)> = None;
    let mut dir = Some(start);
    while let Some(d) = dir {
        // The first file wins; the walk carries on only to find the root.
        if conf.is_none() {
            let candidate = d.join(PROJECT_CONF_NAME);
            if candidate.is_file() {
                conf = Some((read_conf(&candidate, d), candidate));
            }
        }
        // Stop *after* looking in the VCS root: the config belongs at the top of
        // the checkout, and continuing past it would let a file outside the
        // repository silently govern builds inside it.
        if let Some(vcs) = Vcs::at(d) {
            return finish(conf, Some((d.to_path_buf(), vcs)));
        }
        dir = d.parent();
    }
    finish(conf, None)
}

/// Fold the two independent findings -- the project file and the checkout root
/// -- into one answer. Either may be absent without the other.
fn finish(
    conf: Option<(Discovered, PathBuf)>,
    root: Option<(PathBuf, Vcs)>,
) -> (Discovered, Option<PathBuf>) {
    let (mut found, source) = match conf {
        Some((found, path)) => (found, Some(path)),
        None => (Discovered::default(), None),
    };
    if let Some((root, vcs)) = root {
        found.vcs_root = Some(root);
        found.vcs = Some(vcs);
    }
    (found, source)
}

fn read_conf(path: &Path, dir: &Path) -> Discovered {
    let mut out = Vec::new();

    // The directory holding the file, always. This is the whole point: the
    // location supplies the path so the file does not have to.
    match dir.to_str().map(normalize_basedir) {
        Some(Ok(b)) => out.push(b),
        Some(Err(e)) => warn!(
            "{}: cannot use {} as a basedir: {}",
            path.display(),
            dir.display(),
            e
        ),
        None => warn!("{}: directory is not valid UTF-8", path.display()),
    }

    let only_the_root = |basedirs| Discovered {
        basedirs,
        ..Default::default()
    };

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to read {}: {}", path.display(), e);
            return only_the_root(out);
        }
    };
    let conf: ProjectConf = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            // Not fatal: a malformed project file must not stop the build. The
            // root is still usable, which is the part that matters.
            warn!("Ignoring {}: {}", path.display(), e);
            return only_the_root(out);
        }
    };

    for rel in conf.basedirs {
        let joined = dir.join(&rel);
        if joined.is_absolute() && !Path::new(&rel).is_absolute() {
            match joined.to_str().map(normalize_basedir) {
                Some(Ok(b)) if !out.contains(&b) => out.push(b),
                Some(Ok(_)) => {}
                Some(Err(e)) => warn!("{}: bad basedir {rel:?}: {}", path.display(), e),
                None => warn!("{}: basedir {rel:?} is not valid UTF-8", path.display()),
            }
        } else {
            // Absolute entries would reintroduce exactly the problem this
            // exists to remove, and a committed file cannot know anyone's
            // layout.
            warn!(
                "{}: basedirs entries must be relative to the file; ignoring {rel:?}",
                path.display()
            );
        }
    }
    // Named files are resolved against the file's directory and must stay under
    // it. A path reaching out of the checkout is either a mistake or an attempt
    // to make a build depend on something the repository does not carry, and
    // neither is worth honouring.
    let mut extra_hash_files = Vec::new();
    for rel in conf.extra_hash_files {
        let entry = Path::new(&rel);
        if entry.is_absolute() || entry.components().any(|c| c == Component::ParentDir) {
            warn!(
                "{}: extra_hash_files entries must be relative to the file and stay under it; ignoring {rel:?}",
                path.display()
            );
            continue;
        }
        extra_hash_files.push(dir.join(entry));
    }

    Discovered {
        basedirs: out,
        use_dependency_keying: conf.use_dependency_keying,
        use_preprocessor_cache_mode: conf.use_preprocessor_cache_mode,
        ignore_time_macros: conf.ignore_time_macros,
        custom_cache_buster: conf.custom_cache_buster,
        extra_hash_files,
        // Only a refusal is carried forward. `true` is not worth warning about
        // -- it is what the default already is -- but it must not be able to
        // overrule a machine that said no.
        expose_over_mcp: match conf.expose_over_mcp {
            Some(false) => Some(false),
            _ => None,
        },
        source: Some(path.to_path_buf()),
        // Filled in by `finish`: the file has been found but the walk has not
        // reached the root yet.
        vcs_root: None,
        vcs: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings in force at `start`, without the file they came from.
    fn found(start: &Path) -> Discovered {
        walk(start).0
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    /// Adding the file is how a project opts in, so the negative answer is the
    /// one that must expire. It never did: the daemon answered "no project
    /// file" for the rest of its life, so opting in meant restarting it, and
    /// the symptom was indistinguishable from the file not working -- write
    /// `.sccion.toml`, rebuild, watch nothing change. Measured on a real
    /// build before this: a second checkout shared 0 of 561 objects with the
    /// file in place, and 559 of 561 once the daemon was restarted.
    #[test]
    fn a_project_file_added_after_the_first_answer_is_noticed() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        let start = root.join("build");
        std::fs::create_dir_all(&start).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        assert!(
            discover(&start).basedirs.is_empty(),
            "nothing to find yet, and that answer gets memoized"
        );

        write(&root, PROJECT_CONF_NAME, "");
        assert!(
            discover(&start).basedirs.is_empty(),
            "the memo is the point: it must not re-walk on every compilation"
        );

        // Age the memo rather than sleeping through the interval. `Instant` has
        // no epoch to subtract from on a machine that just booted, so fall back
        // to waiting rather than unwrapping something that can legitimately be
        // `None`.
        match Instant::now().checked_sub(ABSENT_REVALIDATE_AFTER) {
            Some(then) => {
                CACHE
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&start)
                    .expect("the negative answer was memoized")
                    .taken = then;
            }
            None => std::thread::sleep(ABSENT_REVALIDATE_AFTER),
        }

        let expected = normalize_basedir(root.to_str().unwrap()).unwrap();
        assert_eq!(
            discover(&start).basedirs,
            vec![expected],
            "once the interval passes, opting in takes effect without a restart"
        );
    }

    #[test]
    fn finds_the_file_by_walking_up_and_uses_its_directory() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        let deep = root.join("src").join("nested");
        std::fs::create_dir_all(&deep).unwrap();
        write(&root, PROJECT_CONF_NAME, "");

        let found = found(&deep);
        let expected = normalize_basedir(root.to_str().unwrap()).unwrap();
        assert_eq!(
            found.basedirs,
            vec![expected],
            "the file's directory is the basedir"
        );
    }

    #[test]
    fn stops_at_the_vcs_root() {
        let t = tempfile::tempdir().unwrap();
        // A config above the repository must not govern builds inside it.
        write(t.path(), PROJECT_CONF_NAME, "");
        let root = t.path().join("repo");
        let deep = root.join("src");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        assert!(
            found(&deep).basedirs.is_empty(),
            "the walk must not escape the checkout"
        );
    }

    #[test]
    fn a_git_worktree_file_ends_the_walk_too() {
        let t = tempfile::tempdir().unwrap();
        write(t.path(), PROJECT_CONF_NAME, "");
        let root = t.path().join("wt");
        std::fs::create_dir_all(&root).unwrap();
        // In a worktree `.git` is a file, not a directory.
        std::fs::write(root.join(".git"), "gitdir: ../repo/.git/worktrees/wt").unwrap();

        assert!(found(&root).basedirs.is_empty());
    }

    /// The walk already passed the checkout root on its way up and used to
    /// throw it away. It is worth more than the config file it was looking for:
    /// it is what says which workspace a compilation belongs to, and most
    /// repositories have no `.sccion.toml` at all.
    #[test]
    fn the_checkout_root_is_found_without_any_project_file() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        let deep = root.join("src").join("nested");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let found = found(&deep);
        assert_eq!(found.vcs_root.as_deref(), Some(root.as_path()));
        assert_eq!(found.vcs, Some(Vcs::Git));
        assert!(found.basedirs.is_empty(), "and it opts into nothing");
    }

    /// A worktree is the case this all exists for: it appears at a path nobody
    /// enumerated, and its root is itself, not the clone it was made from.
    #[test]
    fn a_worktree_resolves_to_its_own_root() {
        let t = tempfile::tempdir().unwrap();
        let main = t.path().join("repo");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        let wt = main.join("worktrees").join("agent-7");
        std::fs::create_dir_all(&wt).unwrap();
        // In a worktree `.git` is a file, not a directory.
        std::fs::write(wt.join(".git"), "gitdir: ../../.git/worktrees/agent-7").unwrap();

        let found = found(&wt);
        assert_eq!(
            found.vcs_root.as_deref(),
            Some(wt.as_path()),
            "the worktree is its own root, not the clone above it"
        );
    }

    /// The file usually sits at the root, but nothing requires it to. Finding
    /// one part-way up must not stop the root being found.
    #[test]
    fn a_project_file_below_the_root_does_not_hide_the_root() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        let sub = root.join("component");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(root.join(".jj")).unwrap();
        write(&sub, PROJECT_CONF_NAME, "custom_cache_buster = \"v1\"\n");

        let found = found(&sub);
        assert_eq!(found.custom_cache_buster.as_deref(), Some("v1"));
        assert_eq!(found.vcs_root.as_deref(), Some(root.as_path()));
        assert_eq!(found.vcs, Some(Vcs::Jj));
    }

    #[test]
    fn extra_basedirs_are_relative_to_the_file() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        write(&root, PROJECT_CONF_NAME, "basedirs = [\"vendor\"]\n");

        let found = found(&root).basedirs;
        assert_eq!(found.len(), 2);
        assert!(found.contains(&normalize_basedir(root.to_str().unwrap()).unwrap()));
        assert!(found.contains(&normalize_basedir(root.join("vendor").to_str().unwrap()).unwrap()));
    }

    #[test]
    fn absolute_entries_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let abs = if cfg!(windows) {
            "C:\\elsewhere"
        } else {
            "/elsewhere"
        };
        write(
            &root,
            PROJECT_CONF_NAME,
            &format!("basedirs = [\"{}\"]\n", abs.replace('\\', "\\\\")),
        );

        // Only the file's own directory survives; a committed file cannot know
        // anyone else's layout, and honouring it would recreate the problem.
        assert_eq!(found(&root).basedirs.len(), 1);
    }

    #[test]
    fn a_malformed_file_does_not_break_the_build() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, PROJECT_CONF_NAME, "this is not toml {{{");

        // The root is still usable; the alternative is failing every
        // compilation over a typo in an optional file.
        assert_eq!(found(&root).basedirs.len(), 1);
    }

    /// Whether dependency keying pays off depends on how a project is built,
    /// which is what the committed file is for.
    #[test]
    fn dependency_keying_can_be_set_by_the_project() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        write(&root, PROJECT_CONF_NAME, "use_dependency_keying = true\n");
        assert_eq!(found(&root).use_dependency_keying, Some(true));

        write(&root, PROJECT_CONF_NAME, "use_dependency_keying = false\n");
        assert_eq!(found(&root).use_dependency_keying, Some(false));
    }

    /// Saying nothing is not the same as saying no: the machine-wide setting
    /// has to be able to stand, or every project without the key would be
    /// opting out of one it never mentioned.
    #[test]
    fn saying_nothing_leaves_the_decision_alone() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        write(&root, PROJECT_CONF_NAME, "");
        assert_eq!(found(&root).use_dependency_keying, None);

        // Including when the file is unusable, or absent entirely.
        write(&root, PROJECT_CONF_NAME, "this is not toml {{{");
        assert_eq!(found(&root).use_dependency_keying, None);
        assert_eq!(found(t.path()).use_dependency_keying, None);
    }

    #[test]
    fn the_project_can_name_files_that_belong_in_every_key() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write(
            &root,
            PROJECT_CONF_NAME,
            "extra_hash_files = [\"toolchain.lock\", \"gen/config.h\"]\n",
        );

        assert_eq!(
            found(&root).extra_hash_files,
            vec![root.join("toolchain.lock"), root.join("gen/config.h")],
            "resolved against the file, not the compilation's directory"
        );
    }

    /// A named file reaching out of the checkout is either a mistake or an
    /// attempt to make a build depend on something the repository does not
    /// carry. `..` gets there without ever looking absolute.
    #[test]
    fn named_files_may_not_escape_the_checkout() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let abs = if cfg!(windows) {
            "C:\\\\elsewhere\\\\x"
        } else {
            "/elsewhere/x"
        };
        write(
            &root,
            PROJECT_CONF_NAME,
            &format!("extra_hash_files = [\"../outside\", \"a/../../b\", \"{abs}\", \"kept\"]\n"),
        );

        assert_eq!(found(&root).extra_hash_files, vec![root.join("kept")]);
    }

    #[test]
    fn the_project_can_set_the_remaining_key_inputs() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write(
            &root,
            PROJECT_CONF_NAME,
            "use_preprocessor_cache_mode = false\ncustom_cache_buster = \"v2\"\n",
        );

        let found = found(&root);
        assert_eq!(found.use_preprocessor_cache_mode, Some(false));
        assert_eq!(found.custom_cache_buster.as_deref(), Some("v2"));
    }

    /// The answer is memoized per directory for the life of the server, which
    /// outlives any number of edits. A cache buster that only took effect after
    /// someone restarted the server would be indistinguishable from one that
    /// did nothing.
    #[test]
    fn an_edited_file_is_read_again() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        write(&root, PROJECT_CONF_NAME, "custom_cache_buster = \"v1\"\n");
        assert_eq!(discover(&root).custom_cache_buster.as_deref(), Some("v1"));

        write(&root, PROJECT_CONF_NAME, "custom_cache_buster = \"v22\"\n");
        assert_eq!(discover(&root).custom_cache_buster.as_deref(), Some("v22"));
    }

    /// A repository may refuse to have its build state answered for, and may
    /// not grant it.
    ///
    /// Every other key here shapes a cache key, where the rule is that the
    /// worst a hostile repository can do is make its own builds slower. This
    /// one governs whether build state can be *read*, so it only satisfies
    /// that rule in one direction: a checkout that could switch the surface on
    /// would be a way to make a machine answer questions about itself by being
    /// cloned onto it.
    #[test]
    fn a_project_may_decline_mcp_but_may_not_demand_it() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();

        write(&root, PROJECT_CONF_NAME, "expose_over_mcp = false\n");
        assert_eq!(
            found(&root).expose_over_mcp,
            Some(false),
            "a refusal counts"
        );

        write(&root, PROJECT_CONF_NAME, "expose_over_mcp = true\n");
        assert_eq!(
            found(&root).expose_over_mcp,
            None,
            "and a demand is not a refusal -- it leaves the machine's setting standing"
        );

        write(&root, PROJECT_CONF_NAME, "");
        assert_eq!(found(&root).expose_over_mcp, None);
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_silently_ignored() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        // Someone will try to set the cache directory here. Refusing the file
        // loudly beats appearing to accept a setting that is deliberately not
        // honoured.
        write(&root, PROJECT_CONF_NAME, "dir = \"//server/share\"\n");
        assert_eq!(
            found(&root).basedirs.len(),
            1,
            "only the root, and a warning logged"
        );
    }
}
