// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! The scheduler and build server, in process.
//!
//! The router tests send requests built as the clients build them straight to
//! the routers, with a scheduler and a build server that only record what they
//! are given: routing, authentication, error responses and streaming, each
//! against the answers the protocol expects -- the statuses, headers and
//! bodies that clients and peers of any version, the rouille servers' among
//! them, rely on. The TLS tests put a real connection in between. The last
//! starts a scheduler and a build server on loopback and takes a job through
//! both.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use byteorder::{BigEndian, WriteBytesExt};
use flate2::Compression;
use flate2::write::ZlibEncoder;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::auth::{ClientAuthCheck, ClientVisibleMsg, JWTJobAuthorizer, ServerAuthCheck};
use super::common::{
    AllocJobHttpResponse, HeartbeatServerHttpRequest, ReqwestRequestBuilderExt, RunJobHttpRequest,
    ServerCertificateHttpResponse,
};
use super::handler::{MAX_BINCODE_BODY, bincode_req, new_reqwest_client};
use super::scheduler::{self, SchedulerState};
use super::serve::{dispatch, serve};
use super::server::{self, ServerState};
use super::tls::{create_https_cert_and_privkey, tls_acceptor};
use super::urls;
use crate::dist::{
    AllocJobResult, AssignJobResult, CompileCommand, HeartbeatServerResult, InputsReader, JobAlloc,
    JobAuthorizer, JobId, JobState, RunJobResult, SchedulerIncoming, SchedulerOutgoing,
    SchedulerStatusResult, ServerId, ServerIncoming, ServerNonce, ServerOutgoing,
    SubmitToolchainResult, Toolchain, ToolchainReader, UpdateJobStateResult,
};
use crate::errors::*;

const CLIENT_TOKEN: &str = "the client token";
const JWT_KEY: [u8; 32] = [7; 32];
/// A job the stub build server answers without reading the toolchain.
const UNREAD_JOB: JobId = JobId(1000);

fn remote() -> SocketAddr {
    "127.0.0.1:40000".parse().unwrap()
}

/// Accepts [`CLIENT_TOKEN`] and refuses anything else, with a message.
struct TokenCheck;
impl ClientAuthCheck for TokenCheck {
    fn check(&self, token: &str) -> std::result::Result<(), ClientVisibleMsg> {
        if token == CLIENT_TOKEN {
            Ok(())
        } else {
            Err(ClientVisibleMsg::from_nonsensitive(
                "that is not the token".to_owned(),
            ))
        }
    }
}

/// A build server's token is its address.
fn server_auth() -> ServerAuthCheck {
    Box::new(|token| token.parse().ok().map(ServerId::new))
}

/// A scheduler that knows the servers that sent heartbeats and records job
/// states; it assigns every job to the first server it knows.
#[derive(Clone, Default)]
struct StubScheduler {
    servers: Arc<Mutex<HashMap<ServerId, Box<dyn JobAuthorizer>>>>,
    job_states: Arc<Mutex<Vec<(JobId, ServerId, JobState)>>>,
    next_job: Arc<AtomicU64>,
}

impl SchedulerIncoming for StubScheduler {
    fn handle_alloc_job(
        &self,
        requester: &dyn SchedulerOutgoing,
        tc: Toolchain,
    ) -> Result<AllocJobResult> {
        match tc.archive_id.as_str() {
            "fail" => return Err(anyhow!("inner").context("outer")),
            "panic" => panic!("the handler panicked"),
            _ => (),
        }
        let (server_id, job_id, auth) = {
            let servers = self.servers.lock().unwrap();
            let Some((&server_id, job_authorizer)) = servers.iter().next() else {
                return Ok(AllocJobResult::Fail {
                    msg: "no servers".to_owned(),
                });
            };
            let job_id = JobId(self.next_job.fetch_add(1, Ordering::SeqCst));
            (server_id, job_id, job_authorizer.generate_token(job_id)?)
        };
        // "unassigned" skips the request to the build server, for the router
        // tests, which have none.
        let need_toolchain = if tc.archive_id == "unassigned" {
            false
        } else {
            requester
                .do_assign_job(server_id, job_id, tc, auth.clone())?
                .need_toolchain
        };
        Ok(AllocJobResult::Success {
            job_alloc: JobAlloc {
                auth,
                job_id,
                server_id,
            },
            need_toolchain,
        })
    }

    fn handle_heartbeat_server(
        &self,
        server_id: ServerId,
        _server_nonce: ServerNonce,
        _num_cpus: usize,
        job_authorizer: Box<dyn JobAuthorizer>,
    ) -> Result<HeartbeatServerResult> {
        let is_new = self
            .servers
            .lock()
            .unwrap()
            .insert(server_id, job_authorizer)
            .is_none();
        Ok(HeartbeatServerResult { is_new })
    }

    fn handle_update_job_state(
        &self,
        job_id: JobId,
        server_id: ServerId,
        job_state: JobState,
    ) -> Result<UpdateJobStateResult> {
        self.job_states
            .lock()
            .unwrap()
            .push((job_id, server_id, job_state));
        Ok(UpdateJobStateResult::Success)
    }

    fn handle_status(&self) -> Result<SchedulerStatusResult> {
        Ok(SchedulerStatusResult {
            num_servers: self.servers.lock().unwrap().len(),
            num_cpus: 1,
            in_progress: 0,
        })
    }
}

/// A build server that records the toolchains and job inputs it is sent.
#[derive(Clone, Default)]
struct StubServer {
    toolchains: Arc<Mutex<Vec<(JobId, Vec<u8>)>>>,
    runs: Arc<Mutex<Vec<(JobId, CompileCommand, Vec<String>, Vec<u8>)>>>,
    /// Signalled once the first byte of a toolchain has been read.
    first_byte_read: Arc<Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
    /// Tell the scheduler a job has started, as the real build server does.
    report_job_state: bool,
}

impl ServerIncoming for StubServer {
    fn handle_assign_job(&self, _job_id: JobId, _tc: Toolchain) -> Result<AssignJobResult> {
        Ok(AssignJobResult {
            state: JobState::Ready,
            need_toolchain: true,
        })
    }

    fn handle_submit_toolchain(
        &self,
        _requester: &dyn ServerOutgoing,
        job_id: JobId,
        mut tc_rdr: ToolchainReader<'_>,
    ) -> Result<SubmitToolchainResult> {
        if job_id == UNREAD_JOB {
            return Ok(SubmitToolchainResult::Success);
        }
        let mut toolchain = vec![];
        if let Some(first_byte_read) = self.first_byte_read.lock().unwrap().take() {
            let mut first = [0; 1];
            tc_rdr.read_exact(&mut first)?;
            toolchain.push(first[0]);
            first_byte_read.send(()).unwrap();
        }
        tc_rdr.read_to_end(&mut toolchain)?;
        self.toolchains.lock().unwrap().push((job_id, toolchain));
        Ok(SubmitToolchainResult::Success)
    }

    fn handle_run_job(
        &self,
        requester: &dyn ServerOutgoing,
        job_id: JobId,
        command: CompileCommand,
        outputs: Vec<String>,
        mut inputs_rdr: InputsReader<'_>,
    ) -> Result<RunJobResult> {
        if self.report_job_state {
            requester.do_update_job_state(job_id, JobState::Started)?;
        }
        let mut inputs = vec![];
        inputs_rdr.read_to_end(&mut inputs)?;
        self.runs
            .lock()
            .unwrap()
            .push((job_id, command, outputs, inputs));
        Ok(RunJobResult::JobNotFound)
    }
}

fn scheduler_router(stub: StubScheduler) -> Router {
    let state = SchedulerState::new(stub, Box::new(TokenCheck), server_auth()).unwrap();
    scheduler::router(Arc::new(state))
}

fn server_router(stub: StubServer) -> Router {
    let state = ServerState::new(
        stub,
        JWT_KEY.to_vec(),
        new_reqwest_client().unwrap(),
        "http://127.0.0.1:1/".parse().unwrap(),
        "unused".to_owned(),
    );
    server::router(Arc::new(state))
}

fn job_token(job_id: JobId) -> String {
    JWTJobAuthorizer::new(JWT_KEY.to_vec())
        .generate_token(job_id)
        .unwrap()
}

/// A request with a bincode body, as `ReqwestRequestBuilderExt::bincode`
/// sends one.
fn bincode_request<T: serde::Serialize>(uri: &str, token: Option<&str>, body: &T) -> Request<Body> {
    let body = bincode::serialize(body).unwrap();
    let mut builder = Request::post(uri)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, body.len());
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::from(body)).unwrap()
}

async fn call(router: &Router, req: Request<Body>) -> Response {
    dispatch(router.clone(), remote(), 0, req).await.unwrap()
}

async fn body_bytes(res: Response) -> Bytes {
    axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap()
}

async fn body_json(res: Response) -> serde_json::Value {
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "application/json; charset=utf-8"
    );
    serde_json::from_slice(&body_bytes(res).await).unwrap()
}

fn challenge(res: &Response) -> &str {
    res.headers()[header::WWW_AUTHENTICATE].to_str().unwrap()
}

/// A `run_job` body: the length of the bincode request, the request, and the
/// inputs compressed -- as the client writes it.
fn run_job_body(command: &CompileCommand, outputs: &[&str], inputs: &[u8]) -> Vec<u8> {
    let request = RunJobHttpRequest {
        command: command.clone(),
        outputs: outputs.iter().map(|o| o.to_string()).collect(),
    };
    let bincode = bincode::serialize(&request).unwrap();
    let mut body = vec![];
    body.write_u32::<BigEndian>(bincode.len() as u32).unwrap();
    body.write_all(&bincode).unwrap();
    let mut compressor = ZlibEncoder::new(&mut body, Compression::fast());
    compressor.write_all(inputs).unwrap();
    compressor.finish().unwrap();
    body
}

fn compile_command() -> CompileCommand {
    CompileCommand {
        executable: "/usr/bin/cc".to_owned(),
        arguments: vec!["-c".to_owned(), "x.c".to_owned()],
        env_vars: vec![("LANG".to_owned(), "C".to_owned())],
        cwd: "/build".to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn status_is_bincode_unless_json_is_preferred() {
    let router = scheduler_router(StubScheduler::default());
    let status = || Request::get("/api/v1/scheduler/status");

    let res = call(&router, status().body(Body::empty()).unwrap()).await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    let status_result: SchedulerStatusResult =
        bincode::deserialize(&body_bytes(res).await).unwrap();
    assert_eq!(status_result.num_servers, 0);

    let res = call(
        &router,
        status()
            .header(header::ACCEPT, "application/json")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(body_json(res).await["num_servers"], 0);

    // A type it cannot give is answered with bincode.
    let res = call(
        &router,
        status()
            .header(header::ACCEPT, "text/html")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(
        res.headers()[header::CONTENT_TYPE],
        "application/octet-stream"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn anything_unrouted_is_an_empty_404() {
    let scheduler = scheduler_router(StubScheduler::default());
    let server = server_router(StubServer::default());
    let cases = [
        (&scheduler, Request::get("/api/v1/scheduler/nothing")),
        // A known path with the wrong method: a 404 too, not axum's 405.
        (&scheduler, Request::get("/api/v1/scheduler/alloc_job")),
        (&scheduler, Request::post("/api/v1/scheduler/status")),
        (&scheduler, Request::get("/api/v1/scheduler/status/")),
        // Parameters that do not parse as a job or a server.
        (
            &scheduler,
            Request::post("/api/v1/scheduler/job_state/seven"),
        ),
        (
            &scheduler,
            Request::get("/api/v1/scheduler/server_certificate/nowhere"),
        ),
        (&server, Request::post("/api/v1/distserver/run_job/-1")),
        (&server, Request::get("/api/v1/distserver/assign_job/1")),
        (&server, Request::get("/")),
    ];
    for (router, req) in cases {
        let req = req.body(Body::empty()).unwrap();
        let uri = req.uri().clone();
        let res = call(router, req).await;
        assert_eq!(res.status(), StatusCode::NOT_FOUND, "{uri}");
        assert!(body_bytes(res).await.is_empty(), "{uri}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn alloc_job_needs_the_client_token() {
    let router = scheduler_router(StubScheduler::default());
    let tc = Toolchain {
        archive_id: "unassigned".to_owned(),
    };
    let uri = "/api/v1/scheduler/alloc_job";

    let res = call(&router, bincode_request(uri, None, &tc)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(challenge(&res), "Bearer error=\"no_bearer_auth\"");
    assert!(body_bytes(res).await.is_empty());

    let mut req = bincode_request(uri, None, &tc);
    req.headers_mut()
        .insert(header::AUTHORIZATION, "Basic dXNlcg==".parse().unwrap());
    let res = call(&router, req).await;
    assert_eq!(challenge(&res), "Bearer error=\"no_bearer_auth\"");

    let res = call(&router, bincode_request(uri, Some("another token"), &tc)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(challenge(&res), "Bearer error=\"bearer_auth_failed\"");
    assert_eq!(&body_bytes(res).await[..], b"that is not the token");

    // With no servers, the scheduler says so, in a successful response.
    let res = call(&router, bincode_request(uri, Some(CLIENT_TOKEN), &tc)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let res: AllocJobHttpResponse = bincode::deserialize(&body_bytes(res).await).unwrap();
    assert!(matches!(res, AllocJobHttpResponse::Fail { msg } if msg == "no servers"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_bincode_is_a_400_with_the_reason_as_json() {
    let router = scheduler_router(StubScheduler::default());
    let uri = "/api/v1/scheduler/alloc_job";
    let authorized = |content_type: &str, body: Body| {
        Request::post(uri)
            .header(header::AUTHORIZATION, format!("Bearer {CLIENT_TOKEN}"))
            .header(header::CONTENT_TYPE, content_type)
            .body(body)
            .unwrap()
    };

    let res = call(&router, authorized("text/plain", Body::from("tc"))).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let json = body_json(res).await;
    assert_eq!(
        json["description"],
        "the request didn't have a binary content type"
    );
    assert!(json["cause"].is_null());

    // A string whose length field claims more than there is.
    let res = call(
        &router,
        authorized("application/octet-stream", Body::from(vec![0xff; 8])),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let json = body_json(res).await;
    assert_eq!(json["description"], "error while parsing the bincode body");
    assert!(json["cause"]["description"].is_string());

    let too_long = vec![0; MAX_BINCODE_BODY as usize + 1];
    let res = call(
        &router,
        authorized("application/octet-stream", Body::from(too_long)),
    )
    .await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    let json = body_json(res).await;
    assert_eq!(
        json["cause"]["description"],
        "the size limit has been reached"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failing_or_panicking_handler_is_a_500() {
    let router = scheduler_router(StubScheduler::default());
    let uri = "/api/v1/scheduler/alloc_job";
    let alloc = |archive_id: &str| {
        let tc = Toolchain {
            archive_id: archive_id.to_owned(),
        };
        bincode_request(uri, Some(CLIENT_TOKEN), &tc)
    };

    let res = call(&router, alloc("fail")).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(res).await;
    assert_eq!(json["description"], "outer");
    assert_eq!(json["cause"]["description"], "inner");

    let res = call(&router, alloc("panic")).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    // And the next request is served as before.
    let res = call(&router, alloc("unassigned")).await;
    assert_eq!(res.status(), StatusCode::OK);
}

fn heartbeat(cert_digest: &[u8], cert_pem: &[u8]) -> HeartbeatServerHttpRequest {
    HeartbeatServerHttpRequest {
        jwt_key: JWT_KEY.to_vec(),
        num_cpus: 4,
        server_nonce: ServerNonce::new(),
        cert_digest: cert_digest.to_vec(),
        cert_pem: cert_pem.to_vec(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn build_servers_are_known_by_token_and_address() {
    let router = scheduler_router(StubScheduler::default());
    let (digest, cert_pem, _) =
        create_https_cert_and_privkey("127.0.0.1:12345".parse().unwrap()).unwrap();
    let uri = "/api/v1/scheduler/heartbeat_server";
    let hb = heartbeat(&digest, &cert_pem);

    let res = call(&router, bincode_request(uri, None, &hb)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(challenge(&res), "Bearer error=\"invalid_bearer_token\"");

    let res = call(&router, bincode_request(uri, Some("not an address"), &hb)).await;
    assert_eq!(challenge(&res), "Bearer error=\"invalid_bearer_token\"");

    // The token names a server elsewhere than the request came from.
    let res = call(&router, bincode_request(uri, Some("127.0.0.2:12345"), &hb)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        challenge(&res),
        "Bearer error=\"invalid_bearer_token_mismatched_address\""
    );

    // ...unless a proxy says it did.
    let mut req = bincode_request(uri, Some("127.0.0.2:12345"), &hb);
    req.headers_mut()
        .insert("X-Real-IP", "127.0.0.2".parse().unwrap());
    let res = call(&router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let res: HeartbeatServerResult = bincode::deserialize(&body_bytes(res).await).unwrap();
    assert!(res.is_new);

    let mut req = bincode_request(uri, Some("127.0.0.2:12345"), &hb);
    req.headers_mut()
        .insert("X-Real-IP", "a proxy".parse().unwrap());
    let res = call(&router, req).await;
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert!(body_bytes(res).await.is_empty());

    // job_state is checked the same way.
    let res = call(
        &router,
        bincode_request(
            "/api/v1/scheduler/job_state/3",
            Some("127.0.0.2:12345"),
            &JobState::Started,
        ),
    )
    .await;
    assert_eq!(
        challenge(&res),
        "Bearer error=\"invalid_bearer_token_mismatched_address\""
    );
    let res = call(
        &router,
        bincode_request(
            "/api/v1/scheduler/job_state/3",
            Some("127.0.0.1:12345"),
            &JobState::Started,
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_heartbeat_certificate_is_served_and_named_in_alloc_job() {
    let stub = StubScheduler::default();
    let router = scheduler_router(stub.clone());
    let server_addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
    let (digest, cert_pem, _) = create_https_cert_and_privkey(server_addr).unwrap();
    let cert_uri = format!("/api/v1/scheduler/server_certificate/{server_addr}");

    let res = call(
        &router,
        Request::get(&cert_uri).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        body_json(res).await["description"],
        "server cert not available"
    );

    let res = call(
        &router,
        bincode_request(
            "/api/v1/scheduler/heartbeat_server",
            Some(&server_addr.to_string()),
            &heartbeat(&digest, &cert_pem),
        ),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);

    let res = call(
        &router,
        Request::get(&cert_uri).body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res: ServerCertificateHttpResponse = bincode::deserialize(&body_bytes(res).await).unwrap();
    assert_eq!(res.cert_digest, digest);
    assert_eq!(res.cert_pem, cert_pem);

    let tc = Toolchain {
        archive_id: "unassigned".to_owned(),
    };
    let res = call(
        &router,
        bincode_request("/api/v1/scheduler/alloc_job", Some(CLIENT_TOKEN), &tc),
    )
    .await;
    let res: AllocJobHttpResponse = bincode::deserialize(&body_bytes(res).await).unwrap();
    let AllocJobHttpResponse::Success {
        job_alloc,
        cert_digest,
        ..
    } = res
    else {
        panic!("no job allocated")
    };
    assert_eq!(cert_digest, digest);
    assert_eq!(job_alloc.server_id, ServerId::new(server_addr));
    // The token is the server's, from the key in its heartbeat.
    JWTJobAuthorizer::new(JWT_KEY.to_vec())
        .verify_token(job_alloc.job_id, &job_alloc.auth)
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn job_routes_need_the_jobs_token() {
    let router = server_router(StubServer::default());
    let tc = Toolchain {
        archive_id: "tc".to_owned(),
    };
    let uri = "/api/v1/distserver/assign_job/5";

    let res = call(&router, bincode_request(uri, None, &tc)).await;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(challenge(&res), "Bearer error=\"invalid_jwt\"");
    let json: serde_json::Value = serde_json::from_slice(&body_bytes(res).await).unwrap();
    assert_eq!(json["description"], "no Authorization header");

    for uri in [
        "/api/v1/distserver/assign_job/5",
        "/api/v1/distserver/submit_toolchain/5",
        "/api/v1/distserver/run_job/5",
    ] {
        let res = call(
            &router,
            bincode_request(uri, Some(&job_token(JobId(6))), &tc),
        )
        .await;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(challenge(&res), "Bearer error=\"invalid_jwt\"", "{uri}");
    }

    let res = call(
        &router,
        bincode_request(uri, Some(&job_token(JobId(5))), &tc),
    )
    .await;
    assert_eq!(res.status(), StatusCode::OK);
    let res: AssignJobResult = bincode::deserialize(&body_bytes(res).await).unwrap();
    assert!(res.need_toolchain);
}

fn submit_toolchain_request(job_id: JobId, body: Body) -> Request<Body> {
    Request::post(format!("/api/v1/distserver/submit_toolchain/{job_id}"))
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", job_token(job_id)),
        )
        .body(body)
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_toolchain_reaches_the_handler_while_it_is_still_uploading() {
    let stub = StubServer::default();
    let (first_byte_read, first_byte_was_read) = futures::channel::oneshot::channel();
    *stub.first_byte_read.lock().unwrap() = Some(first_byte_read);
    let router = server_router(stub.clone());

    // Chunked, as the client streams it, and over the bincode ceiling, which
    // a toolchain is not held to.
    let (mut chunks, chunk_stream) = futures::channel::mpsc::channel::<std::io::Result<Bytes>>(1);
    let chunk = Bytes::from(vec![b't'; 1024 * 1024]);
    let response = tokio::spawn(call_owned(
        router,
        submit_toolchain_request(JobId(9), Body::from_stream(chunk_stream)),
    ));
    use futures::SinkExt as _;
    chunks.send(Ok(chunk.clone())).await.unwrap();
    // The handler has the first byte before the rest of the body exists.
    tokio::time::timeout(Duration::from_secs(10), first_byte_was_read)
        .await
        .expect("the toolchain was not streamed to the handler")
        .unwrap();
    for _ in 1..10 {
        chunks.send(Ok(chunk.clone())).await.unwrap();
    }
    drop(chunks);

    let res = response.await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res: SubmitToolchainResult = bincode::deserialize(&body_bytes(res).await).unwrap();
    assert!(matches!(res, SubmitToolchainResult::Success));
    let toolchains = stub.toolchains.lock().unwrap();
    assert_eq!(toolchains.len(), 1);
    assert_eq!(toolchains[0].0, JobId(9));
    assert_eq!(toolchains[0].1.len(), 10 * 1024 * 1024);
    assert!(toolchains[0].1.iter().all(|&b| b == b't'));
}

async fn call_owned(router: Router, req: Request<Body>) -> Response {
    call(&router, req).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_toolchain_the_handler_does_not_read_is_drained_before_the_answer() {
    // Over a real connection, streamed as the client streams a toolchain:
    // the answer must arrive as an answer, not as a connection reset over
    // the unread rest of the upload.
    let stub = StubServer::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, None, server_router(stub.clone())));

    let chunk = Bytes::from(vec![0; 1024 * 1024]);
    let chunks =
        futures::stream::iter((0..32).map(move |_| Ok::<_, std::io::Error>(chunk.clone())));
    let url = format!("http://{addr}/api/v1/distserver/submit_toolchain/{UNREAD_JOB}");
    let res: SubmitToolchainResult = bincode_req(
        reqwest::Client::new()
            .post(url)
            .bearer_auth(job_token(UNREAD_JOB))
            .body(reqwest::Body::wrap_stream(chunks)),
    )
    .await
    .unwrap();
    assert!(matches!(res, SubmitToolchainResult::Success));
    assert!(stub.toolchains.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn run_job_splits_the_frame_and_inflates_the_inputs() {
    let stub = StubServer::default();
    let router = server_router(stub.clone());
    let inputs: Vec<u8> = (0..200_000u32).flat_map(|i| i.to_le_bytes()).collect();
    let run_job = |job_id: JobId, body: Vec<u8>| {
        Request::post(format!("/api/v1/distserver/run_job/{job_id}"))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", job_token(job_id)),
            )
            .header(header::CONTENT_TYPE, "application/octet-stream")
            .header(header::CONTENT_LENGTH, body.len())
            .body(Body::from(body))
            .unwrap()
    };

    let body = run_job_body(&compile_command(), &["x.o"], &inputs);
    let res = call(&router, run_job(JobId(4), body)).await;
    assert_eq!(res.status(), StatusCode::OK);
    let res: RunJobResult = bincode::deserialize(&body_bytes(res).await).unwrap();
    assert!(matches!(res, RunJobResult::JobNotFound));
    {
        let runs = stub.runs.lock().unwrap();
        assert_eq!(runs.len(), 1);
        let (job_id, command, outputs, received) = &runs[0];
        assert_eq!(*job_id, JobId(4));
        assert_eq!(command.executable, "/usr/bin/cc");
        assert_eq!(command.env_vars, compile_command().env_vars);
        assert_eq!(outputs, &["x.o"]);
        assert_eq!(received, &inputs);
    }

    // Too short to hold even the length: a 500.
    let res = call(&router, run_job(JobId(4), vec![0, 0])).await;
    assert_eq!(res.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let json = body_json(res).await;
    assert_eq!(json["description"], "failed to read run job input length");
}

/// The scheduler's and the clients' side of TLS, against the build server's:
/// reqwest over rustls with the server's certificate as its only extra root,
/// as `SchedulerRequester` and the dist client set it up.
#[tokio::test(flavor = "multi_thread")]
async fn a_rustls_client_reaches_the_build_server_over_tls() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (_, cert_pem, key_pem) = create_https_cert_and_privkey(addr).unwrap();
    let tls = tls_acceptor(&cert_pem, &key_pem).unwrap();
    tokio::spawn(serve(
        listener,
        Some(tls),
        server_router(StubServer::default()),
    ));

    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&cert_pem).unwrap())
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let job_id = JobId(11);
    let url = urls::server_assign_job(ServerId::new(addr), job_id);
    let tc = Toolchain {
        archive_id: "tc".to_owned(),
    };
    let res: AssignJobResult = bincode_req(
        client
            .post(url)
            .bearer_auth(job_token(job_id))
            .bincode(&tc)
            .unwrap(),
    )
    .await
    .unwrap();
    assert!(res.need_toolchain);

    // Plain HTTP to the TLS port gets nowhere.
    let plain = format!("http://{addr}/api/v1/distserver/assign_job/{job_id}");
    assert!(client.post(plain).send().await.is_err());
}

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap()
}

/// A job through a scheduler and a build server, each started as
/// `sccion-dist` starts them, on loopback, as a client takes it: heartbeat,
/// certificate exchange, assignment over TLS, toolchain upload, job run and
/// the job state the build server reports back.
#[tokio::test(flavor = "multi_thread")]
async fn a_job_goes_through_the_scheduler_and_a_build_server() {
    let scheduler_addr = free_addr();
    let scheduler_url: reqwest::Url = format!("http://{scheduler_addr}").parse().unwrap();
    let server_addr = free_addr();
    let stub_scheduler = StubScheduler::default();
    let stub_server = StubServer {
        report_job_state: true,
        ..Default::default()
    };
    // Each on a thread of its own, as `start` makes a runtime of its own and
    // returns only to fail.
    {
        let stub = stub_scheduler.clone();
        thread::spawn(move || {
            let Err(e) =
                super::Scheduler::new(scheduler_addr, stub, Box::new(TokenCheck), server_auth())
                    .start();
            panic!("scheduler stopped: {e:?}");
        });
    }
    {
        let stub = stub_server.clone();
        let scheduler_url = scheduler_url.clone();
        let token = server_addr.to_string();
        thread::spawn(move || {
            let Err(e) = super::Server::new(server_addr, None, scheduler_url, token, stub)
                .unwrap()
                .start();
            panic!("server stopped: {e:?}");
        });
    }

    // The build server's heartbeat reaches the scheduler.
    let client = new_reqwest_client().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status: Result<SchedulerStatusResult> =
            bincode_req(client.get(urls::scheduler_status(&scheduler_url))).await;
        if matches!(status, Ok(SchedulerStatusResult { num_servers: 1, .. })) {
            break;
        }
        assert!(Instant::now() < deadline, "no heartbeat: {status:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The scheduler assigns the job to the build server, over TLS.
    let tc = Toolchain {
        archive_id: "tc".to_owned(),
    };
    let res: AllocJobHttpResponse = bincode_req(
        client
            .post(urls::scheduler_alloc_job(&scheduler_url))
            .bearer_auth(CLIENT_TOKEN)
            .bincode(&tc)
            .unwrap(),
    )
    .await
    .unwrap();
    let AllocJobHttpResponse::Success {
        job_alloc,
        need_toolchain,
        cert_digest,
    } = res
    else {
        panic!("no job allocated")
    };
    assert!(need_toolchain, "the assignment did not reach the server");
    assert_eq!(job_alloc.server_id, ServerId::new(server_addr));

    // The client fetches the certificate the digest names, and trusts it.
    let cert: ServerCertificateHttpResponse = bincode_req(client.get(
        urls::scheduler_server_certificate(&scheduler_url, job_alloc.server_id),
    ))
    .await
    .unwrap();
    assert_eq!(cert.cert_digest, cert_digest);
    let server_client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(&cert.cert_pem).unwrap())
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();

    let res: SubmitToolchainResult = bincode_req(
        server_client
            .post(urls::server_submit_toolchain(
                job_alloc.server_id,
                job_alloc.job_id,
            ))
            .bearer_auth(job_alloc.auth.clone())
            .body(b"a toolchain".to_vec()),
    )
    .await
    .unwrap();
    assert!(matches!(res, SubmitToolchainResult::Success));

    let inputs = b"the inputs".repeat(1000);
    let res: RunJobResult = bincode_req(
        server_client
            .post(urls::server_run_job(job_alloc.server_id, job_alloc.job_id))
            .bearer_auth(job_alloc.auth.clone())
            .bytes(run_job_body(&compile_command(), &["x.o"], &inputs)),
    )
    .await
    .unwrap();
    assert!(matches!(res, RunJobResult::JobNotFound));

    assert_eq!(
        stub_server.toolchains.lock().unwrap()[..],
        [(job_alloc.job_id, b"a toolchain".to_vec())]
    );
    assert_eq!(stub_server.runs.lock().unwrap()[0].3, inputs);
    // The build server told the scheduler the job started.
    assert_eq!(
        stub_scheduler.job_states.lock().unwrap()[..],
        [(job_alloc.job_id, job_alloc.server_id, JobState::Started)]
    );
}
