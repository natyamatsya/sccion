// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Logging setup.
//!
//! The constraint that shapes this: whatever we do to observe a build must not
//! change the build's timing. `tracing_appender::non_blocking` hands every write
//! to a dedicated worker thread, so no measured thread ever touches the disk.
//!
//! The appender writes straight through to the file rather than buffering in user
//! space, so a run that is killed mid-flight still leaves a complete log behind --
//! which is the only kind of run whose log is actually interesting.

use std::path::Path;

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

/// Install the global subscriber, writing to `path`.
///
/// The returned guard must be held for the lifetime of the program: dropping it
/// flushes and joins the worker thread.
pub fn init(path: &Path) -> Result<WorkerGuard> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .context("log path has no file name")?
        .to_owned();

    let (writer, guard) =
        tracing_appender::non_blocking(tracing_appender::rolling::never(dir, name));

    // Default to `debug`, which includes every child line. RUST_LOG overrides it,
    // so a noisy run can be turned down without recompiling.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("debug"));

    tracing_subscriber::fmt()
        .with_writer(writer)
        .with_env_filter(filter)
        // Elapsed-since-start beats wall clock for reading a benchmark log: the
        // question is always "how long was it stuck", not "what time was it".
        .with_timer(tracing_subscriber::fmt::time::uptime())
        .with_ansi(false)
        .with_target(false)
        .try_init()
        .map_err(|e| anyhow::anyhow!("installing tracing subscriber: {e}"))?;

    Ok(guard)
}
