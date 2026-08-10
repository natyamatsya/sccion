// Copyright 2023 Mozilla Foundation
// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
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

use std::{
    collections::{BTreeMap, HashSet},
    ffi::{OsStr, OsString},
    hash::Hash,
    io::Write,
    path::{Path, PathBuf},
    sync::LazyLock,
    time::SystemTime,
};

use anyhow::Context;
use chrono::Datelike;
use serde::{Deserialize, Serialize};

use crate::{
    config::PreprocessorCacheModeConfig,
    util::{
        Digest, HashToDigest, MetadataCtimeExt, TimeMacroFinder, Timestamp, encode_path,
        strip_basedirs, strip_basedirs_in_arg, strip_basedirs_in_dir,
    },
};

use super::{Language, NotCacheable};

/// The current format is 1 header byte for the version + bincode encoding
/// of the [`PreprocessorCacheEntry`] struct.
///
/// Bumped to 1 when include paths started being stored relative to a basedir:
/// an entry written by the old code holds absolute paths that the new code
/// would resolve against a basedir and not find. Rejecting them outright costs
/// one cold build and cannot be confused with a legitimate miss.
const FORMAT_VERSION: u8 = 2;
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

/// Where a recorded include is on *this* machine.
///
/// The counterpart of [`portable_path`]. A stored path that is still absolute
/// was outside every basedir and is used as it stands.
fn resolve_include(stored: &OsStr, basedirs: &[Vec<u8>]) -> PathBuf {
    let path = Path::new(stored);
    if path.is_absolute() || basedirs.is_empty() {
        return path.to_path_buf();
    }
    let rooted = |basedir: &Vec<u8>| {
        std::str::from_utf8(basedir)
            .ok()
            .map(|root| Path::new(root).join(path))
    };
    // One basedir is the overwhelmingly common case, and it needs no guessing:
    // asking the filesystem which root a path belongs under would be a `stat`
    // per include per lookup, on top of the one the check makes anyway.
    if basedirs.len() == 1
        && let Some(p) = rooted(&basedirs[0])
    {
        return p;
    }
    basedirs
        .iter()
        .filter_map(rooted)
        .find(|candidate| candidate.exists())
        .unwrap_or_else(|| path.to_path_buf())
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
        // Check newest result first since it's more likely to match.
        for (digest, includes) in self.results.iter_mut().rev() {
            let result_matches =
                Self::result_matches(digest, includes, config, updated, pool, basedirs).await;
            if result_matches {
                return Some(digest.clone());
            }
        }
        None
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
    ) -> bool {
        // Resolved once: every check below is against the files of the checkout
        // being compiled, not of whichever one happened to write the entry.
        let resolved: Vec<PathBuf> = includes
            .iter()
            .map(|include| resolve_include(&include.path, basedirs))
            .collect();
        let mut needs_contents = Vec::new();

        for (index, include) in includes.iter().enumerate() {
            let path = resolved[index].as_path();
            let meta = match std::fs::symlink_metadata(path) {
                Ok(meta) => {
                    if meta.len() != include.file_size {
                        return false;
                    }
                    meta
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
                        let mtime_matches = meta.modified().map(Into::into).ok() == Some(mtime);
                        let ctime_matches = meta.ctime_or_creation().ok() == Some(ctime);
                        if mtime_matches && ctime_matches {
                            trace!("mtime+ctime hit for {}", path.display());
                            continue;
                        } else {
                            trace!("mtime+ctime miss for {}", path.display());
                        }
                    }
                    (Some(mtime), None) => {
                        let mtime_matches = meta.modified().map(Into::into).ok() == Some(mtime);
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

            needs_contents.push(index);
        }

        // Everything was settled by `stat`. With `file_stat_matches` enabled this
        // is the common case, and no file is opened at all.
        if needs_contents.is_empty() {
            return true;
        }

        let digested = digest_files_concurrently(
            needs_contents.iter().map(|&index| resolved[index].clone()),
            config.ignore_time_macros,
            pool,
        )
        .await;

        // Fold sequentially, in the original order, so the outcome does not depend
        // on which file happened to finish first.
        for (index, outcome) in needs_contents.into_iter().zip(digested) {
            let path = resolved[index].as_path();
            let include = &mut includes[index];

            let Some(contents) = outcome else {
                return false;
            };

            let finder = match contents {
                FileContents::Plain(new_digest) => {
                    if include.digest != new_digest {
                        return false;
                    }
                    // Matched: go on to the next include. Returning here would
                    // decide the whole entry on its first file.
                    continue;
                }
                FileContents::WithTimeMacros(new_digest, finder) => {
                    if !finder.found_time_macros() && include.digest != new_digest {
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
                    include.digest = new_digest.finish();
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
        "SCCION_C_CUSTOM_CACHE_BUSTER",
        "SCCACHE_C_CUSTOM_CACHE_BUSTER",
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
    /// The hash of its contents
    digest: String,
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
                digest: Digest::reader_sync(std::io::Cursor::new(&contents)).unwrap(),
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
            )),
            "a change to any include must be detected, not just the first"
        );
    }
}
