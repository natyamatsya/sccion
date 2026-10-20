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

//! What the scheduler's and the build server's handlers share: reading
//! requests, answering them as the protocol has it, and moving work off the
//! runtime.
//!
//! The protocol's answers are those of the servers before these, on rouille
//! and tiny_http, which clients and peers of any version expect; where a
//! comment here names rouille, that is why.

use axum::body::Body;
use axum::extract::Path;
use axum::extract::rejection::PathRejection;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use bincode::Options as _;
use futures::TryStreamExt as _;
use serde::Serialize;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::time::Duration;

use super::auth::ClientVisibleMsg;
use crate::errors::*;

/// A ceiling on any bincode body read off the network.
///
/// For a `String` or a byte buffer, bincode's `IoReader` sizes its buffer from
/// a length field *in the payload* before reading any of it, and the default
/// limit is `Infinite`. Without a bound, eight bytes from an unauthenticated
/// client choose an allocation. Sequences are safe -- serde reads those
/// element by element -- and so is slice-based `deserialize`, which
/// bounds-checks against the slice it was handed. `util::bincode_limits` is
/// the test that a limit refuses rather than allocating and failing
/// afterwards.
///
/// Eight megabytes is far above anything real here: the largest of these is a
/// compile command with its environment, which is tens of kilobytes. The
/// limit costs nothing when it is not hit and is checked before the
/// allocation, not after.
pub(super) const MAX_BINCODE_BODY: u64 = 8 * 1024 * 1024;

/// An error and its chain of causes, as the JSON body of a 400 or a 500.
///
/// Based on try_or_400 in rouille, but with logging.
#[derive(Serialize)]
pub(super) struct ErrJson {
    description: String,
    cause: Option<Box<ErrJson>>,
}

impl ErrJson {
    fn from_err<E: ?Sized + std::error::Error>(err: &E) -> ErrJson {
        let cause = err.source().map(ErrJson::from_err).map(Box::new);
        ErrJson {
            description: err.to_string(),
            cause,
        }
    }

    fn into_data(self) -> String {
        serde_json::to_string(&self).expect("infallible serialization for ErrJson failed")
    }
}

/// The address a request came from, put on each request by the serving loop.
///
/// axum's own `ConnectInfo` is part of its `tokio` feature, which brings its
/// serving code along; the loop here is sccion's, so the address travels as a
/// plain extension.
#[derive(Clone, Copy, Debug)]
pub(super) struct RemoteAddr(pub SocketAddr);

/// The number the serving loop gave a request, for matching log lines.
#[derive(Clone, Copy, Debug)]
pub(super) struct ReqId(pub usize);

/// How a request failed, in the forms the protocol answers with.
#[derive(Debug)]
pub(super) enum HttpError {
    /// A 400 or a 500, logged: the status, and the error with its chain of
    /// causes as `ErrJson`.
    Logged {
        status: StatusCode,
        message: String,
        json: String,
    },
    /// A 401 with `WWW-Authenticate: Bearer error="..."` and a body, which is
    /// a message for the client or, for a job token, `ErrJson`.
    Unauthorized {
        short_err: &'static str,
        body: String,
    },
    /// No body at all: a 400 for an `X-Real-IP` that is not an address.
    Empty(StatusCode),
}

impl HttpError {
    pub(super) fn bad_request(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::logged(StatusCode::BAD_REQUEST, err.into())
    }

    pub(super) fn internal(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::logged(StatusCode::INTERNAL_SERVER_ERROR, err.into())
    }

    fn logged(status: StatusCode, err: Box<dyn std::error::Error + Send + Sync>) -> Self {
        let mut message = err.to_string();
        let mut maybe_cause = err.source();
        while let Some(cause) = maybe_cause {
            message.push_str(", caused by: ");
            message.push_str(&cause.to_string());
            maybe_cause = cause.source();
        }
        Self::Logged {
            status,
            message,
            json: ErrJson::from_err(&*err).into_data(),
        }
    }

    pub(super) fn unauthorized(short_err: &'static str, body: ClientVisibleMsg) -> Self {
        Self::Unauthorized {
            short_err,
            body: body.0,
        }
    }

    /// A job token that is missing or does not verify for the job, with the
    /// error in the body as `ErrJson`.
    pub(super) fn invalid_job_token(err: Error) -> Self {
        let err: Box<dyn std::error::Error + Send + Sync> = err.into();
        Self::Unauthorized {
            short_err: "invalid_jwt",
            body: ErrJson::from_err(&*err).into_data(),
        }
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        match self {
            HttpError::Logged { status, json, .. } => (
                status,
                [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
                json,
            )
                .into_response(),
            // Built by hand to send no Content-Type, as rouille did not; a
            // `String` body would have axum add `text/plain`.
            HttpError::Unauthorized { short_err, body } => {
                let mut response = Response::new(Body::from(body));
                *response.status_mut() = StatusCode::UNAUTHORIZED;
                let challenge = format!("Bearer error=\"{short_err}\"");
                response.headers_mut().insert(
                    header::WWW_AUTHENTICATE,
                    HeaderValue::from_str(&challenge)
                        .expect("the short error names are header-safe literals"),
                );
                response
            }
            HttpError::Empty(status) => status.into_response(),
        }
    }
}

/// A handler's result as the response, with the error logged.
pub(super) fn respond(req_id: usize, result: StdResult<Response, HttpError>) -> Response {
    match result {
        Ok(response) => response,
        Err(err) => {
            if let HttpError::Logged { message, .. } = &err {
                warn!("Res {} error: {}", req_id, message);
            }
            err.into_response()
        }
    }
}

type StdResult<T, E> = std::result::Result<T, E>;

/// The route's URL parameter, or why axum could not take it from the path.
pub(super) type PathParam = StdResult<Path<String>, PathRejection>;

/// A URL parameter parsed as rouille's router parsed it, with `FromStr`. One
/// that does not parse matched no route there, so it is a 404 here.
pub(super) fn path_param<T: std::str::FromStr>(param: PathParam) -> StdResult<T, HttpError> {
    param
        .ok()
        .and_then(|Path(param)| param.parse().ok())
        .ok_or(HttpError::Empty(StatusCode::NOT_FOUND))
}

/// Anything no route takes, a known path with the wrong method included: an
/// empty 404, as rouille's router answered it. axum would say 405 for the
/// method.
pub(super) async fn not_found(method: Method, uri: Uri) -> StatusCode {
    warn!("Unknown request {} {}", method, uri);
    StatusCode::NOT_FOUND
}

/// Why a bincode body could not be read. The messages are those the rouille
/// server's `RouilleBincodeError` gave, so the `ErrJson` a client sees is
/// unchanged.
#[derive(Debug)]
pub(super) enum BincodeError {
    WrongContentType,
    Parse(bincode::Error),
}

impl std::fmt::Display for BincodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            BincodeError::WrongContentType => "the request didn't have a binary content type",
            BincodeError::Parse(_) => "error while parsing the bincode body",
        })
    }
}

impl std::error::Error for BincodeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            BincodeError::Parse(e) => Some(e),
            BincodeError::WrongContentType => None,
        }
    }
}

/// The options every bincode message on the wire is read with.
///
/// Not `bincode::options()`: that is varint and reject-trailing, where the
/// free functions -- and so every message already on the wire -- are fixint
/// and allow-trailing.
fn bincode_options() -> impl bincode::Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .with_limit(MAX_BINCODE_BODY)
}

/// A bincode request body, for the small messages: a toolchain name, a
/// heartbeat, a job state.
///
/// Collected before decoding, up to [`MAX_BINCODE_BODY`], so no handler
/// thread waits on a slow client. A body over the ceiling fails as bincode's
/// own size limit would -- the rouille server, which decoded straight from the
/// connection, read only as far as the message went and so failed only if the
/// message itself was larger, which no real message is. A body that cannot be
/// read fails as bincode's I/O error, as it did there.
pub(super) async fn read_bincode<T: serde::de::DeserializeOwned>(
    headers: &HeaderMap,
    body: Body,
) -> StdResult<T, HttpError> {
    let is_bincode = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/octet-stream"));
    if !is_bincode {
        return Err(HttpError::bad_request(BincodeError::WrongContentType));
    }
    let parse_error =
        |kind: bincode::ErrorKind| HttpError::bad_request(BincodeError::Parse(kind.into()));

    let limit = usize::try_from(MAX_BINCODE_BODY).expect("eight megabytes fits in a usize");
    let mut stream = body.into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|e| parse_error(bincode::ErrorKind::Io(io::Error::other(e))))?
    {
        if bytes.len() + chunk.len() > limit {
            return Err(parse_error(bincode::ErrorKind::SizeLimit));
        }
        bytes.extend_from_slice(&chunk);
    }
    bincode_options()
        .deserialize(&bytes)
        .map_err(|e| HttpError::bad_request(BincodeError::Parse(e)))
}

/// A request body as a blocking reader, for a handler on the blocking pool.
///
/// Each read waits for the next chunk off the connection, so a toolchain or a
/// job's inputs go from the socket to the handler as they arrive, with
/// hyper's flow control holding the client back while the handler is busy.
/// Nothing collects the body in memory.
///
/// Must be called on the runtime (it keeps a handle to it) and read off it.
pub(super) fn body_reader(body: Body) -> impl Read + Send + 'static {
    let chunks = body.into_data_stream().map_err(io::Error::other);
    tokio_util::io::SyncIoBridge::new(tokio_util::io::StreamReader::new(chunks))
}

/// Reads what a handler left of a request body, and drops it.
///
/// A handler may answer without reading everything -- the build server does
/// when it already has the toolchain being uploaded, or does not know the job
/// -- and a connection closed with unread data can reach the client as a
/// reset rather than as the answer. tiny_http, under rouille, read the rest of
/// a body with a length before answering; this does that for every body. Only
/// after authentication: an unauthenticated request is answered without its
/// body being read.
pub(super) fn drain(reader: &mut impl Read) {
    // An error here is the client going away, and the answer goes nowhere.
    let _ = io::copy(reader, &mut io::sink());
}

/// Runs `f` on tokio's blocking pool.
///
/// For everything that blocks: the scheduler and build server take mutexes,
/// write toolchains to disk, run compilers and make HTTP requests of their own
/// through [`block_on`]. A panic in `f` is a 500.
pub(super) async fn blocking<T, F>(f: F) -> StdResult<T, HttpError>
where
    F: FnOnce() -> StdResult<T, HttpError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(e) => Err(HttpError::internal(anyhow!("request handler failed: {e}"))),
    }
}

/// Waits for `future` from the blocking pool, where the scheduler's and build
/// server's outgoing requests are made: `SchedulerOutgoing` and
/// `ServerOutgoing` are synchronous, and are only ever called from inside
/// [`blocking`]. On an async worker this would panic, by design of
/// `Handle::block_on`.
pub(super) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Handle::current().block_on(future)
}

/// How long an outgoing request -- a heartbeat, a job assignment, a job state
/// -- may take: reqwest's blocking client's default, which these were made
/// with once. The async client has none.
pub(super) const OUTGOING_TIMEOUT: Duration = Duration::from_secs(30);

/// The client for requests to the scheduler: `new_reqwest_blocking_client`,
/// async, with [`OUTGOING_TIMEOUT`].
pub(super) fn new_reqwest_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .tls_certs_merge(crate::util::bundled_root_certs())
        .timeout(OUTGOING_TIMEOUT)
        .build()
        .context("failed to create an async HTTP client")
}

/// Sends `req` and decodes the bincode answer.
///
/// With `Connection: close`, which works around tiny_http issue #151: the
/// other end may be a scheduler or build server from before these, on
/// rouille.
pub(super) async fn bincode_req<T: serde::de::DeserializeOwned>(
    req: reqwest::RequestBuilder,
) -> Result<T> {
    let res = req
        .header(reqwest::header::CONNECTION, "close")
        .send()
        .await?;
    let status = res.status();
    let headers = res.headers().clone();
    let body = res.bytes().await.context("error reading response body")?;
    if !status.is_success() {
        Err(anyhow!(
            "Error {} (Headers={:?}): {}",
            status.as_u16(),
            headers,
            String::from_utf8_lossy(&body)
        ))
    } else {
        bincode::deserialize(&body).map_err(Into::into)
    }
}

/// `content` in the encoding the request's `Accept` header prefers.
pub(super) fn prepare_response<T: serde::Serialize>(
    headers: &HeaderMap,
    content: &T,
) -> StdResult<Response, HttpError> {
    const HANDLED: [&str; 2] = ["application/octet-stream", "application/json"];
    // As rouille's `accept!`: no header, or one it cannot read, is `*/*`, and
    // a header that names neither type gets the first.
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("*/*");
    if preferred(accept, &HANDLED) == Some(1) {
        let json = serde_json::to_string(content)
            .context("Failed to serialize response body")
            .map_err(HttpError::internal)?;
        Ok((
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            json,
        )
            .into_response())
    } else {
        let data = bincode::serialize(content)
            .context("Failed to serialize response body")
            .map_err(HttpError::internal)?;
        Ok(([(header::CONTENT_TYPE, "application/octet-stream")], data).into_response())
    }
}

/// Which of `handled` an `Accept` header prefers, by rouille's
/// `priority_header_preferred`: the highest `q`, the first listed on a tie,
/// with `*` matching either half of a type.
fn preferred(header: &str, handled: &[&str]) -> Option<usize> {
    let mut result = (None, f32::NEG_INFINITY);
    for (index, wanted) in handled.iter().enumerate() {
        for (offered, priority) in priority_header(header) {
            if priority <= result.1 {
                continue;
            }
            let mut wanted_parts = wanted.split('/');
            let (wanted_left, wanted_right) = (wanted_parts.next(), wanted_parts.next());
            let mut offered_parts = offered.split('/');
            let (offered_left, offered_right) = (offered_parts.next(), offered_parts.next());
            let left_matches = wanted_left == offered_left
                || wanted_left == Some("*")
                || offered_left == Some("*");
            let right_matches = wanted_right == offered_right
                || wanted_right == Some("*")
                || offered_right == Some("*");
            if *wanted == offered
                || ((wanted_left == Some("*") || offered_left == Some("*")) && right_matches)
                || ((wanted_right == Some("*") || offered_right == Some("*")) && left_matches)
            {
                result = (Some(index), priority);
            }
        }
    }
    result.0
}

/// The elements of an `Accept`-style header with their `q`, 1.0 if none: as
/// rouille's `parse_priority_header`.
fn priority_header(header: &str) -> impl Iterator<Item = (&str, f32)> {
    header.split(',').map(|element| {
        let mut params = element.split(';');
        let media_type = params.next().unwrap_or_default().trim();
        let priority = params
            .find_map(|p| p.trim_start().strip_prefix("q=")?.trim().parse().ok())
            .unwrap_or(1.0);
        (media_type, priority)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const HANDLED: [&str; 2] = ["application/octet-stream", "application/json"];

    #[test]
    fn accept_negotiation_follows_the_protocol() {
        assert_eq!(preferred("*/*", &HANDLED), Some(0));
        assert_eq!(preferred("application/json", &HANDLED), Some(1));
        assert_eq!(preferred("application/*", &HANDLED), Some(0));
        assert_eq!(preferred("text/html, application/json", &HANDLED), Some(1));
        assert_eq!(
            preferred(
                "application/json; q=0.5, application/octet-stream",
                &HANDLED
            ),
            Some(0)
        );
        assert_eq!(
            preferred(
                "application/json, application/octet-stream; q=0.1",
                &HANDLED
            ),
            Some(1)
        );
        // Neither is acceptable: the answer is the first anyway, as rouille
        // gave it.
        assert_eq!(preferred("text/html", &HANDLED), None);
    }

    #[test]
    fn errors_carry_their_causes_as_json() {
        let err = anyhow!("inner").context("outer");
        let HttpError::Logged {
            status,
            message,
            json,
        } = HttpError::internal(err)
        else {
            panic!("not a logged error")
        };
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(message, "outer, caused by: inner");
        let json: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(json["description"], "outer");
        assert_eq!(json["cause"]["description"], "inner");
        assert!(json["cause"]["cause"].is_null());
    }

    #[test]
    fn a_job_token_error_is_a_401_with_a_bearer_challenge() {
        let response = HttpError::invalid_job_token(anyhow!("JWT decode failed")).into_response();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            "Bearer error=\"invalid_jwt\""
        );
        assert!(response.headers().get(header::CONTENT_TYPE).is_none());
    }
}
