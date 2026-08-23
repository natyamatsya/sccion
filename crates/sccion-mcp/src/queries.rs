// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! The answers, with no protocol in them.
//!
//! Each function here is one question a caller can ask about a local daemon,
//! taking a [`Backend`] and returning either the body of an answer or the error
//! that explains why there is none. What shape that body travels in -- which
//! result type, which envelope fields, which revision -- is [`crate::server`]'s
//! business, and the SDK's underneath it.
//!
//! The split is what makes these testable against a fake daemon without a
//! transport, and it is also where the reasoning lives: an error message that
//! names the way out (`SCCACHE_TRACE_COMPONENTS=1`) is worth more than a code,
//! and it belongs next to the code that knows why the answer was missing.

use std::path::{Path, PathBuf};

use rmcp::ErrorData as McpError;
use rmcp::model::ErrorCode;
use serde_json::{Value, json};

use crate::backend::Backend;

// --- Error codes -----------------------------------------------------------
//
// `-32000`..=`-32019` is implementation-defined and `-32020`..=`-32099` is
// reserved for the specification, so every code this crate invents is in the
// low range. The two below are the only invented ones; everything else reuses a
// code the specification already assigns, through `McpError`'s constructors.

/// The caller asked about a workspace handle this daemon did not mint.
pub const ERR_UNKNOWN_HANDLE: i32 = -32001;
/// No sccion daemon is running, or it could not be reached.
pub const ERR_DAEMON_UNAVAILABLE: i32 = -32002;

fn err(code: i32, message: impl Into<String>) -> McpError {
    McpError::new(ErrorCode(code), message.into(), None)
}

fn not_found(message: impl Into<String>) -> McpError {
    McpError::invalid_params(message.into(), None)
}

/// A daemon that refused says so; one that was never reached does not.
///
/// Reporting "no sccion daemon answered" for a daemon that answered perfectly
/// clearly sends the caller looking for a process that is running fine.
fn unavailable(e: anyhow::Error) -> McpError {
    match e.downcast_ref::<sccion::api::Refused>() {
        Some(refused) => McpError::invalid_params(refused.to_string(), None),
        None => err(
            ERR_DAEMON_UNAVAILABLE,
            format!("no sccion daemon answered: {e}"),
        ),
    }
}

/// An unknown handle is refused rather than answered broadly: a handle is a
/// capability, and one that fell back to "everything" would be no boundary.
fn handle_error(e: anyhow::Error) -> McpError {
    if e.to_string().contains("unknown workspace handle") {
        err(ERR_UNKNOWN_HANDLE, e.to_string())
    } else {
        unavailable(e)
    }
}

// --- The questions ---------------------------------------------------------

/// Resolve a directory to the checkout it is in.
pub fn register_workspace(backend: &dyn Backend, path: &str) -> Result<Value, McpError> {
    let info = backend
        .register_workspace(Path::new(path))
        .map_err(unavailable)?;
    Ok(json!({"handle": info.handle, "root": info.root, "vcs": info.vcs}))
}

pub fn stats(backend: &dyn Backend, handle: &str) -> Result<Value, McpError> {
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

pub fn explain_miss(
    backend: &dyn Backend,
    handle: &str,
    needle: Option<&str>,
    requested: u64,
) -> Result<Value, McpError> {
    // Bounded by the daemon's ring rather than by an invented number: there can
    // never be more than that, so asking for the maximum is guaranteed to be a
    // complete answer. An earlier `.min(1000)` silently truncated a request for
    // 4096 to 1000, and the analysis that hit it only noticed because the counts
    // came back suspiciously round.
    let limit = requested.min(sccion::api::MAX_MISS_RECORDS as u64) as usize;
    let records = backend
        .miss_records(Some(handle), limit)
        .map_err(handle_error)?;
    // Whether the *fetch* was cut short, decided before any filtering: a needle
    // that matches few records would otherwise make a truncated fetch look like
    // a complete answer, which is the failure this field exists to prevent.
    let truncated = records.len() == limit && limit < sccion::api::MAX_MISS_RECORDS;
    let records: Vec<_> = records
        .into_iter()
        .filter(|r| match needle {
            None => true,
            Some(n) => r.output.contains(n) || r.input.as_deref().is_some_and(|i| i.contains(n)),
        })
        .collect();
    Ok(json!({
        "misses": serde_json::to_value(&records).unwrap_or_default(),
        "returned": records.len(),
        "limitApplied": limit,
        "truncated": truncated,
        "note": "`uncacheable` means retrying will reach the same verdict; \
                 every other classification may succeed once the cache warms.",
    }))
}

pub fn cache_health(backend: &dyn Backend, handle: &str) -> Result<Value, McpError> {
    // The handle is checked for the same reason as in `stats`: it is the
    // boundary, even where the answer that follows is machine-wide.
    backend
        .miss_records(Some(handle), 1)
        .map_err(handle_error)?;
    let info = backend.stats().map_err(unavailable)?;
    Ok(json!({
        "cacheLocation": info.cache_location,
        "cacheSize": info.cache_size,
        "maxCacheSize": info.max_cache_size,
        "usePreprocessorCacheMode": info.use_preprocessor_cache_mode,
        "version": info.version,
    }))
}

pub fn uncacheable_summary(backend: &dyn Backend, handle: &str) -> Result<Value, McpError> {
    let records = backend
        .miss_records(Some(handle), 4096)
        .map_err(handle_error)?;
    // Grouped rather than listed: "141 permanent misses" is a fact about a
    // build, and reading it out of 141 separate records is work the caller
    // should not have to do.
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
    Ok(json!({
        "reasons": reasons,
        "note": "These will not start hitting as the cache warms. Each is a property of \
                 the compilation as invoked -- the fix is in the build, or in sccion.",
    }))
}

pub fn explain_key(backend: &dyn Backend, handle: &str, input: &str) -> Result<Value, McpError> {
    let records = backend
        .miss_records(Some(handle), 4096)
        .map_err(handle_error)?;
    let found = latest_for_input(&records, input).ok_or_else(|| not_traced(input))?;
    let components = found
        .components
        .as_ref()
        .ok_or_else(|| not_found(needs_tracing(found)))?;
    Ok(json!({
        "input": found.input,
        "key": found.key,
        "classification": found.classification,
        "components": components,
    }))
}

pub fn compare_keys(
    backend: &dyn Backend,
    handle: &str,
    other: &str,
    input: &str,
) -> Result<Value, McpError> {
    let mine = backend
        .miss_records(Some(handle), 4096)
        .map_err(handle_error)?;
    let theirs = backend
        .miss_records(Some(other), 4096)
        .map_err(handle_error)?;
    let a = latest_for_input(&mine, input).ok_or_else(|| not_traced(input))?;
    let b = latest_for_input(&theirs, input).ok_or_else(|| not_traced(input))?;

    let same_key = a.key.is_some() && a.key == b.key;
    match (&a.components, &b.components) {
        (Some(ca), Some(cb)) => Ok(json!({
            "input": input,
            "sameKey": same_key,
            // The whole point. An empty list with different keys means the
            // difference is in something not broken out as a component, which
            // is itself worth knowing.
            "differingComponents": ca.differences(cb),
            // `seq` is here because records are history, not state. Only misses
            // are recorded by default, so a workspace whose last build *hit*
            // offers its previous record and the comparison silently spans two
            // moments. Far-apart sequence numbers mean the answer is about the
            // past; SCCACHE_TRACE_ALL=1 records hits too and removes the
            // ambiguity.
            "left": {"workspace": a.workspace, "key": a.key, "seq": a.seq,
                     "classification": a.classification, "components": ca},
            "right": {"workspace": b.workspace, "key": b.key, "seq": b.seq,
                      "classification": b.classification, "components": cb},
        })),
        (None, _) => Err(not_found(needs_tracing(a))),
        (_, None) => Err(not_found(needs_tracing(b))),
    }
}

/// The environment a build in `handle`'s checkout would run under, or the
/// machine's own when no handle is given.
///
/// With a handle the report is about that checkout: its project file decides
/// `ignore_time_macros` and `use_dependency_keying`, and answering from the
/// adapter's own directory would describe a tree nobody is building.
pub fn environment(backend: &dyn Backend, handle: Option<&str>) -> Result<Value, McpError> {
    let cwd = match handle {
        Some(handle) => {
            let info = backend.describe_workspace(handle).map_err(handle_error)?;
            PathBuf::from(info.root)
        }
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    Ok(serde_json::to_value(backend.environment(&cwd)).unwrap_or_default())
}

// --- Shared helpers --------------------------------------------------------

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

fn not_traced(input: &str) -> McpError {
    not_found(format!(
        "no record for {input:?} in this workspace's recent history"
    ))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::{Fake, Located};

    /// The question this tool exists for, and the one that cost an afternoon of
    /// shell on 2026-08-09: two workspaces compile the same input and get
    /// different keys — *which part* differs?
    #[test]
    fn compare_keys_names_the_component_that_differs() {
        let body = compare_keys(&Fake, "a", "b", "src/foo.c").unwrap();
        // The fake gives both workspaces the same components, so nothing
        // differs. What matters is the shape of the answer and that it is a
        // list rather than a yes/no.
        assert!(body["differingComponents"].is_array());
        assert!(body["left"]["components"]["arguments"].is_string());
        assert!(body["right"]["components"]["inputs"].is_string());
    }

    /// A record without components must say what to do about it, not just fail.
    /// The fix is one environment variable and a rebuild away, and an agent that
    /// is told so can act; one told "not found" cannot.
    #[test]
    fn a_record_without_components_says_how_to_get_them() {
        let e = explain_key(&crate::backend::fake::Untraced, "a", "src/foo.c").unwrap_err();
        assert!(
            e.message.contains("SCCACHE_TRACE_COMPONENTS"),
            "the error has to name the way out: {}",
            e.message
        );
    }

    /// "141 permanent misses" is a fact about a build. Reading it out of 141
    /// separate records is work the caller should not have to do.
    #[test]
    fn uncacheable_summary_groups_by_reason() {
        let body = uncacheable_summary(&Fake, "a").unwrap();
        assert_eq!(body["reasons"][0]["reason"], "time_macros");
        assert_eq!(body["reasons"][0]["count"], 1);
        assert_eq!(body["reasons"][0]["examples"][0], "src/foo.c");
    }

    /// The classification is the payload: an agent told `uncacheable` should
    /// stop, and one told anything else should try again later.
    #[test]
    fn explain_miss_hands_back_the_classification() {
        let body = explain_miss(&Fake, "ok", None, 50).unwrap();
        assert_eq!(
            body["misses"][0]["classification"]["Uncacheable"],
            "time_macros"
        );
        assert_eq!(body["misses"][0]["input"], "src/foo.c");
    }

    /// A handle is a capability. One that fell back to "everything" would be no
    /// boundary at all, so every tool that takes one refuses what it cannot
    /// resolve.
    #[test]
    fn an_unknown_handle_is_refused() {
        for e in [
            stats(&Fake, "nope").unwrap_err(),
            cache_health(&Fake, "nope").unwrap_err(),
            environment(&Fake, Some("nope")).unwrap_err(),
        ] {
            assert_eq!(e.code.0, ERR_UNKNOWN_HANDLE);
        }
    }

    /// The environment report is about a *tree*: which settings a build there
    /// would run under, and which of them its project file overrides. The
    /// adapter's own working directory is wherever an editor launched it, and
    /// answering from that reported the machine's settings for a checkout that
    /// overrides them -- true of this process, wrong about the build.
    #[test]
    fn check_environment_reports_on_the_workspace_a_handle_names() {
        let backend = Located::default();
        environment(&backend, Some("abc")).unwrap();
        assert_eq!(
            backend.asked_about().as_deref(),
            Some(Path::new("D:/wt")),
            "the handle has to reach the checkout, not the adapter's own cwd"
        );
    }

    /// `-32020`..=`-32099` is reserved for the specification, so nothing this
    /// crate invents may land there.
    #[test]
    fn invented_error_codes_stay_out_of_the_reserved_range() {
        for code in [ERR_UNKNOWN_HANDLE, ERR_DAEMON_UNAVAILABLE] {
            assert!(
                !(-32099..=-32020).contains(&code),
                "code {code} is in the range reserved for the specification"
            );
        }
    }
}
