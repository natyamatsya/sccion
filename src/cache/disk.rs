// Copyright 2016 Mozilla Foundation
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

use crate::cache::{Cache, CacheMode, CacheRead, CacheWrite, GetPathResult, Storage};
use crate::compiler::PreprocessorCacheEntry;
use crate::lru_disk_cache::{Error as LruError, ReadSeek};
use async_trait::async_trait;
use bytes::Bytes;
use std::ffi::OsStr;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::errors::*;

use super::lazy_disk_cache::LazyDiskCache;
use crate::config::PreprocessorCacheModeConfig;

/// A cache that stores entries at local disk paths.
pub struct DiskCache {
    /// `LruDiskCache` does all the real work here.
    lru: Arc<Mutex<LazyDiskCache>>,
    /// Thread pool to execute disk I/O
    pool: tokio::runtime::Handle,
    preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
    preprocessor_cache: Arc<Mutex<LazyDiskCache>>,
    rw_mode: CacheMode,
    basedirs: Vec<Vec<u8>>,
}

impl DiskCache {
    /// Create a new `DiskCache` rooted at `root`, with `max_size` as the maximum cache size on-disk, in bytes.
    pub fn new<T: AsRef<OsStr>>(
        root: T,
        max_size: u64,
        pool: &tokio::runtime::Handle,
        preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
        rw_mode: CacheMode,
        basedirs: Vec<Vec<u8>>,
    ) -> DiskCache {
        DiskCache {
            lru: Arc::new(Mutex::new(LazyDiskCache::Uninit {
                root: root.as_ref().to_os_string(),
                max_size,
            })),
            pool: pool.clone(),
            preprocessor_cache_mode_config,
            preprocessor_cache: Arc::new(Mutex::new(LazyDiskCache::Uninit {
                root: Path::new(root.as_ref())
                    .join("preprocessor")
                    .into_os_string(),
                max_size,
            })),
            rw_mode,
            basedirs,
        }
    }
}

/// Make a path to the cache entry with key `key`.
fn make_key_path(key: &str) -> PathBuf {
    Path::new(&key[0..1]).join(&key[1..2]).join(key)
}

/// Make a path to the preprocessor cache entry with key `key`.
///
/// The same three-level layout `normalize_key` produces, but built as a path
/// rather than a string, so the separators are the platform's.
///
/// That distinction is load-bearing. `LruDiskCache` keys its index on the entry's
/// path relative to the cache root, and it rebuilds that index at startup by
/// stripping the root from real filesystem paths -- which on Windows come back
/// with backslashes. A key inserted as `a/b/c/<key>` therefore never matches the
/// `a\b\c\<key>` reconstructed on the next run, and every preprocessor cache
/// lookup misses for the lifetime of that daemon. The object cache was unaffected
/// only because it already went through `make_key_path`.
///
/// `normalize_key` itself is correct for the object-storage backends, whose keys
/// are forward-slash separated regardless of host, so the fix belongs here.
/// The on-disk layout is unchanged, so caches written before this still resolve.
fn make_preprocessor_key_path(key: &str) -> PathBuf {
    Path::new(&key[0..1])
        .join(&key[1..2])
        .join(&key[2..3])
        .join(key)
}

#[async_trait]
impl Storage for DiskCache {
    async fn get(&self, key: &str) -> Result<Cache> {
        trace!("DiskCache::get({})", key);
        let path = make_key_path(key);
        let lru = self.lru.clone();
        let key = key.to_owned();

        self.pool
            .spawn_blocking(move || {
                let mut lru = lru.lock().unwrap_or_else(|e| e.into_inner());
                let cache = lru.get_or_init()?;
                let io = match cache.get(&path) {
                    Ok(f) => f,
                    Err(LruError::FileNotInCache) => {
                        trace!("DiskCache::get({}): FileNotInCache", key);
                        return Ok(Cache::Miss);
                    }
                    Err(LruError::Io(e)) => {
                        trace!("DiskCache::get({}): IoError: {:?}", key, e);
                        return Err(e.into());
                    }
                    Err(_) => unreachable!(),
                };
                // The entry's on-disk location, which is what lets
                // materialization clone objects out of it instead of copying.
                let hit = match cache.get_abs_path(&path) {
                    Some(abs) => CacheRead::from_file_at(io, abs)?,
                    None => CacheRead::from(io)?,
                };
                Ok(Cache::Hit(hit))
            })
            .await?
    }

    async fn get_raw(&self, key: &str) -> Result<Option<Bytes>> {
        trace!("DiskCache::get_raw({})", key);
        let path = make_key_path(key);
        let lru = self.lru.clone();
        let key = key.to_owned();

        self.pool
            .spawn_blocking(move || {
                match lru
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_init()?
                    .get(&path)
                {
                    Ok(mut io) => {
                        let mut data = Vec::new();
                        io.read_to_end(&mut data)?;
                        trace!("DiskCache::get_raw({}): Found {} bytes", key, data.len());
                        Ok(Some(Bytes::from(data)))
                    }
                    Err(LruError::FileNotInCache) => {
                        trace!("DiskCache::get_raw({}): FileNotInCache", key);
                        Ok(None)
                    }
                    Err(LruError::Io(e)) => {
                        trace!("DiskCache::get_raw({}): IoError: {:?}", key, e);
                        Err(e.into())
                    }
                    Err(_) => unreachable!(),
                }
            })
            .await?
    }

    async fn get_path(&self, key: &str) -> GetPathResult {
        let rel_path = make_key_path(key);
        let lru = self.lru.clone();
        self.pool
            .spawn_blocking(move || {
                match lru
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_init()
                    .ok()
                    .and_then(|c| c.get_abs_path(&rel_path))
                {
                    Some(p) => GetPathResult::Found(p),
                    None => GetPathResult::Miss,
                }
            })
            .await
            .unwrap_or(GetPathResult::Miss)
    }

    async fn put(&self, key: &str, entry: CacheWrite) -> Result<Duration> {
        trace!("DiskCache::put({})", key);
        // Delegate to put_raw after serializing the entry
        let data = entry.finish()?;
        self.put_raw(key, data.into()).await
    }

    async fn put_raw(&self, key: &str, data: Bytes) -> Result<Duration> {
        trace!("DiskCache::put_raw({}, {} bytes)", key, data.len());

        if self.rw_mode == CacheMode::ReadOnly {
            return Err(anyhow!("Cannot write to a read-only cache"));
        }

        let lru = self.lru.clone();
        let key = make_key_path(key);

        self.pool
            .spawn_blocking(move || {
                let start = Instant::now();
                let mut f = lru
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_or_init()?
                    .prepare_add(key, data.len() as u64)?;
                f.as_file_mut().write_all(&data)?;
                lru.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get()
                    .unwrap()
                    .commit(f)?;
                Ok(start.elapsed())
            })
            .await?
    }

    async fn check(&self) -> Result<CacheMode> {
        Ok(self.rw_mode)
    }

    fn location(&self) -> String {
        format!(
            "Local disk: {:?}",
            self.lru.lock().unwrap_or_else(|e| e.into_inner()).path()
        )
    }

    fn cache_type_name(&self) -> &'static str {
        "disk"
    }

    async fn current_size(&self) -> Result<Option<u64>> {
        Ok(self
            .lru
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get()
            .map(|l| l.size()))
    }
    async fn max_size(&self) -> Result<Option<u64>> {
        Ok(Some(
            self.lru
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .capacity(),
        ))
    }
    fn preprocessor_cache_mode_config(&self) -> PreprocessorCacheModeConfig {
        self.preprocessor_cache_mode_config
    }
    fn basedirs(&self) -> &[Vec<u8>] {
        &self.basedirs
    }
    async fn get_preprocessor_cache_entry(&self, key: &str) -> Result<Option<Box<dyn ReadSeek>>> {
        let key = make_preprocessor_key_path(key);
        Ok(self
            .preprocessor_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_init()?
            .get(key)
            .ok())
    }
    async fn put_preprocessor_cache_entry(
        &self,
        key: &str,
        preprocessor_cache_entry: PreprocessorCacheEntry,
    ) -> Result<()> {
        if self.rw_mode == CacheMode::ReadOnly {
            return Err(anyhow!("Cannot write to a read-only cache"));
        }

        let key = make_preprocessor_key_path(key);
        let mut f = self
            .preprocessor_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_init()?
            .prepare_add(key, 0)?;
        preprocessor_cache_entry.serialize_to(BufWriter::new(f.as_file_mut()))?;
        Ok(self
            .preprocessor_cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get()
            .unwrap()
            .commit(f)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_disk_cache_type_name() {
        let tempdir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();

        let disk = DiskCache::new(
            tempdir.path(),
            1024 * 1024,
            runtime.handle(),
            PreprocessorCacheModeConfig::default(),
            CacheMode::ReadWrite,
            vec![],
        );

        assert_eq!(disk.cache_type_name(), "disk");
    }

    /// A preprocessor cache entry must still be findable by a *later* process.
    ///
    /// The daemon exits on an idle timeout, so this is the ordinary case, not an
    /// edge case. Writing and reading through one `DiskCache` passes even when
    /// the key is wrong, because the LRU still holds the key exactly as
    /// inserted; only a fresh index rebuilt from the filesystem catches it. So
    /// the second `DiskCache` over the same directory is the whole test.
    #[test]
    fn preprocessor_cache_entry_survives_a_restart() {
        let tempdir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let config = PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: true,
            ..Default::default()
        };
        let key = "0123456789abcdef0123456789abcdef";

        let first = DiskCache::new(
            tempdir.path(),
            1024 * 1024,
            runtime.handle(),
            config,
            CacheMode::ReadWrite,
            vec![],
        );
        runtime
            .block_on(first.put_preprocessor_cache_entry(key, PreprocessorCacheEntry::new()))
            .unwrap();
        assert!(
            runtime
                .block_on(first.get_preprocessor_cache_entry(key))
                .unwrap()
                .is_some(),
            "entry should be readable by the process that wrote it"
        );
        drop(first);

        let second = DiskCache::new(
            tempdir.path(),
            1024 * 1024,
            runtime.handle(),
            config,
            CacheMode::ReadWrite,
            vec![],
        );
        assert!(
            runtime
                .block_on(second.get_preprocessor_cache_entry(key))
                .unwrap()
                .is_some(),
            "entry should survive a daemon restart; a key whose separators do not \
             match the rebuilt index makes every lookup miss"
        );
    }
}
