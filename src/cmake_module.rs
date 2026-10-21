// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! sccion's CMake module, carried in the binary.
//!
//! `cmake/SccionCompilerCache.cmake` stands alone: a project commits a copy so
//! that it configures on machines without sccion too, uncached or with sccache
//! or ccache. The binary carries the copy matching its own version, and
//! `sccion --cmake-module-dir` writes it out and prints where, so a project can
//! refresh its copy -- or include the installed one -- without finding the
//! sccion source tree it came from.

use std::fs;
use std::path::{Path, PathBuf};

use crate::errors::*;

/// The module's files, by name.
pub const FILES: &[(&str, &str)] = &[(
    "SccionCompilerCache.cmake",
    include_str!("../cmake/SccionCompilerCache.cmake"),
)];

/// Where this version of sccion keeps its module: one directory per version,
/// so two projects pinned to different sccions never see each other's.
///
/// In the data directory rather than the cache one. The cache directory is the
/// disk cache's store, entries at its top level, and nothing else may live
/// there; and this is not a cache -- deleting it costs nothing but deleting it
/// from under a configure that is reading it would fail that configure.
pub fn dir() -> PathBuf {
    crate::config::default_data_dir()
        .join("cmake")
        .join(env!("CARGO_PKG_VERSION"))
}

/// Write the module's files into `dir`, leaving any that already hold the
/// same text alone, and replacing the others atomically: a configure reading
/// the directory while this runs sees the old file or the new one, never half
/// of either.
pub fn write_to(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    for (name, text) in FILES {
        let path = dir.join(name);
        if fs::read(&path).is_ok_and(|existing| existing == text.as_bytes()) {
            continue;
        }
        let partial = dir.join(format!(".{name}.{}", std::process::id()));
        fs::write(&partial, text)
            .with_context(|| format!("could not write {}", partial.display()))?;
        fs::rename(&partial, &path)
            .with_context(|| format!("could not put {} in place", path.display()))?;
    }
    Ok(())
}

/// `--cmake-module-dir`: write the module for this version, and say where.
pub fn run() -> Result<PathBuf> {
    let dir = dir();
    write_to(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The module names the release it comes from, which is how a project
    /// knows what its committed copy is; a release that forgot to bump it
    /// fails here.
    #[test]
    fn the_module_names_this_version() {
        let (_, text) = FILES[0];
        let line = format!(
            "set(SCCION_CMAKE_MODULE_VERSION \"{}\")",
            env!("CARGO_PKG_VERSION")
        );
        assert!(text.contains(&line), "the module should contain `{line}`");
    }

    #[test]
    fn writing_puts_every_file_in_place_and_again_is_harmless() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cmake").join("x.y.z");
        write_to(&dir).unwrap();
        write_to(&dir).unwrap();
        for (name, text) in FILES {
            assert_eq!(fs::read_to_string(dir.join(name)).unwrap(), *text);
        }
        // Nothing but the module itself: no partial files left behind.
        assert_eq!(fs::read_dir(&dir).unwrap().count(), FILES.len());
    }

    #[test]
    fn a_stale_copy_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let (name, text) = FILES[0];
        fs::write(tmp.path().join(name), "# an older module\n").unwrap();
        write_to(tmp.path()).unwrap();
        assert_eq!(fs::read_to_string(tmp.path().join(name)).unwrap(), text);
    }
}
