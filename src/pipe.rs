// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! Windows named pipe transport for the client/server protocol.
//!
//! Loopback TCP works, but it opens a fresh connection per translation unit; at
//! high `-j` across a large target that is tens of thousands of short-lived
//! connections, and the failure mode is ephemeral port exhaustion and TIME_WAIT
//! pressure showing up as sporadic connection failures under load. A pipe has no
//! port to exhaust, does not go through the network stack at all, and carries an
//! ACL instead of being reachable by anything that can open a loopback socket.

use std::fs::OpenOptions;
use std::io;

use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::{Mutex, mpsc};

use crate::net::{Acceptor, SocketAddr};

/// All pipe instances are currently serving a client.
const ERROR_PIPE_BUSY: i32 = 231;
/// Losing the race for `FILE_FLAG_FIRST_PIPE_INSTANCE` surfaces as this.
const ERROR_ACCESS_DENIED: i32 = 5;

/// How long to keep waiting for a free instance before giving up.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The pipe name for the current user.
///
/// Keyed by SID rather than user name: a name is spoofable through the
/// environment, `DOMAIN\user` contains a backslash that is illegal in a pipe
/// name, and it is not unique across domains. A SID string is digits and hyphens,
/// and is exactly the identity an ACL is expressed in.
///
/// The protocol version is in the name so a client and a daemon built either side
/// of a wire-format change simply fail to find each other. bincode is not
/// self-describing, so without this the mistake is silent misinterpretation
/// rather than a clean "no server here".
pub fn default_pipe_name() -> Option<String> {
    current_user_sid().map(|sid| {
        format!(
            "{}sccion-{}-{}",
            SocketAddr::PIPE_PREFIX,
            sid,
            crate::protocol::PROTOCOL_VERSION
        )
    })
}

/// The current process's user SID, in string form.
pub fn current_user_sid() -> Option<String> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: the token handle is closed on every path out of this block. The
    // two-call pattern sizes the buffer before filling it, and the string SID is
    // copied out before being freed with LocalFree as the API requires.
    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return None;
        }
        let mut needed: u32 = 0;
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            CloseHandle(token);
            return None;
        }
        let mut buf = vec![0u8; needed as usize];
        let ok = GetTokenInformation(
            token,
            TokenUser,
            buf.as_mut_ptr().cast(),
            needed,
            &mut needed,
        );
        CloseHandle(token);
        if ok == 0 {
            return None;
        }
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut raw: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut raw) == 0 || raw.is_null() {
            return None;
        }
        let mut len = 0;
        while *raw.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(raw, len));
        LocalFree(raw.cast());
        Some(sid)
    }
}

/// Connect to a named pipe, waiting if every instance is momentarily busy.
///
/// A plain file handle rather than tokio's `NamedPipeClient`: this side of the
/// protocol is synchronous, and the existing startup handshake already relies on
/// a pipe behaving as an ordinary file handle.
pub fn connect_pipe(name: &str) -> io::Result<std::fs::File> {
    let deadline = std::time::Instant::now() + BUSY_TIMEOUT;
    loop {
        match OpenOptions::new().read(true).write(true).open(name) {
            Ok(f) => return Ok(f),
            // Matched on the OS code rather than ErrorKind: Rust's mapping for 231
            // is not something to depend on. Every other error passes through
            // untouched -- notably ERROR_FILE_NOT_FOUND, which is how the caller
            // learns there is no server yet and starts one.
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                wait_for_free_instance(name, 250);
            }
            Err(e) => return Err(e),
        }
    }
}

fn wait_for_free_instance(name: &str, millis: u32) {
    use windows_sys::Win32::System::Pipes::WaitNamedPipeW;
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
    // The result is deliberately ignored: a timeout just means the caller retries,
    // and any real failure is reported by the next open attempt.
    unsafe {
        WaitNamedPipeW(wide.as_ptr(), millis);
    }
}

/// A listener over a Windows named pipe.
///
/// Pipes have no listen queue. An instance must exist before a client can
/// connect, and each instance serves exactly one client. So instances are
/// pre-armed, and each is replaced the instant it is handed out — without that,
/// every connection would race the creation of the instance it needs and see
/// ERROR_PIPE_BUSY.
pub struct NamedPipeListener {
    name: String,
    /// The accept loop is serial, so this is uncontended; it exists only because
    /// `Acceptor::accept` takes `&self`. It must be tokio's mutex rather than
    /// std's: the guard is held across an await, and a std guard is not `Send`.
    connections: Mutex<mpsc::Receiver<io::Result<NamedPipeServer>>>,
}

impl NamedPipeListener {
    /// Bind `name`, pre-arming `instances` server instances.
    ///
    /// Must be called inside a tokio runtime context: each instance is driven by
    /// a task that waits for a client and then re-arms.
    pub fn bind(name: &str, instances: usize) -> io::Result<Self> {
        let instances = instances.clamp(1, 254);
        let (tx, rx) = mpsc::channel(instances);

        for index in 0..instances {
            // Only the first instance asserts exclusivity; that is what makes
            // binding fail for a second daemon rather than silently succeeding.
            let server = create_instance(name, index == 0)?;
            let tx = tx.clone();
            let name = name.to_owned();
            tokio::spawn(async move {
                let mut server = server;
                loop {
                    if let Err(e) = server.connect().await {
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                    // Re-arm before handing this one over, so there is never a
                    // moment with fewer instances listening than intended.
                    let next = match create_instance(&name, false) {
                        Ok(next) => next,
                        Err(e) => {
                            let _ = tx.send(Err(e)).await;
                            break;
                        }
                    };
                    let connected = std::mem::replace(&mut server, next);
                    if tx.send(Ok(connected)).await.is_err() {
                        break; // listener dropped
                    }
                }
            });
        }

        Ok(NamedPipeListener {
            name: name.to_owned(),
            connections: Mutex::new(rx),
        })
    }
}

fn create_instance(name: &str, first: bool) -> io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(first)
        // Belt and braces: no remote client, regardless of the ACL.
        .reject_remote_clients(true)
        .access_inbound(true)
        .access_outbound(true)
        // Buffers come from nonpaged pool and there is a pair per armed and per
        // connected instance. Requests are a few KB; an undersized buffer only
        // means a writer blocks until the reader drains it.
        .in_buffer_size(8192)
        .out_buffer_size(8192)
        .create(name)
        .map_err(|e| {
            // Losing FILE_FLAG_FIRST_PIPE_INSTANCE reports ACCESS_DENIED, which
            // Rust maps to PermissionDenied. Startup looks for AddrInUse to
            // recognise "another server won the race", so translate it -- left
            // alone, parallel bootstrap degrades into a hard failure.
            if first
                && matches!(
                    e.raw_os_error(),
                    Some(ERROR_ACCESS_DENIED) | Some(ERROR_PIPE_BUSY)
                )
            {
                io::Error::new(io::ErrorKind::AddrInUse, e)
            } else {
                e
            }
        })
}

impl Acceptor for NamedPipeListener {
    type Socket = NamedPipeServer;

    async fn accept(&self) -> io::Result<Self::Socket> {
        let mut rx = self.connections.lock().await;
        match rx.recv().await {
            Some(result) => result,
            None => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "named pipe listener stopped",
            )),
        }
    }

    fn local_addr(&self) -> io::Result<Option<SocketAddr>> {
        Ok(Some(SocketAddr::Pipe(self.name.clone())))
    }
}
