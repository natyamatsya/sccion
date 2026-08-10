// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
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
//! **The adapter holds no state.** There is no session in this protocol
//! revision and no connection setup phase, so there is nothing to warm up and
//! nothing to lose: the sccion daemon is the state and this is a translator in
//! front of it. It can be killed between any two requests without consequence.
//! An adapter that invented a cache of its own would be inventing a bug.

mod backend;
mod dispatch;
mod envelope;

use std::io::{BufRead, Write};

use anyhow::Result;

fn main() -> Result<()> {
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

    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(request) => dispatch::handle(&request, &backend),
            Err(e) => {
                // Parse errors have no id to answer against, so the standard
                // JSON-RPC null-id form is the only correct reply.
                eprintln!("sccion-mcp: unparseable request: {e}");
                Some(envelope::err(
                    serde_json::Value::Null,
                    -32700,
                    format!("parse error: {e}"),
                ))
            }
        };
        if let Some(response) = response {
            serde_json::to_writer(&mut stdout, &response)?;
            stdout.write_all(b"\n")?;
            stdout.flush()?;
        }
    }
    Ok(())
}
