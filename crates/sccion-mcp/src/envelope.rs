// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! Everything the protocol revision requires of a message, in one file.
//!
//! MCP revises on roughly a quarterly cadence with date-based versioning, and
//! the 2026-07-28 revision was a substantial break. Keeping every rule it
//! imposes in one place -- rather than scattered through the tool handlers --
//! is what makes the next revision a diff to this file instead of an audit of
//! the whole adapter.
//!
//! The rules, and why each is here rather than assumed:
//!
//! 1. **There is no `initialize` handshake.** It was removed, along with
//!    protocol-level sessions. Every request carries its own
//!    `protocolVersion`, `clientCapabilities` and (SHOULD) `clientInfo` in
//!    `_meta`; every result carries `serverInfo`. There is no connection setup
//!    phase at all, which for this adapter is a good fit rather than a loss:
//!    the sccion daemon is the state, and the adapter is a translator that can
//!    be restarted between any two calls without losing anything.
//! 2. **A version mismatch is an error, not a negotiation.**
//! 3. **Every result carries a `resultType`.** Everything here is `complete`;
//!    `input_required` belongs to flows that need more input mid-request, and
//!    this adapter deliberately has none.
//! 4. **`ttlMs` and `cacheScope` are required** on the list and read results.
//!    `cacheScope` is a correctness matter, not a hint: anything keyed by a
//!    workspace handle must be `private`, or one caller's build state is
//!    cached into another's context.
//! 5. **Errors stay in `-32000`..=`-32019`.** `-32020`..=`-32099` is reserved
//!    for the specification, so nothing here may use it.
//! 6. **No logging, ping, or subscription methods.** `logging/setLevel` and
//!    `ping` were removed and Logging is deprecated; the migration is stderr,
//!    which is where this adapter logs. Subscriptions were replaced by a single
//!    long-lived stream that would reintroduce exactly the per-connection state
//!    the revision removed.

use serde::Deserialize;
use serde_json::{Value, json};

/// The revision this adapter speaks.
pub const PROTOCOL_VERSION: &str = "2026-07-28";

/// Who is answering. Carried on every result, since there is no handshake in
/// which to say it once.
pub fn server_info() -> Value {
    json!({
        "name": "sccion",
        "title": "sccion build cache",
        "version": env!("CARGO_PKG_VERSION"),
    })
}

// --- Error codes -----------------------------------------------------------
//
// `-32000`..=`-32019` is implementation-defined and `-32020`..=`-32099` is
// reserved for the specification, so every code below is in the low range.

/// The caller asked about a workspace handle this daemon did not mint.
pub const ERR_UNKNOWN_HANDLE: i64 = -32001;
/// No sccion daemon is running, or it could not be reached.
pub const ERR_DAEMON_UNAVAILABLE: i64 = -32002;
/// The caller's protocol version is not one this adapter speaks.
///
/// The specification names this `UnsupportedProtocolVersionError`. **Its
/// spec-assigned numeric code is the one thing in this file not verified
/// against the published revision**, and it would live in the reserved range.
/// An implementation-defined code is used deliberately until it can be
/// checked: a wrong guess here can never collide with a specification
/// assignment, whereas an invented number in the reserved range could.
pub const ERR_UNSUPPORTED_PROTOCOL_VERSION: i64 = -32003;
/// A tool or resource that does not exist. Resource-not-found moved to
/// `-32602` in this revision; it is no longer `-32002`.
pub const ERR_NOT_FOUND: i64 = -32602;
/// Malformed arguments.
pub const ERR_INVALID_PARAMS: i64 = -32602;

/// How long a result may be reused, and by whom.
///
/// `Private` is not a performance hint. A result keyed by a workspace handle
/// describes one caller's build, and marking it shareable would let it be
/// served into another caller's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheScope {
    /// Safe to reuse for anyone: the same answer for every caller.
    Shared,
    /// Reusable only for the caller that asked.
    Private,
}

impl CacheScope {
    fn as_str(self) -> &'static str {
        match self {
            CacheScope::Shared => "shared",
            CacheScope::Private => "private",
        }
    }
}

/// A successful result, with everything the revision requires attached.
pub fn result(mut body: Value, cache: Option<(u64, CacheScope)>) -> Value {
    let obj = body.as_object_mut().expect("a result is an object");
    // Required on every result.
    obj.insert("resultType".into(), json!("complete"));
    obj.insert("serverInfo".into(), server_info());
    if let Some((ttl_ms, scope)) = cache {
        obj.insert("ttlMs".into(), json!(ttl_ms));
        obj.insert("cacheScope".into(), json!(scope.as_str()));
    }
    body
}

/// What a caller sent alongside a request.
///
/// `clientInfo` also rides on every request in this revision, which makes MCP
/// *queries* self-identifying for free. It is not read here, and the omission
/// is deliberate rather than an oversight: compilations never touch MCP, so
/// per-caller attribution of the thing that actually costs time is
/// `SCCACHE_AGENT_ID`'s job and cannot be done from this side.
#[derive(Debug, Default, Deserialize)]
pub struct RequestMeta {
    #[serde(rename = "io.modelcontextprotocol/protocolVersion")]
    pub protocol_version: Option<String>,
}

/// Check the caller's protocol version.
///
/// Absent is accepted: the version is carried per request precisely so that a
/// client which has not been updated still gets an answer rather than a wall,
/// and on stdio `server/discover` is how it finds out what is on offer.
pub fn check_protocol_version(meta: &RequestMeta) -> Result<(), (i64, String)> {
    match meta.protocol_version.as_deref() {
        None | Some(PROTOCOL_VERSION) => Ok(()),
        Some(other) => Err((
            ERR_UNSUPPORTED_PROTOCOL_VERSION,
            format!(
                "unsupported protocol version {other:?}; this server speaks {PROTOCOL_VERSION}"
            ),
        )),
    }
}

/// A JSON-RPC response carrying `body` as its result.
pub fn ok(id: Value, body: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": body})
}

/// A JSON-RPC error response.
pub fn err(id: Value, code: i64, message: impl Into<String>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message.into(),
            // Even a failure has to say who produced it: there is no handshake
            // in which the client learned it.
            "data": {"serverInfo": server_info()},
        },
    })
}

/// A tool result, as a `tools/call` reply.
///
/// Tool output is data, never instruction. `explain_miss` returns file paths and
/// header names that originate in a repository the caller -- or another caller
/// -- may have written, so it is handed over as structured content rather than
/// as prose to be read as directions.
pub fn tool_result(structured: Value, scope: CacheScope, ttl_ms: u64) -> Value {
    result(
        json!({
            "content": [{
                "type": "text",
                "text": serde_json::to_string_pretty(&structured).unwrap_or_default(),
            }],
            "structuredContent": structured,
            "isError": false,
        }),
        Some((ttl_ms, scope)),
    )
}
