// Copyright 2017 Mozilla Foundation
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

use crate::cache::{Cache, CacheWrite, Storage};
use crate::compiler::PreprocessorCacheEntry;
use crate::config::PreprocessorCacheModeConfig;
use crate::errors::*;
use async_trait::async_trait;
use futures::channel::mpsc;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::sleep;

/// A mock `Storage` implementation.
pub struct MockStorage {
    rx: Arc<Mutex<mpsc::UnboundedReceiver<Result<Cache>>>>,
    tx: mpsc::UnboundedSender<Result<Cache>>,
    delay: Option<Duration>,
    preprocessor_cache_mode: bool,
    /// Manifests written, so a test can tell whether a compilation recorded
    /// what it read for the next one to find.
    manifests: Arc<std::sync::Mutex<Vec<(String, PreprocessorCacheEntry)>>>,
    /// The machine-wide dependency keying setting, so a test can tell a
    /// compilation that was keyed because the default says so from one that was
    /// keyed because the project asked.
    dependency_keying: bool,
    /// Raw bytes to hand back as the stored manifest, so a test can present one
    /// that cannot be read.
    preprocessor_entry: Option<Vec<u8>>,
}

impl MockStorage {
    /// Create a new `MockStorage`. if `delay` is `Some`, wait for that amount of time before returning from operations.
    pub(crate) fn new(delay: Option<Duration>, preprocessor_cache_mode: bool) -> MockStorage {
        let (tx, rx) = mpsc::unbounded();
        Self {
            tx,
            rx: Arc::new(Mutex::new(rx)),
            delay,
            preprocessor_cache_mode,
            manifests: Default::default(),
            dependency_keying: PreprocessorCacheModeConfig::default().use_dependency_keying,
            preprocessor_entry: None,
        }
    }

    /// Serve `bytes` as the stored manifest for every key.
    pub(crate) fn with_preprocessor_entry(mut self, bytes: Vec<u8>) -> Self {
        self.preprocessor_entry = Some(bytes);
        self
    }

    /// Pin the machine-wide dependency keying setting.
    pub(crate) fn with_dependency_keying(mut self, on: bool) -> Self {
        self.dependency_keying = on;
        self
    }

    /// Queue up `res` to be returned as the next result from `Storage::get`.
    pub(crate) fn next_get(&self, res: Result<Cache>) {
        self.tx.unbounded_send(res).unwrap();
    }

    /// The manifests written so far, in the order they were written.
    pub(crate) fn manifests(&self) -> Vec<(String, PreprocessorCacheEntry)> {
        self.manifests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Storage for MockStorage {
    async fn get(&self, _key: &str) -> Result<Cache> {
        if let Some(delay) = self.delay {
            sleep(delay).await;
        }
        let next = self.rx.lock().await.try_recv();

        next.expect("MockStorage get called but no get results available")
    }
    async fn put(&self, _key: &str, _entry: CacheWrite) -> Result<Duration> {
        Ok(if let Some(delay) = self.delay {
            sleep(delay).await;
            delay
        } else {
            Duration::from_secs(0)
        })
    }
    fn location(&self) -> String {
        "Mock Storage".to_string()
    }
    async fn current_size(&self) -> Result<Option<u64>> {
        Ok(None)
    }
    async fn max_size(&self) -> Result<Option<u64>> {
        Ok(None)
    }
    fn preprocessor_cache_mode_config(&self) -> PreprocessorCacheModeConfig {
        PreprocessorCacheModeConfig {
            use_preprocessor_cache_mode: self.preprocessor_cache_mode,
            use_dependency_keying: self.dependency_keying,
            ..Default::default()
        }
    }
    async fn get_preprocessor_cache_entry(
        &self,
        _key: &str,
    ) -> Result<Option<Box<dyn crate::lru_disk_cache::ReadSeek>>> {
        Ok(self.preprocessor_entry.clone().map(|bytes| {
            Box::new(std::io::Cursor::new(bytes)) as Box<dyn crate::lru_disk_cache::ReadSeek>
        }))
    }
    async fn put_preprocessor_cache_entry(
        &self,
        key: &str,
        preprocessor_cache_entry: PreprocessorCacheEntry,
    ) -> Result<()> {
        self.manifests
            .lock()
            .unwrap()
            .push((key.to_string(), preprocessor_cache_entry));
        Ok(())
    }
}
