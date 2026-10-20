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

//! The build server's HTTP server: HTTPS with its own certificate, for the
//! scheduler and clients, and the heartbeat that tells the scheduler about it.

use axum::Router;
use axum::body::Body;
use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::post;
use bincode::Options as _;
use byteorder::{BigEndian, ReadBytesExt};
use flate2::read::ZlibDecoder as ZlibReadDecoder;
use rand::{Rng, rand_core::UnwrapErr, rngs::SysRng};
use std::convert::Infallible;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use super::auth::{JWT_KEY_LENGTH, JWTJobAuthorizer, bearer_token};
use super::common::{HeartbeatServerHttpRequest, ReqwestRequestBuilderExt, RunJobHttpRequest};
use super::handler::{
    HttpError, MAX_BINCODE_BODY, PathParam, ReqId, bincode_req, block_on, blocking, body_reader,
    drain, new_reqwest_client, not_found, path_param, prepare_response, read_bincode, respond,
};
use super::serve::serve;
use super::tls::{create_https_cert_and_privkey, tls_acceptor};
use super::urls;
use crate::dist::{
    self, HeartbeatServerResult, InputsReader, JobAuthorizer, JobId, JobState, RunJobResult,
    ServerIncoming, ServerNonce, Toolchain, ToolchainReader, UpdateJobStateResult,
};
use crate::errors::*;
use crate::util::num_cpus;

/// How often a build server tells the scheduler it is there.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// How soon it tries again after a heartbeat failed.
const HEARTBEAT_ERROR_INTERVAL: Duration = Duration::from_secs(10);
/// How long the scheduler waits to hear from a build server before it takes
/// it to be gone.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(90);

/// The build server's HTTP server.
pub struct Server<S> {
    bind_address: SocketAddr,
    scheduler_url: reqwest::Url,
    scheduler_auth: String,
    // HTTPS pieces all the builders will use for connection encryption
    cert_digest: Vec<u8>,
    cert_pem: Vec<u8>,
    privkey_pem: Vec<u8>,
    // Key used to sign any requests relating to jobs
    jwt_key: Vec<u8>,
    // Randomly generated nonce to allow the scheduler to detect server restarts
    server_nonce: ServerNonce,
    handler: S,
}

impl<S: ServerIncoming + 'static> Server<S> {
    pub fn new(
        public_addr: SocketAddr,
        bind_address: Option<SocketAddr>,
        scheduler_url: reqwest::Url,
        scheduler_auth: String,
        handler: S,
    ) -> Result<Self> {
        let (cert_digest, cert_pem, privkey_pem) = create_https_cert_and_privkey(public_addr)
            .context("failed to create HTTPS certificate for server")?;
        let mut jwt_key = vec![0; JWT_KEY_LENGTH];
        UnwrapErr(SysRng).fill_bytes(&mut jwt_key);
        let server_nonce = ServerNonce::new();

        Ok(Self {
            bind_address: bind_address.unwrap_or(public_addr),
            scheduler_url,
            scheduler_auth,
            cert_digest,
            cert_pem,
            privkey_pem,
            jwt_key,
            server_nonce,
            handler,
        })
    }

    /// Serves until it fails, on a tokio runtime of its own, sending
    /// heartbeats to the scheduler meanwhile. Returns only an error.
    pub fn start(self) -> Result<Infallible> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to start a runtime for the server")?;
        runtime.block_on(self.run())
    }

    async fn run(self) -> Result<Infallible> {
        let Self {
            bind_address,
            scheduler_url,
            scheduler_auth,
            cert_digest,
            cert_pem,
            privkey_pem,
            jwt_key,
            server_nonce,
            handler,
        } = self;
        let tls = tls_acceptor(&cert_pem, &privkey_pem)?;
        let heartbeat_req = HeartbeatServerHttpRequest {
            num_cpus: num_cpus(),
            jwt_key: jwt_key.clone(),
            server_nonce,
            cert_digest,
            cert_pem,
        };
        let heartbeat_url = urls::scheduler_heartbeat_server(&scheduler_url);
        let client = new_reqwest_client()?;
        let state = Arc::new(ServerState::new(
            handler,
            jwt_key,
            client.clone(),
            scheduler_url,
            scheduler_auth.clone(),
        ));

        // Listening before the first heartbeat, where the rouille server sent
        // it first: the scheduler may assign a job the moment it hears of a
        // server.
        let listener = tokio::net::TcpListener::bind(bind_address)
            .await
            .with_context(|| {
                format!("Failed to start http server for sccache server on {bind_address}")
            })?;
        info!("Server listening for clients on {}", bind_address);
        tokio::spawn(heartbeat(
            client,
            heartbeat_url,
            scheduler_auth,
            heartbeat_req,
        ));
        match serve(listener, Some(tls), router(state)).await {}
    }
}

/// Tells the scheduler this server exists, its certificate and its job key,
/// every `HEARTBEAT_INTERVAL`, or sooner after a failure.
async fn heartbeat(
    client: reqwest::Client,
    url: reqwest::Url,
    scheduler_auth: String,
    heartbeat_req: HeartbeatServerHttpRequest,
) {
    loop {
        trace!(target: "sccache_heartbeat", "Performing heartbeat");
        let res = match client
            .post(url.clone())
            .bearer_auth(scheduler_auth.clone())
            .bincode(&heartbeat_req)
        {
            Ok(req) => bincode_req(req).await,
            Err(e) => Err(e),
        };
        match res {
            Ok(HeartbeatServerResult { is_new }) => {
                trace!(target: "sccache_heartbeat", "Heartbeat success is_new={}", is_new);
                // TODO: if is_new, terminate all running jobs
                if is_new {
                    info!("Server connected to scheduler");
                }
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            }
            Err(e) => {
                error!(target: "sccache_heartbeat", "Failed to send heartbeat to server: {}", e);
                tokio::time::sleep(HEARTBEAT_ERROR_INTERVAL).await;
            }
        }
    }
}

pub(super) struct ServerState<S> {
    handler: S,
    job_authorizer: Box<JWTJobAuthorizer>,
    requester: ServerRequester,
}

impl<S> ServerState<S> {
    pub(super) fn new(
        handler: S,
        jwt_key: Vec<u8>,
        client: reqwest::Client,
        scheduler_url: reqwest::Url,
        scheduler_auth: String,
    ) -> Self {
        Self {
            handler,
            job_authorizer: JWTJobAuthorizer::new(jwt_key),
            requester: ServerRequester {
                client,
                scheduler_url,
                scheduler_auth,
            },
        }
    }
}

pub(super) fn router<S: ServerIncoming + 'static>(state: Arc<ServerState<S>>) -> Router {
    Router::new()
        .route(
            "/api/v1/distserver/assign_job/{job_id}",
            post(assign_job::<S>),
        )
        .route(
            "/api/v1/distserver/submit_toolchain/{job_id}",
            post(submit_toolchain::<S>),
        )
        .route("/api/v1/distserver/run_job/{job_id}", post(run_job::<S>))
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .with_state(state)
}

type Shared<S> = State<Arc<ServerState<S>>>;

/// The request's bearer token must be the job's.
///
/// On the runtime: an HS256 signature over a few bytes, no shared state.
fn check_job_auth<S>(
    state: &ServerState<S>,
    headers: &HeaderMap,
    job_id: JobId,
) -> std::result::Result<(), HttpError> {
    match bearer_token(headers) {
        Some(token) => state.job_authorizer.verify_token(job_id, token),
        None => Err(anyhow!("no Authorization header")),
    }
    .map_err(HttpError::invalid_job_token)
}

async fn assign_job<S: ServerIncoming + 'static>(
    State(state): Shared<S>,
    Extension(ReqId(req_id)): Extension<ReqId>,
    job_id: PathParam,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let result = async {
        let job_id: JobId = path_param(job_id)?;
        check_job_auth(&state, &headers, job_id)?;
        let toolchain: Toolchain = read_bincode(&headers, body).await?;
        trace!("Req {}: assign_job({}): {:?}", req_id, job_id, toolchain);

        let res = blocking(move || {
            state
                .handler
                .handle_assign_job(job_id, toolchain)
                .map_err(HttpError::internal)
        })
        .await?;
        prepare_response(&headers, &res)
    }
    .await;
    respond(req_id, result)
}

async fn submit_toolchain<S: ServerIncoming + 'static>(
    State(state): Shared<S>,
    Extension(ReqId(req_id)): Extension<ReqId>,
    job_id: PathParam,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let result = async {
        let job_id: JobId = path_param(job_id)?;
        check_job_auth(&state, &headers, job_id)?;
        trace!("Req {}: submit_toolchain({})", req_id, job_id);

        // The toolchain goes to the cache on disk as it arrives: it can be
        // hundreds of megabytes.
        let mut body = body_reader(body);
        let res = blocking(move || {
            let res = state.handler.handle_submit_toolchain(
                &state.requester,
                job_id,
                ToolchainReader(Box::new(&mut body)),
            );
            drain(&mut body);
            res.map_err(HttpError::internal)
        })
        .await?;
        prepare_response(&headers, &res)
    }
    .await;
    respond(req_id, result)
}

async fn run_job<S: ServerIncoming + 'static>(
    State(state): Shared<S>,
    Extension(ReqId(req_id)): Extension<ReqId>,
    job_id: PathParam,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let result = async {
        let job_id: JobId = path_param(job_id)?;
        check_job_auth(&state, &headers, job_id)?;

        // The inputs are inflated into the build as they arrive.
        let mut body = body_reader(body);
        let res = blocking(move || {
            let res = run_job_from(&state, req_id, job_id, &mut body);
            drain(&mut body);
            res
        })
        .await?;
        prepare_response(&headers, &res)
    }
    .await;
    respond(req_id, result)
}

/// Runs a job from its request body: a big-endian `u32` length, a
/// `RunJobHttpRequest` in that many bytes of bincode, then the inputs,
/// zlib-compressed, to the end. On the blocking pool.
fn run_job_from<S: ServerIncoming>(
    state: &ServerState<S>,
    req_id: usize,
    job_id: JobId,
    body: &mut (impl Read + Send),
) -> std::result::Result<RunJobResult, HttpError> {
    let bincode_length = body
        .read_u32::<BigEndian>()
        .context("failed to read run job input length")
        .map_err(HttpError::internal)? as u64;

    let mut bincode_reader = body.take(bincode_length);
    // Bounded twice: by the frame the sender declared, since the message
    // cannot be larger than what contains it, and by the ceiling, since the
    // sender chose that frame length too.
    let runjob: RunJobHttpRequest = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .allow_trailing_bytes()
        .with_limit(bincode_length.min(MAX_BINCODE_BODY))
        .deserialize_from(&mut bincode_reader)
        .context("failed to deserialize run job request")
        .map_err(HttpError::internal)?;
    trace!("Req {}: run_job({}): {:?}", req_id, job_id, runjob);
    let RunJobHttpRequest { command, outputs } = runjob;
    let body = bincode_reader.into_inner();
    let inputs_rdr = InputsReader(Box::new(ZlibReadDecoder::new(body)));

    state
        .handler
        .handle_run_job(&state.requester, job_id, command, outputs, inputs_rdr)
        .map_err(HttpError::internal)
}

/// The build server's requests to the scheduler.
struct ServerRequester {
    client: reqwest::Client,
    scheduler_url: reqwest::Url,
    scheduler_auth: String,
}

impl dist::ServerOutgoing for ServerRequester {
    fn do_update_job_state(&self, job_id: JobId, state: JobState) -> Result<UpdateJobStateResult> {
        let url = urls::scheduler_job_state(&self.scheduler_url, job_id);
        block_on(bincode_req(
            self.client
                .post(url)
                .bearer_auth(self.scheduler_auth.clone())
                .bincode(&state)?,
        ))
        .context("POST to scheduler job_state failed")
    }
}
