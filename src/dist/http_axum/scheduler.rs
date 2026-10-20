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

//! The scheduler's HTTP server: plain HTTP, for clients and build servers.

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use super::common::{
    HttpError, OUTGOING_TIMEOUT, PathParam, RemoteAddr, ReqId, bearer_token, bincode_req, block_on,
    blocking, new_reqwest_client, not_found, path_param, prepare_response, read_bincode, respond,
};
use super::serve::serve;
use crate::dist::http::common::{
    AllocJobHttpResponse, HeartbeatServerHttpRequest, ReqwestRequestBuilderExt,
    ServerCertificateHttpResponse,
};
use crate::dist::http::server::{
    ClientAuthCheck, ClientVisibleMsg, JWTJobAuthorizer, ServerAuthCheck,
};
use crate::dist::http::urls;
use crate::dist::{self, AssignJobResult, JobId, JobState, SchedulerIncoming, ServerId, Toolchain};
use crate::errors::*;

/// The scheduler's HTTP server. Constructed and started as the rouille one,
/// [`crate::dist::http::Scheduler`], is.
pub struct Scheduler<S> {
    public_addr: SocketAddr,
    handler: S,
    // Is this client permitted to use the scheduler?
    check_client_auth: Box<dyn ClientAuthCheck>,
    // Do we believe the server is who they appear to be?
    check_server_auth: ServerAuthCheck,
}

impl<S: SchedulerIncoming + 'static> Scheduler<S> {
    pub fn new(
        public_addr: SocketAddr,
        handler: S,
        check_client_auth: Box<dyn ClientAuthCheck>,
        check_server_auth: ServerAuthCheck,
    ) -> Self {
        Self {
            public_addr,
            handler,
            check_client_auth,
            check_server_auth,
        }
    }

    /// Serves until it fails, on a tokio runtime of its own. Returns only an
    /// error.
    pub fn start(self) -> Result<Infallible> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to start a runtime for the scheduler")?;
        runtime.block_on(self.run())
    }

    async fn run(self) -> Result<Infallible> {
        let Self {
            public_addr,
            handler,
            check_client_auth,
            check_server_auth,
        } = self;
        let state = SchedulerState::new(handler, check_client_auth, check_server_auth)?;
        let listener = tokio::net::TcpListener::bind(public_addr)
            .await
            .with_context(|| {
                format!("Failed to start http server for sccache scheduler on {public_addr}")
            })?;
        info!("Scheduler listening for clients on {}", public_addr);
        match serve(listener, None, router(Arc::new(state))).await {}
    }
}

pub(super) struct SchedulerState<S> {
    handler: S,
    check_client_auth: Box<dyn ClientAuthCheck>,
    check_server_auth: ServerAuthCheck,
    requester: SchedulerRequester,
    // From server_id -> cert_digest, cert_pem
    server_certificates: Mutex<HashMap<ServerId, (Vec<u8>, Vec<u8>)>>,
}

impl<S> SchedulerState<S> {
    pub(super) fn new(
        handler: S,
        check_client_auth: Box<dyn ClientAuthCheck>,
        check_server_auth: ServerAuthCheck,
    ) -> Result<Self> {
        Ok(Self {
            handler,
            check_client_auth,
            check_server_auth,
            requester: SchedulerRequester {
                client: Mutex::new(new_reqwest_client()?),
            },
            server_certificates: Default::default(),
        })
    }
}

pub(super) fn router<S: SchedulerIncoming + 'static>(state: Arc<SchedulerState<S>>) -> Router {
    Router::new()
        .route("/api/v1/scheduler/alloc_job", post(alloc_job::<S>))
        .route(
            "/api/v1/scheduler/server_certificate/{server_id}",
            get(server_certificate::<S>),
        )
        .route(
            "/api/v1/scheduler/heartbeat_server",
            post(heartbeat_server::<S>),
        )
        .route("/api/v1/scheduler/job_state/{job_id}", post(job_state::<S>))
        .route("/api/v1/scheduler/status", get(status::<S>))
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

type Shared<S> = State<Arc<SchedulerState<S>>>;

async fn alloc_job<S: SchedulerIncoming + 'static>(
    State(state): Shared<S>,
    Extension(ReqId(req_id)): Extension<ReqId>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let result = async {
        let Some(token) = bearer_token(&headers).map(str::to_owned) else {
            return Err(HttpError::unauthorized(
                "no_bearer_auth",
                ClientVisibleMsg(String::new()),
            ));
        };
        // On the blocking pool: a check may ask another service, over HTTP,
        // whether the token is good.
        let check_state = state.clone();
        blocking(move || {
            check_state.check_client_auth.check(&token).map_err(|msg| {
                warn!("Bearer auth failed: {:?}", msg);
                HttpError::unauthorized("bearer_auth_failed", msg)
            })
        })
        .await?;
        let toolchain: Toolchain = read_bincode(&headers, body).await?;
        trace!("Req {}: alloc_job: {:?}", req_id, toolchain);

        let res = blocking(move || {
            let alloc_job_res = state
                .handler
                .handle_alloc_job(&state.requester, toolchain)
                .map_err(HttpError::internal)?;
            let certs = state.server_certificates.lock().unwrap();
            Ok(AllocJobHttpResponse::from_alloc_job_result(
                alloc_job_res,
                &certs,
            ))
        })
        .await?;
        prepare_response(&headers, &res)
    }
    .await;
    respond(req_id, result)
}

async fn server_certificate<S: SchedulerIncoming + 'static>(
    State(state): Shared<S>,
    Extension(ReqId(req_id)): Extension<ReqId>,
    server_id: PathParam,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let server_id: ServerId = path_param(server_id)?;
        let certs = blocking(move || {
            Ok(state
                .server_certificates
                .lock()
                .unwrap()
                .get(&server_id)
                .cloned())
        })
        .await?;
        let (cert_digest, cert_pem) = certs
            .context("server cert not available")
            .map_err(HttpError::internal)?;
        let res = ServerCertificateHttpResponse {
            cert_digest,
            cert_pem,
        };
        prepare_response(&headers, &res)
    }
    .await;
    respond(req_id, result)
}

async fn heartbeat_server<S: SchedulerIncoming + 'static>(
    State(state): Shared<S>,
    Extension(ReqId(req_id)): Extension<ReqId>,
    Extension(RemoteAddr(remote)): Extension<RemoteAddr>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let result = async {
        let server_id = check_server_auth(&state, &headers, remote)?;
        let heartbeat_server: HeartbeatServerHttpRequest = read_bincode(&headers, body).await?;
        trace!(target: "sccache_heartbeat", "Req {}: heartbeat_server: {:?}", req_id, heartbeat_server);

        let res = blocking(move || {
            let HeartbeatServerHttpRequest {
                num_cpus,
                jwt_key,
                server_nonce,
                cert_digest,
                cert_pem,
            } = heartbeat_server;
            state
                .requester
                .maybe_update_certs(
                    &mut state.server_certificates.lock().unwrap(),
                    server_id,
                    cert_digest,
                    cert_pem,
                )
                .map_err(HttpError::internal)?;
            let job_authorizer = JWTJobAuthorizer::new(jwt_key);
            state
                .handler
                .handle_heartbeat_server(server_id, server_nonce, num_cpus, job_authorizer)
                .map_err(HttpError::internal)
        })
        .await?;
        prepare_response(&headers, &res)
    }
    .await;
    respond(req_id, result)
}

async fn job_state<S: SchedulerIncoming + 'static>(
    State(state): Shared<S>,
    Extension(ReqId(req_id)): Extension<ReqId>,
    Extension(RemoteAddr(remote)): Extension<RemoteAddr>,
    job_id: PathParam,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let result = async {
        let job_id: JobId = path_param(job_id)?;
        let server_id = check_server_auth(&state, &headers, remote)?;
        let job_state: JobState = read_bincode(&headers, body).await?;
        trace!("Req {}: job state: {:?}", req_id, job_state);

        let res = blocking(move || {
            state
                .handler
                .handle_update_job_state(job_id, server_id, job_state)
                .map_err(HttpError::internal)
        })
        .await?;
        prepare_response(&headers, &res)
    }
    .await;
    respond(req_id, result)
}

async fn status<S: SchedulerIncoming + 'static>(
    State(state): Shared<S>,
    Extension(ReqId(req_id)): Extension<ReqId>,
    headers: HeaderMap,
) -> Response {
    let result = async {
        let res =
            blocking(move || state.handler.handle_status().map_err(HttpError::internal)).await?;
        prepare_response(&headers, &res)
    }
    .await;
    respond(req_id, result)
}

/// Which build server a request is from: its bearer token names it, and the
/// request must come from that server's address -- or say it does, in
/// `X-Real-IP`, from behind a proxy. rouille's `check_server_auth_or_err!`.
///
/// Run on the runtime: the check is a token comparison or an HS256 signature,
/// and touches no shared state.
fn check_server_auth<S>(
    state: &SchedulerState<S>,
    headers: &HeaderMap,
    remote: SocketAddr,
) -> std::result::Result<ServerId, HttpError> {
    let Some(server_id) = bearer_token(headers).and_then(&*state.check_server_auth) else {
        return Err(HttpError::unauthorized(
            "invalid_bearer_token",
            ClientVisibleMsg(String::new()),
        ));
    };
    let origin_ip: IpAddr = match headers.get("X-Real-IP") {
        Some(header_val) => {
            trace!("X-Real-IP: {:?}", header_val);
            match header_val.to_str().ok().and_then(|v| v.parse().ok()) {
                Some(ip) => ip,
                None => {
                    warn!("X-Real-IP value {:?} could not be parsed", header_val);
                    return Err(HttpError::Empty(StatusCode::BAD_REQUEST));
                }
            }
        }
        None => remote.ip(),
    };
    if server_id.addr().ip() != origin_ip {
        trace!("server ip: {:?}", server_id.addr().ip());
        trace!("request ip: {:?}", remote.ip());
        return Err(HttpError::unauthorized(
            "invalid_bearer_token_mismatched_address",
            ClientVisibleMsg(String::new()),
        ));
    }
    Ok(server_id)
}

/// The scheduler's requests to build servers, over TLS with their own
/// certificates as roots.
struct SchedulerRequester {
    client: Mutex<reqwest::Client>,
}

impl SchedulerRequester {
    /// Trusts `cert_pem` for `server_id` from now on, if it is not the
    /// certificate already trusted for it: rebuilds the client with every
    /// known certificate as a root. rouille's `maybe_update_certs`.
    ///
    /// The caller holds the certificate map's lock, so the map and the
    /// client change together.
    fn maybe_update_certs(
        &self,
        certs: &mut HashMap<ServerId, (Vec<u8>, Vec<u8>)>,
        server_id: ServerId,
        cert_digest: Vec<u8>,
        cert_pem: Vec<u8>,
    ) -> Result<()> {
        if let Some((saved_cert_digest, _)) = certs.get(&server_id)
            && saved_cert_digest == &cert_digest
        {
            return Ok(());
        }
        info!(
            "Adding new certificate for {} to scheduler",
            server_id.addr()
        );
        let mut client_builder = reqwest::ClientBuilder::new();
        // Add all the certificates we know about
        client_builder = client_builder.add_root_certificate(
            reqwest::Certificate::from_pem(&cert_pem)
                .context("failed to interpret pem as certificate")?,
        );
        for (_, cert_pem) in certs.values() {
            client_builder = client_builder.add_root_certificate(
                reqwest::Certificate::from_pem(cert_pem).expect("previously valid cert"),
            );
        }
        // Finish the client
        let new_client = client_builder
            // Disable connection pool to avoid broken connection
            // between runtime
            .pool_max_idle_per_host(0)
            .timeout(OUTGOING_TIMEOUT)
            .build()
            .context("failed to create a HTTP client")?;
        // Use the updated certificates
        *self.client.lock().unwrap() = new_client;
        certs.insert(server_id, (cert_digest, cert_pem));
        Ok(())
    }
}

impl dist::SchedulerOutgoing for SchedulerRequester {
    fn do_assign_job(
        &self,
        server_id: ServerId,
        job_id: JobId,
        tc: Toolchain,
        auth: String,
    ) -> Result<AssignJobResult> {
        let url = urls::server_assign_job(server_id, job_id);
        let req = self.client.lock().unwrap().post(url);
        block_on(bincode_req(req.bearer_auth(auth).bincode(&tc)?))
            .context("POST to scheduler assign_job failed")
    }
}
