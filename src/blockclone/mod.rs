// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Filesystem-level file cloning.
//!
//! A clone makes a second file that shares the first one's storage until either
//! is written to, so producing it costs a metadata update rather than a copy of
//! the bytes. ReFS calls this block cloning; btrfs and XFS call it a reflink;
//! APFS calls it `clonefile`.
//!
//! Only Windows/ReFS is implemented so far. Everything else reports
//! [`CloneError::Unsupported`], which callers are expected to treat as "fall back
//! to copying" rather than as a failure.
//!
//! The API takes paths rather than open handles deliberately: macOS `clonefile`
//! creates the destination itself, so a handle-based signature could not be
//! implemented there without inventing a fake handle.

use std::path::Path;

#[cfg(windows)]
mod windows;

#[cfg(windows)]
pub use windows::{clone_whole_file, volume_supports_cloning};

/// Why a clone did not happen.
///
/// Only [`CloneError::Io`] indicates something went wrong; the rest describe
/// situations where copying is the correct thing to do instead.
#[derive(Debug)]
pub enum CloneError {
    /// The filesystem cannot clone, or this platform has no implementation.
    Unsupported,
    /// Source and destination are on different volumes. Extents cannot be shared
    /// across volumes, so this can only ever be a copy.
    DifferentVolume,
    /// Not worth cloning: below the size where a metadata operation beats simply
    /// writing the bytes.
    TooSmall,
    /// The filesystem refused this particular clone, for example because the
    /// source region has been shared too many times already. Copying will work.
    Refused(String),
    Io(std::io::Error),
}

impl std::fmt::Display for CloneError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CloneError::Unsupported => write!(f, "file cloning is not supported here"),
            CloneError::DifferentVolume => {
                write!(f, "source and destination are on different volumes")
            }
            CloneError::TooSmall => write!(f, "file is too small to be worth cloning"),
            CloneError::Refused(why) => write!(f, "the filesystem refused the clone: {why}"),
            CloneError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CloneError {}

impl From<std::io::Error> for CloneError {
    fn from(e: std::io::Error) -> Self {
        CloneError::Io(e)
    }
}

#[cfg(not(windows))]
pub fn clone_whole_file(_src: &Path, _dst: &Path) -> Result<(), CloneError> {
    Err(CloneError::Unsupported)
}

#[cfg(not(windows))]
pub fn volume_supports_cloning(_path: &Path) -> bool {
    false
}

/// Clone `src` to `dst`, falling back to a plain copy if cloning is not possible.
///
/// Returns whether the bytes were actually shared. Callers that care about the
/// distinction — statistics, tests asserting the fast path is live — should look
/// at it, because a silent fallback is otherwise indistinguishable from success.
pub fn clone_or_copy(src: &Path, dst: &Path) -> std::io::Result<bool> {
    match clone_whole_file(src, dst) {
        Ok(()) => Ok(true),
        Err(CloneError::Io(e)) => Err(e),
        Err(_reason) => {
            std::fs::copy(src, dst)?;
            Ok(false)
        }
    }
}
