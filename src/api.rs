// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
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
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::client::{ServerConnection, connect_to_server};
use crate::config::Config;
use crate::errors::*;
use crate::protocol::{Request, Response};
use serde::Serialize;

// The report's own field types, not just the report: every field of
// `EnvReport` is public, so anything that can hold one already needs these to
// read it, and an adapter that could only re-serialise it could not test what
// it says.
pub use crate::checkenv::{
    DefenderReport, EnvReport, Finding, Level, Origin, Setting, VolumeReport,
};
pub use crate::missrecord::{
    Classification, DirectState, KeyComponents, MissRecord, RING_CAPACITY as MAX_MISS_RECORDS,
};
pub use crate::protocol::WorkspaceInfo;
pub use crate::server::{AgentStats, DistInfo, ServerInfo, ServerStats};

/// Whether this machine allows the MCP query surface at all.
///
/// Resolved once at daemon startup from the configuration, the same way the
/// object encoding is, because it is a property of the machine for the
/// daemon's lifetime rather than of any one compilation.
///
/// Defaults to enabled so that a developer machine needs no configuration. A
/// build server should turn it off: the surface exists so an agent can ask why
/// a build missed, and where there is no agent it is a way to read build state
/// with nobody to serve.
static MCP_ENABLED: AtomicBool = AtomicBool::new(true);

pub fn set_mcp_enabled(enabled: bool) {
    MCP_ENABLED.store(enabled, Ordering::Relaxed);
}

pub fn mcp_enabled() -> bool {
    MCP_ENABLED.load(Ordering::Relaxed)
}

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

/// The shape of the JSON the CLI prints, for anything parsing it.
///
/// Three version numbers live in this program and they answer different
/// questions. The package version moves for reasons a parser does not care
/// about and, with no releases, does not move at all. `PROTOCOL_VERSION` is
/// about two sccion builds understanding each other on a socket, and is
/// invisible to anyone reading stdout. This is the one a CI script checks
/// before trusting a field name.
///
/// Bump it when a field is removed, renamed, or changes meaning or units --
/// anything that makes a reader of the old shape wrong rather than merely
/// incomplete. Adding a field does not bump it: a parser that ignores unknown
/// keys is unaffected, which is the contract this number is promising.
pub const JSON_SCHEMA_VERSION: u32 = 1;

/// A JSON document with its schema version attached.
///
/// A wrapper rather than a field on the reports themselves, because the
/// version describes *this rendering* and not the daemon's internal types --
/// `ServerInfo` also crosses the wire, where `PROTOCOL_VERSION` already
/// governs, and giving it a second version would be two numbers for one
/// struct.
#[derive(Serialize)]
pub struct Versioned<T: Serialize> {
    pub schema_version: u32,
    #[serde(flatten)]
    pub document: T,
}

impl<T: Serialize> Versioned<T> {
    pub fn new(document: T) -> Self {
        Versioned {
            schema_version: JSON_SCHEMA_VERSION,
            document,
        }
    }
}

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
    ///
    /// Verified, because an answer from a daemon speaking a different protocol
    /// is not a smaller answer than expected -- it is bincode reading one
    /// schema's bytes as another's, which produces a number rather than an
    /// error.
    pub fn connect(&self) -> Result<ServerConnection> {
        let mut conn = connect_to_server(&self.addr)?;
        crate::client::verify_protocol_version(&mut conn)?;
        Ok(conn)
    }

    /// Connect to whatever is listening, protocol be damned.
    ///
    /// Exactly one caller, and it needs to be this way: the remedy for a daemon
    /// speaking the wrong protocol is to stop it, so the command that stops it
    /// cannot be gated on the check it exists to resolve. `Request::Shutdown`
    /// has held the same discriminant since the first version, so every daemon
    /// that could be stale still understands it.
    fn connect_unverified(&self) -> Result<ServerConnection> {
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
            // A daemon on the wrong protocol has real statistics that cannot be
            // read. Reporting an empty set for it would answer the question
            // with a confident zero, which is what the daemon being unreadable
            // was supposed to stop.
            Err(e)
                if e.downcast_ref::<crate::client::ProtocolMismatch>()
                    .is_some() =>
            {
                Err(e)
            }
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
        request_shutdown(self.connect_unverified()?)
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

    /// The checkout a handle names.
    ///
    /// The inverse of [`Daemon::register_workspace`], for a caller that was
    /// given a handle and needs to ask a question about the tree itself.
    pub fn describe_workspace(&self, handle: &str) -> Result<WorkspaceInfo> {
        let mut conn = self.connect()?;
        match conn.request(Request::DescribeWorkspace {
            handle: handle.to_owned(),
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

#[cfg(test)]
mod json_surface {
    use super::*;

    /// The version rides beside the document, not inside it.
    ///
    /// `flatten` is what makes this additive: every field the report already
    /// had keeps its place at the top level, so adding the version is not
    /// itself the kind of change the version exists to announce.
    #[test]
    fn the_document_is_versioned_and_otherwise_unchanged() {
        #[derive(Serialize)]
        struct Report {
            cache_hits: u64,
            nested: Vec<u8>,
        }

        let v = serde_json::to_value(Versioned::new(Report {
            cache_hits: 7,
            nested: vec![1, 2],
        }))
        .expect("serialises");

        assert_eq!(v["schema_version"], JSON_SCHEMA_VERSION);
        assert_eq!(v["cache_hits"], 7, "the document stays at the top level");
        assert_eq!(v["nested"], serde_json::json!([1, 2]));
        assert!(
            v.get("document").is_none(),
            "flattened, not nested -- a consumer reading .cache_hits must not \
             have to learn a new path"
        );
    }

    /// The three version numbers are not interchangeable.
    ///
    /// A reader tempted to check the package version instead would be checking
    /// something that has never moved, and one checking `PROTOCOL_VERSION`
    /// would be reacting to socket changes that leave stdout identical.
    #[test]
    fn the_schema_version_is_its_own_number() {
        assert_eq!(JSON_SCHEMA_VERSION, 1);
        assert_ne!(
            JSON_SCHEMA_VERSION.to_string(),
            env!("CARGO_PKG_VERSION"),
            "the package version is not a schema version"
        );
    }
}
