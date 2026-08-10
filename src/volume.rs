// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Which volume a path sits on, and what that volume does with case.
//!
//! Case folding is not a property of an operating system. macOS ships APFS
//! folding by default and formats it case-sensitive on request; Linux mounts
//! `vfat` and `exfat` that fold; NTFS has per-directory sensitivity. So the
//! question "does `/Users/x/proj` and `/users/x/PROJ` name one directory" has
//! no answer that can be compiled in -- it has to be asked of the filesystem.
//!
//! It has to be asked because basedir matching is a byte comparison. A basedir
//! that does not match is a cache entry that is not shared, which is a slower
//! build; a basedir that matches when the two paths are genuinely different
//! directories is a cache entry served for the wrong tree, which is a wrong
//! build. Folding case unconditionally trades the first mistake for the second,
//! and this codebase does not make that trade anywhere else.
//!
//! **A basedir does not carry the answer.** It stays what it has always been --
//! bytes, normalised, the same on the wire and in `--show-stats`. The answer is
//! looked up from the path when it is needed and cached, so the second lookup
//! and every one after it is a hash of a slice. That keeps the fix out of the
//! protocol entirely: nothing about how a basedir is spelled, stored or
//! transmitted changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, RwLock};

/// What to assume about a volume that cannot be asked.
///
/// The platform's usual answer, so a path that does not exist yet, or whose
/// last component has no letters to flip, behaves the way that platform always
/// has rather than the way some other one does. On Windows the probe never
/// answers at all, so this *is* the Windows behaviour: everything folds,
/// exactly as it did when the matcher lowercased unconditionally.
const UNKNOWN_FOLDS: bool = cfg!(any(
    target_os = "windows",
    target_os = "macos",
    target_os = "ios"
));

/// Answers already paid for.
///
/// Keyed by the path asked about rather than by its volume: resolving a path to
/// a volume is itself a `statfs`, so a volume-keyed cache would still syscall on
/// every lookup and save nothing. The set of paths asked about is the set of
/// basedirs in force, which is one to a handful for the life of the process.
///
/// A volume's case sensitivity is fixed while it is mounted, so nothing here
/// goes stale short of an unmount -- and a build whose basedirs are unmounted
/// underneath it has a larger problem than a cached bool.
static FOLDING: LazyLock<RwLock<HashMap<PathBuf, bool>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Whether the volume under `path` treats `Foo` and `foo` as one name.
///
/// Cached; see [`FOLDING`].
pub fn folds_case(path: &Path) -> bool {
    // A poisoned lock means some other thread panicked while holding it. The
    // contents are still a map of paths to bools -- there is no invariant to
    // have been left half-established -- so recovering beats propagating a
    // panic into the compile path over a cache.
    if let Some(&known) = FOLDING.read().unwrap_or_else(|e| e.into_inner()).get(path) {
        return known;
    }
    let answer = case_sensitive(path).map_or(UNKNOWN_FOLDS, |sensitive| !sensitive);
    FOLDING
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .insert(path.to_path_buf(), answer);
    answer
}

/// [`folds_case`] for a basedir, which is held as bytes rather than as a path.
///
/// Borrows rather than allocates, so a cache hit costs a hash of the bytes.
pub fn basedir_folds_case(basedir: &[u8]) -> bool {
    as_path(basedir).map_or(UNKNOWN_FOLDS, folds_case)
}

#[cfg(unix)]
fn as_path(bytes: &[u8]) -> Option<&Path> {
    use std::os::unix::ffi::OsStrExt;
    Some(Path::new(std::ffi::OsStr::from_bytes(bytes)))
}

/// Basedirs are stored as UTF-8 off Unix, so anything else is not one.
#[cfg(not(unix))]
fn as_path(bytes: &[u8]) -> Option<&Path> {
    std::str::from_utf8(bytes).ok().map(Path::new)
}

/// The nearest ancestor of `path` that exists, including `path` itself.
///
/// The cache directory may not have been created yet on a fresh machine, and
/// "which volume will it land on" is a perfectly answerable question -- the
/// answer is its parent's. Returning `None` there would have made every check
/// say `unknown` for the one case where someone most wants an answer.
pub fn nearest_existing(path: &Path) -> Option<&Path> {
    let mut here = path;
    loop {
        if here.exists() {
            return Some(here);
        }
        here = here.parent()?;
    }
}

#[cfg(windows)]
pub fn root(path: &Path) -> Option<String> {
    let s = path.to_string_lossy().replace('/', "\\");
    let s = s.strip_prefix(r"\\?\").unwrap_or(&s).to_string();
    let b = s.as_bytes();
    (b.len() >= 2 && b[1] == b':' && (b[0] as char).is_ascii_alphabetic())
        .then(|| format!("{}:\\", (b[0] as char).to_ascii_uppercase()))
}

/// The mount point a path sits on.
///
/// A drive letter is what a volume *is* on Windows; it just could not be said
/// anywhere else. Returning `None` for every POSIX path made `same_volume`
/// false by construction, so a correctly configured Mac was warned that its
/// cache and build tree were on different volumes. A diagnostic that cries wolf
/// on a correct machine is worse than no diagnostic.
///
/// Taken from `statfs` where it is available, because that is the same source
/// `df` reports from and it distinguishes volumes that share a device number.
/// On macOS `/` and `/System/Volumes/Data` are two APFS volumes presented under
/// one synthesized root: `stat` gives them the same `st_dev`, and only `statfs`
/// tells them apart.
#[cfg(all(
    unix,
    any(target_os = "macos", target_os = "ios", target_os = "freebsd")
))]
pub fn root(path: &Path) -> Option<String> {
    statfs_of(path).map(|s| {
        // SAFETY: `f_mntonname` is a NUL-terminated C string filled by the
        // kernel, and the cast is between signed and unsigned char.
        let raw = unsafe { std::ffi::CStr::from_ptr(s.f_mntonname.as_ptr()) };
        raw.to_string_lossy().into_owned()
    })
}

/// As above, for platforms without `f_mntonname`.
///
/// Walks up until the device number changes, which is what a mount point *is*
/// and needs no mount table to find.
#[cfg(all(
    unix,
    not(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))
))]
pub fn root(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let start = nearest_existing(path)?;
    let dev = std::fs::metadata(start).ok()?.dev();
    let mut here = start.to_path_buf();
    while let Some(parent) = here.parent() {
        match std::fs::metadata(parent) {
            Ok(m) if m.dev() == dev => here = parent.to_path_buf(),
            _ => break,
        }
    }
    Some(here.to_string_lossy().into_owned())
}

/// `statfs` for the nearest existing ancestor of `path`.
#[cfg(all(
    unix,
    any(target_os = "macos", target_os = "ios", target_os = "freebsd")
))]
pub fn statfs_of(path: &Path) -> Option<libc::statfs> {
    use std::os::unix::ffi::OsStrExt;
    let target = nearest_existing(path)?;
    let c_path = std::ffi::CString::new(target.as_os_str().as_bytes()).ok()?;
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c_path` is NUL-terminated and outlives the call; `buf` is a
    // correctly sized, writable `statfs`.
    let rc = unsafe { libc::statfs(c_path.as_ptr(), &mut buf) };
    (rc == 0).then_some(buf)
}

/// Whether the volume distinguishes `Foo` from `foo`.
///
/// Probed rather than inferred from the filesystem's name, and probed on the
/// path in question: what matters is not what APFS *can* be formatted as but
/// whether this directory spelled two ways resolves to one file. That is
/// exactly what basedir stripping and manifest include paths depend on, and it
/// is a byte comparison in both.
///
/// Read-only: it flips the case of a component that already exists and asks
/// whether the result is the same file, rather than creating anything to find
/// out. A build tool that writes to a directory to answer a question about it
/// is a build tool that fails on a read-only source tree.
#[cfg(unix)]
pub fn case_sensitive(path: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    let target = nearest_existing(path)?;
    let name = target.file_name()?.to_str()?;
    // A component with no letters cannot answer the question either way.
    if !name.chars().any(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let flipped: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                c.to_ascii_uppercase()
            }
        })
        .collect();
    let here = std::fs::metadata(target).ok()?;
    match std::fs::metadata(target.with_file_name(flipped)) {
        // The other spelling reached the very same file: the volume folds case.
        Ok(other) if other.dev() == here.dev() && other.ino() == here.ino() => Some(false),
        // Either nothing is there, or something else is -- both mean the two
        // spellings are two names, which is what case-sensitive means.
        _ => Some(true),
    }
}

#[cfg(windows)]
pub fn case_sensitive(_path: &Path) -> Option<bool> {
    // Per-directory case sensitivity exists on NTFS but is not what any build
    // here relies on, and the probe above needs an inode. Unanswered rather
    // than guessed -- which lands on `UNKNOWN_FOLDS`, i.e. the behaviour
    // Windows has always had.
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drive letter is what a volume *is* on Windows, and nowhere else.
    #[cfg(windows)]
    #[test]
    fn volume_root_handles_the_shapes_paths_actually_take() {
        assert_eq!(root(Path::new(r"D:\cache")).as_deref(), Some("D:\\"));
        // fs::canonicalize hands back verbatim paths; missing this reported every
        // volume as unknown and silently disabled the same-volume check.
        assert_eq!(root(Path::new(r"\\?\D:\cache")).as_deref(), Some("D:\\"));
        assert_eq!(root(Path::new("d:/cache")).as_deref(), Some("D:\\"));
        // A UNC path has no drive letter; claiming one would be a lie.
        assert_eq!(root(Path::new(r"\\server\share")), None);
        assert_eq!(root(Path::new("relative/path")), None);
    }

    /// The defect this replaced: `root` recognised only a drive letter, so every
    /// POSIX path answered `None`, `same_volume` was false by construction, and
    /// a correctly configured Mac was warned that its cache and build tree were
    /// on different volumes.
    #[cfg(unix)]
    #[test]
    fn a_posix_path_gets_a_real_volume() {
        let mount = root(Path::new("/")).expect("the root filesystem has a mount point");
        assert!(
            mount.starts_with('/'),
            "a mount point is an absolute path, got {mount:?}"
        );

        // The point of the whole exercise: two paths on one filesystem agree.
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c");
        assert_eq!(
            root(dir.path()),
            root(&deep),
            "a path and its own subdirectory cannot be on different volumes"
        );

        // And it answers for a path that does not exist yet, which is the state
        // a cache directory is in on a machine that has never run a build.
        assert!(
            root(&deep).is_some(),
            "the volume a directory will be created on is answerable"
        );
    }

    /// Case folding decides whether one checkout has one cache key or two, so
    /// this has to be found out without writing anything.
    #[cfg(unix)]
    #[test]
    fn case_sensitivity_is_probed_not_assumed() {
        let dir = tempfile::tempdir().unwrap();
        let mixed = dir.path().join("MixedCase");
        std::fs::create_dir(&mixed).unwrap();

        let answer = case_sensitive(&mixed).expect("a name with letters can be probed");
        // Whichever way this volume goes, the probe must agree with what the
        // filesystem actually does rather than with what its type suggests.
        let folded = dir.path().join("mixedcase");
        assert_eq!(
            answer,
            !folded.exists(),
            "the probe disagreed with the filesystem: {} exists = {}",
            folded.display(),
            folded.exists()
        );
    }

    /// The cache has to answer what the probe answers, or it is a bug generator
    /// rather than an optimisation.
    #[test]
    fn the_cached_answer_is_the_probed_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mixed = dir.path().join("MixedCase");
        std::fs::create_dir(&mixed).unwrap();

        let first = folds_case(&mixed);
        assert_eq!(
            first,
            folds_case(&mixed),
            "a second lookup changed its mind"
        );
        #[cfg(unix)]
        assert_eq!(
            first,
            case_sensitive(&mixed) == Some(false),
            "the cache and the probe disagree"
        );
    }

    /// The bytes a basedir is actually held as reach the same answer as the path
    /// it names -- including the trailing separator every basedir carries.
    #[test]
    fn a_basedir_is_asked_the_same_question_as_its_path() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("Proj");
        std::fs::create_dir(&proj).unwrap();

        let mut bytes = proj.to_str().unwrap().as_bytes().to_vec();
        bytes.push(b'/');
        assert_eq!(basedir_folds_case(&bytes), folds_case(&proj));
    }

    /// Nothing to probe is not a licence to invent an answer: it falls back to
    /// what this platform does, which is what every caller assumed before there
    /// was a probe at all.
    #[test]
    fn an_unprobeable_path_gets_the_platform_answer() {
        assert_eq!(basedir_folds_case(b""), UNKNOWN_FOLDS);
        assert_eq!(folds_case(Path::new("/0/1/2/")), UNKNOWN_FOLDS);
    }
}
