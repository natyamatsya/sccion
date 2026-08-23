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

use super::utils::{get_file_mode, set_file_mode};
use crate::errors::*;
use fs_err as fs;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::io::{Cursor, Read, Seek, Write};
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Marks a cache entry object whose payload follows verbatim.
///
/// Chosen so it can never be confused with what came before it: every zstd frame
/// starts `28 B5 2F FD`, and skippable frames start `50`-`5F`, so a leading zero
/// byte unambiguously means "not zstd". That lets uncompressed and compressed
/// entries coexist in one cache, and lets a cache written by an older sccache
/// still be read, without a format version or a cache-invalidating key change.
const UNCOMPRESSED_MARKER: u8 = 0x00;

/// Zip member name prefix for objects stored clone-ready.
///
/// A clone-ready payload is the object's bytes verbatim -- no
/// [`UNCOMPRESSED_MARKER`] -- because the marker is what makes cloning
/// impossible: shifting a region by one byte means its source and destination
/// can never both be cluster-aligned. With no marker to dispatch on, the format
/// has to be carried by the member's *name* instead, which is also what lets
/// both layouts coexist in one cache.
const RAW_PREFIX: &str = "raw/";

/// Alignment of a clone-ready payload within the entry file.
///
/// `FSCTL_DUPLICATE_EXTENTS_TO_FILE` addresses whole clusters, so the payload
/// must start on a cluster boundary of the volume the entry lands on. 4096 is
/// the cluster size ReFS and Dev Drive format with by default; a volume using
/// 64K clusters will see these offsets as misaligned and fall back to copying,
/// which is correct just slower. The write side cannot know the destination
/// volume's geometry -- entries are built in memory and may be backfilled from
/// remote storage onto any disk.
const CLONE_ALIGNMENT: u16 = 4096;

/// Below this, the clone-ready layout is not used.
///
/// Alignment costs up to `CLONE_ALIGNMENT - 1` bytes of padding per member, and
/// the clone itself is refused below `MIN_CLONE_CLUSTERS` anyway (see
/// `blockclone`), so small payloads -- stdout, stderr, dependency files --
/// would pay the padding for a fast path they can never take.
const RAW_THRESHOLD: u64 = 16 * 1024;

/// How object payloads are stored inside a cache entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjectEncoding {
    /// zstd at the given level.
    Zstd(i32),
    /// Verbatim, behind [`UNCOMPRESSED_MARKER`].
    ///
    /// Trades cache capacity for materialization speed. Under `/Z7` objects
    /// compress around 7:1, so this costs roughly that much disk -- in exchange
    /// for a cache hit that is a read and a write rather than a decompression.
    /// It is also what makes filesystem-level block cloning possible at all: a
    /// compressed blob shares no extents with the object it expands to.
    Uncompressed,
}

/// What the config file asked for, if the server was told before it served
/// anything. See [`set_configured_encoding`].
static CONFIGURED_ENCODING: std::sync::OnceLock<ObjectEncoding> = std::sync::OnceLock::new();

/// Record what the config file asked for.
///
/// Must be called before the first cache entry is read or written, since
/// [`OBJECT_ENCODING`] resolves once and keeps the answer. The server does it
/// at startup, which is the only place that has both the config and the
/// guarantee that nothing has been served yet.
pub fn set_configured_encoding(encoding: ObjectEncoding) {
    if CONFIGURED_ENCODING.set(encoding).is_err() {
        debug!("Object encoding was already configured; ignoring {encoding:?}");
    }
}

/// Read once per process rather than per object.
///
/// This is the daemon's environment, so it is fixed for the daemon's lifetime;
/// changing it requires restarting the server. That was already true of
/// `SCCACHE_CACHE_ZSTD_LEVEL`, which used to be re-read on every single object.
///
/// The environment wins over the config file: the file is the answer for this
/// machine, the environment is the answer for this run of it.
static OBJECT_ENCODING: std::sync::LazyLock<ObjectEncoding> = std::sync::LazyLock::new(|| {
    let uncompressed = crate::envvar::var("SCCION_CACHE_COMPRESSION").map(|v| {
        let v = v.trim().to_ascii_lowercase();
        v == "none" || v == "off" || v == "false" || v == "0"
    });
    match uncompressed {
        Some(true) => return ObjectEncoding::Uncompressed,
        // Named in the environment as compressed: the level may still come
        // from either place.
        Some(false) => {}
        None => {
            if let Some(configured) = CONFIGURED_ENCODING.get() {
                return *configured;
            }
        }
    }
    let level = crate::envvar::var("SCCION_CACHE_ZSTD_LEVEL")
        .and_then(|value| value.parse::<i32>().ok())
        .or_else(|| match CONFIGURED_ENCODING.get() {
            Some(ObjectEncoding::Zstd(level)) => Some(*level),
            _ => None,
        })
        .unwrap_or(3);
    ObjectEncoding::Zstd(level)
});

/// Cache object sourced by a file.
#[derive(Clone)]
pub struct FileObjectSource {
    /// Identifier for this object. Should be unique within a compilation unit.
    /// Note that a compilation unit is a single source file in C/C++ and a crate in Rust.
    pub key: String,
    /// Absolute path to the file.
    pub path: PathBuf,
    /// Whether the file must be present on disk and is essential for the compilation.
    pub optional: bool,
}

/// Result of a cache lookup.
pub enum Cache {
    /// Result was found in cache.
    Hit(CacheRead),
    /// Result was not found in cache.
    Miss,
    /// Do not cache the results of the compilation.
    None,
    /// Cache entry should be ignored, force compilation.
    Recache,
}

impl fmt::Debug for Cache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Cache::Hit(_) => write!(f, "Cache::Hit(...)"),
            Cache::Miss => write!(f, "Cache::Miss"),
            Cache::None => write!(f, "Cache::None"),
            Cache::Recache => write!(f, "Cache::Recache"),
        }
    }
}

/// CacheMode is used to represent which mode we are using.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CacheMode {
    /// Only read cache from storage.
    ReadOnly,
    /// Full support of cache storage: read and write.
    ReadWrite,
}

/// Trait objects can't be bounded by more than one non-builtin trait.
pub trait ReadSeek: Read + Seek + Send {}

impl<T: Read + Seek + Send> ReadSeek for T {}

/// Data stored in the compiler cache.
pub struct CacheRead {
    zip: ZipArchive<Box<dyn ReadSeek>>,
    /// Where this entry lives on disk, when the backend can say.
    ///
    /// Only a local file can donate extents, so this is what decides whether
    /// materialization may clone instead of copy. Remote backends leave it
    /// `None` and nothing changes for them.
    path: Option<PathBuf>,
}

/// Represents a failure to decompress stored object data.
#[derive(Debug)]
pub struct DecompressionFailure;

impl std::fmt::Display for DecompressionFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "failed to decompress content")
    }
}

impl std::error::Error for DecompressionFailure {}

/// Copy `from` to `to` through a buffer sized for the object, not for `io::copy`.
///
/// `std::io::copy` uses an 8 KiB stack buffer unless it can specialise, and it
/// cannot here: the reader is a zip member and the writer is a generic `W`. That
/// is 256 reads and 256 writes for a 2 MiB object, and it measured as two thirds
/// of the whole extract -- 2.48 ms against 0.83 for the same bytes moved in
/// megabyte strides.
///
/// Sized to the member rather than fixed, so a small object does not allocate a
/// megabyte to move 40 KiB, and capped above because past a megabyte the reads
/// are already amortised.
///
/// The lower clamp is not a tuning choice. `size` comes from the zip central
/// directory, which is data in the entry rather than a fact about it, and a
/// zero there would size the buffer at zero -- `read` into an empty slice
/// returns `Ok(0)`, the loop below would take that for end of input, and the
/// object would be written out empty with no error anywhere. `MIN` is what
/// stops a claimed length from deciding whether any bytes get copied at all.
///
/// `try_from` rather than `as`: `usize` is 32 bits on the i686 and armv7 targets
/// this ships to, where a cast would truncate a large `size` to an arbitrary
/// small one and land on the wrong side of the clamp.
fn copy_buffered<R: Read, W: Write>(from: &mut R, to: &mut W, size: u64) -> std::io::Result<()> {
    const MIN: usize = 64 * 1024;
    const MAX: usize = 1024 * 1024;
    let wanted = usize::try_from(size).unwrap_or(usize::MAX);
    let mut buf = vec![0u8; wanted.clamp(MIN, MAX)];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            return Ok(());
        }
        to.write_all(&buf[..n])?;
    }
}

impl CacheRead {
    /// Create a cache entry from `reader`.
    pub fn from<R>(reader: R) -> Result<CacheRead>
    where
        R: ReadSeek + 'static,
    {
        let z = ZipArchive::new(Box::new(reader) as Box<dyn ReadSeek>)
            .context("Failed to parse cache entry")?;
        Ok(CacheRead { zip: z, path: None })
    }

    /// As [`CacheRead::from`], additionally remembering where the entry lives
    /// on disk so materialization may clone objects out of it.
    pub fn from_file_at<R>(reader: R, path: PathBuf) -> Result<CacheRead>
    where
        R: ReadSeek + 'static,
    {
        let mut read = Self::from(reader)?;
        read.path = Some(path);
        Ok(read)
    }

    /// Get an object from this cache entry at `name` and write it to `to`.
    /// If the file has stored permissions, return them.
    pub fn get_object<T>(&mut self, name: &str, to: &mut T) -> Result<Option<u32>>
    where
        T: Write,
    {
        // Clone-ready members carry the bytes verbatim under a prefixed name;
        // streaming them out is always available, cloning never required.
        if let Ok(mut raw) = self.zip.by_name(&format!("{RAW_PREFIX}{name}")) {
            if raw.compression() != CompressionMethod::Stored {
                bail!(DecompressionFailure);
            }
            let mode = raw.unix_mode();
            let size = raw.size();
            copy_buffered(&mut raw, to, size).or(Err(DecompressionFailure))?;
            return Ok(mode);
        }
        let mut file = self.zip.by_name(name).or(Err(DecompressionFailure))?;
        if file.compression() != CompressionMethod::Stored {
            bail!(DecompressionFailure);
        }
        let mode = file.unix_mode();

        // The encoding is a property of the entry, not of this process: a cache
        // can hold both, and one written before uncompressed storage existed must
        // still read correctly. So dispatch on the payload's first byte rather
        // than on how this daemon happens to be configured.
        let mut first = [0u8; 1];
        match file.read(&mut first) {
            Ok(0) => return Ok(mode), // empty object, nothing to write
            Ok(_) => {}
            Err(_) => bail!(DecompressionFailure),
        }

        if first[0] == UNCOMPRESSED_MARKER {
            let size = file.size();
            copy_buffered(&mut file, to, size).or(Err(DecompressionFailure))?;
        } else {
            // Put the byte back in front; zstd needs its magic intact.
            zstd::stream::copy_decode(first.as_slice().chain(file), to)
                .or(Err(DecompressionFailure))?;
        }
        Ok(mode)
    }

    /// Get the stdout from this cache entry, if it exists.
    pub fn get_stdout(&mut self) -> Vec<u8> {
        self.get_bytes("stdout")
    }

    /// Get the stderr from this cache entry, if it exists.
    pub fn get_stderr(&mut self) -> Vec<u8> {
        self.get_bytes("stderr")
    }

    fn get_bytes(&mut self, name: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        drop(self.get_object(name, &mut bytes));
        bytes
    }

    /// Where the clone-ready member for `name` sits inside the entry file.
    ///
    /// `None` unless everything cloning needs is true at once: the entry has an
    /// on-disk path, the member exists, it is stored (not deflated), and its
    /// payload begins on a [`CLONE_ALIGNMENT`] boundary. The last check matters
    /// because an entry may have been written by a build that did not align --
    /// offsets must be proven, never assumed, or the clone would hand the
    /// compiler a file of shifted garbage.
    fn raw_member_span(&mut self, name: &str) -> Option<(u64, u64, Option<u32>)> {
        self.path.as_ref()?;
        let member = self.zip.by_name(&format!("{RAW_PREFIX}{name}")).ok()?;
        if member.compression() != CompressionMethod::Stored {
            return None;
        }
        // `data_start` is `None` until the member's local header has been read.
        // Unproven is the same as unaligned here: both mean the offset cannot be
        // trusted, and a clone from an offset nobody checked is shifted garbage.
        let data_start = member.data_start()?;
        if !data_start.is_multiple_of(u64::from(CLONE_ALIGNMENT)) {
            return None;
        }
        Some((data_start, member.size(), member.unix_mode()))
    }

    /// Returns how many objects were materialized by cloning rather than
    /// copying. Callers that report statistics should pass it on: a silent
    /// fallback to copying is otherwise indistinguishable from the fast path
    /// working, which is how a fast path quietly dies.
    pub async fn extract_objects<T>(
        mut self,
        objects: T,
        pool: &tokio::runtime::Handle,
    ) -> Result<u64>
    where
        T: IntoIterator<Item = FileObjectSource> + Send + Sync + 'static,
    {
        pool.spawn_blocking(move || {
            let mut cloned: u64 = 0;
            for FileObjectSource {
                key,
                path,
                optional,
            } in objects
            {
                if is_path_null(&path) {
                    // For unix, this is just a fast path to discard such outputs,
                    // so it is not an issue if `is_path_null` has false-negatives.
                    // But for Windows, since `NUL` looks like a relative path, the
                    // temporary file creation logic would happily succeed, creating
                    // a temp file in the CWD, but then the subsequent `persist`
                    // would fail with `ERROR_ALREADY_EXISTS`, since `NUL` always
                    // exists and cannot be `replaced`, so we really need to
                    // short-circuit more of such cases on Windows.
                    debug!("Skipping output to {}", path.display());
                    continue;
                }
                let dir = match path.parent() {
                    Some(d) => d,
                    None => bail!("Output file without a parent directory!"),
                };
                // Write the cache entry to a tempfile and then atomically
                // move it to its final location so that other rustc invocations
                // happening in parallel don't see a partially-written file.
                match (NamedTempFile::new_in(dir), optional) {
                    (Ok(mut tmp), _) => {
                        // Clone-ready member plus an on-disk entry: share the
                        // extents instead of copying the bytes. Any refusal
                        // falls through to the streaming path below -- after
                        // resetting the temp file, which a part-way clone may
                        // have grown.
                        if let Some((offset, len, mode)) = self.raw_member_span(&key) {
                            match crate::blockclone::clone_region_into(
                                self.path.as_deref().expect("span implies a path"),
                                offset,
                                len,
                                tmp.as_file(),
                            ) {
                                Ok(()) => {
                                    tmp.persist(&path)?;
                                    if let Some(mode) = mode {
                                        set_file_mode(path.as_path(), mode)?;
                                    }
                                    cloned += 1;
                                    continue;
                                }
                                Err(reason) => {
                                    trace!("cache_io: not cloning {}: {reason}", path.display());
                                    tmp.as_file().set_len(0)?;
                                }
                            }
                        }
                        match (self.get_object(&key, &mut tmp), optional) {
                            (Ok(mode), _) => {
                                tmp.persist(&path)?;
                                if let Some(mode) = mode {
                                    set_file_mode(path.as_path(), mode)?;
                                }
                            }
                            (Err(e), false) => return Err(e),
                            // skip if no object found and it's optional
                            (Err(_), true) => continue,
                        }
                    }
                    (Err(e), false) => {
                        // Fall back to writing directly to the final location
                        warn!("Failed to create temp file on the same file system: {e}");
                        let mut f = std::fs::File::create(&path)?;
                        // `optional` is false in this branch, so do not ignore errors
                        let mode = self.get_object(&key, &mut f)?;
                        if let Some(mode) = mode
                            && let Err(e) = set_file_mode(path.as_path(), mode)
                        {
                            // Here we ignore errors from setting file mode because
                            // if we could not create a temp file in the same directory,
                            // we probably can't set the mode either (e.g. /dev/stuff)
                            warn!("Failed to reset file mode: {e}");
                        }
                    }
                    // skip if no object found and it's optional
                    (Err(_), true) => continue,
                }
            }
            Ok(cloned)
        })
        .await?
    }
}

#[cfg(unix)]
fn is_path_null(path: &Path) -> bool {
    path == Path::new("/dev/null")
}

#[cfg(windows)]
fn is_path_null(path: &Path) -> bool {
    // For Windows, it appears that `NUL` with whatever extension is also a blackhole
    // (at least for `CreateFileX`), so it does not suffice to check for an exact match
    // Also note that gcc, cl.exe, et al. append a correct extension automatically even
    // if the user asks for output to `NUL`.
    let Some(stem) = path.file_stem() else {
        return false;
    };
    stem.eq_ignore_ascii_case("NUL")
}

/// Data to be stored in the compiler cache.
pub struct CacheWrite {
    zip: ZipWriter<Cursor<Vec<u8>>>,
}

impl CacheWrite {
    /// Create a new, empty cache entry.
    pub fn new() -> CacheWrite {
        CacheWrite {
            zip: ZipWriter::new(Cursor::new(vec![])),
        }
    }

    /// Create a new cache entry populated with the contents of `objects`.
    pub async fn from_objects<T>(objects: T, pool: &tokio::runtime::Handle) -> Result<CacheWrite>
    where
        T: IntoIterator<Item = FileObjectSource> + Send + Sync + 'static,
    {
        pool.spawn_blocking(move || {
            let mut entry = CacheWrite::new();
            for FileObjectSource {
                key,
                path,
                optional,
            } in objects
            {
                let f = fs::File::open(&path)
                    .with_context(|| format!("failed to open file `{:?}`", path));
                match (f, optional) {
                    (Ok(mut f), _) => {
                        let mode = get_file_mode(&f)?;
                        entry.put_object(&key, &mut f, mode).with_context(|| {
                            format!("failed to put object `{:?}` in cache entry", path)
                        })?;
                    }
                    (Err(e), false) => return Err(e),
                    (Err(_), true) => continue,
                }
            }
            Ok(entry)
        })
        .await?
    }

    /// Add an object containing the contents of `from` to this cache entry at `name`.
    /// If `mode` is `Some`, store the file entry with that mode.
    pub fn put_object<T>(&mut self, name: &str, from: &mut T, mode: Option<u32>) -> Result<()>
    where
        T: Read,
    {
        self.put_object_with_encoding(name, from, mode, *OBJECT_ENCODING)
    }

    /// As [`CacheWrite::put_object`], with the encoding stated by the caller.
    ///
    /// The split exists so tests can exercise every layout without fighting
    /// over [`OBJECT_ENCODING`], which resolves once per process.
    fn put_object_with_encoding<T>(
        &mut self,
        name: &str,
        from: &mut T,
        mode: Option<u32>,
        encoding: ObjectEncoding,
    ) -> Result<()>
    where
        T: Read,
    {
        // The zip compression method is always "stored"; what varies is what we
        // store inside the member -- a zstd frame, a marker byte followed by
        // the bytes verbatim, or (clone-ready) the bytes alone under a prefixed
        // name. See `ObjectEncoding` and `RAW_PREFIX`.
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        let opts = if let Some(mode) = mode {
            opts.unix_permissions(mode)
        } else {
            opts
        };

        match encoding {
            ObjectEncoding::Zstd(level) => {
                self.zip
                    .start_file(name, opts)
                    .context("Failed to start cache entry object")?;
                zstd::stream::copy_encode(from, &mut self.zip, level)?;
            }
            ObjectEncoding::Uncompressed => {
                // Payloads big enough to clone go in clone-ready: verbatim
                // bytes, no marker, aligned so a hit can donate the extents
                // instead of copying them. Size is only learnable by reading,
                // so read up to the threshold first and decide then.
                let mut head = Vec::with_capacity(RAW_THRESHOLD as usize + 1);
                from.take(RAW_THRESHOLD + 1).read_to_end(&mut head)?;
                if head.len() as u64 > RAW_THRESHOLD {
                    self.zip
                        .start_file(
                            format!("{RAW_PREFIX}{name}"),
                            opts.with_alignment(CLONE_ALIGNMENT),
                        )
                        .context("Failed to start clone-ready cache entry object")?;
                    self.zip.write_all(&head)?;
                    std::io::copy(from, &mut self.zip)?;
                } else {
                    // Too small to clone: the ordinary marker layout.
                    self.zip
                        .start_file(name, opts)
                        .context("Failed to start cache entry object")?;
                    self.zip.write_all(&[UNCOMPRESSED_MARKER])?;
                    self.zip.write_all(&head)?;
                }
            }
        }
        Ok(())
    }

    pub fn put_stdout(&mut self, bytes: &[u8]) -> Result<()> {
        self.put_bytes("stdout", bytes)
    }

    pub fn put_stderr(&mut self, bytes: &[u8]) -> Result<()> {
        self.put_bytes("stderr", bytes)
    }

    fn put_bytes(&mut self, name: &str, bytes: &[u8]) -> Result<()> {
        if !bytes.is_empty() {
            let mut cursor = Cursor::new(bytes);
            return self.put_object(name, &mut cursor, None);
        }
        Ok(())
    }

    /// Finish writing data to the cache entry writer, and return the data.
    pub fn finish(self) -> Result<Vec<u8>> {
        let CacheWrite { zip } = self;
        let cur = zip.finish().context("Failed to finish cache entry zip")?;
        Ok(cur.into_inner())
    }
}

impl Default for CacheWrite {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A payload the raw-layout threshold considers worth cloning.
    /// A claimed length must not decide whether any bytes are copied.
    ///
    /// `copy_buffered` sizes its buffer from the zip member's declared size,
    /// which is data inside the entry rather than a fact about it. A zero there
    /// would give a zero-length buffer, `read` into an empty slice returns
    /// `Ok(0)`, and the loop would read that as end of input -- writing an empty
    /// object out and reporting success. The lower clamp is what prevents that,
    /// so this asserts the clamp rather than the arithmetic.
    #[test]
    fn a_bogus_declared_size_still_copies_every_byte() {
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();

        for claimed in [0u64, 1, 7, u64::MAX] {
            let mut out = Vec::new();
            copy_buffered(&mut Cursor::new(&payload), &mut out, claimed)
                .expect("the copy itself cannot fail on a vector");
            assert_eq!(
                out, payload,
                "a declared size of {claimed} changed what was copied"
            );
        }
    }

    fn big_payload() -> Vec<u8> {
        (0..(RAW_THRESHOLD as u32 * 4))
            .map(|i| (i.wrapping_mul(2654435761) >> 11) as u8)
            .collect()
    }

    fn entry_with(name: &str, payload: &[u8], encoding: ObjectEncoding) -> Vec<u8> {
        let mut w = CacheWrite::new();
        w.put_object_with_encoding(name, &mut Cursor::new(payload), None, encoding)
            .unwrap();
        w.finish().unwrap()
    }

    fn read_back(entry: &[u8], name: &str) -> Vec<u8> {
        let mut r = CacheRead::from(Cursor::new(entry.to_vec())).unwrap();
        let mut out = Vec::new();
        r.get_object(name, &mut out).unwrap();
        out
    }

    /// The clone-ready layout must hold the payload aligned and verbatim --
    /// aligned because `FSCTL_DUPLICATE_EXTENTS_TO_FILE` addresses whole
    /// clusters, verbatim because the cloned region reaches the compiler
    /// without any decoding step that could strip a marker.
    #[test]
    fn a_large_uncompressed_object_is_stored_clone_ready() {
        let payload = big_payload();
        let entry = entry_with("obj", &payload, ObjectEncoding::Uncompressed);

        let mut zip = ZipArchive::new(Cursor::new(entry.clone())).unwrap();
        let member = zip.by_name("raw/obj").expect("clone-ready member");
        assert_eq!(
            member.data_start().unwrap() % u64::from(CLONE_ALIGNMENT),
            0,
            "payload must start on a cluster boundary"
        );
        assert_eq!(member.size(), payload.len() as u64, "no marker, no padding");
        let start = member.data_start().unwrap() as usize;
        drop(member);
        assert_eq!(
            &entry[start..start + payload.len()],
            &payload[..],
            "the region a clone would share must be the object bytes themselves"
        );

        // And the ordinary read path must still produce them.
        assert_eq!(read_back(&entry, "obj"), payload);
    }

    /// Small payloads must not pay the alignment padding: stdout, stderr and
    /// dependency files would bloat every entry for a fast path they can never
    /// take (the clone itself refuses below a few clusters).
    #[test]
    fn a_small_uncompressed_object_keeps_the_marker_layout() {
        let payload = b"warning: something minor".to_vec();
        let entry = entry_with("stderr", &payload, ObjectEncoding::Uncompressed);

        let mut zip = ZipArchive::new(Cursor::new(entry.clone())).unwrap();
        assert!(zip.by_name("raw/stderr").is_err(), "no clone-ready member");
        drop(zip.by_name("stderr").expect("marker-layout member"));
        assert_eq!(read_back(&entry, "stderr"), payload);
    }

    #[test]
    fn zstd_entries_are_unchanged_by_the_clone_layout() {
        let payload = big_payload();
        let entry = entry_with("obj", &payload, ObjectEncoding::Zstd(3));

        let mut zip = ZipArchive::new(Cursor::new(entry.clone())).unwrap();
        assert!(zip.by_name("raw/obj").is_err());
        assert_eq!(read_back(&entry, "obj"), payload);
    }

    /// Alignment is proven per member, never assumed from the entry having a
    /// path: an entry written by a build that did not align must fall back to
    /// streaming, not clone a shifted region into the compiler's output.
    #[test]
    fn an_unaligned_raw_member_is_not_offered_for_cloning() {
        // A raw/ member written *without* alignment, as a non-aligning writer
        // would: manual zip construction, ordinary start_file.
        let payload = big_payload();
        let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
        zip.start_file("raw/obj", opts).unwrap();
        zip.write_all(&payload).unwrap();
        let entry = zip.finish().unwrap().into_inner();

        let dir = tempfile::tempdir().unwrap();
        let on_disk = dir.path().join("entry");
        std::fs::write(&on_disk, &entry).unwrap();

        let mut aligned_read =
            CacheRead::from_file_at(Cursor::new(entry.clone()), on_disk).unwrap();
        // data_start is 30 + name for this hand-built zip: unaligned.
        assert!(
            aligned_read.raw_member_span("obj").is_none(),
            "an unaligned member must not be offered for cloning"
        );

        // Without a path there is nothing to clone from, aligned or not.
        let aligned_entry = entry_with("obj", &payload, ObjectEncoding::Uncompressed);
        let mut pathless = CacheRead::from(Cursor::new(aligned_entry)).unwrap();
        assert!(pathless.raw_member_span("obj").is_none());

        // And the same bytes remain readable the ordinary way regardless.
        assert_eq!(read_back(&entry, "obj"), payload);
    }

    /// The whole point, end to end: an on-disk entry with an aligned member
    /// names the exact span the writer laid down.
    #[test]
    fn an_aligned_raw_member_names_its_span() {
        let payload = big_payload();
        let entry = entry_with("obj", &payload, ObjectEncoding::Uncompressed);
        let dir = tempfile::tempdir().unwrap();
        let on_disk = dir.path().join("entry");
        std::fs::write(&on_disk, &entry).unwrap();

        let mut read = CacheRead::from_file_at(Cursor::new(entry.clone()), on_disk).unwrap();
        let (offset, len, _mode) = read.raw_member_span("obj").expect("aligned, on disk");
        assert_eq!(offset % u64::from(CLONE_ALIGNMENT), 0);
        assert_eq!(len, payload.len() as u64);
        assert_eq!(
            &entry[offset as usize..offset as usize + len as usize],
            &payload[..]
        );
    }

    /// The full hit path on a clone-capable volume: the extracted object's
    /// physical clusters must be the entry's own, and the count must say so.
    ///
    /// Gated the same way as the blockclone tests: cloning needs a filesystem
    /// that supports it, and silently passing on NTFS would assert nothing.
    #[cfg(windows)]
    #[test]
    fn extraction_clones_from_an_aligned_entry() {
        let Some(root) = crate::envvar::var_os("SCCION_TEST_CLONE_DIR") else {
            eprintln!("skipping: set SCCION_TEST_CLONE_DIR to a clone-capable volume");
            return;
        };
        let dir = PathBuf::from(root).join(format!("sccion-io-clone-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let payload = big_payload();
        let entry = entry_with("obj", &payload, ObjectEncoding::Uncompressed);
        let on_disk = dir.join("entry");
        std::fs::write(&on_disk, &entry).unwrap();

        let read = CacheRead::from_file_at(Cursor::new(entry), on_disk.clone()).unwrap();
        let out = dir.join("restored.obj");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let cloned = runtime
            .block_on(read.extract_objects(
                vec![FileObjectSource {
                    key: "obj".to_string(),
                    path: out.clone(),
                    optional: false,
                }],
                runtime.handle(),
            ))
            .unwrap();

        assert_eq!(std::fs::read(&out).unwrap(), payload, "bytes must survive");
        if cloned == 0 {
            // The volume declined (non-ReFS, 64K clusters, ...). The fallback
            // already proved itself above; just say so and stop.
            eprintln!("skipping extent check: volume declined to clone");
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        assert_eq!(cloned, 1, "the one object must report as cloned");
        let src_extents = crate::blockclone::extent_lcns(&on_disk).unwrap();
        let dst_extents = crate::blockclone::extent_lcns(&out).unwrap();
        assert!(
            !dst_extents.is_empty(),
            "restored object should have extents"
        );
        // The destination's clusters must appear within the entry's runs: the
        // entry is larger (zip headers, central directory), so equality is not
        // expected -- containment of the first physical cluster is the proof.
        let dst_first_lcn = dst_extents[0].1;
        let mut prev_vcn = 0i64;
        let shared = src_extents.iter().any(|&(next_vcn, lcn)| {
            let run_len = next_vcn - prev_vcn;
            prev_vcn = next_vcn;
            lcn >= 0 && dst_first_lcn >= lcn && dst_first_lcn < lcn + run_len
        });
        assert!(
            shared,
            "restored object's first cluster {dst_first_lcn} not within the \
             entry's physical runs {src_extents:?} -- the bytes were copied, \
             not cloned"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_object_to_devnull_works() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let pool = runtime.handle();

        let cache_data = CacheWrite::new();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();

        let objects = vec![FileObjectSource {
            key: "test_key".to_string(),
            path: PathBuf::from("/dev/null"),
            optional: false,
        }];

        let result = runtime.block_on(cache_read.extract_objects(objects, pool));
        assert!(result.is_ok(), "Extracting to /dev/null should succeed");
    }

    #[cfg(unix)]
    #[test]
    fn test_extract_object_to_dev_fd_something() {
        // Open a pipe, write to `/dev/fd/{fd}` and check the other end that the correct data was written.
        use std::os::fd::AsRawFd;
        use tokio::io::AsyncReadExt;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();
        let pool = runtime.handle();
        let mut cache_data = CacheWrite::new();
        let data = b"test data";
        cache_data.put_bytes("test_key", data).unwrap();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();
        runtime.block_on(async {
            let (sender, mut receiver) = tokio::net::unix::pipe::pipe().unwrap();
            let sender_fd = sender.into_blocking_fd().unwrap();
            let raw_fd = sender_fd.as_raw_fd();
            let fd_path = PathBuf::from(format!("/dev/fd/{raw_fd}"));
            let objects = vec![FileObjectSource {
                key: "test_key".to_string(),
                path: fd_path.clone(),
                optional: false,
            }];
            // On FreeBSD, `/dev/fd/{fd}` does not always exist (i.e. without mounting `fdescfs`), so we skip this test if we get `ENOENT`.
            if ! fd_path.exists() {
                info!("Skipping test_extract_object_to_dev_fd_something because /dev/fd/{raw_fd} does not exist");
                return;
            }
            let result = cache_read.extract_objects(objects, pool).await;
            assert!(
                result.is_ok(),
                "Extracting to /dev/fd/{raw_fd} should succeed"
            );
            let mut buf = vec![0; data.len()];
            let n = receiver.read_exact(&mut buf).await.unwrap();
            assert_eq!(n, data.len(), "Read the correct number of bytes");
            assert_eq!(buf, data, "Read the correct data from /dev/fd/{raw_fd}");
        });
    }

    #[test]
    fn test_extract_object_to_non_writable_path() {
        // See `test_extract_object_to_dev_fd_something`: we still cannot cover all platforms by the other tests. Here we test a more portable case of creating a file and making its parent directory non-writable, in which case we should still be able to extract the object successfully.

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let pool = runtime.handle();

        let mut cache_data = CacheWrite::new();
        cache_data.put_bytes("test_key", b"real_test_data").unwrap();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();

        let tmpdir = tempfile::tempdir().unwrap();
        let target_path = tmpdir.path().join("test_file");
        std::fs::write(&target_path, b"test").unwrap();
        // The current Rust fs permissions API is kind of awkward...
        let mut perm = tmpdir.path().metadata().unwrap().permissions();
        perm.set_readonly(true);
        std::fs::set_permissions(tmpdir.path(), perm.clone()).unwrap();
        // Note that this doesn't guarantee that the a new file cannot be created anymore.
        // For example, as documented in `std::fs::Permissions::set_readonly`, the
        // `FILE_ATTRIBUTE_READONLY` attribute on Windows is entirely ignored for directories.
        // std::fs::File::create(tmpdir.path().join("another_file")).unwrap_err();

        let objects = vec![FileObjectSource {
            key: "test_key".to_string(),
            path: target_path.clone(),
            optional: false,
        }];

        let result = runtime.block_on(cache_read.extract_objects(objects, pool));
        assert!(
            result.is_ok(),
            "Extracting to the target path should succeed"
        );
        // Test the content; make sure the old content is overwritten
        let content = std::fs::read(&target_path).unwrap();
        assert_eq!(
            content, b"real_test_data",
            "Extracted content should be correct"
        );

        // `tempfile` needs us to reset permissions for cleanup to work
        #[allow(
            clippy::permissions_set_readonly_false,
            reason = "The affected directory is immediately deleted with no security implications"
        )]
        perm.set_readonly(false);
        std::fs::set_permissions(tmpdir.path(), perm).unwrap();
        tmpdir.close().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn test_extract_object_to_nul_works() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .worker_threads(1)
            .build()
            .unwrap();

        let pool = runtime.handle();

        let cache_data = CacheWrite::new();
        let cache_read =
            CacheRead::from(std::io::Cursor::new(cache_data.finish().unwrap())).unwrap();

        let objects = vec![FileObjectSource {
            key: "test_key".to_string(),
            path: PathBuf::from("NUL"),
            optional: false,
        }];

        let result = runtime.block_on(cache_read.extract_objects(objects, pool));
        assert!(result.is_ok(), "Extracting to NUL should succeed");
    }

    /// An uncompressed payload must round-trip, and -- more importantly -- a
    /// reader must handle both encodings without being told which to expect,
    /// since one cache can hold entries written by differently configured (or
    /// older) daemons.
    #[test]
    fn test_object_encodings_round_trip_and_interoperate() {
        fn write_entry(name: &str, body: &[u8], encoding: ObjectEncoding) -> Vec<u8> {
            let mut w = CacheWrite::new();
            {
                let opts =
                    SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
                w.zip.start_file(name, opts).unwrap();
                match encoding {
                    ObjectEncoding::Zstd(level) => {
                        zstd::stream::copy_encode(&mut Cursor::new(body), &mut w.zip, level)
                            .unwrap();
                    }
                    ObjectEncoding::Uncompressed => {
                        w.zip.write_all(&[UNCOMPRESSED_MARKER]).unwrap();
                        std::io::copy(&mut Cursor::new(body), &mut w.zip).unwrap();
                    }
                }
            }
            w.finish().unwrap()
        }

        // Includes a byte pattern that starts like a zstd magic, to be sure the
        // marker rather than the payload is what decides.
        let bodies: [&[u8]; 4] = [
            b"plain object bytes",
            &[],
            &[0x28, 0xB5, 0x2F, 0xFD, 0x01, 0x02],
            &[0u8; 4096],
        ];

        for body in bodies {
            for encoding in [ObjectEncoding::Uncompressed, ObjectEncoding::Zstd(3)] {
                let bytes = write_entry("obj", body, encoding);
                let mut read = CacheRead::from(Cursor::new(bytes)).unwrap();
                let mut out = Vec::new();
                read.get_object("obj", &mut out).unwrap();
                assert_eq!(
                    out,
                    body,
                    "round trip failed for {encoding:?} with {} bytes",
                    body.len()
                );
            }
        }
    }
}

/// What a cache hit costs after the lookup says yes.
///
/// The preprocessor-cache lookup is now well measured. This is the other half:
/// reading the entry off disk and putting the object where the compiler would
/// have written it. Run deliberately:
///
/// ```text
/// cargo test --release --lib hit_cost -- --ignored --nocapture
/// ```
#[cfg(test)]
mod hit_cost {
    use super::*;

    fn entry_with(name: &str, payload: &[u8], encoding: ObjectEncoding) -> Vec<u8> {
        let mut w = CacheWrite::new();
        w.put_object_with_encoding(name, &mut std::io::Cursor::new(payload), None, encoding)
            .unwrap();
        w.finish().unwrap()
    }

    fn object_of(size: usize) -> Vec<u8> {
        // Compressible the way an object file is -- long runs of zeroes between
        // structured data -- so zstd is not measured against noise it could
        // never see in practice.
        let mut v = Vec::with_capacity(size);
        let mut i: u32 = 0;
        while v.len() < size {
            if (v.len() / 64) % 3 == 0 {
                v.extend(std::iter::repeat_n(0u8, 64.min(size - v.len())));
            } else {
                for _ in 0..64.min(size - v.len()) {
                    i = i.wrapping_mul(2654435761).wrapping_add(1);
                    v.push((i >> 24) as u8);
                }
            }
        }
        v
    }

    fn best<F: FnMut()>(rounds: usize, mut f: F) -> f64 {
        f();
        let mut best = f64::MAX;
        for _ in 0..rounds {
            let t = std::time::Instant::now();
            f();
            best = best.min(t.elapsed().as_secs_f64() * 1e3);
        }
        best
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "a stopwatch, not an assertion; run it deliberately"]
    async fn what_delivering_an_object_costs() {
        let pool = tokio::runtime::Handle::current();
        // Block cloning shares extents, so it can only happen when the entry
        // and the object it produces sit on one volume that supports it. A
        // temp dir is wherever TMP points, which here is the system volume --
        // so point this somewhere deliberate to measure the fast path:
        //
        //     SCCION_BENCH_DIR=D:/dev/caches/bench cargo test --release ...
        let explicit = std::env::var_os("SCCION_BENCH_DIR").map(PathBuf::from);
        let tmp = if explicit.is_none() {
            Some(tempfile::tempdir().unwrap())
        } else {
            None
        };
        let dir: PathBuf = match (explicit, &tmp) {
            (Some(d), _) => {
                std::fs::create_dir_all(&d).expect("bench dir");
                d
            }
            (None, Some(t)) => t.path().to_path_buf(),
            _ => unreachable!(),
        };
        println!("  in {}", dir.display());
        println!("\ncache hit: entry on disk -> object on disk, best of 10\n");

        for size in [256 * 1024usize, 2 * 1024 * 1024] {
            let payload = object_of(size);
            for (label, encoding) in [
                ("zstd 3      ", ObjectEncoding::Zstd(3)),
                ("uncompressed", ObjectEncoding::Uncompressed),
            ] {
                // Write side: what the miss path pays to store it.
                let write = best(10, || {
                    std::hint::black_box(entry_with("obj", &payload, encoding));
                });
                let entry = entry_with("obj", &payload, encoding);

                // The entry as the cache holds it, on disk.
                let entry_path = dir.join(format!("entry_{size}_{}", label.trim()));
                std::fs::write(&entry_path, &entry).unwrap();
                let out_path = dir.join("out.obj");

                // Read side: open the entry and materialise the object.
                let mut cloned_any = 0u64;
                let read = {
                    let mut best_ms = f64::MAX;
                    for _ in 0..11 {
                        let _ = std::fs::remove_file(&out_path);
                        let file = std::fs::File::open(&entry_path).unwrap();
                        let reader = CacheRead::from_file_at(file, entry_path.clone()).unwrap();
                        let objects = vec![FileObjectSource {
                            key: "obj".to_string(),
                            path: out_path.clone(),
                            optional: false,
                        }];
                        let t = std::time::Instant::now();
                        cloned_any = reader.extract_objects(objects, &pool).await.unwrap();
                        best_ms = best_ms.min(t.elapsed().as_secs_f64() * 1e3);
                    }
                    best_ms
                };
                assert_eq!(std::fs::metadata(&out_path).unwrap().len(), size as u64);

                println!(
                    "  {:>5} KiB  {label}  entry {:>7} B ({:>4.1}x)  write {write:>6.2} ms  read+extract {read:>6.2} ms  cloned {cloned_any}",
                    size / 1024,
                    entry.len(),
                    size as f64 / entry.len() as f64,
                );
            }
        }
    }
}
