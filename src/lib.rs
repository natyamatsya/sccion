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

#![deny(rust_2018_idioms)]
#![allow(
    clippy::type_complexity,
    clippy::new_without_default,
    clippy::blocks_in_conditions
)]
#![recursion_limit = "256"]

#[macro_use]
extern crate log;
// To get macros in scope, this has to be first.
#[cfg(test)]
#[macro_use]
mod test;

#[macro_use]
pub mod errors;

pub mod agent;
pub mod api;
pub mod blockclone;
pub mod cache;
mod checkenv;
mod client;
mod cmdline;
mod commands;
mod compiler;
pub mod config;
pub mod dist;
pub mod envvar;
pub mod init;
mod jobserver;
#[cfg(windows)]
pub mod pipe;

pub mod lru_disk_cache;
pub mod missrecord;
mod mock_command;
mod net;
pub mod projectconf;
mod protocol;
pub mod server;
#[doc(hidden)]
pub mod util;
pub mod verify;
pub mod volume;

use std::env;

/// VERSION is the pkg version of sccion.
///
/// This version is safe to be used in cache services to indicate the version
/// that sccion ie.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Used to denote the environment variable that controls
/// logging for sccion, and sccion-dist.
pub const LOGGING_ENV: &str = "SCCION_LOG";

pub fn main() {
    init_logging();

    let command = match cmdline::try_parse() {
        Ok(cmd) => cmd,
        Err(e) => match e.downcast::<clap::error::Error>() {
            // If the error is from clap then let them handle formatting and exiting
            Ok(clap_err) => clap_err.exit(),
            // To stderr: started as a compiler, sccion's stdout is the
            // compiler's, where a build may expect generated output.
            Err(some_other_err) => {
                eprintln!("sccion: {some_other_err}");
                for source in some_other_err.chain().skip(1) {
                    eprintln!("sccion: caused by: {source}");
                }
                std::process::exit(1);
            }
        },
    };

    std::process::exit(match commands::run_command(command) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("sccion: error: {}", e);
            for e in e.chain().skip(1) {
                eprintln!("sccion: caused by: {}", e);
            }
            2
        }
    });
}

fn init_logging() {
    if crate::envvar::var(LOGGING_ENV).is_some() {
        let mut builder = env_logger::Builder::from_env(crate::envvar::logging_var());

        // Enable millisecond precision timestamps if SCCACHE_LOG_MILLIS is set
        if crate::envvar::var("SCCION_LOG_MILLIS").is_some() {
            builder.format_timestamp_millis();
        }

        match builder.try_init() {
            Ok(_) => (),
            Err(e) => panic!("Failed to initialize logging: {:?}", e),
        }
    }
}
