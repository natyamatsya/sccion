// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Writing the one file a project has to commit.
//!
//! The measured difference this file makes is large -- two worktrees of one
//! commit shared 559 of 561 compilations with it and 0 without -- and the whole
//! of it is a file that can be empty. That gap between "trivial to write" and
//! "impossible to guess" is what this command is for: nobody adopts a feature
//! they have to read a design document to learn exists.
//!
//! It writes and never edits. A file that is already there was written by
//! someone who meant it, possibly with settings this generator knows nothing
//! about, and merging into it would be the one operation that could make a
//! project's cache keys change without anybody asking.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::projectconf::{self, PROJECT_CONF_NAME, Vcs};

/// What `init` did, so the caller can report it and tests can assert it.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// A file was written here.
    Written { path: PathBuf, root: Root },
    /// One was already in force, at this path.
    AlreadyPresent { path: PathBuf },
}

/// Why the file landed where it did.
#[derive(Debug, PartialEq, Eq)]
pub enum Root {
    /// The top of a checkout, which is what a basedir should be.
    Checkout(&'static str),
    /// No checkout was found, so the working directory was used. Says so out
    /// loud: a basedir halfway up a source tree is a decision, not a default.
    WorkingDirectory,
}

/// The file this writes.
///
/// Every setting is present and commented out, because the alternative is a
/// file that looks complete while the reader has no idea what else was
/// available. Only comments are uncommented, so it parses as an empty table --
/// which is the common case and the recommended one.
const TEMPLATE: &str = r##"# sccion: this file's *location* is its content.
#
# sccion walks up from each compilation looking for `.sccion.toml`, the way
# clang-format finds `.clang-format`, and the directory holding it becomes a
# basedir. Paths under that root are recorded relative to it, so every checkout
# of this repository computes the same cache keys instead of one set per path.
#
# That is why an empty file is worth committing, and why it names no path: an
# absolute basedir would be right in one checkout and wrong in every other, and
# worktrees created on demand cannot be enumerated in advance at all. A
# machine-local setting cannot do this job either -- the fact this states is
# about the repository, not about who is building it.
#
# Everything below is optional and every line of it shapes cache keys. Settings
# that decide *where* a build reads or writes stay in the machine's own
# configuration, because this file travels with the repository.

# Extra roots to strip, relative to this file. This file's own directory is
# always included, so most projects need nothing here.
# basedirs = ["../shared-headers"]

# Whether `__DATE__`, `__TIME__` and `__TIMESTAMP__` in this tree are taken at
# face value. The scan reads bytes, so it cannot tell a macro that expands from
# one merely named -- and toolchain headers name them, which is enough to make
# every translation unit that includes one uncacheable in direct mode. Set this
# once you have checked that nothing here actually expands them.
# ignore_time_macros = true

# Key compilations on what the compiler reported reading, rather than on
# preprocessed text.
# use_dependency_keying = true

# Decline preprocessor cache mode for this tree.
# use_preprocessor_cache_mode = false

# A string folded into every cache key from this tree, so the project can
# invalidate its own entries.
# custom_cache_buster = "1"

# Files whose contents belong in every cache key from this tree -- a toolchain
# manifest, a generated config header. Relative to this file.
# extra_hash_files = ["toolchain.lock"]

# Refuse to let this checkout's build state be answered for over MCP. Only
# `false` is honoured here: a project may decline and may not grant.
# expose_over_mcp = false
"##;

/// Write a project file for the tree `cwd` is in, unless one already governs it.
pub fn run(cwd: &Path) -> Result<Outcome> {
    let found = projectconf::discover(cwd);

    // A file anywhere above already makes this directory part of a basedir'd
    // tree. Writing a second one deeper down would silently move the root and
    // strand every key computed against the first.
    if let Some(path) = found.source {
        return Ok(Outcome::AlreadyPresent { path });
    }

    let (dir, root) = match (&found.vcs_root, found.vcs) {
        (Some(dir), Some(vcs)) => (dir.clone(), Root::Checkout(name(vcs))),
        _ => (cwd.to_path_buf(), Root::WorkingDirectory),
    };
    let path = dir.join(PROJECT_CONF_NAME);

    // `discover` reports no source for a file it could not parse, so ask the
    // filesystem before writing: overwriting a broken file is still
    // overwriting, and the warning it logged is the thing to act on.
    if path.exists() {
        return Ok(Outcome::AlreadyPresent { path });
    }

    fs_err::write(&path, TEMPLATE).with_context(|| format!("writing {}", path.display()))?;
    Ok(Outcome::Written { path, root })
}

fn name(vcs: Vcs) -> &'static str {
    match vcs {
        Vcs::Git => "git",
        Vcs::Jj => "jj",
        Vcs::Mercurial => "hg",
    }
}

impl Outcome {
    /// What to tell whoever ran the command.
    ///
    /// The point of the file is not obvious from its contents -- it is nearly
    /// all comments and no settings -- so the one thing this must convey is
    /// that committing it is the step that matters.
    pub fn report(&self) -> String {
        match self {
            Outcome::Written { path, root } => {
                let where_ = match root {
                    Root::Checkout(vcs) => format!("the top of this {vcs} checkout"),
                    Root::WorkingDirectory => {
                        "the working directory -- no git, jj or hg checkout was found above it, \
                         so if this is not the root of the tree you build, move the file there"
                            .to_string()
                    }
                };
                format!(
                    "Wrote {}, at {where_}.\n\n\
                     Compilations under that directory now record paths relative to it, so two \
                     checkouts of this repository share cache entries instead of each warming \
                     its own.\n\n\
                     Commit it: the fact it states is about the repository, not about this \
                     machine.",
                    path.display()
                )
            }
            Outcome::AlreadyPresent { path } => format!(
                "{} already governs this directory. Nothing written -- edit it by hand; a \
                 generator that merged into it could change this project's cache keys without \
                 anyone asking.",
                path.display()
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The file lands at the top of the checkout, not in whatever subdirectory
    /// the command happened to be run from. A basedir halfway up a source tree
    /// would strip half of each path and share nothing.
    #[test]
    fn it_writes_at_the_checkout_root() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).expect(".git");
        let deep = root.join("src").join("lib");
        std::fs::create_dir_all(&deep).expect("subdirectories");

        let outcome = run(&deep).expect("init");
        assert_eq!(
            outcome,
            Outcome::Written {
                path: root.join(PROJECT_CONF_NAME),
                root: Root::Checkout("git"),
            }
        );
        assert!(root.join(PROJECT_CONF_NAME).exists());
    }

    /// What it writes has to be what the reader parses, or the first thing a
    /// project does with sccion is see a warning about the file sccion told it
    /// to write.
    #[test]
    fn what_it_writes_parses_and_declares_its_own_directory() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let root = tmp.path();
        std::fs::create_dir(root.join(".jj")).expect(".jj");
        run(root).expect("init");

        // From a directory below, not from `root`: `discover` memoises "no file
        // here" for a couple of seconds, keyed by where the walk started, and
        // `run` has just asked that question about `root`. Asking from deeper
        // down is a fresh walk -- and is what a compilation does anyway.
        let deep = root.join("src");
        std::fs::create_dir(&deep).expect("a subdirectory");
        let found = projectconf::discover(&deep);
        assert_eq!(
            found.source.as_deref(),
            Some(root.join(PROJECT_CONF_NAME).as_path()),
            "a file that parsed is reported as the source; one that did not is not"
        );
        assert_eq!(
            found.basedirs.len(),
            1,
            "the file's own directory is the basedir, and it is the whole point"
        );
    }

    /// Never edit what someone else wrote.
    #[test]
    fn an_existing_file_is_left_exactly_as_it_was() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).expect(".git");
        let path = root.join(PROJECT_CONF_NAME);
        std::fs::write(&path, "custom_cache_buster = \"mine\"\n").expect("an existing file");

        assert_eq!(
            run(root).expect("init"),
            Outcome::AlreadyPresent { path: path.clone() }
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("still readable"),
            "custom_cache_buster = \"mine\"\n"
        );
    }

    /// A file above is already in force. A second one deeper down would move
    /// the root silently and strand every key computed against the first.
    #[test]
    fn a_file_further_up_counts_as_already_present() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let root = tmp.path();
        std::fs::create_dir(root.join(".git")).expect(".git");
        std::fs::write(root.join(PROJECT_CONF_NAME), "").expect("a file at the root");
        let deep = root.join("subproject");
        std::fs::create_dir(&deep).expect("a subdirectory");

        assert_eq!(
            run(&deep).expect("init"),
            Outcome::AlreadyPresent {
                path: root.join(PROJECT_CONF_NAME)
            }
        );
        assert!(!deep.join(PROJECT_CONF_NAME).exists());
    }

    /// Outside a checkout it still works, and says what it assumed.
    #[test]
    fn without_a_checkout_it_uses_the_working_directory_and_says_so() {
        let tmp = tempfile::tempdir().expect("a temporary directory");
        let outcome = run(tmp.path()).expect("init");
        assert_eq!(
            outcome,
            Outcome::Written {
                path: tmp.path().join(PROJECT_CONF_NAME),
                root: Root::WorkingDirectory,
            }
        );
        assert!(outcome.report().contains("move the file there"));
    }
}
