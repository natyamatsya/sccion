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

//! The dist scheduler and build server on axum, hyper 1 and tokio, with TLS
//! through rustls -- the `dist-server-axum` feature.
//!
//! A replacement for the rouille servers in [`super::http`], not a new
//! protocol: the same routes, methods, URL parameters, bincode and JSON
//! bodies, bearer authentication, status codes and error bodies, so a client,
//! scheduler or build server of either implementation works with one of the
//! other. The rouille handlers are the specification; where this differs, the
//! difference is written down where it is made.
//!
//! What changes is underneath. rouille ran every request on a thread of its
//! own pool, with TLS from OpenSSL through tiny_http. Here connections are
//! tokio tasks, TLS is rustls with the aws-lc-rs provider the clients already
//! verify with, and the work that blocks -- every call into the scheduler or
//! build server, which take mutexes, unpack toolchains, run compilers and make
//! HTTP requests of their own -- runs on tokio's blocking pool. Request bodies
//! that can be large, toolchains and job inputs, are streamed into that work
//! as they arrive rather than collected first.
//!
//! Structure and protocol notes adapted from mozilla/sccache#2464, which adds
//! an axum dist server upstream. The certificate is sccion's own, from rcgen
//! and SHA-256 signed, as for rouille.

mod common;
mod scheduler;
mod serve;
mod server;

pub use scheduler::Scheduler;
pub use server::Server;
