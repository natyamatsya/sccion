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
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// Marks a cache entry object whose payload follows verbatim.
///
/// Chosen so it can never be confused with what came before it: every zstd frame
/// starts `28 B5 2F FD`, and skippable frames start `50`-`5F`, so a leading zero
/// byte unambiguously means "not zstd". That lets uncompressed and compressed
/// entries coexist in one cache, and lets a cache written by an older sccache
/// still be read, without a format version or a cache-invalidating key change.
const UNCOMPRESSED_MARKER: u8 = 0x00;

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

/// Read once per process rather than per object.
///
/// This is the daemon's environment, so it is fixed for the daemon's lifetime;
/// changing it requires restarting the server. That was already true of
/// `SCCACHE_CACHE_ZSTD_LEVEL`, which used to be re-read on every single object.
static OBJECT_ENCODING: std::sync::LazyLock<ObjectEncoding> = std::sync::LazyLock::new(|| {
    let uncompressed = std::env::var("SCCACHE_CACHE_COMPRESSION")
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "none" || v == "off" || v == "false" || v == "0"
        })
        .unwrap_or(false);
    if uncompressed {
        return ObjectEncoding::Uncompressed;
    }
    let level = std::env::var("SCCACHE_CACHE_ZSTD_LEVEL")
        .ok()
        .and_then(|value| value.parse::<i32>().ok())
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

impl CacheRead {
    /// Create a cache entry from `reader`.
    pub fn from<R>(reader: R) -> Result<CacheRead>
    where
        R: ReadSeek + 'static,
    {
        let z = ZipArchive::new(Box::new(reader) as Box<dyn ReadSeek>)
            .context("Failed to parse cache entry")?;
        Ok(CacheRead { zip: z })
    }

    /// Get an object from this cache entry at `name` and write it to `to`.
    /// If the file has stored permissions, return them.
    pub fn get_object<T>(&mut self, name: &str, to: &mut T) -> Result<Option<u32>>
    where
        T: Write,
    {
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
            std::io::copy(&mut file, to).or(Err(DecompressionFailure))?;
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

    pub async fn extract_objects<T>(
        mut self,
        objects: T,
        pool: &tokio::runtime::Handle,
    ) -> Result<()>
    where
        T: IntoIterator<Item = FileObjectSource> + Send + Sync + 'static,
    {
        pool.spawn_blocking(move || {
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
            Ok(())
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
        // The zip compression method is always "stored"; what varies is what we
        // store inside the member -- a zstd frame, or a marker byte followed by
        // the bytes verbatim. See `ObjectEncoding`.
        let opts = FileOptions::default().compression_method(CompressionMethod::Stored);
        let opts = if let Some(mode) = mode {
            opts.unix_permissions(mode)
        } else {
            opts
        };
        self.zip
            .start_file(name, opts)
            .context("Failed to start cache entry object")?;

        match *OBJECT_ENCODING {
            ObjectEncoding::Zstd(level) => {
                zstd::stream::copy_encode(from, &mut self.zip, level)?;
            }
            ObjectEncoding::Uncompressed => {
                self.zip.write_all(&[UNCOMPRESSED_MARKER])?;
                std::io::copy(from, &mut self.zip)?;
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
        let CacheWrite { mut zip } = self;
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
                let opts = FileOptions::default().compression_method(CompressionMethod::Stored);
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
