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
        "Report the environment sccion is running in: cache location and size,          filesystem, preprocessor cache settings, and any findings that would          invalidate a measurement. Pass a workspace handle to get the settings a          build in that checkout would actually run under, and which layer          supplied each -- machine, project file, or environment. Reports only;          changes nothing.",
    ),
    (
        "compare_keys",
        "Given one input compiled in two workspaces, say which part of the cache          key differs: compiler, arguments, extra files, environment, working          directory, or the inputs themselves. This is the question to ask when          two checkouts of one project are not sharing entries. Both builds must          have run with SCCACHE_TRACE_COMPONENTS=1.",
    ),
    (
        "explain_key",
        "Break one compilation's cache key into the parts it was computed from.          Needs SCCACHE_TRACE_COMPONENTS=1 on the build being asked about.",
    ),
    (
        "explain_miss",
        "Explain why recent compilations were not served from cache, for one          workspace. Says whether each miss was cold or permanently uncacheable,          which is the difference between retrying and giving up.",
    ),
    (
        "get_cache_health",
        "Cache size against its limit, and how much of the cache the daemon has          served, for one workspace.",
    ),
    (
        "get_stats",
        "Cache hit and miss counts, either machine-wide or broken down per          caller.",
    ),
    (
        "register_workspace",
        "Resolve a directory to the checkout it is in and return a handle naming          it. Every other tool takes that handle.",
    ),
    (
        "uncacheable_summary",
        "Every compilation in this workspace that can never be cached, grouped          by reason with counts and examples. These are the ones where retrying          is pointless and the fix is in the build or in sccion.",
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
        // - `ping`, `logging/setLevel`: removed. sccion logs to stderr, and
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
    envelope::result(
        json!({"tools": tools}),
        Some((TTL_ENVIRONMENT, CacheScope::Shared)),
    )
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
        "get_cache_health" | "uncacheable_summary" => json!({
            "type": "object",
            "properties": {"handle": handle},
            "required": ["handle"],
        }),
        "check_environment" => json!({
            "type": "object",
            "properties": {"handle": {
                "type": "string",
                "description": "From register_workspace. Optional: with it, the settings \
                                reported are the ones a build in that checkout would run \
                                under, including any its project file overrides. Without \
                                it, the machine's own configuration.",
            }},
        }),
        "explain_key" => json!({
            "type": "object",
            "properties": {
                "handle": handle,
                "input": {"type": "string", "description": "Source path, as the records name it."},
            },
            "required": ["handle", "input"],
        }),
        "compare_keys" => json!({
            "type": "object",
            "properties": {
                "handle": handle,
                "other_handle": {"type": "string", "description": "The workspace to compare against."},
                "input": {"type": "string", "description": "The input compiled in both."},
            },
            "required": ["handle", "other_handle", "input"],
        }),
        _ => json!({"type": "object", "properties": {}}),
    }
}

fn resources_list() -> Value {
    envelope::result(
        json!({"resources": [{
            "uri": "sccion://environment",
            "name": "environment",
            "title": "sccion environment report",
            "mimeType": "application/json",
        }]}),
        Some((TTL_ENVIRONMENT, CacheScope::Shared)),
    )
}

fn resource_templates_list() -> Value {
    envelope::result(
        json!({"resourceTemplates": [{
            "uriTemplate": "sccion://stats/{handle}",
            "name": "stats",
            "title": "sccion statistics for one workspace",
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

    let (body, scope, ttl) = if uri == "sccion://environment" {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        (
            serde_json::to_value(backend.environment(&cwd)).unwrap_or_default(),
            CacheScope::Shared,
            TTL_ENVIRONMENT,
        )
    } else if let Some(handle) = uri.strip_prefix("sccion://stats/") {
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
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

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
            // Bounded by the daemon's ring rather than by an invented number:
            // there can never be more than that, so asking for the maximum is
            // guaranteed to be a complete answer. The previous `.min(1000)`
            // silently truncated a request for 4096 to 1000, and the analysis
            // that hit it only noticed because the counts came back suspiciously
            // round.
            let requested = args.get("limit").and_then(Value::as_u64).unwrap_or(50);
            let limit = requested.min(sccion::api::MAX_MISS_RECORDS as u64) as usize;
            let needle = args.get("output").and_then(Value::as_str);
            let records = backend
                .miss_records(Some(&handle), limit)
                .map_err(handle_error)?;
            // Whether the *fetch* was cut short, decided before any filtering:
            // a needle that matches few records would otherwise make a truncated
            // fetch look like a complete answer, which is the failure this field
            // exists to prevent.
            let truncated = records.len() == limit && limit < sccion::api::MAX_MISS_RECORDS;
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
                    "returned": records.len(),
                    "limitApplied": limit,
                    "truncated": truncated,
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
        "uncacheable_summary" => {
            let records = backend
                .miss_records(Some(&handle()?), 4096)
                .map_err(handle_error)?;
            // Grouped rather than listed: "141 permanent misses" is a fact
            // about a build, and reading it out of 141 separate records is
            // work the caller should not have to do.
            let mut by_reason: std::collections::BTreeMap<String, Vec<String>> = Default::default();
            for r in &records {
                if let sccion::api::Classification::Uncacheable(why) = &r.classification {
                    by_reason
                        .entry(why.clone())
                        .or_default()
                        .push(r.input.clone().unwrap_or_else(|| r.output.clone()));
                }
            }
            let reasons: Vec<Value> = by_reason
                .into_iter()
                .map(|(why, inputs)| {
                    json!({
                        "reason": why,
                        "count": inputs.len(),
                        "examples": inputs.iter().take(5).collect::<Vec<_>>(),
                    })
                })
                .collect();
            Ok(envelope::tool_result(
                json!({
                    "reasons": reasons,
                    "note": "These will not start hitting as the cache warms. Each is a \
                             property of the compilation as invoked -- the fix is in the \
                             build, or in sccion.",
                }),
                CacheScope::Private,
                TTL_STATS,
            ))
        }
        "explain_key" => {
            let handle = handle()?;
            let input = arg_str(&args, "input")?;
            let records = backend
                .miss_records(Some(&handle), 4096)
                .map_err(handle_error)?;
            let found = latest_for_input(&records, &input).ok_or_else(|| not_traced(&input))?;
            let components = found
                .components
                .as_ref()
                .ok_or_else(|| (ERR_NOT_FOUND, needs_tracing(found)))?;
            Ok(envelope::tool_result(
                json!({
                    "input": found.input,
                    "key": found.key,
                    "classification": found.classification,
                    "components": components,
                }),
                CacheScope::Private,
                TTL_STATS,
            ))
        }
        "compare_keys" => {
            let handle = handle()?;
            let other = arg_str(&args, "other_handle")?;
            let input = arg_str(&args, "input")?;

            let mine = backend
                .miss_records(Some(&handle), 4096)
                .map_err(handle_error)?;
            let theirs = backend
                .miss_records(Some(&other), 4096)
                .map_err(handle_error)?;
            let a = latest_for_input(&mine, &input).ok_or_else(|| not_traced(&input))?;
            let b = latest_for_input(&theirs, &input).ok_or_else(|| not_traced(&input))?;

            let same_key = a.key.is_some() && a.key == b.key;
            let body = match (&a.components, &b.components) {
                (Some(ca), Some(cb)) => {
                    let differing = ca.differences(cb);
                    json!({
                        "input": input,
                        "sameKey": same_key,
                        // The whole point. An empty list with different keys
                        // means the difference is in something not broken out
                        // as a component, which is itself worth knowing.
                        "differingComponents": differing,
                        // `seq` is here because records are history, not state.
                        // Only misses are recorded by default, so a workspace
                        // whose last build *hit* offers its previous record and
                        // the comparison silently spans two moments. Far-apart
                        // sequence numbers mean the answer is about the past;
                        // SCCACHE_TRACE_ALL=1 records hits too and removes the
                        // ambiguity.
                        "left": {"workspace": a.workspace, "key": a.key, "seq": a.seq,
                                 "classification": a.classification, "components": ca},
                        "right": {"workspace": b.workspace, "key": b.key, "seq": b.seq,
                                  "classification": b.classification, "components": cb},
                    })
                }
                (None, _) => return Err((ERR_NOT_FOUND, needs_tracing(a))),
                (_, None) => return Err((ERR_NOT_FOUND, needs_tracing(b))),
            };
            Ok(envelope::tool_result(body, CacheScope::Private, TTL_STATS))
        }
        "check_environment" => {
            // With a handle the report is about that checkout: its project
            // file decides `ignore_time_macros` and `use_dependency_keying`,
            // and answering from the adapter's own directory would describe a
            // tree nobody is building. Without one it stays machine-wide,
            // which is what a caller who has not registered anything can be
            // told.
            let (cwd, scope) = match args.get("handle").and_then(Value::as_str) {
                Some(handle) => {
                    let info = backend.describe_workspace(handle).map_err(handle_error)?;
                    // Keyed by a handle, so it describes one caller's checkout
                    // -- including the contents of its project file -- and must
                    // not be served to another.
                    (PathBuf::from(info.root), CacheScope::Private)
                }
                None => (
                    std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                    // The same answer for every caller, and stable for as long
                    // as the machine is.
                    CacheScope::Shared,
                ),
            };
            let report = serde_json::to_value(backend.environment(&cwd)).unwrap_or_default();
            Ok(envelope::tool_result(report, scope, TTL_ENVIRONMENT))
        }
        _ => Err((ERR_NOT_FOUND, format!("no such tool: {name}"))),
    }
}

fn arg_str(args: &Value, name: &str) -> Result<String, (i64, String)> {
    args.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or((ERR_INVALID_PARAMS, format!("missing argument: {name}")))
}

/// The most recent record for an input, matched on the tail of the path.
///
/// Records store the input basedir-relative, so a caller holding an absolute
/// path, or only a file name, should still find it rather than be told it does
/// not exist.
fn latest_for_input<'a>(
    records: &'a [sccion::api::MissRecord],
    input: &str,
) -> Option<&'a sccion::api::MissRecord> {
    let needle = input.replace('\\', "/").to_lowercase();
    records.iter().rev().find(|r| {
        r.input
            .as_deref()
            .map(|i| {
                let i = i.replace('\\', "/").to_lowercase();
                i == needle || i.ends_with(&needle) || needle.ends_with(&i)
            })
            .unwrap_or(false)
    })
}

fn not_traced(input: &str) -> (i64, String) {
    (
        ERR_NOT_FOUND,
        format!("no record for {input:?} in this workspace's recent history"),
    )
}

/// The one failure worth spelling out, because the fix is a build away.
fn needs_tracing(record: &sccion::api::MissRecord) -> String {
    let input = record.input.as_deref().unwrap_or(&record.output);
    // Two quite different reasons, and telling a caller to set a variable it
    // has already set is worse than saying nothing. A key that came out of a
    // preprocessor cache entry was never computed from parts at all, so there
    // is nothing to break down however much tracing is on.
    if record.direct == sccion::api::DirectState::Hit {
        format!(
            "{input:?} was served from a preprocessor cache entry, so its key came \
             from that entry rather than being computed from parts -- there is \
             nothing to break down. Ask about a compilation that missed direct \
             mode, or rerun that build with SCCACHE_DIRECT=false."
        )
    } else {
        format!(
            "the record for {input:?} carries no key components; rebuild with \
             SCCACHE_TRACE_COMPONENTS=1 in the build's environment (both builds, \
             for compare_keys) and ask again"
        )
    }
}

fn stats_for(handle: &str, backend: &dyn Backend) -> Result<Value, (i64, String)> {
    // Asking for one record proves the handle is one this daemon minted, before
    // any of its statistics are handed over.
    backend
        .miss_records(Some(handle), 1)
        .map_err(handle_error)?;
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
/// Reporting "no sccion daemon answered" for a daemon that answered perfectly
/// clearly sends the caller looking for a process that is running fine.
fn unavailable(e: anyhow::Error) -> (i64, String) {
    match e.downcast_ref::<sccion::api::Refused>() {
        Some(refused) => (ERR_INVALID_PARAMS, refused.to_string()),
        None => (
            ERR_DAEMON_UNAVAILABLE,
            format!("no sccion daemon answered: {e}"),
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
    use sccion::api::{Classification, DirectState, MissRecord, ServerInfo, WorkspaceInfo};

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
        fn describe_workspace(&self, handle: &str) -> anyhow::Result<WorkspaceInfo> {
            if handle == "nope" {
                anyhow::bail!("unknown workspace handle");
            }
            Ok(WorkspaceInfo {
                handle: handle.to_owned(),
                root: "D:/wt".into(),
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
                components: Some(sccion::api::KeyComponents {
                    compiler: "c0".into(),
                    arguments: "a0".into(),
                    extra_files: "x0".into(),
                    env: "e0".into(),
                    working_directory: "w0".into(),
                    inputs: "i0".into(),
                }),
            }])
        }
        fn environment(&self, _cwd: &Path) -> sccion::api::EnvReport {
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
        assert_eq!(
            r["error"]["code"],
            envelope::ERR_UNSUPPORTED_PROTOCOL_VERSION
        );

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
            json!({"uri": "sccion://stats/abc", "_meta": meta(PROTOCOL_VERSION)}),
        );
        assert_eq!(r["result"]["cacheScope"], "private");

        // The environment is the same for everyone and says nothing about a
        // build, so it is the one thing that may be shared.
        let r = call("tools/list", json!({"_meta": meta(PROTOCOL_VERSION)}));
        assert_eq!(r["result"]["cacheScope"], "shared");
    }

    /// The environment report is about a *tree*: which settings a build there
    /// would run under, and which of them its project file overrides. The
    /// adapter's own working directory is wherever an editor launched it, and
    /// answering from that reported the machine's settings for a checkout that
    /// overrides them -- true of this process, wrong about the build.
    #[test]
    fn check_environment_reports_on_the_workspace_a_handle_names() {
        /// Records which directory the report was asked about.
        struct Located(std::sync::Mutex<Option<PathBuf>>);
        impl Backend for Located {
            fn register_workspace(&self, _: &Path) -> anyhow::Result<WorkspaceInfo> {
                unimplemented!()
            }
            fn describe_workspace(&self, handle: &str) -> anyhow::Result<WorkspaceInfo> {
                Fake.describe_workspace(handle)
            }
            fn stats(&self) -> anyhow::Result<ServerInfo> {
                unimplemented!()
            }
            fn miss_records(&self, _: Option<&str>, _: usize) -> anyhow::Result<Vec<MissRecord>> {
                unimplemented!()
            }
            fn environment(&self, cwd: &Path) -> sccion::api::EnvReport {
                *self.0.lock().unwrap() = Some(cwd.to_path_buf());
                empty_report()
            }
        }

        let backend = Located(Default::default());
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "check_environment",
                "arguments": {"handle": "abc"},
                "_meta": meta(PROTOCOL_VERSION),
            },
        });
        let r = handle(&request, &backend).unwrap();
        assert_eq!(
            backend.0.lock().unwrap().as_deref(),
            Some(Path::new("D:/wt")),
            "the handle has to reach the checkout, not the adapter's own cwd"
        );
        // It now carries one checkout's project settings, so it stops being
        // the one answer that may be shared between callers.
        assert_eq!(r["result"]["cacheScope"], "private");

        // Without a handle it is machine-wide again, and shareable.
        let backend = Located(Default::default());
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "check_environment",
                "arguments": {},
                "_meta": meta(PROTOCOL_VERSION),
            },
        });
        let r = handle(&request, &backend).unwrap();
        assert_eq!(r["result"]["cacheScope"], "shared");
    }

    /// A handle this daemon never minted is refused here too. Falling back to
    /// the adapter's own directory would answer confidently about the wrong
    /// tree, which is worse than not answering.
    #[test]
    fn check_environment_refuses_a_handle_it_cannot_resolve() {
        let r = call(
            "tools/call",
            json!({
                "name": "check_environment",
                "arguments": {"handle": "nope"},
                "_meta": meta(PROTOCOL_VERSION),
            }),
        );
        assert_eq!(r["error"]["code"], ERR_UNKNOWN_HANDLE);
    }

    /// Enough of a report to be serialised; the contents are `checkenv`'s
    /// business and are tested there.
    fn empty_report() -> sccion::api::EnvReport {
        let volume = sccion::api::VolumeReport {
            path: "D:/wt".into(),
            root: Some("D:\\".into()),
            filesystem: "ReFS".into(),
            dev_drive: "trusted".into(),
            supports_block_cloning: true,
        };
        sccion::api::EnvReport {
            sccion_version: "test".into(),
            sccion_path: None,
            cache: volume.clone(),
            working_directory: volume,
            same_volume: true,
            cache_location: "D:/cache".into(),
            max_cache_size: 0,
            compression: "none".into(),
            project_config: None,
            preprocessor_cache_mode: sccion::api::Setting {
                name: "use_preprocessor_cache_mode".into(),
                value: true,
                origin: sccion::api::Origin::Machine,
                machine_value: None,
                note: None,
            },
            preprocessor_cache_settings: vec![],
            defender: Default::default(),
            findings: vec![],
        }
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

    /// The question this tool exists for, and the one that cost an afternoon of
    /// shell on 2026-08-09: two workspaces compile the same input and get
    /// different keys — *which part* differs?
    #[test]
    fn compare_keys_names_the_component_that_differs() {
        let r = call(
            "tools/call",
            json!({
                "name": "compare_keys",
                "arguments": {"handle": "a", "other_handle": "b", "input": "src/foo.c"},
                "_meta": meta(PROTOCOL_VERSION),
            }),
        );
        let body = &r["result"]["structuredContent"];
        // The fake gives both workspaces the same components, so nothing
        // differs. What matters is the shape of the answer and that it is a
        // list rather than a yes/no.
        assert!(body["differingComponents"].is_array());
        assert!(body["left"]["components"]["arguments"].is_string());
        assert!(body["right"]["components"]["inputs"].is_string());
    }

    /// A record without components must say what to do about it, not just fail.
    /// The fix is one environment variable and a rebuild away, and an agent
    /// that is told so can act; one told "not found" cannot.
    #[test]
    fn a_record_without_components_says_how_to_get_them() {
        struct Untraced;
        impl Backend for Untraced {
            fn register_workspace(&self, _: &Path) -> anyhow::Result<WorkspaceInfo> {
                unimplemented!()
            }
            fn describe_workspace(&self, _: &str) -> anyhow::Result<WorkspaceInfo> {
                unimplemented!()
            }
            fn stats(&self) -> anyhow::Result<ServerInfo> {
                unimplemented!()
            }
            fn miss_records(&self, _: Option<&str>, _: usize) -> anyhow::Result<Vec<MissRecord>> {
                let mut r = Fake.miss_records(Some("ok"), 1).unwrap();
                r[0].components = None;
                Ok(r)
            }
            fn environment(&self, _: &Path) -> sccion::api::EnvReport {
                unimplemented!()
            }
        }
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {
                "name": "explain_key",
                "arguments": {"handle": "a", "input": "src/foo.c"},
                "_meta": meta(PROTOCOL_VERSION),
            },
        });
        let r = handle(&request, &Untraced).unwrap();
        let msg = r["error"]["message"].as_str().unwrap();
        assert!(
            msg.contains("SCCACHE_TRACE_COMPONENTS"),
            "the error has to name the way out: {msg}"
        );
    }

    /// "141 permanent misses" is a fact about a build. Reading it out of 141
    /// separate records is work the caller should not have to do.
    #[test]
    fn uncacheable_summary_groups_by_reason() {
        let r = call(
            "tools/call",
            json!({
                "name": "uncacheable_summary",
                "arguments": {"handle": "a"},
                "_meta": meta(PROTOCOL_VERSION),
            }),
        );
        let reasons = &r["result"]["structuredContent"]["reasons"];
        assert_eq!(reasons[0]["reason"], "time_macros");
        assert_eq!(reasons[0]["count"], 1);
        assert_eq!(reasons[0]["examples"][0], "src/foo.c");
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
