//! Telemetry consent endpoints.
//!
//! The browser never posts to the telemetry backend, which would leak its IP
//! and User-Agent and create a second identity surface. It manages opt-in state
//! through the local daemon, which owns the install id and does all sending.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::AppState;
use super::{api_error, read_only_response};

#[derive(Serialize)]
pub struct TelemetryStatus {
    /// `config.telemetry.enabled`.
    enabled: bool,
    /// Whether the user has answered the opt-in prompt, which drives the web
    /// consent modal.
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
        return read_only_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    if let Err(e) = crate::session::update_config(|config| {
        config.telemetry.enabled = req.enabled;
    }) {
        tracing::error!(target: "http.api.telemetry", "failed to save telemetry consent: {e}");
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            "Failed to save telemetry setting",
        );
    }
    if let Err(e) = crate::session::update_app_state(|state| {
        state.has_responded_to_telemetry = true;
    }) {
        tracing::error!(target: "http.api.telemetry", "failed to save telemetry consent: {e}");
        return api_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "save_failed",
            "Failed to save telemetry setting",
        );
    }
    // Reconcile the install id (no-op under DO_NOT_TRACK). The daemon, not the
    // browser, owns the id.
    crate::telemetry::apply_opt_in_change(req.enabled);
    (StatusCode::OK, Json(current_status())).into_response()
}

#[derive(Deserialize)]
pub struct SeenRequest {
    /// `"web"` or `"structured_view"`.
    surface: String,
    /// Optional coarse client form-factor. Absent on older clients; any value
    /// outside the closed allowlist is rejected, never stored (#1883).
    #[serde(default)]
    form_factor: Option<String>,
}

/// Record that the web dashboard or acp web UI was opened, folded into the
/// daemon's next opt-in snapshot. Returns 204; the client need not branch on
/// consent state, since the daemon only sends the flag when opted in.
pub async fn post_telemetry_seen(
    State(state): State<Arc<AppState>>,
    body: Result<Json<SeenRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    // Validate an optional form-factor up front, so a non-allowlisted value is
    // rejected before any counter moves, the way an unknown surface is.
    let form_factor = match req.form_factor.as_deref() {
        Some(value) => match crate::telemetry::form_factor::parse(value) {
            Some(ff) => Some(ff),
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "bad_form_factor",
                    format!("unknown form_factor '{value}'"),
                );
            }
        },
        None => None,
    };

    // Validate and count the surface against the allowlisted registry; an
    // off-list name never creates a counter, so it can never reach a snapshot.
    if !state.telemetry_usage_seen.record(&req.surface) {
        return api_error(
            StatusCode::BAD_REQUEST,
            "bad_surface",
            format!("unknown surface '{}'", req.surface),
        );
    }

    // Layer the per-form-factor class onto the browser surfaces: the registry
    // already counted the open, this records which client class it came from.
    if let Some(ff) = form_factor {
        match req.surface.as_str() {
            "web" => state.telemetry_web_clients.increment(ff),
            "structured_view" => state.telemetry_structured_clients.increment(ff),
            _ => {}
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(Deserialize)]
pub struct StructuredInteractionRequest {
    /// Allowlisted interaction kind. Only `"prompt_queued"` today; an open
    /// string, so adding a kind is a one-line match arm here.
    kind: String,
}

/// Report a browser acp interaction for the daemon's next opt-in snapshot.
/// Only queued prompts come through here; the other interaction signals are
/// tallied daemon-side in their REST handlers. Returns 204.
pub async fn post_telemetry_structured_interaction(
    State(state): State<Arc<AppState>>,
    body: Result<Json<StructuredInteractionRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return read_only_response();
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };
    match req.kind.as_str() {
        "prompt_queued" => {
            state
                .telemetry_structured
                .prompts_queued
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        other => {
            return api_error(
                StatusCode::BAD_REQUEST,
                "bad_kind",
                format!("unknown interaction kind '{other}'"),
            );
        }
    }
    StatusCode::NO_CONTENT.into_response()
}
