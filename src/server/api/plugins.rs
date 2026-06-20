//! Plugin management REST API: list plugins and enable/disable them. The web
//! twin of `aoe plugin`.
//!
//! The enable/disable toggle is a mutation that runs on the host, so it
//! requires read-write mode AND an elevated session when login is enabled,
//! mirroring the requires-elevation settings fields.

use axum::extract::{Path, State};
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
