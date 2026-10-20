// Copyright 2016 Mozilla Foundation
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

//! Accepting connections, with or without TLS, and handing their requests to
//! a router.
//!
//! sccion's own loop over hyper rather than `axum::serve`, because the build
//! server speaks TLS: the handshake happens in each connection's task, so a
//! client that stalls in it holds up nobody else.

use axum::Router;
use axum::body::{Body, Bytes, HttpBody};
use axum::http::Request;
use axum::response::Response;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt as _;

use super::handler::{RemoteAddr, ReqId};

/// How long a client has to complete the TLS handshake. tiny_http set no
/// limit; without one, a connection that never finishes its handshake would
/// hold a task and a socket forever.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a client has to send a request's headers: hyper's default, named
/// because it only applies with a timer, which the loop sets for it.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// After a failed `accept` -- out of file descriptors, most likely -- how
/// long to wait before the next, rather than spinning on the error.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

/// Accepts connections on `listener` forever, each in a task of its own,
/// through TLS if there is an acceptor, and serves their requests with
/// `router`.
pub(super) async fn serve(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    router: Router,
) -> Infallible {
    let request_count = Arc::new(AtomicUsize::new(0));
    loop {
        let (stream, remote) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!("Failed to accept a connection: {e}");
                tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                continue;
            }
        };
        let router = router.clone();
        let request_count = request_count.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            match tls {
                None => serve_connection(stream, remote, router, request_count).await,
                Some(acceptor) => {
                    match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await
                    {
                        Ok(Ok(stream)) => {
                            serve_connection(stream, remote, router, request_count).await;
                        }
                        Ok(Err(e)) => debug!("TLS handshake with {remote} failed: {e}"),
                        Err(_) => debug!("TLS handshake with {remote} timed out"),
                    }
                }
            }
        });
    }
}

/// Serves the HTTP/1.1 requests on one connection.
async fn serve_connection<IO>(
    io: IO,
    remote: SocketAddr,
    router: Router,
    request_count: Arc<AtomicUsize>,
) where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
        let req_id = request_count.fetch_add(1, Ordering::SeqCst);
        dispatch(router.clone(), remote, req_id, req)
    });
    if let Err(e) = hyper::server::conn::http1::Builder::new()
        .timer(TokioTimer::new())
        .header_read_timeout(HEADER_READ_TIMEOUT)
        .serve_connection(TokioIo::new(io), service)
        .await
    {
        debug!("Connection from {remote} ended with an error: {e}");
    }
}

/// Hands one request to `router`, with where it came from and its number.
pub(super) async fn dispatch<B>(
    router: Router,
    remote: SocketAddr,
    req_id: usize,
    mut req: Request<B>,
) -> std::result::Result<Response, Infallible>
where
    B: HttpBody<Data = Bytes> + Send + 'static,
    B::Error: Into<axum::BoxError>,
{
    trace!(target: "sccache_http", "Req {} ({}): {} {}", req_id, remote, req.method(), req.uri());
    req.extensions_mut().insert(RemoteAddr(remote));
    req.extensions_mut().insert(ReqId(req_id));
    let response = router.oneshot(req.map(Body::new)).await?;
    trace!(target: "sccache_http", "Res {}: {}", req_id, response.status());
    Ok(response)
}
