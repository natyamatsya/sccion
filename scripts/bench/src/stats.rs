// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Reading sccache's own counters.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::proc::run_capture;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PerLanguage {
    #[serde(default)]
    pub counts: HashMap<String, u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawStats {
    #[serde(default)]
    pub compile_requests: u64,
    #[serde(default)]
    pub cache_hits: PerLanguage,
    #[serde(default)]
    pub cache_misses: PerLanguage,
    /// Absent on an sccache built before these counters existed, which is exactly
    /// the "before" side of a comparison, so this must not be a hard error.
    #[serde(default)]
    pub preprocessor_cache_hits: Option<u64>,
    #[serde(default)]
    pub preprocessor_cache_misses: Option<u64>,
    #[serde(default)]
    pub cache_write_errors: u64,
    #[serde(default)]
    pub cache_read_errors: u64,
    #[serde(default)]
    pub compilations: u64,
    #[serde(default)]
    pub forced_recaches: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RawInfo {
    pub stats: RawStats,
    #[serde(default)]
    pub use_preprocessor_cache_mode: bool,
}

/// Flattened counters for one scenario.
#[derive(Debug, Clone, Serialize)]
pub struct ScenarioStats {
    pub compile_requests: u64,
    pub cache_hits: u64,
    pub cache_misses: u64,
    pub preprocessor_cache_hits: Option<u64>,
    pub preprocessor_cache_misses: Option<u64>,
    pub cache_write_errors: u64,
    pub cache_read_errors: u64,
    pub compilations: u64,
    pub forced_recaches: u64,
    pub use_preprocessor_cache_mode: bool,
}

impl From<&RawInfo> for ScenarioStats {
    fn from(info: &RawInfo) -> Self {
        let sum = |p: &PerLanguage| p.counts.values().sum();
        Self {
            compile_requests: info.stats.compile_requests,
            cache_hits: sum(&info.stats.cache_hits),
            cache_misses: sum(&info.stats.cache_misses),
            preprocessor_cache_hits: info.stats.preprocessor_cache_hits,
            preprocessor_cache_misses: info.stats.preprocessor_cache_misses,
            cache_write_errors: info.stats.cache_write_errors,
            cache_read_errors: info.stats.cache_read_errors,
            compilations: info.stats.compilations,
            forced_recaches: info.stats.forced_recaches,
            use_preprocessor_cache_mode: info.use_preprocessor_cache_mode,
        }
    }
}

pub fn show(sccache: &Path, env: &HashMap<String, String>) -> Result<RawInfo> {
    let out = run_capture(
        sccache,
        &["--show-stats".into(), "--stats-format=json".into()],
        None,
        env,
        Duration::from_secs(60),
    )?;
    serde_json::from_str(&out).with_context(|| format!("parsing sccache stats JSON:\n{out}"))
}

pub fn zero(sccache: &Path, env: &HashMap<String, String>) -> Result<()> {
    run_capture(
        sccache,
        &["--zero-stats".into()],
        None,
        env,
        Duration::from_secs(60),
    )?;
    Ok(())
}
