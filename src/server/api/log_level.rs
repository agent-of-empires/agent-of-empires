//! REST handlers for runtime log-level control.
//!
//! Two endpoints under `/api/log-level`:
//!   - GET → current filter directive, reload availability
//!   - PATCH → swap filter via `{level}` (expanded to known roots) or
//!     `{filter}` (raw EnvFilter syntax with regex disabled)
//!
//! Backed by the process-global `FilterController` in `crate::logging`,
//! not `AppState`. Same module is reused by the runner subprocess so
//! a future runner-side IPC can swap filters with identical semantics.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::logging::{self, LogFilterError, LogLevel};

use super::AppState;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchRequest {
    pub level: Option<String>,
    pub filter: Option<String>,
}

#[derive(Serialize)]
pub struct LogLevelResponse {
    pub previous: String,
    pub current: String,
    pub ephemeral: bool,
}

#[derive(Serialize)]
pub struct LogLevelStatus {
    pub current: Option<String>,
    pub reloadable: bool,
    pub ephemeral: bool,
}

pub async fn get_log_level(State(_state): State<Arc<AppState>>) -> Json<LogLevelStatus> {
    Json(LogLevelStatus {
        current: logging::current_filter(),
        reloadable: logging::controller().is_some(),
        ephemeral: true,
    })
}

pub async fn patch_log_level(
    State(state): State<Arc<AppState>>,
    req: Result<Json<PatchRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<LogLevelResponse>, (StatusCode, String)> {
    if state.read_only {
        return Err((StatusCode::FORBIDDEN, "Server is in read-only mode".into()));
    }
    let Json(req) = req.map_err(|rej| (rej.status(), rej.body_text()))?;
    let result = match (req.level.as_deref(), req.filter.as_deref()) {
        (Some(_), Some(_)) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "specify exactly one of {\"level\", \"filter\"}".into(),
            ));
        }
        (None, None) => {
            return Err((
                StatusCode::BAD_REQUEST,
                "missing field; specify {\"level\"} or {\"filter\"}".into(),
            ));
        }
        (Some(level), None) => {
            let parsed = LogLevel::parse(level).ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    format!("unknown level {level:?}; expected trace|debug|info|warn|error"),
                )
            })?;
            logging::set_level(parsed)
        }
        (None, Some(filter)) => logging::set_filter(filter),
    };

    match result {
        Ok(swap) => {
            // Skip the log and persist on a no-op swap so an unchanged
            // directive cannot feed the file-watch loop (#1894).
            if swap.changed {
                tracing::info!(
                    target: "log.runtime",
                    previous = %swap.previous,
                    current = %swap.current,
                    source = "rest",
                    "filter swapped"
                );
                if let Ok(app_dir) = crate::session::get_app_dir() {
                    logging::persist_runtime_filter(&swap.current, &app_dir);
                }
            }
            Ok(Json(LogLevelResponse {
                previous: swap.previous,
                current: swap.current,
                ephemeral: true,
            }))
        }
        Err(LogFilterError::Unavailable) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "log subscriber not initialized in reloadable mode".into(),
        )),
        Err(LogFilterError::BareGlobalLevel) => Err((
            StatusCode::BAD_REQUEST,
            "bare global level not accepted in filter mode; use {\"level\": ...} instead".into(),
        )),
        Err(LogFilterError::Invalid(msg)) => {
            Err((StatusCode::BAD_REQUEST, format!("invalid filter: {msg}")))
        }
    }
}
