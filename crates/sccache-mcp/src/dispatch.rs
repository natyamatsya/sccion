// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! One request in, one response out.
//!
//! Every tool here is read-only. An agent must not be able to evict entries,
//! change configuration, or trigger a compilation with a tool call, and the one
//! mutating tool the design contemplates -- promoting entries between cache
//! tiers -- is deliberately absent until the trust boundary that would make it
//! safe exists.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::envelope::{
    self, CacheScope, ERR_DAEMON_UNAVAILABLE, ERR_INVALID_PARAMS, ERR_NOT_FOUND,
    ERR_UNKNOWN_HANDLE, PROTOCOL_VERSION, RequestMeta,
};

/// Volatile: a build changes these every few milliseconds.
const TTL_STATS: u64 = 2_000;
/// The environment report describes hardware, filesystems and antivirus
/// configuration, none of which changes during a build.
const TTL_ENVIRONMENT: u64 = 300_000;

/// The tool surface, in the order `tools/list` reports it.
///
/// Sorted by name, and asserted to be: the revision says a client may cache the
/// list, and a `HashMap`'s idea of order would invalidate that cache on every
/// call for no reason -- and defeat prompt caching downstream.
const TOOLS: &[(&str, &str)] = &[
    (
        "check_environment",
        "Report the environment sccache is running in: cache location and size, \
         filesystem, preprocessor cache settings, and any findings that would \
         invalidate a measurement. Reports only; changes nothing.",
    ),
    (
        "explain_miss",
        "Explain why recent compilations were not served from cache, for one \
         workspace. Says whether each miss was cold or permanently uncacheable, \
         which is the difference between retrying and giving up.",
    ),
    (
        "get_cache_health",
        "Cache size against its limit, and how much of the cache the daemon has \
         served, for one workspace.",
    ),
    (
        "get_stats",
        "Cache hit and miss counts, either machine-wide or broken down per \
         caller.",
    ),
    (
        "register_workspace",
        "Resolve a directory to the checkout it is in and return a handle naming \
         it. Every other tool takes that handle.",
    ),
];

pub fn handle(request: &Value, backend: &dyn Backend) -> Option<Value> {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(Value::as_str).unwrap_or("");
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));

    // A notification has no id and takes no response. Since there is no
    // `notifications/initialized` any more, the only ones that arrive are from
    // clients that have not caught up; they are dropped rather than answered.
    let id = id?;

    let meta: RequestMeta = params
        .get("_meta")
        .cloned()
        .and_then(|m| serde_json::from_value(m).ok())
        .unwrap_or_default();

    // `server/discover` is exempt: it is how a client that does not yet know
    // what this server speaks finds out, so refusing it over a version
    // mismatch would make the mismatch unrecoverable.
    if method != "server/discover"
        && let Err((code, message)) = envelope::check_protocol_version(&meta)
    {
        return Some(envelope::err(id, code, message));
    }

    let response = match method {
        "server/discover" => Ok(discover()),
        "tools/list" => Ok(tools_list()),
        "resources/list" => Ok(resources_list()),
        "resources/templates/list" => Ok(resource_templates_list()),
        "resources/read" => resources_read(&params, backend),
        "tools/call" => tools_call(&params, backend),

        // Removed or deprecated in this revision, and deliberately not
        // implemented. Answering them would be inventing a protocol.
        //
        // - `initialize` / `notifications/initialized`: the handshake is gone.
        // - `ping`, `logging/setLevel`: removed. sccache logs to stderr, and
        //   `SCCACHE_LOG` / `SCCACHE_MISS_LOG` are the logging surface.
        // - `resources/subscribe`, `subscriptions/listen`: a long-lived stream
        //   would reintroduce the per-connection state this revision removed.
        //   `ttlMs` on `resources/read` makes polling adequate.
        _ => Err((-32601, format!("method not found: {method}"))),
    };

    Some(match response {
        Ok(body) => envelope::ok(id, body),
        Err((code, message)) => envelope::err(id, code, message),
    })
}

/// The one endpoint that cannot be skipped: what this server is and speaks.
fn discover() -> Value {
    envelope::result(
        json!({
            "serverInfo": envelope::server_info(),
            "protocolVersions": [PROTOCOL_VERSION],
            "capabilities": {
                "tools": {"listChanged": false},
                "resources": {"listChanged": false},
                // Named explicitly as absent rather than omitted, because their
                // absence is a decision: logging is stderr, and subscriptions
                // would reintroduce per-connection state.
                "logging": null,
                "subscriptions": null,
            },
        }),
        Some((TTL_ENVIRONMENT, CacheScope::Shared)),
    )
}

fn tools_list() -> Value {
    let tools: Vec<Value> = TOOLS
        .iter()
        .map(|(name, description)| {
            json!({
                "name": name,
                "description": description,
                "inputSchema": input_schema(name),
                // Every tool here reads. Saying so is not decoration: it is the
                // property that makes this surface safe to expose to an agent.
                "annotations": {"readOnlyHint": true, "destructiveHint": false},
            })
        })
        .collect();
    // The list is the same for everyone and changes only when the binary does.
    envelope::result(json!({"tools": tools}), Some((TTL_ENVIRONMENT, CacheScope::Shared)))
}

fn input_schema(name: &str) -> Value {
    let handle = json!({"type": "string", "description": "From register_workspace."});
    match name {
        "register_workspace" => json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "Any directory inside the checkout."}},
            "required": ["path"],
        }),
        "get_stats" => json!({
            "type": "object",
            "properties": {
                "handle": handle,
                "scope": {"type": "string", "enum": ["agent", "machine"], "default": "machine"},
            },
            "required": ["handle"],
        }),
        "explain_miss" => json!({
            "type": "object",
            "properties": {
                "handle": handle,
                "output": {"type": "string", "description": "Only misses whose output or input contains this."},
                "limit": {"type": "integer", "default": 50},
            },
            "required": ["handle"],
        }),
        "get_cache_health" => json!({
            "type": "object",
            "properties": {"handle": handle},
            "required": ["handle"],
        }),
        _ => json!({"type": "object", "properties": {}}),
    }
}

fn resources_list() -> Value {
    envelope::result(
        json!({"resources": [{
            "uri": "sccache://environment",
            "name": "environment",
            "title": "sccache environment report",
            "mimeType": "application/json",
        }]}),
        Some((TTL_ENVIRONMENT, CacheScope::Shared)),
    )
}

fn resource_templates_list() -> Value {
    envelope::result(
        json!({"resourceTemplates": [{
            "uriTemplate": "sccache://stats/{handle}",
            "name": "stats",
            "title": "sccache statistics for one workspace",
            "mimeType": "application/json",
        }]}),
        Some((TTL_ENVIRONMENT, CacheScope::Shared)),
    )
}

fn resources_read(params: &Value, backend: &dyn Backend) -> Result<Value, (i64, String)> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or((ERR_INVALID_PARAMS, "resources/read needs a uri".to_string()))?;

    let (body, scope, ttl) = if uri == "sccache://environment" {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        (
            serde_json::to_value(backend.environment(&cwd)).unwrap_or_default(),
            CacheScope::Shared,
            TTL_ENVIRONMENT,
        )
    } else if let Some(handle) = uri.strip_prefix("sccache://stats/") {
        let stats = stats_for(handle, backend)?;
        // Keyed by a handle, so it describes one caller's build and must never
        // be reused for another.
        (stats, CacheScope::Private, TTL_STATS)
    } else {
        return Err((ERR_NOT_FOUND, format!("no such resource: {uri}")));
    };

    Ok(envelope::result(
        json!({"contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": serde_json::to_string_pretty(&body).unwrap_or_default(),
        }]}),
        Some((ttl, scope)),
    ))
}

fn tools_call(params: &Value, backend: &dyn Backend) -> Result<Value, (i64, String)> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or((ERR_INVALID_PARAMS, "tools/call needs a name".to_string()))?;
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

    let handle = || -> Result<String, (i64, String)> {
        args.get("handle")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or((
                ERR_INVALID_PARAMS,
                format!("{name} needs a handle from register_workspace"),
            ))
    };

    match name {
        "register_workspace" => {
            let path = args.get("path").and_then(Value::as_str).ok_or((
                ERR_INVALID_PARAMS,
                "register_workspace needs a path".to_string(),
            ))?;
            let info = backend
                .register_workspace(Path::new(path))
                .map_err(unavailable)?;
            // The handle is a capability, so its result belongs to the caller
            // that asked for it and to nobody else.
            Ok(envelope::tool_result(
                json!({"handle": info.handle, "root": info.root, "vcs": info.vcs}),
                CacheScope::Private,
                TTL_STATS,
            ))
        }
        "get_stats" => {
            let body = stats_for(&handle()?, backend)?;
            Ok(envelope::tool_result(body, CacheScope::Private, TTL_STATS))
        }
        "explain_miss" => {
            let handle = handle()?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(50)
                .min(1000) as usize;
            let needle = args.get("output").and_then(Value::as_str);
            let records = backend
                .miss_records(Some(&handle), limit)
                .map_err(handle_error)?;
            let records: Vec<_> = records
                .into_iter()
                .filter(|r| match needle {
                    None => true,
                    Some(n) => {
                        r.output.contains(n) || r.input.as_deref().is_some_and(|i| i.contains(n))
                    }
                })
                .collect();
            Ok(envelope::tool_result(
                json!({
                    "misses": serde_json::to_value(&records).unwrap_or_default(),
                    "note": "`uncacheable` means retrying will reach the same verdict; \
                             every other classification may succeed once the cache warms.",
                }),
                CacheScope::Private,
                TTL_STATS,
            ))
        }
        "get_cache_health" => {
            let _ = handle()?;
            let info = backend.stats().map_err(unavailable)?;
            Ok(envelope::tool_result(
                json!({
                    "cacheLocation": info.cache_location,
                    "cacheSize": info.cache_size,
                    "maxCacheSize": info.max_cache_size,
                    "usePreprocessorCacheMode": info.use_preprocessor_cache_mode,
                    "version": info.version,
                }),
                CacheScope::Private,
                TTL_STATS,
            ))
        }
        "check_environment" => {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            let report = serde_json::to_value(backend.environment(&cwd)).unwrap_or_default();
            // The same answer for every caller, and stable for as long as the
            // machine is.
            Ok(envelope::tool_result(
                report,
                CacheScope::Shared,
                TTL_ENVIRONMENT,
            ))
        }
        _ => Err((ERR_NOT_FOUND, format!("no such tool: {name}"))),
    }
}

fn stats_for(handle: &str, backend: &dyn Backend) -> Result<Value, (i64, String)> {
    // Asking for one record proves the handle is one this daemon minted, before
    // any of its statistics are handed over.
    backend.miss_records(Some(handle), 1).map_err(handle_error)?;
    let info = backend.stats().map_err(unavailable)?;
    Ok(json!({
        "cacheHits": info.stats.cache_hits.all(),
        "cacheMisses": info.stats.cache_misses.all(),
        "compilations": info.stats.compilations,
        "nonCacheableCompilations": info.stats.non_cacheable_compilations,
        "perAgent": serde_json::to_value(&info.stats.per_agent).unwrap_or_default(),
    }))
}

/// A daemon that refused says so; one that was never reached does not.
///
/// Reporting "no sccache daemon answered" for a daemon that answered perfectly
/// clearly sends the caller looking for a process that is running fine.
fn unavailable(e: anyhow::Error) -> (i64, String) {
    match e.downcast_ref::<sccache::api::Refused>() {
        Some(refused) => (ERR_INVALID_PARAMS, refused.to_string()),
        None => (
            ERR_DAEMON_UNAVAILABLE,
            format!("no sccache daemon answered: {e}"),
        ),
    }
}

fn handle_error(e: anyhow::Error) -> (i64, String) {
    if e.to_string().contains("unknown workspace handle") {
        (ERR_UNKNOWN_HANDLE, e.to_string())
    } else {
        unavailable(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sccache::api::{Classification, DirectState, MissRecord, ServerInfo, WorkspaceInfo};

    /// A daemon that is not there, so the dispatcher can be checked on its own.
    struct Fake;

    impl Backend for Fake {
        fn register_workspace(&self, path: &Path) -> anyhow::Result<WorkspaceInfo> {
            Ok(WorkspaceInfo {
                handle: "d34db33f".repeat(4),
                root: path.to_string_lossy().into_owned(),
                vcs: "git".into(),
            })
        }
        fn stats(&self) -> anyhow::Result<ServerInfo> {
            Ok(ServerInfo {
                stats: Default::default(),
                cache_location: "D:/cache".into(),
                cache_size: Some(1),
                max_cache_size: Some(2),
                use_preprocessor_cache_mode: true,
                version: "test".into(),
                basedirs: vec![],
            })
        }
        fn miss_records(
            &self,
            handle: Option<&str>,
            _limit: usize,
        ) -> anyhow::Result<Vec<MissRecord>> {
            if handle == Some("nope") {
                anyhow::bail!("unknown workspace handle");
            }
            Ok(vec![MissRecord {
                seq: 1,
                classification: Classification::Uncacheable("time_macros".into()),
                direct: DirectState::Off,
                key: None,
                output: "foo.obj".into(),
                input: Some("src/foo.c".into()),
                language: Some("C/C++".into()),
                workspace: Some("D:/wt".into()),
                agent: Some("agent-7".into()),
                command_digest: Some("abc".into()),
            }])
        }
        fn environment(&self, _cwd: &Path) -> sccache::api::EnvReport {
            unimplemented!("not reached by these tests")
        }
    }

    fn call(method: &str, params: Value) -> Value {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        handle(&request, &Fake).expect("a request with an id gets a response")
    }

    fn meta(version: &str) -> Value {
        json!({
            // The literal, not a shared constant: if it ever stops matching
            // the `serde` rename in `RequestMeta`, the version check silently
            // never fires -- which the mismatch test below would then catch.
            "io.modelcontextprotocol/protocolVersion": version,
            "clientCapabilities": {},
            "clientInfo": {"name": "test", "version": "1"},
        })
    }

    /// There is no handshake, so a first call must be answerable on its own.
    #[test]
    fn a_request_is_answered_without_any_handshake() {
        let r = call("tools/list", json!({"_meta": meta(PROTOCOL_VERSION)}));
        assert!(r["result"]["tools"].is_array());
    }

    /// `server/discover` is the one endpoint that cannot be skipped.
    #[test]
    fn discover_answers_and_names_what_is_absent() {
        let r = call("server/discover", json!({}));
        let result = &r["result"];
        assert_eq!(result["protocolVersions"][0], PROTOCOL_VERSION);
        assert_eq!(result["resultType"], "complete");
        assert!(result["serverInfo"]["name"].is_string());
        assert!(
            result["capabilities"]["logging"].is_null(),
            "logging is deprecated and this adapter has none"
        );
        assert!(result["capabilities"]["subscriptions"].is_null());
    }

    /// A version mismatch is an error rather than a negotiation -- except on
    /// `server/discover`, which is how a client finds out what to speak.
    #[test]
    fn a_version_mismatch_is_refused_but_discover_still_answers() {
        let r = call("tools/list", json!({"_meta": meta("2025-11-25")}));
        assert_eq!(r["error"]["code"], envelope::ERR_UNSUPPORTED_PROTOCOL_VERSION);

        let r = call("server/discover", json!({"_meta": meta("2025-11-25")}));
        assert!(r["result"].is_object(), "discover must stay reachable");
    }

    /// The revision says a client may cache the tool list. A `HashMap`'s order
    /// would invalidate that cache on every call for no reason.
    #[test]
    fn tools_are_listed_in_a_deterministic_order() {
        let r = call("tools/list", json!({"_meta": meta(PROTOCOL_VERSION)}));
        let names: Vec<&str> = r["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
        assert!(
            r["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .all(|t| t["annotations"]["readOnlyHint"] == true),
            "every tool on this surface reads and nothing writes"
        );
    }

    /// Required on every result, and `ttlMs`/`cacheScope` on the cacheable ones.
    #[test]
    fn every_result_carries_what_the_revision_requires() {
        for (method, params) in [
            ("tools/list", json!({"_meta": meta(PROTOCOL_VERSION)})),
            ("resources/list", json!({"_meta": meta(PROTOCOL_VERSION)})),
            (
                "resources/templates/list",
                json!({"_meta": meta(PROTOCOL_VERSION)}),
            ),
        ] {
            let r = call(method, params);
            let result = &r["result"];
            assert_eq!(result["resultType"], "complete", "{method}");
            assert!(result["serverInfo"].is_object(), "{method}");
            assert!(result["ttlMs"].is_number(), "{method}");
            assert!(result["cacheScope"].is_string(), "{method}");
        }
    }

    /// Getting this wrong leaks one caller's build state into another's
    /// context, which is why it is a test and not a comment.
    #[test]
    fn anything_keyed_by_a_handle_is_private() {
        let r = call(
            "tools/call",
            json!({
                "name": "get_stats",
                "arguments": {"handle": "abc"},
                "_meta": meta(PROTOCOL_VERSION),
            }),
        );
        assert_eq!(r["result"]["cacheScope"], "private");

        let r = call(
            "resources/read",
            json!({"uri": "sccache://stats/abc", "_meta": meta(PROTOCOL_VERSION)}),
        );
        assert_eq!(r["result"]["cacheScope"], "private");

        // The environment is the same for everyone and says nothing about a
        // build, so it is the one thing that may be shared.
        let r = call("tools/list", json!({"_meta": meta(PROTOCOL_VERSION)}));
        assert_eq!(r["result"]["cacheScope"], "shared");
    }

    /// `-32020`..=`-32099` is reserved for the specification, so nothing this
    /// adapter invents may land there.
    #[test]
    fn adapter_errors_stay_out_of_the_reserved_range() {
        let cases = [
            call(
                "tools/call",
                json!({"name": "nope", "arguments": {}, "_meta": meta(PROTOCOL_VERSION)}),
            ),
            call(
                "tools/call",
                json!({"name": "explain_miss", "arguments": {"handle": "nope"}, "_meta": meta(PROTOCOL_VERSION)}),
            ),
            call("tools/list", json!({"_meta": meta("1999-01-01")})),
            call("no/such/method", json!({"_meta": meta(PROTOCOL_VERSION)})),
        ];
        for r in cases {
            let code = r["error"]["code"].as_i64().expect("an error code");
            assert!(
                !(-32099..=-32020).contains(&code),
                "code {code} is in the range reserved for the specification"
            );
            assert!(
                r["error"]["data"]["serverInfo"].is_object(),
                "even a failure has to say who produced it"
            );
        }
    }

    /// An unknown handle is refused rather than answered broadly. A handle is a
    /// capability, and one that fell back to "everything" would be no boundary.
    #[test]
    fn an_unknown_handle_is_refused() {
        let r = call(
            "tools/call",
            json!({"name": "get_stats", "arguments": {"handle": "nope"}, "_meta": meta(PROTOCOL_VERSION)}),
        );
        assert_eq!(r["error"]["code"], ERR_UNKNOWN_HANDLE);
    }

    /// The methods this revision removed must not be quietly answered.
    #[test]
    fn removed_methods_are_not_implemented() {
        for method in [
            "initialize",
            "ping",
            "logging/setLevel",
            "resources/subscribe",
            "subscriptions/listen",
        ] {
            let r = call(method, json!({"_meta": meta(PROTOCOL_VERSION)}));
            assert_eq!(
                r["error"]["code"], -32601,
                "{method} was removed from the protocol and inventing it back is a bug"
            );
        }
    }

    /// A notification has no id and gets no response.
    #[test]
    fn a_notification_is_not_answered() {
        let request = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        assert!(handle(&request, &Fake).is_none());
    }

    /// The classification is the payload: an agent told `uncacheable` should
    /// stop, and one told anything else should try again later.
    #[test]
    fn explain_miss_hands_back_the_classification() {
        let r = call(
            "tools/call",
            json!({"name": "explain_miss", "arguments": {"handle": "ok"}, "_meta": meta(PROTOCOL_VERSION)}),
        );
        let misses = &r["result"]["structuredContent"]["misses"];
        assert_eq!(misses[0]["classification"]["Uncacheable"], "time_macros");
        assert_eq!(misses[0]["input"], "src/foo.c");
    }
}
