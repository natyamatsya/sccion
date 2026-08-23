// Copyright 2023 Mozilla Foundation
// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The preprocessor cache entry is a description of all information needed
//! to cache pre-processor output in C-family languages for a given input file.
//! The current implementation is very much inspired from the "manifest"
//! that `ccache` uses for its "direct mode", though the on-disk format is
//! different.

use std::collections::HashMap;
use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    hash::Hash,
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock},
    time::SystemTime,
};

use anyhow::Context;
use chrono::Datelike;
use serde::{Deserialize, Serialize};

use crate::{
    config::PreprocessorCacheModeConfig,
    util::{
        DIGEST_BYTES, Digest, HashToDigest, MetadataCtimeExt, TimeMacroFinder, Timestamp,
        encode_path, strip_basedirs, strip_basedirs_in_arg, strip_basedirs_in_dir,
    },
};

use super::{Language, NotCacheable};

/// The current format is 1 header byte for the version + bincode encoding
/// of the [`PreprocessorCacheEntry`] struct.
///
/// This number does two jobs. `read` rejects an entry whose header byte is not
/// this one, and [`preprocessor_cache_entry_hash_key`] hashes it, so a bump
/// serves both a struct that has changed shape and a key that has changed
/// inputs. Either way the effect is the same: entries from before the bump stop
/// being found, which costs one cold build and cannot be mistaken for a
/// legitimate miss.
///
/// - **1** (`7ea3117`): include paths became relative to a basedir. An entry
///   written before it holds absolute paths that the new code resolves against
///   a basedir and does not find.
/// - **2** (`9c0c80f`): the working directory joined the key, so
///   `hash_working_directory` finally did what its name says. A key input
///   changed rather than the struct.
/// - **3** (`4c21b17`): `IncludeEntry::digest` became the digest's 32 bytes
///   instead of its 64 hex characters, which bincode encodes differently.
const FORMAT_VERSION: u8 = 3;
const MAX_PREPROCESSOR_CACHE_ENTRIES: usize = 100;
const MAX_PREPROCESSOR_CACHE_FILE_INFO_ENTRIES: usize = 10000;

/// How an include is written down: relative to its basedir when it has one.
///
/// A path that no basedir covers -- a system header, the Qt headers -- stays
/// absolute, which is right: it is at the same place for every checkout.
fn portable_path(path: PathBuf, basedirs: &[Vec<u8>]) -> OsString {
    if basedirs.is_empty() {
        return path.into_os_string();
    }
    let stripped = strip_basedirs_in_arg(path.as_os_str().as_encoded_bytes(), basedirs);
    // Only rewrite when the result is still a path this platform can name.
    // Anything else is stored as it was: absolute, and no worse than before.
    match std::str::from_utf8(&stripped) {
        Ok(s) => OsString::from(s),
        Err(_) => path.into_os_string(),
    }
}

/// How many includes to stat at once when the cache runs dry.
///
/// A `stat` here costs ~13 us of filter-stack latency and almost no CPU, so
/// they overlap nearly perfectly. The batch is what keeps the loop's cheap
/// exit: the loop still stops at the first include that does not match, and a
/// stale first file costs one batch rather than a whole entry.
///
/// Larger batches buy more overlap and waste more on an early miss, so the
/// batch starts here and doubles as the entry proves itself: a stale *first*
/// include costs eight stats, while an entry that keeps matching widens fast.
///
/// There is deliberately no ceiling. "Twice what has already matched" bounds
/// the waste by work that has already paid off, whatever the absolute size, and
/// a ceiling only adds batches -- each of which is a round trip through the
/// blocking pool. Capping at 128 cost 2000 includes about 27%.
const STAT_BATCH_MIN: usize = 8;

/// How many includes one blocking task stats before another is spawned.
///
/// A `spawn_blocking` is not free, and off Windows this path used to spend one
/// per include: task allocation, a channel send, a thread wakeup and a
/// `JoinHandle` await, to wrap a single `stat`. Measured on APFS the syscall is
/// 2.08 us and the dispatch around it was 7.8 us -- so the pool was costing
/// nearly four times what it was buying, and statting every include inline on
/// one thread beat it 4.7x.
///
/// Batching the dispatch instead of the syscall puts that back: 9.84 us per
/// include becomes 1.41 at sixteen per task, 1.33 at sixty-four and 1.49 at two
/// hundred and fifty-six. The floor is the concurrent syscall itself, and
/// anything from a few dozen up reaches it, so this is chosen well inside the
/// flat part rather than tuned to it.
///
/// Fixed-size, not grouped by directory. Grouping was measured and buys nothing
/// -- shuffling the list so no task shares a parent changes nothing, which says
/// there is no locality to capture -- while costing 47% on a lopsided tree,
/// where one directory of 400 headers becomes one task and eleven threads idle.
/// The Windows path groups by directory for a different reason: there a scan
/// *is* the metadata.
const STAT_CHUNK: usize = 64;

/// One directory scan in place of a `stat` per file in it.
///
/// On Windows a directory scan is `FindFirstFileW`/`FindNextFileW`, which
/// carries size and timestamps for every entry, so the metadata arrives as a
/// side effect of walking the directory rather than as a call per file. Includes
/// cluster into a few dozen directories, so this trades many small waits for a
/// few larger ones.
///
/// It pays for the whole directory whether or not every file in it was wanted,
/// so it wins on density and merely breaks even when sparse. It also reads all
/// of a directory at one moment rather than each file at its own, which is the
/// more consistent reading of a tree and not a weaker one.
///
/// Off Windows there is nothing to gain: `DirEntry::metadata` still issues a
/// call per file, so the per-file path below is already the same work.
///
/// What it gives up is worth stating, because the two calls are not the same
/// question asked twice. `DirEntry::metadata` on Windows hands back the
/// `WIN32_FIND_DATAW` the scan already returned -- no handle, no second call --
/// while `symlink_metadata` opens the file and asks
/// `GetFileInformationByHandle`. A file's *directory entry* is updated lazily,
/// so while a writer holds it open the entry can still describe the file as it
/// was before that writer started. The straggler path below still uses
/// `symlink_metadata`, so without care two includes in one lookup would be
/// judged against different readings of the same tree.
///
/// [`SCAN_TRUST_AGE`] is the care. A recently written file is sent to the
/// straggler path to be asked directly, which costs nothing in a build whose
/// headers are hours old and is exactly the file worth spending a call on.
/// Counts scans, so a test can ask how many were issued. Test-only.
#[cfg(all(windows, test))]
pub(crate) static SCANS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// How recently a file must have been written for a scan's answer to be refused.
///
/// A directory entry lags a writer that still has the file open, so the one file
/// a scan can describe wrongly is the one being written right now. Age is how
/// that is recognised without opening anything: a file untouched for this long
/// has no writer whose buffered size and timestamps have yet to land.
///
/// Two seconds because the risk is a writer mid-flight, not a clock skewed by
/// minutes -- and because the cost of being wrong in the safe direction is one
/// `stat`. In a warm build every header predates the build by hours, so this
/// sends nothing extra to the straggler path; in the case it exists for, it
/// sends exactly the file that matters.
#[cfg(windows)]
const SCAN_TRUST_AGE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a scan's answer for one file is settled enough to be believed.
///
/// `cutoff` is [`SCAN_TRUST_AGE`] ago, computed once per batch. `mtime` and
/// `ctime` come out of the same directory entry and go stale together, so the
/// later of the two is what says whether that entry has stopped moving --
/// the same `max` [`PreprocessorCacheEntry::add_result`] uses to decide whether
/// a timestamp is worth recording at all.
///
/// Anything unanswerable -- no timestamps, no cutoff -- is not settled. The
/// caller's fallback is a real `stat`, so the cautious answer is also the
/// correct one.
#[cfg(windows)]
fn scan_is_settled(facts: &FileFacts, cutoff: Option<Timestamp>) -> bool {
    let touched = match (facts.mtime, facts.ctime) {
        (Some(m), Some(c)) => m.max(c),
        (Some(t), None) | (None, Some(t)) => t,
        (None, None) => return false,
    };
    cutoff.is_some_and(|cutoff| touched < cutoff)
}

#[cfg(windows)]
fn stat_directory(dir: &Path) -> HashMap<OsString, FileFacts> {
    #[cfg(test)]
    SCANS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut found = HashMap::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            // `DirEntry::metadata` does not traverse symlinks, which is the
            // same promise `symlink_metadata` makes. It does not open the file
            // either, which is the speedup and the caveat both -- see
            // `SCAN_TRUST_AGE`.
            if let Ok(meta) = entry.metadata() {
                found.insert(entry.file_name(), FileFacts::from(&meta));
            }
        }
    }
    found
}

/// What one lookup has already learned about the files on disk.
///
/// Keyed by the *stored* path, so a hit skips resolving as well as stat'ing.
/// The error is kept as an [`std::io::ErrorKind`] because `io::Error` is not
/// `Clone`, and the kind is what the caller logs.
type StatCache = HashMap<OsString, (PathBuf, Result<FileFacts, std::io::ErrorKind>)>;

/// The three things the first pass reads off a file.
///
/// `std::fs::Metadata` is larger than this, is not `Copy`, and was being cloned
/// twice per include -- once into the lookup's cache and once back out. Worse,
/// the timestamps were converted on every comparison, so an entry with several
/// results converted the same file's times once per result.
///
/// Taken once where the metadata is read, and a `Copy` of forty bytes
/// thereafter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct FileFacts {
    len: u64,
    mtime: Option<Timestamp>,
    ctime: Option<Timestamp>,
}

impl From<&std::fs::Metadata> for FileFacts {
    fn from(meta: &std::fs::Metadata) -> Self {
        Self {
            len: meta.len(),
            mtime: meta.modified().map(Into::into).ok(),
            ctime: meta.ctime_or_creation().ok(),
        }
    }
}

/// What one lookup remembers while it runs.
///
/// Both halves exist for the same reason -- a lookup asks the filesystem the
/// same questions repeatedly, across results and across read-ahead batches --
/// so they are created together, live exactly as long as each other, and are
/// passed as one thing.
#[derive(Default)]
pub(crate) struct LookupCaches {
    /// What is known about individual files.
    seen: StatCache,
    /// What is known about the directories they sit in.
    #[cfg(windows)]
    dirs: DirCache,
}

/// The directories this lookup has already read.
///
/// A batch bounds how many directories get scanned, not how many times each is
/// scanned. Without this the read-ahead batches and the directory grouping do
/// not know about each other and re-enumerate the same directory: measured at
/// 14 scans for 8 directories, and 34 for 16.
#[cfg(windows)]
type DirCache = HashMap<PathBuf, HashMap<OsString, FileFacts>>;

/// Where a recorded include is, when that needs no guessing.
///
/// Valid only for an absolute stored path or a single basedir; with more, which
/// root a path belongs under is decided by which candidate exists, and that is
/// a question only the filesystem answers -- see [`resolve_and_stat`].
#[cfg(windows)]
fn resolve_without_stat(stored: &OsStr, basedirs: &[Vec<u8>]) -> PathBuf {
    let path = Path::new(stored);
    if path.is_absolute() || basedirs.is_empty() {
        return path.to_path_buf();
    }
    std::str::from_utf8(&basedirs[0])
        .ok()
        .map(|root| Path::new(root).join(path))
        .unwrap_or_else(|| path.to_path_buf())
}

/// Where a recorded include is, and what it looks like, in one pass.
///
/// [`resolve_include`] asks `exists` of each candidate and the caller then asks
/// `symlink_metadata` of the winner: two syscalls to learn one thing, on a path
/// whose whole cost is syscalls. `symlink_metadata` answers both questions at
/// once -- a candidate that can be stat'd is the candidate that exists -- so
/// trying it directly halves the calls whenever more than one basedir is
/// configured. With one basedir, or an absolute stored path, there is no guess
/// to make and this is exactly what the old code did.
fn resolve_and_stat(stored: &OsStr, basedirs: &[Vec<u8>]) -> (PathBuf, std::io::Result<FileFacts>) {
    let path = Path::new(stored);
    if path.is_absolute() || basedirs.is_empty() {
        let owned = path.to_path_buf();
        let meta = std::fs::symlink_metadata(&owned).map(|m| FileFacts::from(&m));
        return (owned, meta);
    }
    let rooted = |basedir: &Vec<u8>| {
        std::str::from_utf8(basedir)
            .ok()
            .map(|root| Path::new(root).join(path))
    };
    if basedirs.len() == 1
        && let Some(p) = rooted(&basedirs[0])
    {
        let meta = std::fs::symlink_metadata(&p).map(|m| FileFacts::from(&m));
        return (p, meta);
    }
    let mut last: Option<(PathBuf, std::io::Error)> = None;
    for candidate in basedirs.iter().filter_map(rooted) {
        match std::fs::symlink_metadata(&candidate) {
            Ok(meta) => return (candidate, Ok(FileFacts::from(&meta))),
            Err(e) => last = Some((candidate, e)),
        }
    }
    // Nothing resolved. Report against the stored path, as before, so the log
    // line names what was recorded rather than the last guess that failed.
    match last {
        Some((_, e)) => (path.to_path_buf(), Err(e)),
        None => {
            let owned = path.to_path_buf();
            let meta = std::fs::symlink_metadata(&owned).map(|m| FileFacts::from(&m));
            (owned, meta)
        }
    }
}

#[derive(Clone, Deserialize, Serialize, Debug, Default, PartialEq, Eq)]
pub struct PreprocessorCacheEntry {
    /// A counter of the overall number of [`IncludeEntry`] in this
    /// preprocessor cache entry, as an optimization when checking
    /// we're not ballooning in size.
    number_of_entries: usize,
    /// The digest of a result is computed by hashing the output of the
    /// C preprocessor. Entries correspond to the included files during the
    /// preprocessing step.
    results: BTreeMap<String, Vec<IncludeEntry>>,
}

impl PreprocessorCacheEntry {
    pub fn new() -> Self {
        Default::default()
    }

    /// Tries to deserialize a preprocessor cache entry from `contents`
    pub fn read(contents: &[u8]) -> Result<Self, Error> {
        if contents.is_empty() {
            Ok(Self {
                number_of_entries: 0,
                results: Default::default(),
            })
        } else if contents[0] != FORMAT_VERSION {
            Err(Error::UnknownFormat(contents[0]))
        } else {
            Ok(bincode::deserialize(&contents[1..])?)
        }
    }

    /// Serialize the preprocessor cache entry to `buf`
    pub fn serialize_to(&self, mut buf: impl Write) -> Result<(), Error> {
        // Add the starting byte for version check since `bincode` doesn't
        // support it.
        buf.write_all(&[FORMAT_VERSION])?;
        bincode::serialize_into(buf, self)?;
        Ok(())
    }

    /// Insert the full compilation key and included files for a given source file.
    ///
    /// There can be more than one result at once for a source file if one
    /// or more of the include files has changed but not the source file.
    ///
    /// Paths under a basedir are stored with that prefix removed, so that a
    /// second checkout resolves them against *its own* root and checks its own
    /// files. Storing them absolute would either pin the entry to one checkout
    /// or, once the key stopped being pinned, validate one checkout's manifest
    /// against another's files -- which is how a stale object gets served.
    pub fn add_result(
        &mut self,
        compilation_time_start: SystemTime,
        result_key: &str,
        included_files: impl IntoIterator<Item = (String, PathBuf)>,
        basedirs: &[Vec<u8>],
    ) {
        if self.results.len() > MAX_PREPROCESSOR_CACHE_ENTRIES {
            // Normally, there shouldn't be many result entries in the
            // preprocessor cache entry since new entries are added only if
            // an include file has changed but not the source file, and you
            // typically change source files more often than header files.
            // However, it's certainly possible to imagine cases where the
            // preprocessor cache entry will grow large (for instance,
            // a generated header file that changes for every build), and this
            // must be taken care of since processing an ever growing
            // preprocessor cache entry eventually will take too much time.
            // A good way of solving this would be to maintain the
            // result entries in LRU order and discarding the old ones.
            // An easy way is to throw away all entries when there are too many.
            // Let's do that for now.
            debug!(
                "Too many entries in preprocessor cache entry file ({}/{}), starting over",
                self.results.len(),
                MAX_PREPROCESSOR_CACHE_ENTRIES
            );
            self.results.clear();
            self.number_of_entries = 0;
        }
        let includes: Result<Vec<_>, std::io::Error> = included_files
            .into_iter()
            .map(|(digest, path)| {
                let meta = std::fs::symlink_metadata(&path)?;
                let mtime: Option<Timestamp> = meta.modified().ok().map(|t| t.into());
                let ctime = meta.ctime_or_creation().ok();

                let should_cache_time = match (mtime, ctime) {
                    (Some(mtime), Some(ctime)) => {
                        Timestamp::from(compilation_time_start) > mtime.max(ctime)
                    }
                    _ => false,
                };
                // Callers hand these over hexed, because that is what
                // `Digest::finish` returns and what everything else wants.
                let digest = crate::util::unhex_digest(&digest).ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "include digest was not a digest",
                    )
                })?;
                Ok(IncludeEntry {
                    path: portable_path(path, basedirs),
                    digest,
                    file_size: meta.len(),
                    mtime: if should_cache_time { mtime } else { None },
                    ctime: if should_cache_time { ctime } else { None },
                })
            })
            .collect();
        match includes {
            Ok(includes) => {
                let new_number_of_entries = includes.len() + self.number_of_entries;
                if new_number_of_entries > MAX_PREPROCESSOR_CACHE_FILE_INFO_ENTRIES {
                    // Rarely, entries can grow large in pathological cases
                    // where many included files change, but the main file
                    // does not. This also puts an upper bound on the number
                    // of entries.
                    debug!(
                        "Too many include entries in preprocessor cache entry file ({}/{}), starting over",
                        new_number_of_entries, MAX_PREPROCESSOR_CACHE_FILE_INFO_ENTRIES
                    );
                    self.results.clear();
                }
                match self.results.entry(result_key.to_string()) {
                    std::collections::btree_map::Entry::Occupied(mut entry) => {
                        self.number_of_entries -= entry.get().len();
                        self.number_of_entries += includes.len();
                        *entry.get_mut() = includes;
                    }
                    std::collections::btree_map::Entry::Vacant(vacant) => {
                        self.number_of_entries += includes.len();
                        vacant.insert(includes);
                    }
                }
                debug!("Added result key {result_key} to preprocessor cache entry");
            }
            Err(e) => {
                debug!("Could not add result key {result_key} to preprocessor cache entry: {e}");
            }
        }
    }

    /// Returns the digest of the first result whose expected included files
    /// are already on disk and have not changed.
    pub async fn lookup_result_digest(
        &mut self,
        config: PreprocessorCacheModeConfig,
        updated: &mut bool,
        pool: &tokio::runtime::Handle,
        basedirs: &[Vec<u8>],
    ) -> Option<String> {
        // One `stat` per distinct file, not one per file *per result*.
        //
        // Results in an entry are different preprocessor outcomes of the same
        // translation unit, so their include sets overlap almost entirely. The
        // loop below stops at the first result that matches, which makes the
        // repetition invisible when the newest one hits -- and expensive
        // exactly when it does not, since every result that misses re-stats
        // files the previous one just asked about.
        //
        // Scoped to this lookup, so it is a snapshot of one moment rather than
        // one per result. That is the more consistent reading of a tree, not a
        // weaker one.
        let mut caches = LookupCaches::default();

        // Check newest result first since it's more likely to match.
        for (digest, includes) in self.results.iter_mut().rev() {
            let result_matches = Self::result_matches(
                digest,
                includes,
                config,
                updated,
                pool,
                basedirs,
                &mut caches,
            )
            .await;
            if result_matches {
                return Some(digest.clone());
            }
        }
        None
    }

    /// Stat the next batch of unknown includes at once.
    ///
    /// The work is waiting, not computing, so it goes to the blocking pool
    /// where waiting is what the threads are for. Failures are recorded exactly
    /// as the sequential path records them, so this changes when the answers
    /// arrive and not what they are.
    async fn prefetch_stats(
        rest: &[IncludeEntry],
        validated: usize,
        basedirs: &[Vec<u8>],
        pool: &tokio::runtime::Handle,
        caches: &mut LookupCaches,
    ) {
        let wanted: Vec<OsString> = rest
            .iter()
            .map(|i| i.path.clone())
            .filter(|p| !caches.seen.contains_key(p))
            // Twice what this result has already got right, so reading ahead is
            // always bounded by work that has already paid off.
            .take((validated * 2).max(STAT_BATCH_MIN))
            .collect();
        if wanted.len() < 2 {
            // One file is not worth a thread; the caller resolves it inline.
            return;
        }

        // With at most one basedir every stored path resolves without asking
        // the filesystem which root it belongs under, so the resolved paths are
        // known up front and can be grouped by directory. With more, resolution
        // *is* the stat, and there is nothing to group.
        #[cfg(windows)]
        if basedirs.len() <= 1 {
            Self::prefetch_by_directory(wanted, basedirs, pool, caches).await;
            return;
        }

        // One `Arc` for the batch rather than a copy of the list per include.
        // Every task needs the same basedirs and none of them modify it, so the
        // clone per include was a `Vec<Vec<u8>>` allocated and dropped for each
        // file on the path this batching exists to make cheap.
        let shared: Arc<[Vec<u8>]> = basedirs.into();
        let mut tasks = Vec::with_capacity(wanted.len().div_ceil(STAT_CHUNK));
        // `take` off the iterator rather than `chunks`, so each path is *moved*
        // into its task. `chunks` would hand out slices to clone from, which is
        // an allocation per include on the path being made cheap.
        let mut wanted = wanted.into_iter();
        loop {
            let chunk: Vec<OsString> = wanted.by_ref().take(STAT_CHUNK).collect();
            if chunk.is_empty() {
                break;
            }
            let basedirs = Arc::clone(&shared);
            tasks.push(pool.spawn_blocking(move || {
                chunk
                    .into_iter()
                    .map(|stored| {
                        let (path, meta) = resolve_and_stat(&stored, &basedirs);
                        (stored, path, meta.map_err(|e| e.kind()))
                    })
                    .collect::<Vec<_>>()
            }));
        }
        for task in tasks {
            // A panicked or cancelled chunk is simply not cached; the loop then
            // resolves those inline and behaves as it always did.
            if let Ok(done) = task.await {
                for (stored, path, meta) in done {
                    caches.seen.insert(stored, (path, meta));
                }
            }
        }
    }

    /// Read ahead a batch by scanning the directories it falls in.
    ///
    /// One task per directory rather than one per file, so the number of waits
    /// drops to the number of directories. Anything a scan does not account for
    /// -- an unreadable directory, a file that appeared since, a name the scan
    /// missed, or one written too recently for its directory entry to be
    /// trusted ([`SCAN_TRUST_AGE`]) -- falls back to a plain `stat` for that
    /// file alone, so the answer is the same set of answers either way.
    #[cfg(windows)]
    async fn prefetch_by_directory(
        wanted: Vec<OsString>,
        basedirs: &[Vec<u8>],
        pool: &tokio::runtime::Handle,
        caches: &mut LookupCaches,
    ) {
        let resolved: Vec<(OsString, PathBuf)> = wanted
            .into_iter()
            .map(|stored| {
                let path = resolve_without_stat(&stored, basedirs);
                (stored, path)
            })
            .collect();

        let mut by_dir: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        for (index, (_, path)) in resolved.iter().enumerate() {
            by_dir
                .entry(path.parent().unwrap_or(Path::new("")).to_path_buf())
                .or_default()
                .push(index);
        }

        // Only what this lookup has not already read.
        let fresh: Vec<PathBuf> = by_dir
            .keys()
            .filter(|d| !caches.dirs.contains_key(*d))
            .cloned()
            .collect();
        let mut scans = Vec::with_capacity(fresh.len());
        for dir in fresh {
            scans.push(pool.spawn_blocking(move || {
                let found = stat_directory(&dir);
                (dir, found)
            }));
        }
        for scan in scans {
            if let Ok((dir, found)) = scan.await {
                caches.dirs.insert(dir, found);
            }
        }

        // Once per batch, not once per file: the point is which files were
        // written *recently*, and the few milliseconds this loop takes are not
        // the scale that decides that.
        let cutoff = SystemTime::now()
            .checked_sub(SCAN_TRUST_AGE)
            .map(Timestamp::from);

        let mut stragglers = Vec::new();
        for (stored, path) in resolved {
            let hit = path
                .parent()
                .and_then(|d| caches.dirs.get(d))
                .and_then(|found| path.file_name().and_then(|n| found.get(n)))
                .copied();
            match hit {
                // A scan answered, and the file has been still long enough for
                // that answer to be the file's own rather than its directory
                // entry's memory of it.
                Some(meta) if scan_is_settled(&meta, cutoff) => {
                    caches.seen.insert(stored, (path, Ok(meta)));
                }
                // Either no scan covered it, or one did and the file is too
                // fresh to take on the entry's word. Both are asked directly.
                _ => stragglers.push((stored, path)),
            }
        }

        // Whatever the scans could not answer, asked the old way.
        let mut tasks = Vec::with_capacity(stragglers.len());
        for (stored, path) in stragglers {
            tasks.push(pool.spawn_blocking(move || {
                let meta = std::fs::symlink_metadata(&path)
                    .map(|m| FileFacts::from(&m))
                    .map_err(|e| e.kind());
                (stored, path, meta)
            }));
        }
        for task in tasks {
            if let Ok((stored, path, meta)) = task.await {
                caches.seen.insert(stored, (path, meta));
            }
        }
    }

    /// A result matches if all of its include files exist on disk and have not changed.
    ///
    /// Done in two passes. The first is sequential and cheap -- a `stat` per
    /// include, which rejects the common "file changed size" case without reading
    /// a byte of content, and applies the mtime/ctime shortcut. Only includes that
    /// still need their contents compared reach the second pass, which reads and
    /// hashes them concurrently.
    ///
    /// The split matters because the cost here is dominated by `open()` latency
    /// through the filesystem filter stack rather than by hashing throughput, so
    /// overlapping the opens is where the time goes. It also preserves the cheap
    /// short-circuit: a mismatch found by `stat` still costs nothing.
    async fn result_matches(
        digest: &str,
        includes: &mut [IncludeEntry],
        config: PreprocessorCacheModeConfig,
        updated: &mut bool,
        pool: &tokio::runtime::Handle,
        basedirs: &[Vec<u8>],
        caches: &mut LookupCaches,
    ) -> bool {
        // Resolved against the files of the checkout being compiled, not of
        // whichever one happened to write the entry -- and resolved *as we go*.
        // The loop below returns on the first file that does not match, so
        // resolving all of them up front made an early miss pay for a whole
        // entry it never looked at.
        // Only the includes whose contents still have to be compared keep their
        // resolved path. The stat-only path settles every file without opening
        // one, and building a `PathBuf` per include for a second pass that
        // usually does not happen is a clone per include thrown away.
        let mut needs_contents: Vec<(usize, PathBuf)> = Vec::new();

        for (index, include) in includes.iter().enumerate() {
            // Read ahead when the cache runs dry. Everything the batch learns
            // lands in the cache, so the comparison below is unchanged -- it
            // just stops finding it empty.
            //
            // One lookup, not a `contains_key` followed by a `get`: the stored
            // path is hashed once per include per result, and that is the term
            // this loop is made of.
            // Make sure the cache holds this include, then read it by
            // reference. Taking it by value cost a `PathBuf` clone for every
            // include, and only the few that reach the second pass need one.
            if !caches.seen.contains_key(&include.path) {
                Self::prefetch_stats(&includes[index..], index, basedirs, pool, caches).await;
                if !caches.seen.contains_key(&include.path) {
                    let (path, meta) = resolve_and_stat(&include.path, basedirs);
                    // The error is kept as its kind: `io::Error` is not `Clone`,
                    // and the kind is what the log line below actually says.
                    caches
                        .seen
                        .insert(include.path.clone(), (path, meta.map_err(|e| e.kind())));
                }
            }
            let Some((candidate, stat)) = caches.seen.get(&include.path) else {
                // Unreachable: it was just put there. Declining is still the
                // right answer if it somehow is not.
                return false;
            };
            let stat = *stat;
            let path = candidate.as_path();
            let meta = match stat {
                Ok(facts) => {
                    if facts.len != include.file_size {
                        return false;
                    }
                    facts
                }
                Err(e) => {
                    debug!(
                        "{} is in a preprocessor cache entry but can't be read ({})",
                        path.display(),
                        e
                    );
                    return false;
                }
            };

            if config.file_stat_matches {
                match (include.mtime, include.ctime) {
                    (Some(mtime), Some(ctime)) if config.use_ctime_for_stat => {
                        let mtime_matches = meta.mtime == Some(mtime);
                        let ctime_matches = meta.ctime == Some(ctime);
                        if mtime_matches && ctime_matches {
                            trace!("mtime+ctime hit for {}", path.display());
                            continue;
                        } else {
                            trace!("mtime+ctime miss for {}", path.display());
                        }
                    }
                    (Some(mtime), None) => {
                        let mtime_matches = meta.mtime == Some(mtime);
                        if mtime_matches {
                            trace!("mtime hit for {}", path.display());
                            continue;
                        } else {
                            trace!("mtime miss for {}", path.display());
                        }
                    }
                    _ => { /* Nothing was recorded, fall back to contents comparison */ }
                }
            }

            needs_contents.push((index, candidate.clone()));
        }

        // Everything was settled by `stat`. With `file_stat_matches` enabled this
        // is the common case, and no file is opened at all.
        if needs_contents.is_empty() {
            return true;
        }

        let digested = digest_files_concurrently(
            needs_contents.iter().map(|(_, path)| path.clone()),
            config.ignore_time_macros,
            pool,
        )
        .await;

        // Fold sequentially, in the original order, so the outcome does not depend
        // on which file happened to finish first.
        for ((index, resolved), outcome) in needs_contents.into_iter().zip(digested) {
            let path = resolved.as_path();
            let include = &mut includes[index];

            let Some(contents) = outcome else {
                return false;
            };

            let finder = match contents {
                FileContents::Plain(new_digest) => {
                    if crate::util::unhex_digest(&new_digest) != Some(include.digest) {
                        return false;
                    }
                    // Matched: go on to the next include. Returning here would
                    // decide the whole entry on its first file.
                    continue;
                }
                FileContents::WithTimeMacros(new_digest, finder) => {
                    if !finder.found_time_macros()
                        && crate::util::unhex_digest(&new_digest) != Some(include.digest)
                    {
                        return false;
                    }
                    finder
                }
            };
            {
                if finder.found_time() {
                    // We don't know for sure that the program actually uses the __TIME__ macro,
                    // but we have to assume it anyway and hash the time stamp. However, that's
                    // not very useful since the chance that we get a cache hit later the same
                    // second should be quite slim... So, just signal back to the caller that
                    // __TIME__ has been found so that the preprocessor cache mode can be disabled.
                    debug!("Found __TIME__ in {}", path.display());
                    return false;
                }

                // __DATE__ or __TIMESTAMP__ found. We now make sure that the digest changes
                // if the (potential) expansion of those macros changes by computing a new
                // digest comprising the file digest and time information that represents the
                // macro expansions.
                let mut new_digest = Digest::new();
                new_digest.update(digest.as_bytes());

                if finder.found_date() {
                    debug!("found __DATE__ in {}", path.display());
                    new_digest.delimiter(b"date");
                    let date = chrono::Local::now().date_naive();
                    new_digest.update(&date.year().to_le_bytes());
                    new_digest.update(&date.month().to_le_bytes());
                    new_digest.update(&date.day().to_le_bytes());

                    // If the compiler has support for it, the expansion of __DATE__ will change
                    // according to the value of SOURCE_DATE_EPOCH. Note: We have to hash both
                    // SOURCE_DATE_EPOCH and the current date since we can't be sure that the
                    // compiler honors SOURCE_DATE_EPOCH.
                    if let Ok(source_date_epoch) = std::env::var("SOURCE_DATE_EPOCH") {
                        new_digest.update(source_date_epoch.as_bytes());
                    }
                }

                if finder.found_timestamp() {
                    debug!("found __TIMESTAMP__ in {}", path.display());
                    let meta = match std::fs::symlink_metadata(path) {
                        Ok(meta) => meta,
                        Err(e) => {
                            debug!(
                                "{} is in a preprocessor cache entry but can't be read ({})",
                                path.display(),
                                e
                            );
                            return false;
                        }
                    };
                    let mtime = match meta.modified() {
                        Ok(mtime) => mtime,
                        Err(_) => {
                            debug!(
                                "Couldn't get mtime of {} which contains __TIMESTAMP__",
                                path.display()
                            );
                            return false;
                        }
                    };
                    let mtime: chrono::DateTime<chrono::Local> = chrono::DateTime::from(mtime);
                    new_digest.delimiter(b"timestamp");
                    new_digest.update(&mtime.naive_local().and_utc().timestamp().to_le_bytes());
                    include.digest = new_digest.finish_bytes();
                    // Signal that the preprocessor cache entry has been updated and needs to be
                    // written to disk.
                    *updated = true;
                }
            }
        }
        true
    }
}

/// How many include files are read and hashed at once.
///
/// Sized to overlap `open()` latency, not to saturate cores: hashing a
/// kilobyte-sized header is microseconds while opening it traverses the whole
/// filesystem filter stack. It is deliberately a per-call bound rather than a
/// process-wide one -- the daemon may be serving dozens of translation units
/// concurrently, and a shared permit pool would couple them and introduce a
/// deadlock surface for a benefit that is unproven until measured.
const DIGEST_FANOUT: usize = 16;

/// What reading an include file produced.
enum FileContents {
    /// Digest only, when time macros are being ignored.
    Plain(String),
    /// Digest plus what the time-macro scan found along the way.
    WithTimeMacros(String, TimeMacroFinder),
}

/// Read and hash `paths` concurrently, preserving input order in the result.
///
/// `None` in place of a result means the file could not be read; the caller
/// decides what that means. Order is preserved so callers can fold the outcomes
/// deterministically regardless of which file finished first.
async fn digest_files_concurrently(
    paths: impl IntoIterator<Item = PathBuf>,
    ignore_time_macros: bool,
    pool: &tokio::runtime::Handle,
) -> Vec<Option<FileContents>> {
    let paths: Vec<PathBuf> = paths.into_iter().collect();
    let mut out = Vec::with_capacity(paths.len());

    for chunk in paths.chunks(DIGEST_FANOUT) {
        let batch = chunk.iter().map(|path| {
            let path = path.clone();
            pool.spawn_blocking(move || digest_one_file(&path, ignore_time_macros))
        });
        // `join_all` rather than `try_join_all`: one unreadable include is a
        // normal outcome that means "no match", not an error to propagate, and
        // aborting the batch would leave the rest of the work half done.
        for joined in futures::future::join_all(batch).await {
            out.push(joined.unwrap_or(None));
        }
    }

    out
}

/// Build a manifest's include list from a dependency file a tool wrote.
///
/// The counterpart to scraping `#line` records out of preprocessed output: the
/// tool is asked what it read instead of being made to preprocess a second time
/// so the answer can be recovered. Returns pairs shaped for
/// [`PreprocessorCacheEntry::add_result`].
///
/// An `Err` means **do not cache this compilation**, and every path out of here
/// that is not a complete, verified list returns one. That asymmetry is
/// deliberate. A dependency list is used later to decide whether a cached
/// result is still valid, so a list that is missing an entry cannot cause a
/// miss -- it causes a *hit* on a stale result, which is the one failure mode
/// that hands back wrong output rather than merely doing redundant work.
///
/// So: an empty list is refused rather than read as "no dependencies", and a
/// single unreadable file refuses the whole list rather than silently dropping
/// that file from what will later be checked. The [`NotCacheable`] the refusal
/// carries is the only way a caller can tell those apart afterwards.
pub async fn includes_from_dep_file(
    dep_file_contents: &str,
    cwd: &Path,
    ignore_time_macros: bool,
    pool: &tokio::runtime::Handle,
) -> Result<Vec<(String, PathBuf)>, NotCacheable> {
    includes_from_paths(
        super::depfile::DepFileFormat::Make
            .read(dep_file_contents)
            .unwrap_or_default(),
        cwd,
        ignore_time_macros,
        pool,
    )
    .await
}

/// As [`includes_from_dep_file`], for tools that report their inputs in some
/// format other than Make's.
///
/// Kept separate because the formats genuinely differ -- MSVC's
/// `/sourceDependencies` is JSON, moc's `--output-dep-file` is Make -- while
/// everything after the parse is identical, and it is the part where the
/// correctness rules live.
pub async fn includes_from_paths(
    paths: impl IntoIterator<Item = PathBuf>,
    cwd: &Path,
    ignore_time_macros: bool,
    pool: &tokio::runtime::Handle,
) -> Result<Vec<(String, PathBuf)>, NotCacheable> {
    let mut paths: Vec<PathBuf> = paths
        .into_iter()
        .map(|p| if p.is_absolute() { p } else { cwd.join(p) })
        .collect();

    // Sorted and deduplicated so the manifest is a function of the inputs and
    // not of the order the tool happened to report them in; a dep file lists
    // the same header repeatedly when several sources pull it in.
    paths.sort_unstable();
    paths.dedup();
    if paths.is_empty() {
        return Err(NotCacheable::NothingReported);
    }

    let digested = digest_files_concurrently(paths.clone(), ignore_time_macros, pool).await;

    let mut out = Vec::with_capacity(paths.len());
    for (path, contents) in paths.into_iter().zip(digested) {
        let digest = match contents {
            Some(FileContents::Plain(d)) => d,
            Some(FileContents::WithTimeMacros(d, finder)) => {
                // A dependency that expands __DATE__ or __TIME__ produces
                // different output every build, so nothing keyed on it can be
                // reused. Refusing here is what stops a stale result being
                // served; the alternative is a cache that is confidently wrong
                // once a day.
                if finder.found_time_macros() {
                    debug!("{} contains time macros; not caching", path.display());
                    return Err(NotCacheable::TimeMacros);
                }
                d
            }
            // Unreadable now means unverifiable later.
            None => return Err(NotCacheable::DependencyUnreadable),
        };
        out.push((digest, path));
    }
    Ok(out)
}

fn digest_one_file(path: &Path, ignore_time_macros: bool) -> Option<FileContents> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            debug!(
                "{} is in a preprocessor cache entry but can't be opened ({})",
                path.display(),
                e
            );
            return None;
        }
    };

    let read_failed = |e| {
        debug!(
            "{} is in a preprocessor cache entry but can't be read ({})",
            path.display(),
            e
        );
        None
    };

    if ignore_time_macros {
        match Digest::reader_sync(file) {
            Ok(digest) => Some(FileContents::Plain(digest)),
            Err(e) => read_failed(e),
        }
    } else {
        match Digest::reader_sync_time_macros(file) {
            Ok((digest, finder)) => Some(FileContents::WithTimeMacros(digest, finder)),
            Err(e) => read_failed(e),
        }
    }
}

/// Environment variables that are factored into the preprocessor cache entry cached key.
static CACHED_ENV_VARS: LazyLock<HashSet<&'static OsStr>> = LazyLock::new(|| {
    [
        // SCCACHE_C_CUSTOM_CACHE_BUSTER has no particular meaning behind it,
        // serving as a way for the user to factor custom data into the hash.
        // One can set it to different values for different invocations
        // to prevent cache reuse between them.
        //
        // Both spellings are honoured, and unlike everywhere else they are not
        // interchangeable: the name is hashed along with the value, so moving a
        // buster from one spelling to the other busts the cache once. For a
        // variable whose entire purpose is to bust the cache, that is the least
        // surprising behaviour available.
        crate::envvar::CACHE_BUSTER,
        crate::envvar::LEGACY_CACHE_BUSTER,
        "CPATH",
        "C_INCLUDE_PATH",
        "CPLUS_INCLUDE_PATH",
        "OBJC_INCLUDE_PATH",
        "OBJCPLUS_INCLUDE_PATH",
    ]
    .iter()
    .map(OsStr::new)
    .collect()
});

/// Compute the hash key of compiler preprocessing `input` with `args`.
#[allow(clippy::too_many_arguments)]
pub fn preprocessor_cache_entry_hash_key(
    compiler_digest: &str,
    language: Language,
    arguments: &[OsString],
    extra_hashes: &[String],
    env_vars: &[(OsString, OsString)],
    input_file: &Path,
    plusplus: bool,
    config: PreprocessorCacheModeConfig,
    basedirs: &[Vec<u8>],
    cwd: &Path,
) -> anyhow::Result<Option<String>> {
    // If you change any of the inputs to the hash, you should change `FORMAT_VERSION`.
    let mut m = Digest::new();
    m.update(compiler_digest.as_bytes());
    // clang and clang++ have different behavior despite being byte-for-byte identical binaries, so
    // we have to incorporate that into the hash as well.
    m.update(&[plusplus as u8]);
    m.update(&[FORMAT_VERSION]);
    m.update(language.as_str().as_bytes());
    for arg in arguments {
        if basedirs.is_empty() {
            arg.hash(&mut HashToDigest { digest: &mut m });
        } else {
            // The same reason the result key strips them: a compilation's
            // arguments are full of absolute paths -- `-I`, `/FI`, `-Fp` -- so
            // leaving them raw pins the manifest to one checkout, and the
            // second one starts from nothing however carefully everything else
            // was normalised. Framed with a length because the arguments are
            // otherwise concatenated into one digest.
            let stripped = strip_basedirs_in_arg(arg.as_encoded_bytes(), basedirs);
            m.update(&(stripped.len() as u64).to_le_bytes());
            m.update(&stripped);
        }
    }
    for hash in extra_hashes {
        m.update(hash.as_bytes());
    }

    for (var, val) in env_vars.iter() {
        if CACHED_ENV_VARS.contains(var.as_os_str()) {
            var.hash(&mut HashToDigest { digest: &mut m });
            m.update(&b"="[..]);
            val.hash(&mut HashToDigest { digest: &mut m });
        }
    }

    // Hash the input file otherwise:
    // - a/r.h exists.
    // - a/x.c has #include "r.h".
    // - b/x.c is identical to a/x.c.
    // - Compiling a/x.c records a/r.h in the preprocessor cache entry.
    // - Compiling b/x.c results in a false cache hit since a/x.c and b/x.c
    // share preprocessor cache entries and a/r.h exists.
    let mut buf = vec![];
    encode_path(&mut buf, input_file)?;

    // Strip basedirs from the input file path if configured
    let buf_to_hash = strip_basedirs(&buf, basedirs);
    m.update(&buf_to_hash);
    let reader = std::fs::File::open(input_file)
        .with_context(|| format!("while hashing the input file '{}'", input_file.display()))?;

    let digest = if config.ignore_time_macros {
        Digest::reader_sync(reader)?
    } else {
        let (digest, finder) = Digest::reader_sync_time_macros(reader)?;
        if finder.found_time() {
            // Disable preprocessor cache mode
            debug!("Found __TIME__ in {}", input_file.display());
            return Ok(None);
        }
        digest
    };
    m.update(digest.as_bytes());

    // The input path above pins the directory only when the build names its
    // sources relatively. Given absolute paths -- which is what CMake
    // generates -- it does not, and the manifest would then hand a result key
    // across directories that the object's debug information distinguishes.
    if config.hash_working_directory {
        let stripped = strip_basedirs_in_dir(cwd, basedirs);
        m.update(b"cwd=");
        m.update(&(stripped.len() as u64).to_le_bytes());
        m.update(&stripped);
    }

    Ok(Some(m.finish()))
}

/// Corresponds to a cached include file used in the pre-processor stage
#[derive(Clone, Deserialize, Serialize, Debug, PartialEq, Eq)]
pub struct IncludeEntry {
    /// Its absolute path
    path: OsString,
    /// The hash of its contents.
    ///
    /// The digest itself, not its hex: 64 characters on the heap became 32
    /// bytes inline, which is about 15% off an entry on disk and one fewer
    /// allocation per include every time one is read.
    digest: [u8; DIGEST_BYTES],
    /// Its file size, in bytes
    file_size: u64,
    /// Its modification time, `None` if not recorded.
    mtime: Option<Timestamp>,
    /// Its status change time, `None` if not recorded.
    ctime: Option<Timestamp>,
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Deserialization(bincode::Error),
    UnknownFormat(u8),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<bincode::Error> for Error {
    fn from(e: bincode::Error) -> Self {
        Self::Deserialization(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => e.fmt(f),
            Error::Deserialization(e) => e.fmt(f),
            Error::UnknownFormat(format) => f.write_fmt(format_args!(
                "Unknown preprocessor cache entry format {:x}",
                format
            )),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod test {
    use crate::util::{HASH_BUFFER_SIZE, MAX_TIME_MACRO_HAYSTACK_LEN};

    use super::*;

    /// The direction of the freshness test, which is the whole of its value.
    ///
    /// A scan's answer for a file is refused while the file is new, because a
    /// directory entry lags a writer that still holds it open. Invert the
    /// comparison and this reads as "believe the scan for files being written
    /// right now, ask the disk about files nobody has touched in a week" --
    /// which is not a compile error, is not a test failure anywhere else, and
    /// is precisely backwards.
    #[cfg(windows)]
    #[test]
    fn a_scan_is_believed_only_once_a_file_has_settled() {
        let secs =
            |s: u64| Timestamp::from(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(s));
        let cutoff = Some(secs(1_000));
        let facts = |mtime, ctime| FileFacts {
            len: 0,
            mtime,
            ctime,
        };

        assert!(
            scan_is_settled(&facts(Some(secs(900)), Some(secs(900))), cutoff),
            "a file untouched since before the cutoff is settled"
        );
        assert!(
            !scan_is_settled(&facts(Some(secs(1_100)), Some(secs(1_100))), cutoff),
            "a file written after the cutoff may still have a writer attached"
        );
        // The later of the two decides: ctime moves when mtime does not, and an
        // entry that moved at all is an entry that may not have landed.
        assert!(
            !scan_is_settled(&facts(Some(secs(900)), Some(secs(1_100))), cutoff),
            "a recent ctime is enough to refuse, even with an old mtime"
        );
        assert!(
            !scan_is_settled(&facts(None, None), cutoff),
            "no timestamps is not evidence of age"
        );
        assert!(
            !scan_is_settled(&facts(Some(secs(900)), Some(secs(900))), None),
            "no cutoff is no basis to believe anything"
        );
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn dep_file_includes_are_digested_and_sorted() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a.h");
        let b = t.path().join("b.h");
        std::fs::write(&a, "int a;\n").unwrap();
        std::fs::write(&b, "int b;\n").unwrap();
        // Deliberately out of order, and with a repeat: a dep file lists the
        // same header once per source that pulled it in.
        let dep = format!("out.o: {} {} {}\n", b.display(), a.display(), b.display());

        let rt = rt();
        let got = rt
            .block_on(includes_from_dep_file(&dep, t.path(), true, rt.handle()))
            .expect("should be cacheable");
        assert_eq!(got.len(), 2, "duplicates must collapse");
        assert_eq!(got[0].1, a, "must be ordered by path, not by mention");
        assert_eq!(got[1].1, b);
        assert_ne!(got[0].0, got[1].0, "different contents, different digests");
    }

    #[test]
    fn an_unreadable_dependency_refuses_the_whole_list() {
        let t = tempfile::tempdir().unwrap();
        let a = t.path().join("a.h");
        std::fs::write(&a, "int a;\n").unwrap();
        let missing = t.path().join("gone.h");
        let dep = format!("out.o: {} {}\n", a.display(), missing.display());

        let rt = rt();
        // Dropping the unreadable one and caching the rest would leave a
        // dependency nothing later checks, which is how a stale result gets
        // served.
        assert_eq!(
            rt.block_on(includes_from_dep_file(&dep, t.path(), true, rt.handle())),
            Err(NotCacheable::DependencyUnreadable)
        );
    }

    #[test]
    fn an_empty_dep_file_is_not_read_as_having_no_dependencies() {
        let t = tempfile::tempdir().unwrap();
        let rt = rt();
        for dep in ["", "out.o:\n", "no colon here\n"] {
            assert_eq!(
                rt.block_on(includes_from_dep_file(dep, t.path(), true, rt.handle())),
                Err(NotCacheable::NothingReported),
                "{dep:?} must be refused, not treated as zero dependencies"
            );
        }
    }

    #[test]
    fn a_dependency_with_time_macros_refuses_the_list() {
        let t = tempfile::tempdir().unwrap();
        let h = t.path().join("stamp.h");
        std::fs::write(&h, "const char* built = __DATE__ \" \" __TIME__;\n").unwrap();
        let dep = format!("out.o: {}\n", h.display());

        let rt = rt();
        // Anything keyed on this changes every second; caching it produces a
        // result that is confidently wrong rather than merely stale. Naming the
        // reason is what lets a caller stop retrying and report the line.
        assert_eq!(
            rt.block_on(includes_from_dep_file(&dep, t.path(), false, rt.handle())),
            Err(NotCacheable::TimeMacros)
        );
        // ...unless the caller has explicitly accepted that trade.
        assert!(
            rt.block_on(includes_from_dep_file(&dep, t.path(), true, rt.handle()))
                .is_ok()
        );
    }

    #[test]
    fn relative_dependencies_resolve_against_the_working_directory() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("inc")).unwrap();
        std::fs::write(t.path().join("inc").join("x.h"), "int x;\n").unwrap();
        let rt = rt();
        let got = rt
            .block_on(includes_from_dep_file(
                "out.o: inc/x.h\n",
                t.path(),
                true,
                rt.handle(),
            ))
            .expect("relative path should resolve");
        assert_eq!(got[0].1, t.path().join("inc/x.h"));
    }

    #[test]
    fn test_find_time_macros_empty_file() {
        let buf: Vec<u8> = vec![];
        let hash = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().0;
        assert_eq!(hash, Digest::new().finish());
    }

    #[test]
    fn test_find_time_macros_small_file_no_match() {
        let buf = b"This is a small file, which doesn't contain any time macros.";
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(!finder.found_time_macros());
    }

    #[test]
    fn test_find_time_macros_small_file_match() {
        let buf = b"__TIME__";
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(finder.found_time());
        assert!(!finder.found_timestamp());
        assert!(!finder.found_date());
        let buf = b"__DATE__";
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(!finder.found_time());
        assert!(!finder.found_timestamp());
        assert!(finder.found_date());
        let buf = b"__TIMESTAMP__";
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(!finder.found_time());
        assert!(finder.found_timestamp());
        assert!(!finder.found_date());
    }

    #[test]
    fn test_find_time_macros_small_file_match_multiple() {
        let buf = b"__TIMESTAMP__ __DATE__ __TIME__";
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(finder.found_time());
        assert!(finder.found_timestamp());
        assert!(finder.found_date());
    }

    #[test]
    fn test_find_time_macros_large_file_no_match() {
        let buf = vec![0; HASH_BUFFER_SIZE * 2];
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(!finder.found_time_macros());
        assert!(!finder.found_time());
        assert!(!finder.found_timestamp());
        assert!(!finder.found_date());
    }

    #[test]
    fn test_find_time_macros_large_file_match_no_overlap() {
        let mut buf = vec![0; HASH_BUFFER_SIZE * 2];
        buf.extend(b"__TIMESTAMP__ __DATE__ __TIME__");
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(finder.found_time());
        assert!(finder.found_timestamp());
        assert!(finder.found_date());
    }
    #[test]
    fn test_find_time_macros_large_file_match_overlap() {
        let mut buf = vec![0; HASH_BUFFER_SIZE * 2];
        // Make the pattern overlap two buffer chunks to make sure we account for this
        let start = HASH_BUFFER_SIZE - MAX_TIME_MACRO_HAYSTACK_LEN / 2;
        buf[start..][..b"__TIMESTAMP__".len()].copy_from_slice(b"__TIMESTAMP__");
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(!finder.found_time());
        assert!(finder.found_timestamp());
        assert!(!finder.found_date());

        let mut buf = vec![0; HASH_BUFFER_SIZE * 2];
        // Make the pattern overlap two buffer chunks to make sure we account for this
        let start = HASH_BUFFER_SIZE - MAX_TIME_MACRO_HAYSTACK_LEN / 2;
        buf[start..][..b"__TIME__".len()].copy_from_slice(b"__TIME__");
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(finder.found_time());
        assert!(!finder.found_timestamp());
        assert!(!finder.found_date());

        let mut buf = vec![0; HASH_BUFFER_SIZE * 2];
        // Make the pattern overlap two buffer chunks to make sure we account for this
        let start = HASH_BUFFER_SIZE - MAX_TIME_MACRO_HAYSTACK_LEN / 2;
        buf[start..][..b"__DATE__".len()].copy_from_slice(b"__DATE__");
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(!finder.found_time());
        assert!(!finder.found_timestamp());
        assert!(finder.found_date());
    }

    #[test]
    fn test_find_time_macros_large_file_match_overlap_multiple_pages() {
        let mut buf = vec![0; HASH_BUFFER_SIZE * 3];
        // Make the patterns overlap buffer chunks twice to make sure we account for this
        let start = HASH_BUFFER_SIZE - MAX_TIME_MACRO_HAYSTACK_LEN / 2;
        buf[start..][..b"__TIME__".len()].copy_from_slice(b"__TIME__");
        let start = HASH_BUFFER_SIZE * 2 - MAX_TIME_MACRO_HAYSTACK_LEN / 2;
        buf[start..][..b"__DATE__".len()].copy_from_slice(b"__DATE__");
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(finder.found_time());
        assert!(!finder.found_timestamp());
        assert!(finder.found_date());
    }

    #[test]
    fn test_find_time_macros_large_file_match_overlap_multiple_pages_tiny() {
        let mut buf = vec![0; HASH_BUFFER_SIZE * 3];
        // Make the patterns overlap buffer chunks twice to make sure we account for this
        let start = HASH_BUFFER_SIZE - MAX_TIME_MACRO_HAYSTACK_LEN / 2;
        buf[start..][..b"__TIME__".len()].copy_from_slice(b"__TIME__");
        let start = HASH_BUFFER_SIZE * 2 - MAX_TIME_MACRO_HAYSTACK_LEN / 2;
        buf[start..][..b"__DATE__".len()].copy_from_slice(b"__DATE__");
        // Test overlap with the last chunk being less than the haystack
        buf.extend([0; MAX_TIME_MACRO_HAYSTACK_LEN / 2 + 1]);
        let start = HASH_BUFFER_SIZE * 3 - MAX_TIME_MACRO_HAYSTACK_LEN / 2;
        buf[start..][..b"__TIMESTAMP__".len()].copy_from_slice(b"__TIMESTAMP__");
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(finder.found_time_macros());
        assert!(finder.found_time());
        assert!(finder.found_timestamp());
        assert!(finder.found_date());
    }

    #[test]
    fn test_find_time_macros_ghost_pattern() {
        // Check the (unlikely) case of a pattern being spread between the
        // start of a chunk and its end.
        let mut buf = vec![0; HASH_BUFFER_SIZE * 3];
        buf[HASH_BUFFER_SIZE..HASH_BUFFER_SIZE + b"__TI".len()].copy_from_slice(b"__TI");
        buf[HASH_BUFFER_SIZE * 2 - "ME__".len()..HASH_BUFFER_SIZE * 2].copy_from_slice(b"ME__");
        let finder = Digest::reader_sync_time_macros(buf.as_slice()).unwrap().1;
        assert!(!finder.found_time_macros());
        assert!(!finder.found_time());
        assert!(!finder.found_timestamp());
        assert!(!finder.found_date());
    }

    /// Sharing a manifest between checkouts is only safe if the checking is
    /// done against the files of the checkout being compiled. Recording
    /// absolute paths would make the second checkout confirm the *first*
    /// one's files -- they exist, they are unchanged -- and hand back an object
    /// built from them, whatever the second checkout actually contains.
    #[test]
    fn an_entry_is_checked_against_the_checkout_being_compiled() {
        use std::fs;
        use tempfile::TempDir;

        let runtime = rt();
        let (a, b) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let basedirs_of = |d: &TempDir| {
            vec![crate::config::normalize_basedir(d.path().to_str().unwrap()).unwrap()]
        };
        for d in [&a, &b] {
            fs::create_dir_all(d.path().join("inc")).unwrap();
            fs::write(d.path().join("inc/h.h"), b"shared header").unwrap();
        }

        let config = PreprocessorCacheModeConfig::activated();
        let digest = match digest_one_file(&a.path().join("inc/h.h"), config.ignore_time_macros) {
            Some(FileContents::Plain(d)) | Some(FileContents::WithTimeMacros(d, _)) => d,
            None => panic!("could not digest the header"),
        };

        // Written in checkout A...
        let mut entry = PreprocessorCacheEntry::new();
        entry.add_result(
            SystemTime::now(),
            "the-result-key",
            [(digest, a.path().join("inc/h.h"))],
            &basedirs_of(&a),
        );
        let serialized = {
            let mut buf = Vec::new();
            entry.serialize_to(&mut buf).unwrap();
            buf
        };

        // ...and read in checkout B, whose copy of the header is identical.
        let mut updated = false;
        let hit = runtime.block_on(
            PreprocessorCacheEntry::read(&serialized)
                .unwrap()
                .lookup_result_digest(config, &mut updated, runtime.handle(), &basedirs_of(&b)),
        );
        assert_eq!(hit.as_deref(), Some("the-result-key"));

        // Now B's copy differs. The entry must not match, even though A's copy
        // is still on disk and still says what it said.
        fs::write(b.path().join("inc/h.h"), b"changed in this checkout").unwrap();
        let miss = runtime.block_on(
            PreprocessorCacheEntry::read(&serialized)
                .unwrap()
                .lookup_result_digest(config, &mut updated, runtime.handle(), &basedirs_of(&b)),
        );
        assert_eq!(
            miss, None,
            "the second checkout's own file has to be what decides"
        );
    }

    /// The manifest key names the input file absolutely, which pins the
    /// directory only when the build names its sources relatively. Given
    /// absolute paths -- what CMake generates -- it does not, and the manifest
    /// would hand a result key across directories that the object's debug
    /// information distinguishes.
    #[test]
    fn manifest_keys_carry_the_working_directory() {
        use std::fs;
        use tempfile::TempDir;

        let shared = TempDir::new().unwrap();
        let input = shared.path().join("x.c");
        fs::write(&input, b"int main(void) { return 0; }").unwrap();

        let key_from = |cwd: &str, config| {
            preprocessor_cache_entry_hash_key(
                "test_digest",
                Language::C,
                &[],
                &[],
                &[],
                &input,
                false,
                config,
                &[],
                Path::new(cwd),
            )
            .unwrap()
            .unwrap()
        };

        let on = PreprocessorCacheModeConfig::activated();
        assert_neq!(key_from("/build/a", on), key_from("/build/b", on));

        let off = PreprocessorCacheModeConfig {
            hash_working_directory: false,
            ..on
        };
        assert_eq!(key_from("/build/a", off), key_from("/build/b", off));
    }

    /// The manifest key strips basedirs from the input file but not from the
    /// arguments, and a compilation's arguments are full of absolute paths --
    /// `-I`, `/FI`, `-Fp`. So two checkouts of one tree never share a manifest,
    /// every compilation in the second one starts from nothing, and the
    /// dependency-keyed result key inherits the same problem because it is
    /// built on top of this one.
    ///
    /// Same defect as the one fixed for the result key in `148bce9`, one layer
    /// down.
    #[test]
    fn manifest_keys_ignore_where_the_checkout_sits() {
        use std::fs;
        use tempfile::TempDir;

        let (dir1, dir2) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let basedirs_of = |d: &TempDir| {
            let bytes = d.path().to_string_lossy().into_owned().into_bytes();
            #[cfg(target_os = "windows")]
            return vec![crate::util::normalize_win_path(&bytes)];
            #[cfg(not(target_os = "windows"))]
            vec![bytes]
        };

        let content = b"int main() { return 0; }";
        fs::write(dir1.path().join("test.c"), content).unwrap();
        fs::write(dir2.path().join("test.c"), content).unwrap();

        let key_in = |d: &TempDir| {
            let include = {
                let mut a = std::ffi::OsString::from("-I");
                a.push(d.path().join("include"));
                a
            };
            preprocessor_cache_entry_hash_key(
                "test_digest",
                Language::C,
                &[include],
                &[],
                &[],
                &d.path().join("test.c"),
                false,
                PreprocessorCacheModeConfig::activated(),
                &basedirs_of(d),
                d.path(),
            )
            .unwrap()
        };

        assert_eq!(
            key_in(&dir1),
            key_in(&dir2),
            "two checkouts of one project must agree on which compilation this is"
        );
    }

    #[test]
    fn test_preprocessor_cache_entry_hash_key_basedirs() {
        #[cfg(target_os = "windows")]
        use crate::util::normalize_win_path;
        use std::fs;
        use tempfile::TempDir;

        // Create two different base directories
        let dir1 = TempDir::new().unwrap();
        let dir2 = TempDir::new().unwrap();
        let dirs = [&dir1, &dir2]
            .iter()
            .map(|dir| {
                let bytes = dir.path().to_string_lossy().into_owned().into_bytes();
                #[cfg(target_os = "windows")]
                return normalize_win_path(&bytes);
                #[cfg(not(target_os = "windows"))]
                bytes
            })
            .collect::<Vec<_>>();

        // Create identical files with the same relative path in each directory
        let file1_path = dir1.path().join("test.c");
        let file2_path = dir2.path().join("test.c");

        let content = b"int main() { return 0; }";
        fs::write(&file1_path, content).unwrap();
        fs::write(&file2_path, content).unwrap();

        let config = PreprocessorCacheModeConfig::activated();

        // Test 1: With basedirs, hashes should be the same
        let hash1_with_basedirs = preprocessor_cache_entry_hash_key(
            "test_digest",
            Language::C,
            &[],
            &[],
            &[],
            &file1_path,
            false,
            config,
            &dirs,
            Path::new("/wd"),
        )
        .unwrap()
        .unwrap();

        let hash2_with_basedirs = preprocessor_cache_entry_hash_key(
            "test_digest",
            Language::C,
            &[],
            &[],
            &[],
            &file2_path,
            false,
            config,
            &dirs,
            Path::new("/wd"),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            hash1_with_basedirs, hash2_with_basedirs,
            "Hashes should be equal when using basedirs with identical files in different directories"
        );

        // Test 2: With basedir1 for first, and basedir2 for second, hashes should be the same
        let hash1_with_basedirs = preprocessor_cache_entry_hash_key(
            "test_digest",
            Language::C,
            &[],
            &[],
            &[],
            &file1_path,
            false,
            config,
            &dirs[..1],
            dir1.path(),
        )
        .unwrap()
        .unwrap();

        let hash2_with_basedirs = preprocessor_cache_entry_hash_key(
            "test_digest",
            Language::C,
            &[],
            &[],
            &[],
            &file2_path,
            false,
            config,
            &dirs[1..],
            dir2.path(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            hash1_with_basedirs, hash2_with_basedirs,
            "Hashes should be equal when using basedirs with identical files in different directories"
        );

        // Test 3: Without basedirs, hashes should be different
        let hash1_no_basedirs = preprocessor_cache_entry_hash_key(
            "test_digest",
            Language::C,
            &[],
            &[],
            &[],
            &file1_path,
            false,
            config,
            &[],
            Path::new("/wd"),
        )
        .unwrap()
        .unwrap();

        let hash2_no_basedirs = preprocessor_cache_entry_hash_key(
            "test_digest",
            Language::C,
            &[],
            &[],
            &[],
            &file2_path,
            false,
            config,
            &[],
            Path::new("/wd"),
        )
        .unwrap()
        .unwrap();

        assert_ne!(
            hash1_no_basedirs, hash2_no_basedirs,
            "Hashes should be different without basedirs for files in different directories"
        );
    }

    /// `result_matches` used to `return` on its first include when
    /// `ignore_time_macros` was set, so an entry was declared matching on the
    /// strength of its first header alone. Any later header could change freely
    /// and the entry would still be reported as a hit -- serving a stale object
    /// file, which is the one failure mode a compiler cache must not have.
    #[test]
    fn test_result_matches_checks_every_include_with_ignore_time_macros() {
        let dir = tempfile::Builder::new()
            .prefix("sccache_result_matches")
            .tempdir()
            .unwrap();

        let unchanged = dir.path().join("unchanged.h");
        let changed = dir.path().join("changed.h");
        std::fs::write(
            &unchanged,
            b"#define A 1
",
        )
        .unwrap();
        std::fs::write(
            &changed,
            b"#define B 1
",
        )
        .unwrap();

        let entry_for = |path: &std::path::Path| {
            let contents = std::fs::read(path).unwrap();
            IncludeEntry {
                path: path.as_os_str().to_owned(),
                digest: crate::util::unhex_digest(
                    &Digest::reader_sync(std::io::Cursor::new(&contents)).unwrap(),
                )
                .unwrap(),
                file_size: contents.len() as u64,
                mtime: None,
                ctime: None,
            }
        };

        let config = PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: true,
            ignore_time_macros: true,
            ..Default::default()
        };

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        // The unchanged header is deliberately first: with the old code its match
        // decided the whole entry.
        let mut includes = vec![entry_for(&unchanged), entry_for(&changed)];
        let mut updated = false;
        assert!(
            runtime.block_on(PreprocessorCacheEntry::result_matches(
                "digest",
                &mut includes,
                config,
                &mut updated,
                runtime.handle(),
                &[],
                // Fresh caches per call, which is what a lookup gets. Sharing
                // them across these two would hide the very change the second
                // assertion is looking for.
                &mut LookupCaches::default(),
            )),
            "nothing has changed yet, so the entry should match"
        );

        // Same length, different contents, so the file_size shortcut cannot be
        // what catches it -- only the digest of the second include can.
        std::fs::write(
            &changed,
            b"#define B 2
",
        )
        .unwrap();
        let mut updated = false;
        assert!(
            !runtime.block_on(PreprocessorCacheEntry::result_matches(
                "digest",
                &mut includes,
                config,
                &mut updated,
                runtime.handle(),
                &[],
                // Fresh caches per call, which is what a lookup gets. Sharing
                // them across these two would hide the very change the second
                // assertion is looking for.
                &mut LookupCaches::default(),
            )),
            "a change to any include must be detected, not just the first"
        );
    }
}

/// A stopwatch on the real lookup, for the optimisation work.
///
/// Not part of the suite -- `#[ignore]`d, run deliberately:
///
/// ```text
/// cargo test --release --lib lookup_cost -- --ignored --nocapture
/// ```
///
/// It builds a tree shaped like a translation unit's includes, records an entry
/// against it, and times `lookup_result_digest` on the stat-only path, which is
/// the common case with `file_stat_matches` on and the one that dominates a
/// preprocessor cache hit. Release matters: a debug build measures rustc.
#[cfg(test)]
mod lookup_cost {
    use super::*;
    use std::time::Duration;

    /// What [`LookupCaches::dirs`] holds, for an include set of this shape.
    ///
    /// A scan pays for a whole directory, and the map keeps the whole of it --
    /// every name, wanted or not -- until the lookup ends. That is the term
    /// nobody had a number for, and the sparse case is where it grows: the same
    /// include count spread over directories holding far more than was asked
    /// for.
    ///
    /// Bytes are a structural estimate, not a measurement: one hashbrown slot
    /// per entry (key, value and a control byte) over the 7/8 load factor it
    /// keeps, plus the name bytes each `OsString` puts on the heap. Counting is
    /// the point; the estimate is only there to say whether the count matters.
    #[cfg(windows)]
    fn retained_by_dirs(files: &[PathBuf]) -> (usize, usize, usize) {
        let parents: std::collections::BTreeSet<PathBuf> = files
            .iter()
            .filter_map(|f| f.parent().map(Path::to_path_buf))
            .collect();
        let (mut entries, mut name_bytes) = (0usize, 0usize);
        for dir in &parents {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    entries += 1;
                    name_bytes += e.file_name().len();
                }
            }
        }
        let slot = std::mem::size_of::<(OsString, FileFacts)>() + 1;
        (parents.len(), entries, entries * slot * 8 / 7 + name_bytes)
    }

    /// `dirs` directories of `per_dir` files each, which is how includes
    /// actually sit -- clustered, not scattered.
    fn tree(dirs: usize, per_dir: usize) -> (tempfile::TempDir, Vec<PathBuf>) {
        let root = tempfile::tempdir().expect("tempdir");
        let mut files = Vec::with_capacity(dirs * per_dir);
        for d in 0..dirs {
            let dir = root.path().join(format!("include_{d}"));
            std::fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..per_dir {
                let p = dir.join(format!("header_{f}.hpp"));
                std::fs::write(&p, format!("#pragma once\n// {d}/{f}\n").repeat(8)).expect("write");
                files.push(p);
            }
        }
        (root, files)
    }

    fn stat_only_config() -> PreprocessorCacheModeConfig {
        PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: true,
            file_stat_matches: true,
            use_ctime_for_stat: true,
            ignore_time_macros: false,
            ..Default::default()
        }
    }

    /// One entry holding `results` results, each naming every file, which is the
    /// shape that makes repeated work visible: results in one entry are
    /// different preprocessor outcomes of the same translation unit, so their
    /// include sets overlap almost entirely.
    /// Results that miss, and miss as late as possible.
    ///
    /// `lookup_result_digest` walks results newest-first and returns on the
    /// first match, so extra results are free when the newest one hits. They
    /// cost only when earlier ones miss -- and a miss is cheapest when found on
    /// the first file. The expensive shape is a miss discovered on the *last*
    /// file, which a stale size on the final include produces.
    fn entry_with_late_misses(
        files: &[PathBuf],
        misses: usize,
        basedirs: &[Vec<u8>],
    ) -> PreprocessorCacheEntry {
        let start = SystemTime::now() + Duration::from_secs(60);
        let mut entry = PreprocessorCacheEntry::new();
        for r in 0..=misses {
            let included: Vec<(String, PathBuf)> = files
                .iter()
                .enumerate()
                .map(|(i, p)| (format!("{:064x}", (r * 1_000_003 + i) as u128), p.clone()))
                .collect();
            entry.add_result(start, &format!("result_{r:03}"), included, basedirs);
        }
        // Break the last include of every result but the oldest, so each stats
        // everything before deciding against itself.
        let keys: Vec<String> = entry.results.keys().cloned().collect();
        for key in keys.iter().skip(1) {
            if let Some(v) = entry.results.get_mut(key)
                && let Some(last) = v.last_mut()
            {
                last.file_size = last.file_size.wrapping_add(1);
            }
        }
        entry
    }

    fn entry_over(
        files: &[PathBuf],
        results: usize,
        basedirs: &[Vec<u8>],
    ) -> PreprocessorCacheEntry {
        // Ahead of every file's mtime, so mtime and ctime are recorded and the
        // lookup can settle by `stat` without opening anything.
        let start = SystemTime::now() + Duration::from_secs(60);
        let mut entry = PreprocessorCacheEntry::new();
        for r in 0..results {
            // Real digests, so a content comparison hits rather than doing the
            // same work and then declining. A digest that is not the file's own
            // makes the entry a miss by construction, which would measure the
            // wrong thing in the configuration that compares contents.
            let included = files.iter().map(|p| {
                let file = std::fs::File::open(p).expect("open");
                (Digest::reader_sync(file).expect("digest"), p.clone())
            });
            entry.add_result(start, &format!("result_{r:03}"), included, basedirs);
        }
        entry
    }

    async fn time_lookup(
        entry: &mut PreprocessorCacheEntry,
        config: PreprocessorCacheModeConfig,
        basedirs: &[Vec<u8>],
        rounds: usize,
    ) -> (f64, bool) {
        let pool = tokio::runtime::Handle::current();
        let mut best = f64::MAX;
        let mut hit = false;
        for _ in 0..rounds {
            let mut updated = false;
            let t = std::time::Instant::now();
            let got = entry
                .lookup_result_digest(config, &mut updated, &pool, basedirs)
                .await;
            let ms = t.elapsed().as_secs_f64() * 1e3;
            hit = got.is_some();
            if ms < best {
                best = ms;
            }
        }
        (best, hit)
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "a stopwatch, not an assertion; run it deliberately"]
    async fn lookup_cost_by_shape() {
        let config = stat_only_config();
        println!("\nlookup_result_digest, stat-only path, best of 5\n");

        // One result: the cost of a single first pass.
        for (dirs, per_dir) in [(4usize, 50usize), (8, 62), (16, 125)] {
            let (_root, files) = tree(dirs, per_dir);
            let n = files.len();
            let mut entry = entry_over(&files, 1, &[]);
            let (ms, hit) = time_lookup(&mut entry, config, &[], 5).await;
            println!(
                "  {n:>5} includes / {dirs:>2} dirs / 1 result    {ms:>8.2} ms  ({:>5.1} us each)  hit: {hit}",
                ms * 1000.0 / n as f64
            );
        }

        // The same entries under the *default* configuration, where
        // `file_stat_matches` is false and every include is compared by hashing
        // its contents rather than by its size and timestamps. That is what a
        // machine that has not been tuned actually runs.
        let default_config = PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: true,
            file_stat_matches: false,
            ..Default::default()
        };
        for (dirs_n, per_dir) in [(4usize, 50usize), (8, 62)] {
            let (_root, files) = tree(dirs_n, per_dir);
            let n = files.len();
            let mut stat_entry = entry_over(&files, 1, &[]);
            let (stat_ms, stat_hit) = time_lookup(&mut stat_entry, config, &[], 5).await;
            let mut hash_entry = entry_over(&files, 1, &[]);
            let (hash_ms, hash_hit) = time_lookup(&mut hash_entry, default_config, &[], 5).await;
            println!(
                "  {n:>5} includes: stat_matches on {stat_ms:>6.2} ms (hit {stat_hit})                   off {hash_ms:>7.2} ms (hit {hash_hit})   {:.1}x",
                hash_ms / stat_ms
            );
        }

        // Diagnose before optimising: is the stored path even relative? If a
        // basedir did not strip, `resolve_include` returns early on
        // `is_absolute` and the multi-basedir cost never arises at all.
        {
            let (_root, files) = tree(1, 2);
            let root_bytes = files[0]
                .parent()
                .and_then(|p| p.parent())
                // Normalised the way `Config` stores basedirs: forward slashes,
                // lowercase, trailing separator. Anything else silently fails to
                // strip, and then every stored path stays absolute and this
                // measures nothing.
                .map(|p| {
                    let mut b = p
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/")
                        .to_lowercase();
                    b.push('/');
                    b.into_bytes()
                })
                .expect("root");
            let entry = entry_over(&files, 1, std::slice::from_ref(&root_bytes));
            let stored = entry.results.values().next().unwrap()[0].path.clone();
            println!(
                "  stored path with a basedir is relative: {}  ({stored:?})",
                Path::new(&stored).is_relative()
            );
        }

        // Several results over the same files: what a memo would collapse.
        let (_root, files) = tree(8, 62);
        let n = files.len();
        for misses in [0usize, 2, 7] {
            let mut entry = entry_with_late_misses(&files, misses, &[]);
            let (ms, hit) = time_lookup(&mut entry, config, &[], 5).await;
            println!("  {n:>5} includes / {misses} late miss(es)       {ms:>8.2} ms  hit: {hit}");
        }

        // What share of the lookup is now the entry itself? The digests are
        // stored as 64 hex characters where 32 raw bytes would do, and the case
        // for changing that is only as good as this number.
        for (dirs, per_dir) in [(8usize, 62usize), (16, 125)] {
            let (_root, files) = tree(dirs, per_dir);
            let n = files.len();
            let entry = entry_over(&files, 1, &[]);
            let mut buf = Vec::new();
            entry.serialize_to(&mut buf).expect("serialize");
            let mut enc = f64::MAX;
            let mut dec = f64::MAX;
            for _ in 0..20 {
                let t = std::time::Instant::now();
                let mut b = Vec::new();
                entry.serialize_to(&mut b).expect("serialize");
                enc = enc.min(t.elapsed().as_secs_f64() * 1e3);
                let t = std::time::Instant::now();
                let back = PreprocessorCacheEntry::read(&buf).expect("read");
                dec = dec.min(t.elapsed().as_secs_f64() * 1e3);
                std::hint::black_box(back);
            }
            println!(
                "  {n:>5} includes / entry on disk  {:>6} B   encode {enc:.2} ms   decode {dec:.2} ms",
                buf.len()
            );
        }

        // Where the remaining time goes. The lookup no longer contains a stat
        // per include, so the question is what replaced it.
        for (dirs_n, per_dir) in [(8usize, 62usize), (16, 125)] {
            let (_root, files) = tree(dirs_n, per_dir);
            let n = files.len();
            let mut entry = entry_over(&files, 1, &[]);
            let (full, _) = time_lookup(&mut entry, config, &[], 5).await;

            // (a) the syscalls, in the shape *this platform's* lookup issues
            // them.
            //
            // On Windows that is one directory scan per directory. Everywhere
            // else it is `STAT_CHUNK` files per blocking task, because
            // `prefetch_by_directory` is Windows-only and `prefetch_stats`
            // batches `resolve_and_stat` instead. Timing the
            // Windows shape on a platform that never takes it reports a path the
            // lookup does not enter and leaves all the real work in the
            // residual, which is what this line used to do.
            let pool = tokio::runtime::Handle::current();

            #[cfg(windows)]
            let parents: Vec<PathBuf> = {
                let mut v: Vec<PathBuf> = files
                    .iter()
                    .filter_map(|f| f.parent().map(|p| p.to_path_buf()))
                    .collect();
                v.sort();
                v.dedup();
                v
            };

            let mut syscalls = f64::MAX;
            for _ in 0..5 {
                let t = std::time::Instant::now();
                let mut tasks = Vec::new();
                #[cfg(windows)]
                for dir in &parents {
                    let dir = dir.clone();
                    tasks.push(pool.spawn_blocking(move || {
                        let mut count = 0usize;
                        if let Ok(rd) = std::fs::read_dir(&dir) {
                            for e in rd.flatten() {
                                if e.metadata().is_ok() {
                                    count += 1;
                                }
                            }
                        }
                        count
                    }));
                }
                #[cfg(not(windows))]
                for chunk in files.chunks(STAT_CHUNK) {
                    let chunk: Vec<PathBuf> = chunk.to_vec();
                    tasks.push(pool.spawn_blocking(move || {
                        chunk
                            .iter()
                            .filter(|f| std::fs::symlink_metadata(f).is_ok())
                            .count()
                    }));
                }
                for t in tasks {
                    std::hint::black_box(t.await.unwrap_or(0));
                }
                syscalls = syscalls.min(t.elapsed().as_secs_f64() * 1e3);
            }

            // (a2) Windows only: the same scans, but building the filename ->
            // metadata maps the lookup actually builds, so that hashing and
            // allocation can be told apart from waiting. There is no such step
            // elsewhere -- the per-file path puts each answer straight into the
            // lookup's cache -- so there is nothing here to separate, and (a3)
            // below already measures that plumbing.
            #[cfg(windows)]
            let mut scans_mapped = f64::MAX;
            #[cfg(windows)]
            for _ in 0..5 {
                let t = std::time::Instant::now();
                let mut tasks = Vec::new();
                for dir in &parents {
                    let dir = dir.clone();
                    tasks.push(pool.spawn_blocking(move || {
                        let mut found: HashMap<OsString, std::fs::Metadata> = HashMap::new();
                        if let Ok(rd) = std::fs::read_dir(&dir) {
                            for e in rd.flatten() {
                                if let Ok(m) = e.metadata() {
                                    found.insert(e.file_name(), m);
                                }
                            }
                        }
                        found
                    }));
                }
                for t in tasks {
                    std::hint::black_box(t.await.map(|m| m.len()).unwrap_or(0));
                }
                scans_mapped = scans_mapped.min(t.elapsed().as_secs_f64() * 1e3);
            }

            // (a3) the per-include plumbing, with no filesystem in it at all:
            // hash the stored name, find the metadata, clone the path, put it in
            // the lookup cache, take it out again. This is what the lookup does
            // around each include now.
            //
            // Where that metadata comes from is the one platform difference. On
            // Windows it is looked up in the directory map a scan built, so that
            // indirection is part of the plumbing; elsewhere the per-file stat
            // hands its answer straight over and there is no map in between.
            let mut plumbing = f64::MAX;
            {
                #[cfg(windows)]
                let mut dir_maps: HashMap<PathBuf, HashMap<OsString, FileFacts>> = HashMap::new();
                #[cfg(windows)]
                for dir in &parents {
                    let mut found = HashMap::new();
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        for e in rd.flatten() {
                            if let Ok(m) = e.metadata() {
                                found.insert(e.file_name(), FileFacts::from(&m));
                            }
                        }
                    }
                    dir_maps.insert(dir.clone(), found);
                }
                #[cfg(not(windows))]
                let direct: HashMap<OsString, FileFacts> = files
                    .iter()
                    .filter_map(|f| {
                        std::fs::symlink_metadata(f)
                            .ok()
                            .map(|m| (f.as_os_str().to_os_string(), FileFacts::from(&m)))
                    })
                    .collect();
                let stored: Vec<OsString> =
                    files.iter().map(|f| f.as_os_str().to_os_string()).collect();
                for _ in 0..5 {
                    let t = std::time::Instant::now();
                    let mut cache: StatCache = HashMap::new();
                    for name in &stored {
                        let path = PathBuf::from(name);
                        #[cfg(windows)]
                        let meta = path
                            .parent()
                            .and_then(|d| dir_maps.get(d))
                            .and_then(|m| path.file_name().and_then(|n| m.get(n)))
                            .copied();
                        #[cfg(not(windows))]
                        let meta = direct.get(name).copied();
                        cache.insert(
                            name.clone(),
                            (path.clone(), meta.ok_or(std::io::ErrorKind::NotFound)),
                        );
                    }
                    for name in &stored {
                        if let Some((p, m)) = cache.get(name) {
                            let _ = std::hint::black_box((p.clone(), *m));
                        }
                    }
                    plumbing = plumbing.min(t.elapsed().as_secs_f64() * 1e3);
                }
            }
            println!("  {n:>5} includes: plumbing alone, no filesystem  {plumbing:>6.2} ms");

            // How many directory scans does one lookup actually issue? There are
            // only `parents.len()` distinct directories; anything more is the
            // read-ahead batches asking the same directory again.
            #[cfg(windows)]
            {
                use std::sync::atomic::Ordering;
                let mut fresh = entry_over(&files, 1, &[]);
                SCANS.store(0, Ordering::Relaxed);
                let mut upd = false;
                let _ = fresh
                    .lookup_result_digest(config, &mut upd, &pool, &[])
                    .await;
                println!(
                    "  {n:>5} includes: {} scans for {} directories",
                    SCANS.load(Ordering::Relaxed),
                    parents.len()
                );
                let (dirs, kept, bytes) = retained_by_dirs(&files);
                println!(
                    "  {n:>5} includes: dirs cache keeps {kept} entries over {dirs} dirs \
                     (~{:.0} KiB, {:.0}% of it wanted)",
                    bytes as f64 / 1024.0,
                    n as f64 / kept as f64 * 100.0,
                );
            }

            // (b) the entry itself, paid when it is read off disk.
            let mut buf = Vec::new();
            entry.serialize_to(&mut buf).expect("serialize");
            let mut decode = f64::MAX;
            for _ in 0..20 {
                let t = std::time::Instant::now();
                std::hint::black_box(PreprocessorCacheEntry::read(&buf).expect("read"));
                decode = decode.min(t.elapsed().as_secs_f64() * 1e3);
            }

            #[cfg(windows)]
            println!(
                "  {n:>5} includes: lookup {full:>6.2} = scan {syscalls:>5.2} + maps {:>5.2} + rest {:>5.2} ({:>2.0}%)  | decode {decode:.2}",
                scans_mapped - syscalls,
                full - scans_mapped,
                (full - scans_mapped) / full * 100.0,
            );
            #[cfg(not(windows))]
            println!(
                "  {n:>5} includes: lookup {full:>6.2} = stats {syscalls:>5.2} + rest {:>5.2} ({:>2.0}%)  | decode {decode:.2}",
                full - syscalls,
                (full - syscalls) / full * 100.0,
            );
        }

        // Density: a directory scan pays for the whole directory. Same include
        // count, same directory count, but each directory holds far more files
        // than the entry names -- which is the shape that argues against it.
        {
            let root = tempfile::tempdir().expect("tempdir");
            let mut included = Vec::new();
            for d in 0..8 {
                let dir = root.path().join(format!("crowded_{d}"));
                std::fs::create_dir_all(&dir).expect("mkdir");
                for f in 0..500 {
                    let p = dir.join(format!("header_{f}.hpp"));
                    std::fs::write(
                        &p,
                        "#pragma once
",
                    )
                    .expect("write");
                    if f % 8 == 0 {
                        included.push(p);
                    }
                }
            }
            let n = included.len();
            let mut entry = entry_over(&included, 1, &[]);
            let (ms, hit) = time_lookup(&mut entry, config, &[], 5).await;
            println!(
                "  {n:>5} includes / 8 crowded dirs   {ms:>8.2} ms  ({:.0}% density)  hit: {hit}",
                n as f64 / 4000.0 * 100.0
            );
            #[cfg(windows)]
            {
                let (dirs, kept, bytes) = retained_by_dirs(&included);
                println!(
                    "  {n:>5} includes: dirs cache keeps {kept} entries over {dirs} dirs \
                     (~{:.0} KiB, {:.0}% of it wanted)",
                    bytes as f64 / 1024.0,
                    n as f64 / kept as f64 * 100.0,
                );
            }
        }

        // The shape a prefetch makes worse: a miss on the *first* include, which
        // the lazy loop settles in one `stat` and a prefetch pays for in full.
        {
            let (_root, files) = tree(8, 62);
            let n = files.len();
            let mut entry = entry_over(&files, 1, &[]);
            if let Some(v) = entry.results.values_mut().next()
                && let Some(first) = v.first_mut()
            {
                first.file_size = first.file_size.wrapping_add(1);
            }
            let (ms, hit) = time_lookup(&mut entry, config, &[], 5).await;
            println!("  {n:>5} includes / miss on file 1     {ms:>8.2} ms  hit: {hit}");
        }

        // Basedirs: the multi-basedir path adds an `exists` per candidate.
        let (_root, files) = tree(8, 62);
        let root_bytes = files[0]
            .parent()
            .and_then(|p| p.parent())
            // Normalised the way `Config` stores basedirs: forward slashes,
            // lowercase, trailing separator. Anything else silently fails to
            // strip, and then every stored path stays absolute and this
            // measures nothing.
            .map(|p| {
                let mut b = p
                    .to_string_lossy()
                    .replace(std::path::MAIN_SEPARATOR, "/")
                    .to_lowercase();
                b.push('/');
                b.into_bytes()
            })
            .expect("root");
        for count in [1usize, 3] {
            let basedirs: Vec<Vec<u8>> = std::iter::repeat_n(root_bytes.clone(), count).collect();
            let mut entry = entry_over(&files, 1, &basedirs);
            let (ms, hit) = time_lookup(&mut entry, config, &basedirs, 5).await;
            println!(
                "  {:>5} includes / {count} basedir(s)         {ms:>8.2} ms  hit: {hit}",
                files.len()
            );
        }
    }
}

/// What a cache-key change to raw digests would actually buy.
///
/// `dependency_result_key` hashes `digest.as_bytes()` on the *hex* form: 64
/// bytes per include where the digest is 32. Carrying raw bytes would halve
/// that, drop a `String` allocation per include, and remove the hex decode at
/// `add_result`. It would also change every key.
///
/// Run deliberately:
///
/// ```text
/// cargo test --release --lib key_cost -- --ignored --nocapture
/// ```
#[cfg(test)]
mod key_cost {
    use super::*;
    use crate::util::{DIGEST_BYTES, Digest};

    fn best<F: FnMut()>(rounds: usize, mut f: F) -> f64 {
        for _ in 0..3 {
            f();
        }
        let mut best = f64::MAX;
        for _ in 0..rounds {
            let t = std::time::Instant::now();
            f();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        best
    }

    /// The key, as it is computed today.
    fn key_from_hex(
        manifest: &str,
        includes: &[(String, PathBuf)],
        basedirs: &[Vec<u8>],
    ) -> String {
        crate::compiler::c::dependency_result_key(manifest, includes, basedirs)
    }

    /// The same function with the digest carried raw: half the bytes hashed per
    /// include, and nothing else changed.
    fn key_from_bytes(
        manifest: &str,
        includes: &[([u8; DIGEST_BYTES], PathBuf)],
        basedirs: &[Vec<u8>],
    ) -> String {
        let mut m = Digest::new();
        m.update(manifest.as_bytes());
        for (digest, path) in includes {
            let stripped =
                crate::util::strip_basedirs_in_arg(path.as_os_str().as_encoded_bytes(), basedirs);
            m.update(&(stripped.len() as u64).to_le_bytes());
            m.update(&stripped);
            m.update(digest);
        }
        m.finish()
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "a stopwatch, not an assertion; run it deliberately"]
    async fn what_raw_digests_would_buy() {
        println!("\ncache-key change: hex digests vs raw, best of 20\n");

        // A header of a size C++ headers actually are, so the hashing that
        // dominates the miss path is represented rather than assumed.
        let header = vec![b'x'; 8 * 1024];

        for n in [500usize, 2000] {
            let paths: Vec<PathBuf> = (0..n)
                .map(|i| {
                    PathBuf::from(format!(
                        "C:/Program Files/MSVC/include/detail/header_{i}.hpp"
                    ))
                })
                .collect();

            // (1) producing the digests: hashing the file, then hexing it.
            let hash_and_hex = best(20, || {
                for _ in 0..n {
                    let mut d = Digest::new();
                    d.update(std::hint::black_box(&header));
                    std::hint::black_box(d.finish());
                }
            });
            let hash_only = best(20, || {
                for _ in 0..n {
                    let mut d = Digest::new();
                    d.update(std::hint::black_box(&header));
                    std::hint::black_box(d.finish_bytes());
                }
            });

            // (2) the key itself.
            let hex_digest = {
                let mut d = Digest::new();
                d.update(&header);
                d.finish()
            };
            let raw_digest = crate::util::unhex_digest(&hex_digest).unwrap();
            let hex_includes: Vec<(String, PathBuf)> = paths
                .iter()
                .map(|p| (hex_digest.clone(), p.clone()))
                .collect();
            let raw_includes: Vec<([u8; DIGEST_BYTES], PathBuf)> =
                paths.iter().map(|p| (raw_digest, p.clone())).collect();

            let key_hex = best(20, || {
                std::hint::black_box(key_from_hex("manifest", &hex_includes, &[]));
            });
            let key_raw = best(20, || {
                std::hint::black_box(key_from_bytes("manifest", &raw_includes, &[]));
            });

            println!("  {n} includes, 8 KiB each:");
            println!(
                "    hash + hex      {hash_and_hex:>7.3} ms    hash + raw  {hash_only:>7.3} ms   ({:+.1}%)",
                (hash_only / hash_and_hex - 1.0) * 100.0
            );
            println!(
                "    key from hex    {key_hex:>7.3} ms    key from raw {key_raw:>6.3} ms   ({:+.1}%)",
                (key_raw / key_hex - 1.0) * 100.0
            );
            println!(
                "    total saved     {:>7.3} ms  of {:.3} ms  ({:.1}%)",
                (hash_and_hex - hash_only) + (key_hex - key_raw),
                hash_and_hex + key_hex,
                ((hash_and_hex - hash_only) + (key_hex - key_raw)) / (hash_and_hex + key_hex)
                    * 100.0
            );
        }
    }
}
