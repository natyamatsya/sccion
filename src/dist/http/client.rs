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

//! The dist client: what `sccion` asks of a scheduler and its build servers.

use crate::config;
use crate::dist::cache;
use crate::dist::pkg::{InputsPackager, ToolchainPackager};
use crate::dist::{
    self, AllocJobResult, CompileCommand, JobAlloc, PathTransformer, RunJobResult,
    SchedulerStatusResult, SubmitToolchainResult, Toolchain,
};

use async_trait::async_trait;
use byteorder::{BigEndian, WriteBytesExt};
use flate2::Compression;
use flate2::write::ZlibEncoder as ZlibWriteEncoder;
use futures::TryFutureExt;
use reqwest::Body;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::common::{
    AllocJobHttpResponse, ReqwestRequestBuilderExt, RunJobHttpRequest,
    ServerCertificateHttpResponse, bincode_req_fut,
};
use super::urls;
use crate::errors::*;

const REQUEST_TIMEOUT_SECS: u64 = 1200;
const CONNECT_TIMEOUT_SECS: u64 = 5;

pub struct Client {
    auth_token: String,
    scheduler_url: reqwest::Url,
    // cert_digest -> cert_pem
    server_certs: Arc<Mutex<HashMap<Vec<u8>, Vec<u8>>>>,
    client: Arc<Mutex<reqwest::Client>>,
    pool: tokio::runtime::Handle,
    tc_cache: Arc<cache::ClientToolchains>,
    rewrite_includes_only: bool,
}

impl Client {
    pub fn new(
        pool: &tokio::runtime::Handle,
        scheduler_url: reqwest::Url,
        cache_dir: &Path,
        cache_size: u64,
        toolchain_configs: &[config::DistToolchainConfig],
        auth_token: String,
        rewrite_includes_only: bool,
    ) -> Result<Self> {
        let timeout = Duration::new(REQUEST_TIMEOUT_SECS, 0);
        let connect_timeout = Duration::new(CONNECT_TIMEOUT_SECS, 0);
        let client = reqwest::ClientBuilder::new()
            .timeout(timeout)
            .connect_timeout(connect_timeout)
            // Disable connection pool to avoid broken connection
            // between runtime
            .pool_max_idle_per_host(0)
            .tls_certs_merge(crate::util::bundled_root_certs())
            .build()
            .context("failed to create an async HTTP client")?;
        let client_toolchains =
            cache::ClientToolchains::new(cache_dir, cache_size, toolchain_configs)
                .context("failed to initialise client toolchains")?;
        Ok(Self {
            auth_token,
            scheduler_url,
            server_certs: Default::default(),
            client: Arc::new(Mutex::new(client)),
            pool: pool.clone(),
            tc_cache: Arc::new(client_toolchains),
            rewrite_includes_only,
        })
    }

    fn update_certs(
        client: &mut reqwest::Client,
        certs: &mut HashMap<Vec<u8>, Vec<u8>>,
        cert_digest: Vec<u8>,
        cert_pem: Vec<u8>,
    ) -> Result<()> {
        let mut client_async_builder = reqwest::ClientBuilder::new();
        // Add all the certificates we know about
        client_async_builder = client_async_builder.add_root_certificate(
            reqwest::Certificate::from_pem(&cert_pem)
                .context("failed to interpret pem as certificate")?,
        );
        for cert_pem in certs.values() {
            client_async_builder = client_async_builder.add_root_certificate(
                reqwest::Certificate::from_pem(cert_pem).expect("previously valid cert"),
            );
        }
        // Finish the client
        let timeout = Duration::new(REQUEST_TIMEOUT_SECS, 0);
        let new_client_async = client_async_builder
            .timeout(timeout)
            // Disable keep-alive
            .pool_max_idle_per_host(0)
            .build()
            .context("failed to create an async HTTP client")?;
        // Use the updated certificates
        *client = new_client_async;
        certs.insert(cert_digest, cert_pem);
        Ok(())
    }
}

#[async_trait]
impl dist::Client for Client {
    async fn do_alloc_job(&self, tc: Toolchain) -> Result<AllocJobResult> {
        let scheduler_url = self.scheduler_url.clone();
        let url = urls::scheduler_alloc_job(&scheduler_url);
        let mut req = self.client.lock().unwrap().post(url);
        req = req.bearer_auth(self.auth_token.clone()).bincode(&tc)?;

        let client = self.client.clone();
        let server_certs = self.server_certs.clone();

        match bincode_req_fut(req).await? {
            AllocJobHttpResponse::Success {
                job_alloc,
                need_toolchain,
                cert_digest,
            } => {
                let server_id = job_alloc.server_id;
                let alloc_job_res = Ok(AllocJobResult::Success {
                    job_alloc,
                    need_toolchain,
                });
                if server_certs.lock().unwrap().contains_key(&cert_digest) {
                    return alloc_job_res;
                }
                info!(
                    "Need to request new certificate for server {}",
                    server_id.addr()
                );
                let url = urls::scheduler_server_certificate(&scheduler_url, server_id);
                let req = client.lock().unwrap().get(url);
                let res: ServerCertificateHttpResponse = bincode_req_fut(req)
                    .await
                    .context("GET to scheduler server_certificate failed")?;

                // TODO: Move to asynchronous reqwest client only.
                // This function internally builds a blocking reqwest client;
                // However, it does so by utilizing a runtime which it drops,
                // triggering (rightfully) a sanity check that prevents from
                // dropping a runtime in asynchronous context.
                // For the time being, we work around this by off-loading it
                // to a dedicated blocking-friendly thread pool.
                let _ = self
                    .pool
                    .spawn_blocking(move || {
                        Self::update_certs(
                            &mut client.lock().unwrap(),
                            &mut server_certs.lock().unwrap(),
                            res.cert_digest,
                            res.cert_pem,
                        )
                        .context("Failed to update certificate")
                        .unwrap_or_else(|e| warn!("Failed to update certificate: {:?}", e));
                    })
                    .await;

                alloc_job_res
            }
            AllocJobHttpResponse::Fail { msg } => Ok(AllocJobResult::Fail { msg }),
        }
    }

    async fn do_get_status(&self) -> Result<SchedulerStatusResult> {
        let scheduler_url = self.scheduler_url.clone();
        let url = urls::scheduler_status(&scheduler_url);
        let req = self.client.lock().unwrap().get(url);
        bincode_req_fut(req).await
    }

    async fn do_submit_toolchain(
        &self,
        job_alloc: JobAlloc,
        tc: Toolchain,
    ) -> Result<SubmitToolchainResult> {
        match self.tc_cache.get_toolchain(&tc) {
            Ok(Some(toolchain_file)) => {
                let url = urls::server_submit_toolchain(job_alloc.server_id, job_alloc.job_id);
                let req = self.client.lock().unwrap().post(url);
                let toolchain_file = tokio::fs::File::from_std(toolchain_file.into());
                let toolchain_file_stream = tokio_util::io::ReaderStream::new(toolchain_file);
                let body = Body::wrap_stream(toolchain_file_stream);
                let req = req.bearer_auth(job_alloc.auth).body(body);
                bincode_req_fut(req).await
            }
            Ok(None) => Err(anyhow!("couldn't find toolchain locally")),
            Err(e) => Err(e),
        }
    }

    async fn do_run_job(
        &self,
        job_alloc: JobAlloc,
        command: CompileCommand,
        outputs: Vec<String>,
        inputs_packager: Box<dyn InputsPackager>,
    ) -> Result<(RunJobResult, PathTransformer)> {
        let url = urls::server_run_job(job_alloc.server_id, job_alloc.job_id);

        let (body, path_transformer) = self
            .pool
            .spawn_blocking(move || -> Result<_> {
                let bincode = bincode::serialize(&RunJobHttpRequest { command, outputs })
                    .context("failed to serialize run job request")?;
                let bincode_length = bincode.len();

                let mut body = vec![];
                body.write_u32::<BigEndian>(bincode_length as u32)
                    .expect("Infallible write of bincode length to vec failed");
                body.write_all(&bincode)
                    .expect("Infallible write of bincode body to vec failed");
                let path_transformer;
                {
                    let mut compressor = ZlibWriteEncoder::new(&mut body, Compression::fast());
                    path_transformer = inputs_packager
                        .write_inputs(&mut compressor)
                        .context("Could not write inputs for compilation")?;
                    compressor.flush().context("failed to flush compressor")?;
                    trace!(
                        "Compressed inputs from {} -> {}",
                        compressor.total_in(),
                        compressor.total_out()
                    );
                    compressor.finish().context("failed to finish compressor")?;
                }

                Ok((body, path_transformer))
            })
            .await??;
        let mut req = self.client.lock().unwrap().post(url);
        req = req.bearer_auth(job_alloc.auth.clone()).bytes(body);
        bincode_req_fut(req)
            .map_ok(|res| (res, path_transformer))
            .await
    }

    async fn put_toolchain(
        &self,
        compiler_path: PathBuf,
        weak_key: String,
        toolchain_packager: Box<dyn ToolchainPackager>,
    ) -> Result<(Toolchain, Option<(String, PathBuf)>)> {
        let compiler_path = compiler_path.clone();
        let weak_key = weak_key.clone();
        let tc_cache = self.tc_cache.clone();

        self.pool
            .spawn_blocking(move || {
                tc_cache.put_toolchain(&compiler_path, &weak_key, toolchain_packager)
            })
            .await?
    }

    fn rewrite_includes_only(&self) -> bool {
        self.rewrite_includes_only
    }
    fn get_custom_toolchain(&self, exe: &Path) -> Option<PathBuf> {
        match self.tc_cache.get_custom_toolchain(exe) {
            Some(Ok((_, _, path))) => Some(path),
            _ => None,
        }
    }
}
