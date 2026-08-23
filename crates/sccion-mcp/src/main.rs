// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! `sccion-mcp`: the out-of-band query surface, for agents rather than people.
//!
//! **Nothing here is on the compile path.** The compiler wrapper stays exactly
//! as it is -- a fast native binary on a local pipe -- and no part of a
//! compilation goes through JSON-RPC. This answers questions asked *between*
//! builds: why did that miss, what is the cache doing, is this machine set up
//! to cache at all.
//!
//! **stdio only.** Everything sccion does is same-machine, and the remote
//! transport brings an authorization surface that is itself mid-migration.
//! Under stdio a leaked workspace handle is confined to the process tree that
//! could already talk to the daemon, which is why this is a security decision
//! and not just the simpler one.
//!
//! **The adapter holds no state of its own.** The sccion daemon is the state
//! and this is a translator in front of it; an adapter that invented a cache of
//! its own would be inventing a bug. `rmcp` keeps what a protocol revision
//! requires -- a client that opens with `initialize` gets a session, because
//! that revision has one -- but no answer here depends on it.
//!
//! **The protocol is the SDK's.** `rmcp` implements the MCP revisions and
//! negotiates between them per connection, so an editor that opens with
//! `initialize` and one that speaks 2026-07-28 with no handshake at all are
//! both answered by this one binary, and the next revision is a version bump
//! rather than a second implementation of a moving specification.

mod backend;
mod queries;
mod server;

use std::sync::Arc;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    // Logging goes to stderr and nowhere else, under the same `SCCACHE_LOG` as
    // the rest of sccion. The protocol's logging surface is deprecated and its
    // own migration guidance is "use stderr", so there is nothing to add on the
    // protocol side -- but there does have to be something, or a failure here
    // is a bare error code with no way to find out more.
    if sccion::envvar::var(sccion::LOGGING_ENV).is_some() {
        let _ = env_logger::Builder::from_env(sccion::envvar::logging_var())
            .target(env_logger::Target::Stderr)
            .try_init();
    }

    let backend = match backend::DaemonBackend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("sccion-mcp: cannot load configuration: {e}");
            return Err(e);
        }
    };

    let service = server::SccionServer::new(Arc::new(backend))
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
