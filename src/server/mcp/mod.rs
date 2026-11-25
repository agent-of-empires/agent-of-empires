//! Model Context Protocol server hosted on the daemon's router at `/mcp`.
//!
//! Tool handlers call the same in-process code as the HTTP handlers. Auth, host
//! and origin checks come from the router's middleware, not from `rmcp`.

use std::sync::Arc;

use rmcp::{
    handler::server::{
        router::tool::ToolRouter,
        wrapper::{Json, Parameters},
    },
    model::{
        CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams,
        ServerCapabilities, ServerConfig,
    },
    service::{RequestContext, RoleServer},
    transport::streamable_http_server::{
        session::never::NeverSessionManager, StreamableHttpServerConfig, StreamableHttpService,
    },
    ServerHandler,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::api::sessions::{
    list_sessions_envelope, session_response_from_instance, SessionFilter, SessionsView,
};
use super::auth::AuthenticatedCallerSession;
use super::state::AppState;
use crate::daemon::SessionResponse;
use crate::session::SessionScope;

mod control;
mod wait;

#[derive(Debug, Default, Clone, Copy, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
enum StateFilter {
    /// Sessions that are neither archived nor trashed.
    #[default]
    Live,
    /// Every session, including archived and trashed ones.
    All,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ListSessionsParams {
    /// Which sessions to return. Defaults to `live`.
    #[serde(default)]
    state: StateFilter,
    /// Only sessions in this group path or beneath it.
    group: Option<String>,
    /// Only direct children of the session with this id.
    children_of: Option<String>,
}

/// `whoami`'s result. MCP 2025-06-18 requires an object for `outputSchema` and
/// `structuredContent`, so the optional session is a field rather than the whole result.
#[derive(Debug, Serialize, JsonSchema)]
struct WhoAmI {
    /// The calling session, or null when the caller is not a session AoE launched.
    session: Option<SessionResponse>,
}

#[derive(Clone)]
struct McpServer {
    state: Arc<AppState>,
    tool_router: ToolRouter<Self>,
}

#[rmcp::tool_router]
impl McpServer {
    fn new(state: Arc<AppState>) -> Self {
        Self {
            state,
            tool_router: Self::tool_router() + Self::control_router() + Self::wait_router(),
        }
    }

    #[rmcp::tool(description = "List sessions, in the same shape as `GET /api/sessions`.")]
    async fn list_sessions(
        &self,
        Parameters(params): Parameters<ListSessionsParams>,
    ) -> Result<Json<SessionsView>, rmcp::ErrorData> {
        let filter = SessionFilter {
            scope: Some(match params.state {
                StateFilter::Live => SessionScope::Live,
                StateFilter::All => SessionScope::All,
            }),
            group: params.group,
            children_of: params.children_of,
        };
        list_sessions_envelope(&self.state, filter)
            .await
            .map(Json)
            .map_err(|_| rmcp::ErrorData::internal_error("Session snapshot unavailable", None))
    }

    #[rmcp::tool(
        description = "`{session}`: the calling session, or null when the caller is not a session AoE launched."
    )]
    async fn whoami(&self, context: RequestContext<RoleServer>) -> Json<WhoAmI> {
        let Some(id) = caller_session_id(&context) else {
            return Json(WhoAmI { session: None });
        };
        let instances = self.state.instances.read().await;
        let session = instances.iter().find(|inst| inst.id == id).map(|inst| {
            session_response_from_instance(inst, crate::claude_settings::read_tui_fullscreen())
        });
        Json(WhoAmI { session })
    }
}

#[rmcp::tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("aoe", env!("CARGO_PKG_VERSION")))
            .with_instructions("Read, create and drive Agent of Empires sessions.")
    }

    /// `send_to_parent` is listed only to a caller session with a live parent.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        let mut tools = self.tool_router.list_all();
        if self.caller_parent(&context).await.is_none() {
            tools.retain(|tool| tool.name != "send_to_parent");
        }
        Ok(ListToolsResult::with_all_items(tools))
    }
}

/// The session that made this request with its session credential, if any.
fn caller_session_id(context: &RequestContext<RoleServer>) -> Option<String> {
    context
        .extensions
        .get::<axum::http::request::Parts>()
        .and_then(|parts| parts.extensions.get::<AuthenticatedCallerSession>())
        .map(|AuthenticatedCallerSession(id)| id.clone())
}

/// A tool error the model can read and act on: `isError` with a stable `code`.
fn tool_error(code: &str, message: impl Into<String>) -> CallToolResult {
    CallToolResult::structured_error(serde_json::json!({
        "code": code,
        "message": message.into(),
    }))
}

/// `/mcp` has no read-only layer, so each mutating tool refuses here, in the HTTP wording.
fn read_only_refusal(state: &AppState) -> Option<CallToolResult> {
    state
        .read_only
        .then(|| tool_error("read_only", super::api::READ_ONLY_MESSAGE))
}

pub(super) fn service(
    state: Arc<AppState>,
) -> StreamableHttpService<impl ServerHandler, NeverSessionManager> {
    StreamableHttpService::new(
        move || Ok(McpServer::new(state.clone())),
        Arc::default(),
        // `rmcp` reads the body itself, so the router's `DefaultBodyLimit` does not apply.
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts()
            .with_max_request_body_bytes(1024 * 1024),
    )
}

#[cfg(test)]
mod tests;
