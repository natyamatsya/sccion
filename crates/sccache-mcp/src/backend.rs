// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! What the adapter can ask, and who answers it.
//!
//! The trait exists so the dispatcher can be tested without a daemon. It is
//! deliberately a narrow restatement of `sccache::api` rather than a second
//! abstraction over it: every method here is one call there.

use std::path::Path;

use anyhow::Result;
use sccache::api::{EnvReport, MissRecord, ServerInfo, WorkspaceInfo};

pub trait Backend: Send + Sync {
    fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo>;
    fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo>;
    fn stats(&self) -> Result<ServerInfo>;
    fn miss_records(&self, handle: Option<&str>, limit: usize) -> Result<Vec<MissRecord>>;
    fn environment(&self, cwd: &Path) -> EnvReport;
}

/// The real thing: a local sccache daemon.
pub struct DaemonBackend {
    daemon: sccache::api::Daemon,
    config: sccache::config::Config,
}

impl DaemonBackend {
    pub fn new() -> Result<Self> {
        let config = sccache::config::Config::load()?;
        // The daemon refuses too, and it is the authority -- this only turns
        // "every tool call fails" into one clear line at startup, which is what
        // an operator who disabled this on a build server should see.
        if !config.mcp_enabled {
            anyhow::bail!(
                "the MCP query surface is disabled on this machine; \
                 set `mcp = true` in the sccache config or SCCACHE_MCP=true to enable it"
            );
        }
        Ok(DaemonBackend {
            daemon: sccache::api::Daemon::from_config(&config),
            config,
        })
    }
}

impl Backend for DaemonBackend {
    fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo> {
        self.daemon.register_workspace(path)
    }

    fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo> {
        self.daemon.describe_workspace(handle)
    }

    fn stats(&self) -> Result<ServerInfo> {
        self.daemon.stats()
    }

    fn miss_records(&self, handle: Option<&str>, limit: usize) -> Result<Vec<MissRecord>> {
        self.daemon.miss_records(handle, limit)
    }

    fn environment(&self, cwd: &Path) -> EnvReport {
        sccache::api::environment(&self.config, cwd)
    }
}
