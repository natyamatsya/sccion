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
/// Bumped to 2 when `RegisterWorkspace` and `GetMissRecords` were added. A
/// daemon left running across the change will not be found by the new client,
/// which is the intended outcome: stop it with `sccache --stop-server`.
pub const PROTOCOL_VERSION: u32 = 2;
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
    StoragePutRaw { key: String, data: Vec<u8> },
    /// Retrieve the preprocessor cache entry for `key`.
    StorageGetPreprocessorEntry { key: String },
    /// Store or overwrite the preprocessor cache entry for `key`.
    StoragePutPreprocessorEntry { key: String, entry_bytes: Vec<u8> },
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
    StorageGetRaw(Option<Vec<u8>>),
    /// Response for `Request::StoragePutRaw`.
    StoragePutRaw(Result<(), String>),
    /// Response for `Request::StorageGetPreprocessorEntry`.
    StorageGetPreprocessorEntry(Result<Option<Vec<u8>>, String>),
    /// Response for `Request::StoragePutPreprocessorEntry`.
    StoragePutPreprocessorEntry(Result<(), String>),
    /// Response for `Request::RecordStats`.
    RecordStats,

    // --- Out-of-band query responses ---
    /// Response for `Request::RegisterWorkspace`.
    Workspace(Result<WorkspaceInfo, String>),
    /// Response for `Request::GetMissRecords`.
    MissRecords(Result<Vec<MissRecord>, String>),
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
    pub stdout: Vec<u8>,
    /// The compiler's stderr.
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
