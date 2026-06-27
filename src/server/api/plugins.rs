//! Plugin management REST API: list plugins and enable/disable them. The web
//! twin of `aoe plugin`.
//!
//! The enable/disable toggle is a mutation that runs on the host, so it
//! requires read-write mode AND an elevated session when login is enabled,
//! mirroring the requires-elevation settings fields.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

use super::AppState;
use crate::plugin;
use crate::server::auth::AuthenticatedSession;

fn error_response(status: StatusCode, code: &str, message: String) -> Response {
    (status, Json(json!({ "error": code, "message": message }))).into_response()
}

/// Resolve the read-only and elevation gates shared by every mutation.
async fn mutation_gate(
    state: &AppState,
    session: Option<&AuthenticatedSession>,
) -> Result<(), Response> {
    if state.read_only {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode".into(),
        ));
    }
    let elevated = if state.login_manager.is_enabled() {
        match session {
            Some(AuthenticatedSession(id)) => state.login_manager.is_elevated(id).await,
            None => false,
        }
    } else {
        true
    };
    if !elevated {
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "elevation_required",
            "Re-enter the passphrase to continue".into(),
        ));
    }
    Ok(())
}

/// `GET /api/plugins`: every known plugin plus load errors.
pub async fn list_plugins() -> Json<serde_json::Value> {
    let registry = plugin::registry();
    Json(json!({
        "plugins": registry.all().iter().map(|p| p.view()).collect::<Vec<_>>(),
        "load_errors": registry.load_errors(),
    }))
}

/// `GET /api/plugins/ui-state`: the plugin host's aggregated UI-state snapshot
/// (the slots workers have pushed, plus the notification ring). Empty when no
/// host is running (read-only mode, or a TUI-only build with no daemon). The
/// dashboard polls this alongside `/api/sessions` and renders each slot itself.
pub async fn plugin_ui_state(
    State(state): State<std::sync::Arc<AppState>>,
) -> Json<serde_json::Value> {
    let empty = || json!({ "entries": [], "notifications": [] });
    match state.plugin_host.as_ref().map(|h| h.ui_snapshot()) {
        Some(snapshot) => Json(serde_json::to_value(snapshot).unwrap_or_else(|e| {
            // Serializing the snapshot should never fail; if it somehow does,
            // keep the response shape stable rather than returning JSON null.
            tracing::warn!(target: "serve.api", "failed to serialize plugin UI snapshot: {e}");
            empty()
        })),
        None => Json(empty()),
    }
}

/// `GET /api/plugins/updates`: which installed external plugins have an update
/// available. An explicit, on-demand network check (the dashboard "Check for
/// updates" button), kept off the always-on `GET /api/plugins` list path so a
/// settings render never blocks on git/network. Allowed in read-only mode: it
/// reads remote state and mutates nothing.
pub async fn plugin_updates() -> Json<serde_json::Value> {
    Json(json!({ "updates": plugin::update_check::outdated().await }))
}

#[derive(Deserialize)]
pub struct DiscoverQuery {
    #[serde(default)]
    pub q: Option<String>,
}

/// `GET /api/plugins/discover?q=`: search the `aoe-plugin` GitHub topic. The
/// dashboard "Search GitHub" button. Browse-only: the dashboard has no install
/// path (capability approval needs a terminal), so each result carries an
/// `install_command` the user copies. On a GitHub failure (notably the
/// unauthenticated search rate limit) the message is returned for the UI to
/// show, rather than a generic 500.
pub async fn plugin_discover(Query(query): Query<DiscoverQuery>) -> Response {
    match plugin::discover::discover(query.q.as_deref()).await {
        Ok(results) => Json(json!({ "results": results })).into_response(),
        Err(e) => error_response(StatusCode::BAD_GATEWAY, "discover_failed", format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct DetailsQuery {
    pub source: String,
}

/// `GET /api/plugins/details?source=gh:owner/repo`: the on-demand detail for one
/// plugin source (manifest fields + release tags) backing the dashboard detail
/// modal. Allowed in read-only mode; reads remote state and mutates nothing.
pub async fn plugin_details(Query(query): Query<DetailsQuery>) -> Response {
    match plugin::discover::details(&query.source).await {
        Ok(detail) => Json(detail).into_response(),
        // `details()` only hard-errors on an invalid / unsupported `source`; a
        // GitHub fetch failure is reported in-band (manifest_error / empty
        // release tags), so a hard error here is bad client input, not an
        // upstream outage.
        Err(e) => error_response(StatusCode::BAD_REQUEST, "invalid_source", format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct PluginActionBody {
    /// The worker method to invoke (the plugin names it in its pane's action
    /// block, e.g. `github.refresh`).
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

/// `POST /api/plugins/{id}/action`: forward a dashboard UI action (a pane
/// button) to the plugin's worker as a fire-and-forget JSON-RPC notification.
/// The worker is the trust boundary: it acts only on methods it implements and
/// ignores the rest, so this never waits for or returns a worker result.
///
/// Gated on read-write mode only, not elevation. Unlike enable/disable, a pane
/// action does not mutate host-managed state (config, registry, grants,
/// lockfile) and grants no new host capability, so it does not warrant the
/// passphrase step-up, the same reasoning as `update_theme` in `system.rs`.
/// A routine `github.refresh` should not prompt for the passphrase.
pub async fn invoke_plugin_action(
    State(state): State<std::sync::Arc<AppState>>,
    Path(id): Path<String>,
    Json(body): Json<PluginActionBody>,
) -> Response {
    if state.read_only {
        return error_response(
            StatusCode::FORBIDDEN,
            "read_only",
            "Server is in read-only mode".into(),
        );
    }
    let Some(host) = state.plugin_host.as_ref() else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "no_host",
            "Plugin host is not running".into(),
        );
    };
    if host.notify_worker(&id, &body.method, body.params).await {
        (StatusCode::ACCEPTED, Json(json!({ "ok": true }))).into_response()
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            "no_worker",
            format!("No running worker for plugin {id}"),
        )
    }
}

#[derive(Deserialize)]
pub struct SetEnabledBody {
    pub enabled: bool,
}

/// `POST /api/plugins/{id}/enabled`
pub async fn set_plugin_enabled(
    State(state): State<std::sync::Arc<AppState>>,
    session: Option<axum::Extension<AuthenticatedSession>>,
    Path(id): Path<String>,
    Json(body): Json<SetEnabledBody>,
) -> Response {
    if let Err(resp) = mutation_gate(&state, session.as_deref()).await {
        return resp;
    }
    let result =
        tokio::task::spawn_blocking(move || plugin::install::set_enabled(&id, body.enabled)).await;
    match result {
        Ok(Ok(())) => list_plugins().await.into_response(),
        Ok(Err(e)) => error_response(StatusCode::BAD_REQUEST, "plugin_error", format!("{e:#}")),
        Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, "internal", e.to_string()),
    }
}
