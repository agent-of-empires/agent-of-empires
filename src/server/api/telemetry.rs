//! Telemetry consent endpoints.
//!
//! The browser never posts to the telemetry backend (that would leak its IP /
//! User-Agent and create a second identity surface). Instead it manages the
//! opt-in state through the local daemon, which owns the install id and does
//! all sending. `seen` lets the web UI report that the dashboard / cockpit was
//! opened so the daemon's next snapshot can carry `web_seen` / `cockpit_seen`.

use std::sync::atomic::Ordering;
use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::AppState;

#[derive(Serialize)]
pub struct TelemetryStatus {
    /// `config.telemetry.enabled`.
    enabled: bool,
    /// Whether the user has answered the opt-in prompt (drives whether the
    /// web consent modal should show).
    responded: bool,
    /// `DO_NOT_TRACK` is set; the toggle is forced off and nothing is sent.
    do_not_track: bool,
}

fn current_status() -> TelemetryStatus {
    let config = crate::session::Config::load_or_warn();
    TelemetryStatus {
        enabled: config.telemetry.enabled,
        responded: config.app_state.has_responded_to_telemetry,
        do_not_track: crate::telemetry::do_not_track(),
    }
}

pub async fn get_telemetry_status() -> impl IntoResponse {
    (StatusCode::OK, Json(current_status())).into_response()
}

#[derive(Deserialize)]
pub struct ConsentRequest {
    enabled: bool,
}

pub async fn set_telemetry_consent(
    State(state): State<Arc<AppState>>,
    body: Result<Json<ConsentRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let mut config = crate::session::Config::load_or_warn();
    config.telemetry.enabled = req.enabled;
    config.app_state.has_responded_to_telemetry = true;
    if let Err(e) = crate::session::save_config(&config) {
        tracing::error!(target: "http.api.telemetry", "failed to save telemetry consent: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "save_failed", "message": "Failed to save telemetry setting"})),
        )
            .into_response();
    }
    // Reconcile the install id (no-op under DO_NOT_TRACK). The daemon, not the
    // browser, owns the id.
    crate::telemetry::apply_opt_in_change(req.enabled);
    (StatusCode::OK, Json(current_status())).into_response()
}

#[derive(Deserialize)]
pub struct SeenRequest {
    /// `"web"` or `"cockpit"`.
    surface: String,
}

/// Record that the web dashboard / cockpit web UI was opened. Folded into the
/// daemon's next opt-in snapshot. Returns 204 on success; the client need not
/// branch on consent state (the daemon only sends the flag when opted in).
pub async fn post_telemetry_seen(
    State(state): State<Arc<AppState>>,
    body: Result<Json<SeenRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    match req.surface.as_str() {
        "web" => state.telemetry_web_seen.store(true, Ordering::Relaxed),
        "cockpit" => state.telemetry_cockpit_seen.store(true, Ordering::Relaxed),
        other => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "bad_surface", "message": format!("unknown surface '{other}'")})),
            )
                .into_response();
        }
    }
    StatusCode::NO_CONTENT.into_response()
}
