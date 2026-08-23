// SPDX-FileCopyrightText: Copyright natyamatsya and sccion contributors
// SPDX-License-Identifier: Apache-2.0

//! The MCP surface: eight questions, two resources, no protocol of our own.
//!
//! The protocol lives in `rmcp`, the official SDK, and that is the whole point
//! of this file being as short as it is. What used to be here -- a hand-written
//! envelope, a version check, a tool-schema table, a dispatcher -- was a second
//! implementation of a specification that revises quarterly, and it was wrong in
//! the way second implementations are: it spoke exactly one revision, so every
//! editor in the field (which opens with `initialize`, removed in that revision)
//! saw a dead server. The SDK speaks both lifecycles and negotiates per
//! connection, and keeping up with the next revision becomes a version bump.
//!
//! What this file still owns is everything that is *sccion's* judgement rather
//! than the protocol's:
//!
//! 1. **Every tool reads.** `readOnlyHint` is on all of them, and no tool can
//!    evict an entry, change configuration or start a compilation. That is the
//!    property that makes this surface safe to hand to an agent, so it is
//!    asserted by a test rather than left to review.
//! 2. **A handle is a capability.** It is minted by the daemon, never derived
//!    from anything a caller supplies, and a tool that cannot resolve one
//!    refuses instead of falling back to "everything".
//! 3. **`ttlMs` and `cacheScope`** on the results that carry them. `private` is
//!    a correctness matter, not a hint: a result keyed by a handle describes one
//!    caller's build, and marking it shareable would let it be served into
//!    another caller's context.
//! 4. **The tool list is stable** — sorted by name, so a client that caches it
//!    is not re-fetching for a reordering, and prompt caching downstream is not
//!    defeated by one.
//!
//! On (3), one thing changed with the move and is worth naming: `CallToolResult`
//! has no `ttlMs`/`cacheScope` in the 2026-07-28 schema -- they live on the
//! discovery and list results -- so tool answers no longer carry the `private`
//! marking they used to. The old adapter put them there anyway, which was a
//! field the specification does not define; the intent survives in this file's
//! comments and in what the tools refuse, not on the wire.

use std::future::Future;
use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CacheScope, CallToolResult, ContentBlock, Implementation, ListResourceTemplatesResult,
    ListResourcesResult, ListToolsResult, PaginatedRequestParams, ReadResourceRequestParams,
    ReadResourceResponse, ReadResourceResult, Resource, ResourceContents, ResourceTemplate,
    ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler, tool_router};
use serde_json::Value;

use crate::backend::Backend;
use crate::queries;

/// Volatile: a build changes these every few milliseconds.
const TTL_STATS: u64 = 2_000;
/// The environment report describes hardware, filesystems and antivirus
/// configuration, none of which changes during a build.
const TTL_ENVIRONMENT: u64 = 300_000;

const ENVIRONMENT_URI: &str = "sccion://environment";
const STATS_PREFIX: &str = "sccion://stats/";

#[derive(Clone)]
pub struct SccionServer {
    backend: Arc<dyn Backend>,
}

// --- Tool arguments --------------------------------------------------------
//
// The JSON Schema each tool advertises is derived from these, rather than
// written out and kept in step by hand. A description on a field is the one an
// agent reads when it decides what to pass.

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct RegisterArgs {
    /// Any directory inside the checkout.
    pub path: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct HandleArgs {
    /// From register_workspace.
    pub handle: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ExplainMissArgs {
    /// From register_workspace.
    pub handle: String,
    /// Only misses whose output or input contains this.
    pub output: Option<String>,
    /// How many records to fetch. Capped at the daemon's ring size, so the
    /// maximum is always a complete answer rather than a truncated one.
    pub limit: Option<u64>,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct ExplainKeyArgs {
    /// From register_workspace.
    pub handle: String,
    /// Source path, as the records name it.
    pub input: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CompareKeysArgs {
    /// From register_workspace.
    pub handle: String,
    /// The workspace to compare against.
    pub other_handle: String,
    /// The input compiled in both.
    pub input: String,
}

#[derive(serde::Deserialize, rmcp::schemars::JsonSchema)]
#[schemars(crate = "rmcp::schemars")]
pub struct CheckEnvironmentArgs {
    /// From register_workspace. Optional: with it, the settings reported are the
    /// ones a build in that checkout would run under, including any its project
    /// file overrides. Without it, the machine's own configuration.
    pub handle: Option<String>,
}

/// Hand an answer back as both text and structured content.
///
/// Tool output is data, never instruction. `explain_miss` returns file paths and
/// header names that originate in a repository the caller -- or another caller
/// -- may have written, so it is handed over as structured content rather than
/// as prose to be read as directions.
fn answer(body: Value) -> Result<CallToolResult, McpError> {
    let mut result = CallToolResult::structured(body.clone());
    // `structured` renders its text form compactly. These answers are read by
    // people as often as by agents -- a miss report is something you scroll --
    // so the text block stays pretty-printed while the structured content is
    // what a program should parse.
    result.content = vec![ContentBlock::text(
        serde_json::to_string_pretty(&body).unwrap_or_default(),
    )];
    Ok(result)
}

#[tool_router]
impl SccionServer {
    pub fn new(backend: Arc<dyn Backend>) -> Self {
        Self { backend }
    }

    #[tool(
        description = "Resolve a directory to the checkout it is in and return a handle naming it. Every other tool takes that handle.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn register_workspace(
        &self,
        Parameters(args): Parameters<RegisterArgs>,
    ) -> Result<CallToolResult, McpError> {
        answer(queries::register_workspace(&*self.backend, &args.path)?)
    }

    #[tool(
        description = "Cache hit and miss counts, either machine-wide or broken down per caller.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn get_stats(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        answer(queries::stats(&*self.backend, &args.handle)?)
    }

    #[tool(
        description = "Explain why recent compilations were not served from cache, for one workspace. Says whether each miss was cold or permanently uncacheable, which is the difference between retrying and giving up.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn explain_miss(
        &self,
        Parameters(args): Parameters<ExplainMissArgs>,
    ) -> Result<CallToolResult, McpError> {
        answer(queries::explain_miss(
            &*self.backend,
            &args.handle,
            args.output.as_deref(),
            args.limit.unwrap_or(50),
        )?)
    }

    #[tool(
        description = "Cache size against its limit, and how much of the cache the daemon has served, for one workspace.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn get_cache_health(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        answer(queries::cache_health(&*self.backend, &args.handle)?)
    }

    #[tool(
        description = "Every compilation in this workspace that can never be cached, grouped by reason with counts and examples. These are the ones where retrying is pointless and the fix is in the build or in sccion.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn uncacheable_summary(
        &self,
        Parameters(args): Parameters<HandleArgs>,
    ) -> Result<CallToolResult, McpError> {
        answer(queries::uncacheable_summary(&*self.backend, &args.handle)?)
    }

    #[tool(
        description = "Break one compilation's cache key into the parts it was computed from. Needs SCCACHE_TRACE_COMPONENTS=1 on the build being asked about.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn explain_key(
        &self,
        Parameters(args): Parameters<ExplainKeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        answer(queries::explain_key(
            &*self.backend,
            &args.handle,
            &args.input,
        )?)
    }

    #[tool(
        description = "Given one input compiled in two workspaces, say which part of the cache key differs: compiler, arguments, extra files, environment, working directory, or the inputs themselves. This is the question to ask when two checkouts of one project are not sharing entries. Both builds must have run with SCCACHE_TRACE_COMPONENTS=1.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn compare_keys(
        &self,
        Parameters(args): Parameters<CompareKeysArgs>,
    ) -> Result<CallToolResult, McpError> {
        answer(queries::compare_keys(
            &*self.backend,
            &args.handle,
            &args.other_handle,
            &args.input,
        )?)
    }

    #[tool(
        description = "Report the environment sccion is running in: cache location and size, filesystem, preprocessor cache settings, and any findings that would invalidate a measurement. Pass a workspace handle to get the settings a build in that checkout would actually run under, and which layer supplied each -- machine, project file, or environment. Reports only; changes nothing.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn check_environment(
        &self,
        Parameters(args): Parameters<CheckEnvironmentArgs>,
    ) -> Result<CallToolResult, McpError> {
        answer(queries::environment(
            &*self.backend,
            args.handle.as_deref(),
        )?)
    }
}

#[tool_handler]
impl ServerHandler for SccionServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(
            Implementation::new("sccion", env!("CARGO_PKG_VERSION"))
                .with_title("sccion build cache"),
        )
        .with_instructions(
            "Read-only questions about a local sccion compiler cache. Call \
             register_workspace with any directory inside a checkout to get a handle; \
             every other tool takes it. explain_miss says whether a miss was cold or \
             permanently uncacheable, which is the difference between retrying and \
             giving up.",
        )
    }

    /// The list is the same for everyone and changes only when the binary does.
    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + Send + '_ {
        let tools = Self::tool_router().list_all();
        std::future::ready(Ok(ListToolsResult::with_all_items(tools)
            .with_ttl_ms(TTL_ENVIRONMENT)
            .with_cache_scope(CacheScope::Public)))
    }

    fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        let environment = Resource::new(ENVIRONMENT_URI, "environment")
            .with_title("sccion environment report")
            .with_mime_type("application/json");
        std::future::ready(Ok(ListResourcesResult::with_all_items(vec![environment])
            .with_ttl_ms(TTL_ENVIRONMENT)
            .with_cache_scope(CacheScope::Public)))
    }

    fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, McpError>> + Send + '_ {
        let stats = ResourceTemplate::new(format!("{STATS_PREFIX}{{handle}}"), "stats")
            .with_title("sccion statistics for one workspace")
            .with_mime_type("application/json");
        std::future::ready(Ok(ListResourceTemplatesResult::with_all_items(vec![stats])
            .with_ttl_ms(TTL_ENVIRONMENT)
            .with_cache_scope(CacheScope::Public)))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResponse, McpError>> + Send + '_ {
        std::future::ready(self.read(&request.uri))
    }
}

impl SccionServer {
    /// Split out of `read_resource` so it can be tested without a transport.
    fn read(&self, uri: &str) -> Result<ReadResourceResponse, McpError> {
        let (body, ttl, scope) = if uri == ENVIRONMENT_URI {
            (
                queries::environment(&*self.backend, None)?,
                TTL_ENVIRONMENT,
                // The same answer for every caller, and stable for as long as
                // the machine is.
                CacheScope::Public,
            )
        } else if let Some(handle) = uri.strip_prefix(STATS_PREFIX) {
            (
                queries::stats(&*self.backend, handle)?,
                TTL_STATS,
                // Keyed by a handle, so it describes one caller's build and must
                // never be reused for another.
                CacheScope::Private,
            )
        } else {
            return Err(McpError::resource_not_found(
                format!("no such resource: {uri}"),
                None,
            ));
        };

        Ok(ReadResourceResult::new(vec![ResourceContents::text(
            serde_json::to_string_pretty(&body).unwrap_or_default(),
            uri,
        )])
        .with_ttl_ms(ttl)
        .with_cache_scope(scope)
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::fake::Fake;

    fn server() -> SccionServer {
        SccionServer::new(Arc::new(Fake))
    }

    /// Every tool on this surface reads. Saying so is not decoration: it is the
    /// property that makes the surface safe to expose to an agent, and an agent
    /// that trusts `readOnlyHint` is trusting this assertion.
    #[test]
    fn every_tool_is_annotated_read_only() {
        for tool in SccionServer::tool_router().list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} carries no annotations", tool.name));
            assert_eq!(
                annotations.read_only_hint,
                Some(true),
                "{} is not marked read-only",
                tool.name
            );
            assert_eq!(annotations.destructive_hint, Some(false), "{}", tool.name);
        }
    }

    /// A client may cache the tool list, and an order that varied between two
    /// calls would invalidate that cache for no reason -- and defeat prompt
    /// caching downstream.
    #[test]
    fn tools_are_listed_in_a_deterministic_order() {
        let names: Vec<String> = SccionServer::tool_router()
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert_eq!(names.len(), 8, "the surface is eight tools");
    }

    /// Getting this wrong leaks one caller's build state into another's context,
    /// which is why it is a test and not a comment.
    #[test]
    fn a_resource_keyed_by_a_handle_is_private() {
        let response = server().read("sccion://stats/abc").unwrap();
        let ReadResourceResponse::Complete(result) = response else {
            panic!("this surface has no input_required flows");
        };
        assert_eq!(result.cache_scope, Some(CacheScope::Private));
        assert_eq!(result.ttl_ms, Some(TTL_STATS));

        // The environment is the same for everyone and says nothing about a
        // build, so it is the one thing that may be shared.
        let response = server().read(ENVIRONMENT_URI).unwrap();
        let ReadResourceResponse::Complete(result) = response else {
            panic!("this surface has no input_required flows");
        };
        assert_eq!(result.cache_scope, Some(CacheScope::Public));
    }

    /// A handle this daemon never minted is refused here too. Falling back to
    /// the adapter's own directory would answer confidently about the wrong
    /// tree, which is worse than not answering.
    #[test]
    fn an_unknown_resource_and_an_unknown_handle_are_both_refused() {
        let e = server().read("sccion://nope").unwrap_err();
        assert!(e.message.contains("no such resource"));

        let e = server().read("sccion://stats/nope").unwrap_err();
        assert_eq!(e.code.0, queries::ERR_UNKNOWN_HANDLE);
    }

    /// The tools are what an agent sees first, so their descriptions are part of
    /// the interface: each has to say what the answer is *for*.
    #[test]
    fn every_tool_describes_itself() {
        for tool in SccionServer::tool_router().list_all() {
            let description = tool.description.as_deref().unwrap_or("");
            assert!(
                description.len() > 40,
                "{} needs a description an agent can choose on",
                tool.name
            );
        }
    }
}
