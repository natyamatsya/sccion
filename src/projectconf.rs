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
//! **Only `basedirs` is honoured.** The file travels with the repository, so
//! anything it could set is something a repository can set. A basedir can only
//! make cache keys less specific to a path; letting it move the cache
//! directory, or point at a network path, would turn a checkout into a way to
//! redirect another user's build. That is the same reason `.clang-format`
//! cannot run a command.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::Deserialize;

use crate::config::normalize_basedir;

/// The file name looked for at each level.
pub const PROJECT_CONF_NAME: &str = ".sccache.toml";

/// Markers that end the upward walk.
///
/// `.git` is a directory in a clone and a *file* in a worktree, so both are
/// accepted; a worktree is precisely the case this feature is for.
const VCS_MARKERS: [&str; 3] = [".git", ".jj", ".hg"];

/// The subset of the file that is honoured. Everything else is ignored on
/// purpose -- see the module comment.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConf {
    /// Extra roots, relative to the file. The file's own directory is always
    /// included, so the common case is an empty file.
    #[serde(default)]
    basedirs: Vec<String>,
}

/// Discovered basedirs, keyed by the directory the search started from.
///
/// The walk is a handful of `stat`s, but it would otherwise run on every single
/// compilation, and a build asks the same question a thousand times with the
/// same answer.
static CACHE: LazyLock<Mutex<HashMap<PathBuf, Vec<Vec<u8>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Basedirs contributed by a project file at or above `start`.
///
/// Empty when there is no such file, which is the case for every project that
/// has not opted in.
pub fn discover_basedirs(start: &Path) -> Vec<Vec<u8>> {
    if let Some(hit) = CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(start)
        .cloned()
    {
        return hit;
    }
    let found = walk(start);
    CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(start.to_path_buf(), found.clone());
    found
}

fn walk(start: &Path) -> Vec<Vec<u8>> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(PROJECT_CONF_NAME);
        if candidate.is_file() {
            return read_conf(&candidate, d);
        }
        // Stop *after* looking in the VCS root: the config belongs at the top of
        // the checkout, and continuing past it would let a file outside the
        // repository silently govern builds inside it.
        if VCS_MARKERS.iter().any(|m| d.join(m).exists()) {
            return Vec::new();
        }
        dir = d.parent();
    }
    Vec::new()
}

fn read_conf(path: &Path, dir: &Path) -> Vec<Vec<u8>> {
    let mut out = Vec::new();

    // The directory holding the file, always. This is the whole point: the
    // location supplies the path so the file does not have to.
    match dir.to_str().map(normalize_basedir) {
        Some(Ok(b)) => out.push(b),
        Some(Err(e)) => warn!("{}: cannot use {} as a basedir: {}", path.display(), dir.display(), e),
        None => warn!("{}: directory is not valid UTF-8", path.display()),
    }

    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            warn!("Failed to read {}: {}", path.display(), e);
            return out;
        }
    };
    let conf: ProjectConf = match toml::from_str(&text) {
        Ok(c) => c,
        Err(e) => {
            // Not fatal: a malformed project file must not stop the build. The
            // root is still usable, which is the part that matters.
            warn!("Ignoring {}: {}", path.display(), e);
            return out;
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
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), body).unwrap();
    }

    #[test]
    fn finds_the_file_by_walking_up_and_uses_its_directory() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        let deep = root.join("src").join("nested");
        std::fs::create_dir_all(&deep).unwrap();
        write(&root, PROJECT_CONF_NAME, "");

        let found = walk(&deep);
        let expected = normalize_basedir(root.to_str().unwrap()).unwrap();
        assert_eq!(found, vec![expected], "the file's directory is the basedir");
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
            walk(&deep).is_empty(),
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

        assert!(walk(&root).is_empty());
    }

    #[test]
    fn extra_basedirs_are_relative_to_the_file() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        write(&root, PROJECT_CONF_NAME, "basedirs = [\"vendor\"]\n");

        let found = walk(&root);
        assert_eq!(found.len(), 2);
        assert!(found.contains(&normalize_basedir(root.to_str().unwrap()).unwrap()));
        assert!(found.contains(&normalize_basedir(root.join("vendor").to_str().unwrap()).unwrap()));
    }

    #[test]
    fn absolute_entries_are_refused() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        let abs = if cfg!(windows) { "C:\\elsewhere" } else { "/elsewhere" };
        write(
            &root,
            PROJECT_CONF_NAME,
            &format!("basedirs = [\"{}\"]\n", abs.replace('\\', "\\\\")),
        );

        // Only the file's own directory survives; a committed file cannot know
        // anyone else's layout, and honouring it would recreate the problem.
        assert_eq!(walk(&root).len(), 1);
    }

    #[test]
    fn a_malformed_file_does_not_break_the_build() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("repo");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, PROJECT_CONF_NAME, "this is not toml {{{");

        // The root is still usable; the alternative is failing every
        // compilation over a typo in an optional file.
        assert_eq!(walk(&root).len(), 1);
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
        assert_eq!(walk(&root).len(), 1, "only the root, and a warning logged");
    }
}
