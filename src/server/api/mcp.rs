//! REST handlers for the unified MCP management surface (#1996).
//!
//! `GET /api/mcp/servers` resolves the effective MCP set for an agent and
//! returns the redaction-safe view the dashboard renders. The mutating routes
//! resolve a conflict and keep or drop a server removed from a native config,
//! and create, edit or delete AoE-owned global servers.
//! All values are redacted; AoE never writes an agent-native config.

use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    Json,
};
use serde::Deserialize;

use super::mutation_guard::ElevatedMutationGuard;
use super::read_only_response;
use super::{api_error, AppState};
use crate::session::config::profile_config;
use crate::session::mcp::mcp_model::{self, McpProvenance, ResolvedMcpServer};
use crate::session::mcp::mcp_overrides::{self, McpServerInput, McpWriteError};
use crate::session::mcp::mcp_state::{self, ConflictWinner};
use crate::session::mcp::project_mcp::ProjectMcpServer;

#[derive(Debug, Deserialize)]
pub struct AgentQuery {
    /// Agent whose effective set to resolve; defaults to the configured tool.
    agent: Option<String>,
}

/// Resolve the agent to inspect: explicit query value, else the profile's
/// configured default tool, else `claude`.
fn resolve_agent(state: &AppState, requested: Option<String>) -> String {
    requested.unwrap_or_else(|| {
        profile_config::resolve_config_or_warn(&state.profile)
            .session
            .default_tool
            .unwrap_or_else(|| "claude".to_string())
    })
}

/// `GET /api/mcp/servers?agent=<a>`: the effective set plus drift, redacted.
pub async fn get_mcp_servers(
    State(state): State<Arc<AppState>>,
    Query(query): Query<AgentQuery>,
) -> impl IntoResponse {
    let agent = resolve_agent(&state, query.agent);
    let profile = state.profile.clone();
    let result = tokio::task::spawn_blocking(move || {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let profile_opt = (!profile.is_empty()).then_some(profile.as_str());
        let view = mcp_model::resolve_surface(&agent, profile_opt, &cwd);
        serde_json::json!({
            "agent": agent,
            "effective": view.effective.iter().map(|s| s.redacted()).collect::<Vec<_>>(),
            "keptOnRemoval": view.kept_on_removal.iter().map(|s| s.redacted()).collect::<Vec<_>>(),
            "conflicts": view
                .conflicts
                .iter()
                .map(|c| serde_json::json!({
                    "name": c.current.name,
                    "agent": c.agent,
                    "previous": c.previous.redacted_summary(),
                    "current": c.current.redacted_summary(),
                    // Optimistic-concurrency token for the resolve endpoint.
                    "fingerprint": c.fingerprint(),
                }))
                .collect::<Vec<_>>(),
            "driftPaused": view.drift_paused,
        })
    })
    .await;

    match result {
        Ok(body) => Json(body).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "resolve_failed"})),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct ResolveConflictBody {
    agent: String,
    /// "aoe" keeps AoE's last-seen definition (promoted to global mcp.json);
    /// "native" accepts the native config's current definition.
    winner: String,
    /// The token the surface captured when it opened the modal; the resolution
    /// is rejected as stale if the snapshot changed since.
    fingerprint: String,
}

/// `POST /api/mcp/servers/{name}/resolve`: resolve a conflict (feature C).
pub async fn resolve_mcp_conflict(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    body: Result<Json<ResolveConflictBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    // CityHall renders the MCP servers tab read-only (display only).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Ok(Json(body)) = body else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "invalid_body"})),
        )
            .into_response();
    };
    let winner = match body.winner.as_str() {
        "aoe" => ConflictWinner::Aoe,
        "native" => ConflictWinner::Native,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_winner"})),
            )
                .into_response()
        }
    };

    let profile = state.profile.clone();
    let result = tokio::task::spawn_blocking(move || {
        // Re-resolve the current conflicts and find the one for `name`; the
        // fingerprint guard in resolve_conflict rejects a stale resolution.
        let profile_opt = (!profile.is_empty()).then_some(profile.as_str());
        let session_env = mcp_model::session_env_for_discovery(profile_opt);
        let read = mcp_model::load_native_mcp_servers_checked_from_home(
            &body.agent,
            profile_opt,
            &session_env,
            None,
        )?;
        let reconcile = mcp_state::reconcile_agent(&body.agent, &read)?;
        let Some(conflict) = reconcile
            .conflicts
            .into_iter()
            .find(|c| c.current.name == name)
        else {
            return Ok(mcp_state::ResolveStatus::Stale);
        };
        mcp_state::resolve_conflict(&conflict, winner, &body.fingerprint)
    })
    .await;

    match result {
        Ok(Ok(mcp_state::ResolveStatus::Applied)) => {
            Json(serde_json::json!({"status": "applied"})).into_response()
        }
        Ok(Ok(mcp_state::ResolveStatus::Stale)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"status": "stale"})),
        )
            .into_response(),
        _ => internal_error(),
    }
}

#[derive(Debug, Deserialize)]
pub struct AgentBody {
    agent: String,
}

/// `POST /api/mcp/servers/{name}/keep`: keep a removed server, promoting it
/// into the global `mcp.json`.
pub async fn keep_mcp_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    body: Result<Json<AgentBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    // CityHall renders the MCP servers tab read-only (display only).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Ok(Json(body)) = body else {
        return bad_body();
    };
    let result =
        tokio::task::spawn_blocking(move || mcp_state::keep_removed(&body.agent, &name)).await;
    match result {
        Ok(Ok(true)) => Json(serde_json::json!({"status": "kept"})).into_response(),
        Ok(Ok(false)) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"status": "not_found"})),
        )
            .into_response(),
        _ => internal_error(),
    }
}

/// `POST /api/mcp/servers/{name}/drop`: drop a kept-on-removal server without
/// promoting it (feature D).
pub async fn drop_mcp_server(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    body: Result<Json<AgentBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    // CityHall renders the MCP servers tab read-only (display only).
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Ok(Json(body)) = body else {
        return bad_body();
    };
    let result =
        tokio::task::spawn_blocking(move || mcp_state::forget_native(&body.agent, &name)).await;
    match result {
        Ok(Ok(())) => Json(serde_json::json!({"status": "dropped"})).into_response(),
        _ => internal_error(),
    }
}

#[derive(Debug, Deserialize)]
pub struct CreateMcpServerBody {
    name: String,
    #[serde(flatten)]
    input: McpServerInput,
}

type JsonBody<T> = Result<Json<T>, axum::extract::rejection::JsonRejection>;

fn write_error(error: McpWriteError) -> axum::response::Response {
    match error {
        McpWriteError::Invalid(m) => api_error(StatusCode::BAD_REQUEST, "invalid_mcp_server", m),
        McpWriteError::Exists(name) => api_error(
            StatusCode::CONFLICT,
            "mcp_server_exists",
            format!("an AoE-owned server named \"{name}\" already exists"),
        ),
        McpWriteError::NotFound(m) => api_error(StatusCode::NOT_FOUND, "mcp_server_not_found", m),
        McpWriteError::ReadOnly(m) => api_error(StatusCode::FORBIDDEN, "mcp_read_only", m),
        McpWriteError::Io(e) => {
            tracing::warn!(target: "acp.mcp", error = %e, "global mcp.json write failed");
            api_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                "could not write the global mcp.json",
            )
        }
    }
}

fn redacted_global(server: ProjectMcpServer) -> serde_json::Value {
    let resolved = ResolvedMcpServer {
        def: server,
        provenance: McpProvenance::Global,
        shadowed: Vec::new(),
    };
    serde_json::json!(resolved.redacted())
}

/// Runs a blocking global write, mapping a miss to read-only when another layer owns the name.
async fn run_write<R: Send + 'static>(
    state: &AppState,
    write: impl FnOnce() -> Result<R, McpWriteError> + Send + 'static,
) -> Result<R, McpWriteError> {
    let agent = resolve_agent(state, None);
    let profile = state.profile.clone();
    tokio::task::spawn_blocking(move || {
        write().map_err(|e| {
            let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
            let profile_opt = (!profile.is_empty()).then_some(profile.as_str());
            mcp_overrides::classify_missing(e, &agent, profile_opt, &cwd)
        })
    })
    .await
    .unwrap_or_else(|e| Err(McpWriteError::Io(e.into())))
}

fn bad_json() -> axum::response::Response {
    api_error(
        StatusCode::BAD_REQUEST,
        "invalid_body",
        "request body is not a valid MCP server definition",
    )
}

/// `POST /api/mcp/servers`: add an AoE-owned global server.
pub async fn create_mcp_server(
    State(state): State<Arc<AppState>>,
    _guard: ElevatedMutationGuard,
    body: JsonBody<CreateMcpServerBody>,
) -> impl IntoResponse {
    let Ok(Json(body)) = body else {
        return bad_json();
    };
    match run_write(&state, move || {
        mcp_overrides::create_global_server(&body.name, &body.input)
    })
    .await
    {
        Ok(server) => (StatusCode::CREATED, Json(redacted_global(server))).into_response(),
        Err(e) => write_error(e),
    }
}

/// `PUT /api/mcp/servers/{name}`: replace an AoE-owned global server. Secret entries without a
/// value keep their stored value.
pub async fn edit_mcp_server(
    State(state): State<Arc<AppState>>,
    _guard: ElevatedMutationGuard,
    Path(name): Path<String>,
    body: JsonBody<McpServerInput>,
) -> impl IntoResponse {
    let Ok(Json(input)) = body else {
        return bad_json();
    };
    match run_write(&state, move || {
        mcp_overrides::update_global_server(&name, &input)
    })
    .await
    {
        Ok(server) => Json(redacted_global(server)).into_response(),
        Err(e) => write_error(e),
    }
}

/// `DELETE /api/mcp/servers/{name}`: remove an AoE-owned global server.
pub async fn delete_mcp_server(
    State(state): State<Arc<AppState>>,
    _guard: ElevatedMutationGuard,
    Path(name): Path<String>,
) -> impl IntoResponse {
    match run_write(&state, move || mcp_overrides::delete_global_server(&name)).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => write_error(e),
    }
}

fn bad_body() -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": "invalid_body"})),
    )
        .into_response()
}

fn internal_error() -> axum::response::Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": "internal"})),
    )
        .into_response()
}
