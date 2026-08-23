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

use crate::errors::*;
use crate::net::Connection;
use crate::protocol::{Request, Response};
use crate::util;
use backon::BlockingRetryable;
use byteorder::{BigEndian, ByteOrder};
use std::io::{self, BufReader, Read};

/// A connection to an sccache server.
///
/// One handle, used for both directions. It previously held two, obtained by
/// duplicating the socket so a reader and a writer could be owned separately --
/// but the protocol is strictly alternating request/response, so nothing ever
/// used them concurrently, and on Windows duplicating a socket means
/// `WSADuplicateSocket`, which measured ~2 ms per invocation against ~0.08 ms for
/// the connect itself.
pub struct ServerConnection {
    io: BufReader<Box<dyn Connection>>,
}

impl ServerConnection {
    /// Create a new connection using `stream`.
    pub fn new(conn: Box<dyn Connection>) -> ServerConnection {
        ServerConnection {
            io: BufReader::new(conn),
        }
    }

    /// Send `request` to the server, read and return a `Response`.
    pub fn request(&mut self, request: Request) -> Result<Response> {
        trace!("ServerConnection::request");
        util::write_length_prefixed_bincode(self.io.get_mut(), request)?;
        trace!("ServerConnection::request: sent request");
        self.read_one_response()
    }

    /// Read a single `Response` from the server.
    pub fn read_one_response(&mut self) -> Result<Response> {
        trace!("ServerConnection::read_one_response");
        let mut bytes = [0; 4];
        self.io
            .read_exact(&mut bytes)
            .context("Failed to read response header")?;
        let len = BigEndian::read_u32(&bytes);
        trace!("Should read {} more bytes", len);
        let mut data = vec![0; len as usize];
        self.io.read_exact(&mut data)?;
        trace!("Done reading");
        Ok(bincode::deserialize(&data)?)
    }
}

/// Establish a TCP connection to an sccache server listening on `addr`.
pub fn connect_to_server(addr: &crate::net::SocketAddr) -> io::Result<ServerConnection> {
    trace!("connect_to_server({addr})");
    let conn = crate::net::connect(addr)?;
    Ok(ServerConnection::new(conn))
}

/// A daemon answered, but not in this build's protocol.
///
/// Carried as its own type so callers can tell it apart from "nothing is
/// listening". They want opposite handling: no daemon means an empty answer is
/// the true one, while a daemon on the wrong protocol means there is a real
/// answer that cannot be read -- and reporting zeros for that is the confident
/// wrong number this whole check exists to prevent.
#[derive(Debug)]
pub struct ProtocolMismatch;

impl std::fmt::Display for ProtocolMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("daemon protocol mismatch")
    }
}

impl std::error::Error for ProtocolMismatch {}

/// Refuse a daemon that is not speaking this client's protocol.
///
/// Off Windows the address carries no version -- a fixed TCP port, or a socket
/// path the user chose -- so connecting proves only that *something* is
/// listening. bincode is not self-describing, so without this a daemon left
/// running across an upgrade decodes the new client's bytes against its own
/// older schema and acts on whatever that produced.
///
/// Two failures, one remedy. A daemon that answers with a different version is
/// simply too old or too new. A daemon that cannot answer at all predates the
/// question, and bincode's `InvalidTagEncoding` on the unknown discriminant is
/// how it says so -- the request fails rather than being guessed at, which is
/// the whole reason the variant was appended rather than inserted.
///
/// Windows already fails this at the rendezvous, since the version is in the
/// pipe name and a mismatched pair never connects. The check is left in for it
/// anyway: one code path, and a redundant round trip on a transport that does
/// not need it is worth less than a second way for the two to disagree.
pub fn verify_protocol_version(conn: &mut ServerConnection) -> Result<()> {
    check_version_answer(conn.request(Request::ProtocolVersion))
}

/// The verdict, separated from the connection so it can be tested.
///
/// Every arm but one is a refusal, and that is deliberate: the question has a
/// single right answer and anything else means the two ends do not agree about
/// what the bytes mean. Erring towards refusal costs a `--stop-server`; erring
/// the other way costs a build compiled against misread state.
fn check_version_answer(answer: Result<Response>) -> Result<()> {
    let stale = || {
        Error::new(ProtocolMismatch).context(
            "sccion: the running daemon speaks a different protocol than this build. \
             Stop it with `sccion --stop-server`; the next command starts one that \
             matches.",
        )
    };
    match answer {
        Ok(Response::ProtocolVersion(v)) if v == crate::protocol::PROTOCOL_VERSION => Ok(()),
        // Answered, but not with this version.
        Ok(Response::ProtocolVersion(_)) => Err(stale()),
        // Answered something else entirely: not a daemon this understands.
        Ok(_) => Err(stale()),
        // Could not answer. A daemon older than the question lands here, because
        // bincode refuses the unknown discriminant rather than guessing.
        Err(_) => Err(stale()),
    }
}

/// Attempt to establish a TCP connection to an sccache server listening on `addr`.
///
/// If the connection fails, retry a few times.
pub fn connect_with_retry(addr: &crate::net::SocketAddr) -> io::Result<ServerConnection> {
    trace!("connect_with_retry({addr})");
    // TODOs:
    // * Pass the server Child in here, so we can stop retrying
    //   if the process exited.
    // * Send a pipe handle to the server process so it can notify
    //   us once it starts the server instead of us polling.
    let backoff = backon::ConstantBuilder::default()
        .with_delay(std::time::Duration::from_millis(500))
        .with_max_times(10);
    match (|| connect_to_server(addr)).retry(backoff).call() {
        Ok(conn) => Ok(conn),
        Err(e) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("Connection to server timed out: {:?}", e),
        )),
    }
}

#[cfg(test)]
mod version_check {
    use super::*;

    #[test]
    fn only_this_version_is_accepted() {
        let ours = crate::protocol::PROTOCOL_VERSION;
        assert!(check_version_answer(Ok(Response::ProtocolVersion(ours))).is_ok());

        // Older and newer alike: this build cannot read either one's bytes.
        for other in [ours - 1, ours + 1, 0] {
            let e = check_version_answer(Ok(Response::ProtocolVersion(other)))
                .expect_err("version {other} must be refused");
            assert!(
                e.downcast_ref::<ProtocolMismatch>().is_some(),
                "the refusal has to be recognisable, or `stats_or_empty` reports \
                 zeros for a daemon that has real numbers"
            );
        }

        // A daemon too old to know the question cannot answer it at all.
        assert!(check_version_answer(Err(anyhow!("InvalidTagEncoding"))).is_err());

        // And an answer to some other question is not an answer to this one.
        assert!(check_version_answer(Ok(Response::RecordStats)).is_err());
    }
}
