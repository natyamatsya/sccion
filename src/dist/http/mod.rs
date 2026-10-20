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

//! The dist protocol over HTTP: the client in `sccion`, and the scheduler and
//! build server behind `sccion-dist`.
//!
//! The scheduler and build server run on axum, hyper 1 and tokio, with TLS
//! through rustls. Connections are tokio tasks, TLS is rustls with the
//! aws-lc-rs provider the clients already verify with, and the work that
//! blocks -- every call into the scheduler or build server, which take
//! mutexes, unpack toolchains, run compilers and make HTTP requests of their
//! own -- runs on tokio's blocking pool. Request bodies that can be large,
//! toolchains and job inputs, are streamed into that work as they arrive
//! rather than collected first.
//!
//! They replaced servers on rouille and tiny_http, with TLS from OpenSSL, and
//! kept their protocol: the same routes, methods, URL parameters, bincode and
//! JSON bodies, bearer authentication, status codes and error bodies, so a
//! client, scheduler or build server from before the change works with one
//! from after it. Where the answers differ, the difference is written down
//! where it is made; `tests` holds the answers the protocol expects.
//!
//! Server structure and protocol notes adapted from mozilla/sccache#2464,
//! which adds an axum dist server upstream. The certificate is sccion's own,
//! from rcgen and SHA-256 signed.

#[cfg(feature = "dist-server")]
mod auth;
#[cfg(feature = "dist-client")]
mod client;
#[cfg(feature = "dist-server")]
mod handler;
#[cfg(feature = "dist-server")]
mod scheduler;
#[cfg(feature = "dist-server")]
mod serve;
#[cfg(feature = "dist-server")]
mod server;
#[cfg(all(test, feature = "dist-server"))]
mod tests;
#[cfg(feature = "dist-server")]
mod tls;

#[cfg(feature = "dist-server")]
pub use self::auth::{ClientAuthCheck, ClientVisibleMsg, ServerAuthCheck};
#[cfg(feature = "dist-client")]
pub use self::client::Client;
#[cfg(feature = "dist-server")]
pub use self::scheduler::Scheduler;
#[cfg(feature = "dist-server")]
pub use self::server::{HEARTBEAT_TIMEOUT, Server};

/// The messages on the wire, which the client and both servers share.
mod common {
    use reqwest::header;
    use serde::{Deserialize, Serialize};
    #[cfg(feature = "dist-server")]
    use {crate::util::BASE64_URL_SAFE_ENGINE, base64::Engine, std::collections::HashMap};

    use crate::dist;
    use crate::errors::*;

    // The Content-Length is for a scheduler or build server from before the
    // axum servers: tiny_http, under rouille, needs it --
    // https://github.com/tiny-http/tiny-http/issues/147
    pub trait ReqwestRequestBuilderExt: Sized {
        fn bincode<T: serde::Serialize + ?Sized>(self, bincode: &T) -> Result<Self>;
        fn bytes(self, bytes: Vec<u8>) -> Self;
    }
    impl ReqwestRequestBuilderExt for reqwest::RequestBuilder {
        fn bincode<T: serde::Serialize + ?Sized>(self, bincode: &T) -> Result<Self> {
            let bytes =
                bincode::serialize(bincode).context("Failed to serialize body to bincode")?;
            Ok(self.bytes(bytes))
        }
        fn bytes(self, bytes: Vec<u8>) -> Self {
            self.header(
                header::CONTENT_TYPE,
                mime::APPLICATION_OCTET_STREAM.to_string(),
            )
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(bytes)
        }
    }

    #[cfg(feature = "dist-client")]
    pub async fn bincode_req_fut<T: serde::de::DeserializeOwned + 'static>(
        req: reqwest::RequestBuilder,
    ) -> Result<T> {
        // Work around tiny_http issue #151 by disabling HTTP pipeline with
        // `Connection: close`: the scheduler or build server may be one from
        // before the axum servers, on rouille.
        let res = req.header(header::CONNECTION, "close").send().await?;

        let status = res.status();
        let bytes = res.bytes().await?;
        if !status.is_success() {
            let errmsg = format!(
                "Error {}: {}",
                status.as_u16(),
                String::from_utf8_lossy(&bytes)
            );
            if status.is_client_error() {
                anyhow::bail!(HttpClientError(errmsg));
            } else {
                anyhow::bail!(errmsg);
            }
        } else {
            Ok(bincode::deserialize(&bytes)?)
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub enum AllocJobHttpResponse {
        Success {
            job_alloc: dist::JobAlloc,
            need_toolchain: bool,
            cert_digest: Vec<u8>,
        },
        Fail {
            msg: String,
        },
    }
    impl AllocJobHttpResponse {
        #[cfg(feature = "dist-server")]
        pub fn from_alloc_job_result(
            res: dist::AllocJobResult,
            certs: &HashMap<dist::ServerId, (Vec<u8>, Vec<u8>)>,
        ) -> Self {
            match res {
                dist::AllocJobResult::Success {
                    job_alloc,
                    need_toolchain,
                } => {
                    if let Some((digest, _)) = certs.get(&job_alloc.server_id) {
                        AllocJobHttpResponse::Success {
                            job_alloc,
                            need_toolchain,
                            cert_digest: digest.to_owned(),
                        }
                    } else {
                        AllocJobHttpResponse::Fail {
                            msg: format!(
                                "missing certificates for server {}",
                                job_alloc.server_id.addr()
                            ),
                        }
                    }
                }
                dist::AllocJobResult::Fail { msg } => AllocJobHttpResponse::Fail { msg },
            }
        }
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct ServerCertificateHttpResponse {
        pub cert_digest: Vec<u8>,
        pub cert_pem: Vec<u8>,
    }

    #[cfg(feature = "dist-server")]
    #[derive(Clone, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct HeartbeatServerHttpRequest {
        pub jwt_key: Vec<u8>,
        pub num_cpus: usize,
        pub server_nonce: dist::ServerNonce,
        pub cert_digest: Vec<u8>,
        pub cert_pem: Vec<u8>,
    }

    #[cfg(feature = "dist-server")]
    impl std::fmt::Debug for HeartbeatServerHttpRequest {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("HeartbeatServerHttpRequest")
                .field("jwt_key", &BASE64_URL_SAFE_ENGINE.encode(&self.jwt_key))
                .field("num_cpus", &self.num_cpus)
                .field("server_nonce", &self.server_nonce)
                .field(
                    "cert_digest",
                    &BASE64_URL_SAFE_ENGINE.encode(&self.cert_digest),
                )
                .finish()
        }
    }
    #[derive(Clone, Debug, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct RunJobHttpRequest {
        pub command: dist::CompileCommand,
        pub outputs: Vec<String>,
    }
}

pub mod urls {
    use crate::dist::{JobId, ServerId};

    pub fn scheduler_alloc_job(scheduler_url: &reqwest::Url) -> reqwest::Url {
        scheduler_url
            .join("/api/v1/scheduler/alloc_job")
            .expect("failed to create alloc job url")
    }
    pub fn scheduler_server_certificate(
        scheduler_url: &reqwest::Url,
        server_id: ServerId,
    ) -> reqwest::Url {
        scheduler_url
            .join(&format!(
                "/api/v1/scheduler/server_certificate/{}",
                server_id.addr()
            ))
            .expect("failed to create server certificate url")
    }
    pub fn scheduler_heartbeat_server(scheduler_url: &reqwest::Url) -> reqwest::Url {
        scheduler_url
            .join("/api/v1/scheduler/heartbeat_server")
            .expect("failed to create heartbeat url")
    }
    pub fn scheduler_job_state(scheduler_url: &reqwest::Url, job_id: JobId) -> reqwest::Url {
        scheduler_url
            .join(&format!("/api/v1/scheduler/job_state/{}", job_id))
            .expect("failed to create job state url")
    }
    pub fn scheduler_status(scheduler_url: &reqwest::Url) -> reqwest::Url {
        scheduler_url
            .join("/api/v1/scheduler/status")
            .expect("failed to create alloc job url")
    }

    pub fn server_assign_job(server_id: ServerId, job_id: JobId) -> reqwest::Url {
        let url = format!(
            "https://{}/api/v1/distserver/assign_job/{}",
            server_id.addr(),
            job_id
        );
        reqwest::Url::parse(&url).expect("failed to create assign job url")
    }
    pub fn server_submit_toolchain(server_id: ServerId, job_id: JobId) -> reqwest::Url {
        let url = format!(
            "https://{}/api/v1/distserver/submit_toolchain/{}",
            server_id.addr(),
            job_id
        );
        reqwest::Url::parse(&url).expect("failed to create submit toolchain url")
    }
    pub fn server_run_job(server_id: ServerId, job_id: JobId) -> reqwest::Url {
        let url = format!(
            "https://{}/api/v1/distserver/run_job/{}",
            server_id.addr(),
            job_id
        );
        reqwest::Url::parse(&url).expect("failed to create run job url")
    }
}
