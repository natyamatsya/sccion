// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! The typed surface everything outside the cache core asks questions through.
//!
//! There are two consumers and there will be more: the CLI, and an MCP adapter
//! that answers the same questions to an agent rather than to a person. They
//! must not drift, and neither should have to know that the wire format is
//! bincode over a named pipe, or that `Response::Stats` has to be matched
//! against by hand to get a `ServerInfo` out.
//!
//! It is also a boundary against churn from the other direction. MCP revises on
//! roughly a quarterly cadence with date-based versioning, and its 2026-07-28
//! revision was a substantial break. Keeping the adapter a thin translator over
//! this module is what confines that to one crate, rather than letting a
//! protocol change reach into the cache.
//!
//! Nothing here holds state. A [`Daemon`] is an address and a timeout; every
//! call opens a connection, asks one question and drops it. The sccache daemon
//! *is* the state, and a second cache in front of it could only be wrong.

use std::path::Path;
use std::time::Duration;

use crate::client::{ServerConnection, connect_to_server};
use crate::config::Config;
use crate::errors::*;
use crate::protocol::{Request, Response};

pub use crate::checkenv::EnvReport;
pub use crate::protocol::WorkspaceInfo;
pub use crate::missrecord::{Classification, DirectState, MissRecord};
pub use crate::server::{AgentStats, DistInfo, ServerInfo, ServerStats};

/// The daemon answered, and said no.
///
/// Distinct from failing to reach it at all, because the two want opposite
/// responses: a refusal means the request was wrong and repeating it will not
/// help, while an unreachable daemon means try again once one is running.
/// Collapsing them reports "no daemon answered" for a daemon that answered
/// perfectly clearly.
#[derive(Debug)]
pub struct Refused(pub String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// A local sccache daemon, running or not.
#[derive(Debug)]
pub struct Daemon {
    addr: crate::net::SocketAddr,
    startup_timeout: Option<Duration>,
}

impl Daemon {
    /// The daemon this machine's configuration points at.
    pub fn from_config(config: &Config) -> Self {
        Daemon {
            addr: crate::commands::get_addr(),
            startup_timeout: config.server_startup_timeout,
        }
    }

    /// Connect to a running daemon, without starting one.
    ///
    /// Read-only questions use this: starting a daemon in order to ask it for
    /// statistics would report the statistics of the daemon just started, which
    /// is a confident zero rather than an answer.
    pub fn connect(&self) -> Result<ServerConnection> {
        Ok(connect_to_server(&self.addr)?)
    }

    /// Connect, starting a daemon if none is running.
    pub fn connect_or_start(&self) -> Result<ServerConnection> {
        crate::commands::connect_or_start_server(&self.addr, self.startup_timeout)
    }

    /// Statistics and cache metadata.
    pub fn stats(&self) -> Result<ServerInfo> {
        request_stats(self.connect()?)
    }

    /// As [`Daemon::stats`], answering with an empty report when no daemon is
    /// running rather than starting one to ask.
    pub fn stats_or_empty(&self, config: &Config) -> Result<ServerInfo> {
        match self.connect() {
            Ok(conn) => request_stats(conn).context("failed to get stats from server"),
            Err(_) => {
                let runtime = crate::util::new_client_runtime()?;
                let storage = crate::cache::storage_from_config(config, runtime.handle()).ok();
                runtime.block_on(ServerInfo::new(ServerStats::default(), storage.as_deref()))
            }
        }
    }

    pub fn zero_stats(&self) -> Result<()> {
        request_zero_stats(self.connect_or_start()?)
    }

    pub fn shutdown(&self) -> Result<ServerInfo> {
        request_shutdown(self.connect()?)
    }

    pub fn dist_status(&self) -> Result<DistInfo> {
        request_dist_status(self.connect_or_start()?)
    }

    /// Resolve a directory to the checkout it is in, and get a handle naming
    /// it. Every later question about that build passes the handle instead of
    /// the path.
    pub fn register_workspace(&self, path: &Path) -> Result<WorkspaceInfo> {
        let mut conn = self.connect()?;
        match conn.request(Request::RegisterWorkspace {
            path: path.as_os_str().to_owned(),
        })? {
            Response::Workspace(Ok(info)) => Ok(info),
            Response::Workspace(Err(e)) => Err(Refused(e).into()),
            _ => bail!("Unexpected server response!"),
        }
    }

    /// Recent cache misses in detail, most recent last, scoped to the workspace
    /// the handle names.
    pub fn miss_records(&self, handle: Option<&str>, limit: usize) -> Result<Vec<MissRecord>> {
        let mut conn = self.connect()?;
        match conn.request(Request::GetMissRecords {
            handle: handle.map(str::to_owned),
            limit,
        })? {
            Response::MissRecords(Ok(records)) => Ok(records),
            Response::MissRecords(Err(e)) => Err(Refused(e).into()),
            _ => bail!("Unexpected server response!"),
        }
    }
}

/// What sccache is about to run in, as structured data.
///
/// Needs no daemon: every check reads the environment directly, and none of
/// them changes anything. See [`crate::checkenv`] for why that restraint is
/// deliberate rather than incidental.
pub fn environment(config: &Config, cwd: &Path) -> EnvReport {
    crate::checkenv::check(config, cwd)
}

/// Send a `ZeroStats` request to the server.
pub fn request_zero_stats(mut conn: ServerConnection) -> Result<()> {
    debug!("request_zero_stats");
    let response = conn.request(Request::ZeroStats).context(
        "failed to send zero statistics command to server or failed to receive response",
    )?;
    if let Response::ZeroStats = response {
        Ok(())
    } else {
        bail!("Unexpected server response!")
    }
}

/// Send a `GetStats` request to the server, and return the `ServerInfo` if successful.
pub fn request_stats(mut conn: ServerConnection) -> Result<ServerInfo> {
    debug!("request_stats");
    let response = conn.request(Request::GetStats).context(
        "Failed to send data to or receive data from server. Mismatch of client/server versions?",
    )?;
    if let Response::Stats(stats) = response {
        Ok(*stats)
    } else {
        bail!("Unexpected server response!")
    }
}

/// Send a `DistStatus` request to the server, and return `DistStatus` if successful.
pub fn request_dist_status(mut conn: ServerConnection) -> Result<DistInfo> {
    debug!("request_dist_status");
    let response = conn
        .request(Request::DistStatus)
        .context("Failed to send data to or receive data from server")?;
    if let Response::DistStatus(info) = response {
        Ok(info)
    } else {
        bail!("Unexpected server response!")
    }
}

/// Send a `Shutdown` request to the server, and return the `ServerInfo` contained
/// within the response if successful.
pub fn request_shutdown(mut conn: ServerConnection) -> Result<ServerInfo> {
    debug!("request_shutdown");
    //TODO: better error mapping
    let response = conn
        .request(Request::Shutdown)
        .context("Failed to send data to or receive data from server")?;
    if let Response::ShuttingDown(stats) = response {
        Ok(*stats)
    } else {
        bail!("Unexpected server response!")
    }
}
