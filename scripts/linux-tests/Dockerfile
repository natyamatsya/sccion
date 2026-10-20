# SPDX-License-Identifier: Apache-2.0
#
# The Linux environment `run.sh` tests in: the oldest supported Rust (keep in
# sync with README.md), a Docker daemon for the dist tests' containers, and
# the C/C++ compilers the integration tests drive.
FROM rust:1.91.0-bookworm
RUN apt-get update \
 && apt-get install -y --no-install-recommends docker.io iptables clang pkg-config \
 && rm -rf /var/lib/apt/lists/*
