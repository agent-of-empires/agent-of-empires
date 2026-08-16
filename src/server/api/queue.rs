//! Server-owned prompt-queue HTTP handlers.
//!
//! The structured-view prompt queue's source of truth is the daemon (see
//! `docs/development/server-side-prompt-queue.md`), so a follow-up queued
//! behind a busy turn survives a client reload / closed PWA and drains
//! server-side. These handlers are the client's view/editor of that queue;
//! the drain and force-send-now live in the reconciler / supervisor.
//!
//! Attachments on a queued prompt are added in a later increment; enqueue is
//! text-only for now and rejects an empty prompt.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use super::acp::read_only_block;
use crate::server::AppState;

#[derive(Debug, Deserialize)]
pub struct EnqueueRequest {
    /// Client-minted stable id, so an optimistic UI row reconciles against the
    /// server row and a retry does not double-queue.
    pub id: String,
    pub text: String,
    /// RFC3339 enqueue time; the server stamps one if omitted.
    #[serde(default)]
    pub created_at: Option<String>,
    /// Optional provenance: which device queued it.
    #[serde(default)]
    pub origin_device: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct EditRequest {
    pub text: String,
}

async fn session_exists(state: &AppState, id: &str) -> bool {
    state.instances.read().await.iter().any(|i| i.id == id)
}

/// `POST /api/sessions/{id}/queue`: append a prompt to the server queue.
pub async fn queue_enqueue(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<EnqueueRequest>, JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    if !session_exists(&state, &id).await {
        return (StatusCode::NOT_FOUND, "session not found").into_response();
    }
    if req.text.trim().is_empty() {
        return (StatusCode::BAD_REQUEST, "empty prompt").into_response();
    }
    let created_at = req
        .created_at
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    match state
        .session_service
        .enqueue_prompt(&id, req.id, req.text, vec![], req.origin_device, created_at)
        .await
    {
        Some(entry) => (StatusCode::OK, Json(entry)).into_response(),
        None => (StatusCode::NOT_FOUND, "session not found").into_response(),
    }
}

/// `GET /api/sessions/{id}/queue`: the queue ordered by `seq`.
pub async fn queue_list(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    Json(state.session_service.queued_prompts_snapshot(&id).await).into_response()
}

/// `PATCH /api/sessions/{id}/queue/{promptId}`: replace a queued prompt's text.
pub async fn queue_edit(
    State(state): State<Arc<AppState>>,
    Path((id, prompt_id)): Path<(String, String)>,
    req: Result<Json<EditRequest>, JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    if state
        .session_service
        .edit_queued_prompt(&id, prompt_id, req.text)
        .await
    {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "queued prompt not found").into_response()
    }
}

/// `DELETE /api/sessions/{id}/queue/{promptId}`: remove one queued prompt.
pub async fn queue_remove(
    State(state): State<Arc<AppState>>,
    Path((id, prompt_id)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    if state
        .session_service
        .remove_queued_prompt(&id, prompt_id)
        .await
    {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (StatusCode::NOT_FOUND, "queued prompt not found").into_response()
    }
}

/// `DELETE /api/sessions/{id}/queue`: drop every queued prompt.
pub async fn queue_clear(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    state.session_service.clear_queued_prompts(&id).await;
    StatusCode::NO_CONTENT.into_response()
}
