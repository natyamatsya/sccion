// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

use crate::cache::{CacheMode, GetPathResult};
use crate::compiler::ColorMode;
use crate::config::PreprocessorCacheModeConfig;
use crate::missrecord::MissRecord;
use crate::server::{DistInfo, ServerInfo, ServerStats};
use serde::{Deserialize, Serialize};

/// Bumped whenever the wire types below change shape.
///
/// It forms part of the named pipe's name, so a client and a daemon built either
/// side of a change fail to find each other rather than exchanging bytes one of
/// them will misread. bincode is not self-describing, so without this the
/// mismatch is silent misinterpretation rather than a clean error.
/// Bumped to 2 when `RegisterWorkspace` and `GetMissRecords` were added, and
/// to 3 for `DescribeWorkspace`. A daemon left running across the change will
/// not be found by the new client, which is the intended outcome: stop it with
/// `sccion --stop-server`.
///
/// Windows carries it in the pipe name, so a mismatched pair never finds each
/// other. Every other transport rendezvous on an address with no version in it
/// -- a fixed TCP port, or a socket path the user chose -- so off Windows the
/// check has to happen on the connection instead: [`Request::ProtocolVersion`]
/// is the first thing a client asks, and a daemon answering anything else is
/// refused before it is handed any real work.
///
/// That has to work against a daemon too old to know the question, which is the
/// case worth handling: a daemon is long-lived and a client is one compile, so
/// the mismatch is nearly always a new client meeting an old daemon. bincode
/// rejects an unknown enum discriminant with `InvalidTagEncoding` rather than
/// guessing at it, so an older daemon fails the request outright and the client
/// can say which remedy applies.
pub const PROTOCOL_VERSION: u32 = 4;
use std::ffi::OsString;

/// A client request.
#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    /// Zero the server's statistics.
    ZeroStats,
    /// Get server statistics.
    GetStats,
    /// Get dist status.
    DistStatus,
    /// Shut the server down gracefully.
    Shutdown,
    /// Execute a compile or fetch a cached compilation result.
    Compile(Compile),

    // --- Storage RPCs (client-side mode) ---
    /// One-shot handshake: client requests cache metadata from the daemon.
    StorageHandshake,
    /// Fetch the filesystem path of the cached entry for `key`.
    /// Returns `None` if the backend does not support direct file access or the key is absent.
    StorageGetPath { key: String },
    /// Fetch raw (zip) bytes for `key`; returns `None` on a miss.
    StorageGetRaw { key: String },
    /// Store raw (zip) bytes under `key`.
    StoragePutRaw {
        key: String,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// Retrieve the preprocessor cache entry for `key`.
    StorageGetPreprocessorEntry { key: String },
    /// Store or overwrite the preprocessor cache entry for `key`.
    StoragePutPreprocessorEntry {
        key: String,
        #[serde(with = "serde_bytes")]
        entry_bytes: Vec<u8>,
    },
    /// Merge per-invocation stats into the daemon's running totals.
    RecordStats(Box<ServerStats>),

    // --- Out-of-band queries ---
    /// Resolve a directory to the checkout it is in, and mint a handle naming
    /// it. The handle is how a later request says *which* build it is asking
    /// about without having to repeat, or be trusted with, a path.
    RegisterWorkspace { path: OsString },
    /// Recent cache misses in detail, most recent last. Scoped to one workspace
    /// when a handle is given, and to everything the daemon has seen when not.
    GetMissRecords {
        handle: Option<String>,
        limit: usize,
    },
    /// The checkout a handle names.
    ///
    /// A handle is deliberately opaque, and a caller that holds one has no
    /// other way back to a directory. Without this, any question that has to be
    /// answered *about a tree* -- what its project file says, which settings a
    /// build in it would run under -- could only be answered about the asking
    /// process's own directory, which for the MCP adapter is wherever it
    /// happened to be launched.
    DescribeWorkspace { handle: String },

    /// What protocol version is the daemon speaking?
    ///
    /// Appended last on purpose. A daemon built before this variant existed
    /// cannot decode the discriminant and fails the request, which is exactly
    /// the answer the caller needs: not a daemon it can talk to.
    ProtocolVersion,
}

/// A server response.
#[derive(Serialize, Deserialize, Debug)]
pub enum Response {
    /// Response for `Request::Compile`.
    Compile(CompileResponse),
    /// Response for `Request::ZeroStats`.
    ZeroStats,
    /// Response for `Request::GetStats`, containing server statistics.
    Stats(Box<ServerInfo>),
    /// Response for `Request::DistStatus`, containing client info.
    DistStatus(DistInfo),
    /// Response for `Request::Shutdown`, containing server statistics.
    ShuttingDown(Box<ServerInfo>),
    /// Second response for `Request::Compile`, containing the results of the compilation.
    CompileFinished(CompileFinished),

    // --- Storage RPC responses (client-side mode) ---
    /// Response for `Request::StorageHandshake`.
    StorageHandshake(StorageHandshakeInfo),
    /// Response for `Request::StorageGetPath`.
    StorageGetPath(GetPathResult),
    /// Response for `Request::StorageGetRaw`: zip bytes on hit, `None` on miss.
    StorageGetRaw(#[serde(with = "serde_bytes")] Option<Vec<u8>>),
    /// Response for `Request::StoragePutRaw`.
    StoragePutRaw(Result<(), String>),
    /// Response for `Request::StorageGetPreprocessorEntry`.
    StorageGetPreprocessorEntry(Result<Option<serde_bytes::ByteBuf>, String>),
    /// Response for `Request::StoragePutPreprocessorEntry`.
    StoragePutPreprocessorEntry(Result<(), String>),
    /// Response for `Request::RecordStats`.
    RecordStats,

    // --- Out-of-band query responses ---
    /// Response for `Request::RegisterWorkspace` and
    /// `Request::DescribeWorkspace`. One type, because both answer the same
    /// question -- which checkout is this? -- and only differ in what they are
    /// given to answer it from.
    Workspace(Result<WorkspaceInfo, String>),
    /// Response for `Request::GetMissRecords`.
    MissRecords(Result<Vec<MissRecord>, String>),

    /// Response for `Request::ProtocolVersion`.
    ProtocolVersion(u32),
}

/// A registered workspace, and the handle that names it.
///
/// The handle is a capability: holding one grants read access to that
/// workspace's build state, so it is minted by the daemon rather than derived
/// from anything a caller supplies, and it is unguessable. There are no
/// sessions to bound its lifetime -- it lives as long as the daemon does, which
/// is the same lifetime as the state it names.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub handle: String,
    /// The checkout root the path resolved to, absolute.
    pub root: String,
    /// `git`, `jj` or `hg`.
    pub vcs: String,
}

/// Possible responses from the server for a `Compile` request.
#[derive(Serialize, Deserialize, Debug)]
pub enum CompileResponse {
    /// The compilation was started.
    CompileStarted,
    /// The server could not handle this compilation request.
    UnhandledCompile,
    /// The compiler was not supported.
    UnsupportedCompiler(OsString),
}

/// Information about a finished compile, either from cache or executed locally.
#[derive(Serialize, Deserialize, Debug, Default)]
pub struct CompileFinished {
    /// The return code of the compile process, if available.
    pub retcode: Option<i32>,
    /// The signal that terminated the compile process, if available.
    pub signal: Option<i32>,
    /// The compiler's stdout.
    #[serde(with = "serde_bytes")]
    pub stdout: Vec<u8>,
    /// The compiler's stderr.
    #[serde(with = "serde_bytes")]
    pub stderr: Vec<u8>,
    /// The state of any compiler options passed to control color output.
    pub color_mode: ColorMode,
}

/// The contents of a compile request from a client.
#[derive(Serialize, Deserialize, Debug)]
pub struct Compile {
    /// The full path to the compiler executable.
    pub exe: OsString,
    /// The current working directory in which to execute the compile.
    pub cwd: OsString,
    /// The commandline arguments passed to the compiler.
    pub args: Vec<OsString>,
    /// The environment variables present when the compiler was executed, as (var, val).
    pub env_vars: Vec<(OsString, OsString)>,
}

/// Cache metadata returned by the daemon on `StorageHandshake`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StorageHandshakeInfo {
    pub location: String,
    pub cache_type_name: String,
    pub basedirs: Vec<Vec<u8>>,
    pub preprocessor_cache_mode_config: PreprocessorCacheModeConfig,
    pub cache_mode: CacheMode,
    pub max_size: Option<u64>,
}

/// The wire format, in bytes.
///
/// `serde_bytes` on the blob-carrying fields is an optimisation and has to stay
/// one: bincode's `serialize_bytes` writes the same length prefix followed by
/// the same payload that a sequence of `u8` produces, so annotating a field
/// changes how long the encoding takes and not what it says. That is only true
/// while it is true, and nothing else would notice if it stopped -- a client
/// and a daemon that disagree here exchange plausible garbage rather than
/// failing, which is why the protocol version is in the pipe name to begin
/// with.
///
/// These bytes were recorded before the annotations went on.
#[cfg(test)]
mod wire_format {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn the_blob_carrying_messages_encode_as_they_always_did() {
        let put = bincode::serialize(&Request::StoragePutRaw {
            key: "abc".into(),
            data: vec![0u8, 1, 2, 250, 255],
        })
        .unwrap();
        assert_eq!(
            hex(&put),
            "0800000003000000000000006162630500000000000000000102faff",
            "StoragePutRaw"
        );

        let hit = bincode::serialize(&Response::StorageGetRaw(Some(vec![9u8, 8, 7]))).unwrap();
        assert_eq!(
            hex(&hit),
            "08000000010300000000000000090807",
            "StorageGetRaw hit"
        );

        let miss = bincode::serialize(&Response::StorageGetRaw(None)).unwrap();
        assert_eq!(
            hex(&miss),
            "0800000000",
            "StorageGetRaw miss -- the None arm is not a byte string at all"
        );

        let pre = bincode::serialize(&Response::StorageGetPreprocessorEntry(Ok(Some(
            serde_bytes::ByteBuf::from(vec![4u8, 5, 6]),
        ))))
        .unwrap();
        assert_eq!(
            hex(&pre),
            "0a00000000000000010300000000000000040506",
            "StorageGetPreprocessorEntry hit -- ByteBuf must match the Vec it replaced"
        );

        let err = bincode::serialize(&Response::StorageGetPreprocessorEntry(Err(
            "boom".to_string()
        )))
        .unwrap();
        assert_eq!(
            hex(&err),
            "0a000000010000000400000000000000626f6f6d",
            "StorageGetPreprocessorEntry error"
        );
    }

    /// And they still decode to what went in.
    #[test]
    fn the_blob_carrying_messages_round_trip() {
        let data = vec![0u8, 1, 2, 250, 255];
        let bytes = bincode::serialize(&Request::StoragePutRaw {
            key: "abc".into(),
            data: data.clone(),
        })
        .unwrap();
        match bincode::deserialize(&bytes).unwrap() {
            Request::StoragePutRaw { key, data: got } => {
                assert_eq!(key, "abc");
                assert_eq!(got, data);
            }
            other => panic!("decoded as {other:?}"),
        }
    }
}
