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
pub use windows::{clone_region_into, clone_whole_file, volume_supports_cloning};

/// For tests that must prove storage is *shared*, not merely equal.
#[cfg(all(test, windows))]
pub(crate) use windows::extent_lcns;

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

/// See the Windows implementation; nothing else has one yet. A reflink-capable
/// Unix (btrfs, XFS) would implement this with `copy_file_range`, which shares
/// extents on those filesystems when the ranges are aligned.
#[cfg(not(windows))]
pub fn clone_region_into(
    _src: &Path,
    _src_offset: u64,
    _len: u64,
    _dst: &std::fs::File,
) -> Result<(), CloneError> {
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

/// What cloning would be worth on APFS, which has no implementation here.
///
/// The module above is `#[cfg(windows)]` in its entirety, so every cache hit on
/// macOS copies its bytes. This is the measurement that says whether that is
/// worth changing, kept next to the gap it describes rather than in a shell
/// script nobody runs. A stopwatch, not an assertion:
///
/// ```text
/// cargo test --release --lib clone_versus_copy -- --ignored --nocapture
/// ```
///
/// `clonefile` is a metadata operation: it does not scale with size, so the
/// interesting column is the ratio at the sizes real objects are. Measured
/// in-process for a reason -- `cp -c` against `cp` reports 31 ms against 32,
/// which is two process spawns and no information.
#[cfg(all(test, target_os = "macos"))]
mod clone_versus_copy {
    use std::io::{Read, Write};

    fn clonefile(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &std::path::Path| std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        let (s, d) = (c(src), c(dst));
        // SAFETY: both pointers are NUL-terminated and live across the call.
        let rc = unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 0) };
        if rc == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn best(rounds: usize, mut f: impl FnMut()) -> f64 {
        let mut best = f64::MAX;
        for _ in 0..rounds {
            let t = std::time::Instant::now();
            f();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        best
    }

    #[test]
    #[ignore = "a stopwatch, not an assertion; run it deliberately"]
    fn what_cloning_would_buy_on_apfs() {
        let dir = tempfile::tempdir().expect("tempdir");
        println!("\nAPFS clonefile vs read/write copy, best of 50\n");

        for size in [256 * 1024usize, 2 * 1024 * 1024, 32 * 1024 * 1024] {
            let src = dir.path().join(format!("src_{size}"));
            let dst = dir.path().join("dst");
            std::fs::write(&src, vec![0xA5u8; size]).expect("write source");

            let cloned = best(50, || {
                let _ = std::fs::remove_file(&dst);
                clonefile(&src, &dst).expect("clonefile");
            });
            let copied = best(50, || {
                let _ = std::fs::remove_file(&dst);
                let mut r = std::fs::File::open(&src).expect("open");
                let mut w = std::fs::File::create(&dst).expect("create");
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    match r.read(&mut buf).expect("read") {
                        0 => break,
                        n => w.write_all(&buf[..n]).expect("write"),
                    }
                }
            });
            println!(
                "  {:>5} KiB   clonefile {cloned:>7.3} ms   copy {copied:>7.3} ms   {:>5.1}x",
                size / 1024,
                copied / cloned,
            );
        }
    }
}
