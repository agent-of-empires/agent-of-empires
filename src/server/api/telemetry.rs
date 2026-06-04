//! Telemetry consent endpoints.
//!
//! The browser never posts to the telemetry backend (that would leak its IP /
//! User-Agent and create a second identity surface). Instead it manages the
//! opt-in state through the local daemon, which owns the install id and does
//! all sending. `seen` lets the web UI report that the dashboard / cockpit was
//! opened so the daemon's next snapshot can carry the `usage_seen` map.

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
    /// Optional coarse client form-factor (`"desktop"` / `"desktop_pwa"` /
    /// `"mobile"` / `"mobile_pwa"`). Absent on older clients; any value outside
    /// the closed allowlist is rejected, never stored. See
    /// `telemetry::form_factor` and #1883.
    #[serde(default)]
    form_factor: Option<String>,
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
    // Validate an optional form-factor up front so a non-allowlisted value (a
    // user-agent string, a screen size, a typo) is rejected before any counter
    // moves, the way an unknown surface is. Absent on older clients.
    let form_factor = match req.form_factor.as_deref() {
        Some(value) => match crate::telemetry::form_factor::parse(value) {
            Some(ff) => Some(ff),
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "bad_form_factor", "message": format!("unknown form_factor '{value}'")})),
                )
                    .into_response();
            }
        },
        None => None,
    };

    // Validate + count the surface against the allowlisted registry; an off-list
    // name is rejected and never creates a counter, so it can never reach a
    // snapshot. This is the open count for the surface.
    if !state.telemetry_usage_seen.record(&req.surface) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bad_surface", "message": format!("unknown surface '{}'", req.surface)})),
        )
            .into_response();
    }

    // Layer the per-form-factor class onto the browser surfaces. The registry
    // already counted the open; this records which client class it came from.
    if let Some(ff) = form_factor {
        match req.surface.as_str() {
            "web" => state.telemetry_web_clients.increment(ff),
            "cockpit" => state.telemetry_cockpit_clients.increment(ff),
            _ => {}
        }
    }
    StatusCode::NO_CONTENT.into_response()
}
