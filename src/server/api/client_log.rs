//! Browser-side log relay.
//!
//! `POST /api/client-log` accepts a batch of structured entries and re-emits
//! them through `tracing` under target `web.client`, with the client-side module
//! name preserved as the `client_target` field. Caps and truncation are enforced
//! server-side because the frontend throttle is best-effort: a broken or
//! malicious client can POST directly.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;

use super::AppState;

const MAX_ENTRIES: usize = 50;
const MAX_MESSAGE: usize = 4096;
const MAX_STACK: usize = 16384;
const MAX_PATH: usize = 512;
const MAX_USER_AGENT: usize = 512;
const MAX_TARGET: usize = 64;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientLogEntry {
    pub level: String,
    pub message: String,
    pub stack: Option<String>,
    #[serde(rename = "componentStack")]
    pub component_stack: Option<String>,
    pub target: Option<String>,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    pub path: String,
    #[serde(rename = "userAgent")]
    pub user_agent: String,
    pub ts: i64,
    pub dropped: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientLogBatch {
    pub entries: Vec<ClientLogEntry>,
}

pub async fn post_client_log(
    State(_state): State<Arc<AppState>>,
    Json(batch): Json<ClientLogBatch>,
) -> Result<StatusCode, (StatusCode, String)> {
    if batch.entries.len() > MAX_ENTRIES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("max {MAX_ENTRIES} entries per batch"),
        ));
    }
    for entry in batch.entries {
        emit_event(entry);
    }
    Ok(StatusCode::NO_CONTENT)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        let mut out = s[..end].to_string();
        out.push('…');
        out
    }
}

fn sanitize_target(s: Option<&str>) -> String {
    let raw = s.unwrap_or("default");
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'))
        .take(MAX_TARGET)
        .collect()
}

fn emit_event(e: ClientLogEntry) {
    let message = truncate(&e.message, MAX_MESSAGE);
    let stack = e.stack.as_deref().map(|s| truncate(s, MAX_STACK));
    let component_stack = e.component_stack.as_deref().map(|s| truncate(s, MAX_STACK));
    let client_target = sanitize_target(e.target.as_deref());
    let path = truncate(&e.path, MAX_PATH);
    let ua = truncate(&e.user_agent, MAX_USER_AGENT);
    let session = e.session_id.as_deref().map(|s| truncate(s, 128));
    let dropped = e.dropped;
    let ts = e.ts;

    // Tracing macros need a static target, so `web.client` is fixed and the
    // dynamic client module name rides as a field that downstream filters can
    // match.
    match e.level.as_str() {
        "error" => tracing::error!(
            target: "web.client",
            client_target = %client_target,
            path = %path,
            ua = %ua,
            session = ?session,
            stack = ?stack,
            component_stack = ?component_stack,
            ts,
            dropped = ?dropped,
            "{message}"
        ),
        "warn" => tracing::warn!(
            target: "web.client",
            client_target = %client_target,
            path = %path,
            ua = %ua,
            session = ?session,
            stack = ?stack,
            component_stack = ?component_stack,
            ts,
            dropped = ?dropped,
            "{message}"
        ),
        "info" => tracing::info!(
            target: "web.client",
            client_target = %client_target,
            path = %path,
            ua = %ua,
            session = ?session,
            ts,
            "{message}"
        ),
        _ => tracing::debug!(
            target: "web.client",
            client_target = %client_target,
            path = %path,
            ua = %ua,
            session = ?session,
            ts,
            "{message}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_and_sanitize_bound_client_log_fields() {
        assert_eq!(truncate("hello", 100), "hello");
        let t = truncate(&"a".repeat(50), 10);
        assert!(t.starts_with("aaaaaaaaaa"));
        assert!(t.ends_with("…"));
        assert_eq!(sanitize_target(Some("ok.module-1")), "ok.module-1");
        assert_eq!(sanitize_target(Some("bad<script>")), "badscript");
        assert_eq!(sanitize_target(None), "default");
        assert_eq!(sanitize_target(Some(&"a".repeat(200))).len(), MAX_TARGET);
    }
}
