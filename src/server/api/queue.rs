//! Server-owned prompt-queue HTTP handlers.
//!
//! The structured-view prompt queue's source of truth is the daemon (see
//! `docs/development/server-side-prompt-queue.md`), so a follow-up queued
//! behind a busy turn survives a client reload / closed PWA and drains
//! server-side. These handlers are the client's view/editor of that queue;
//! the drain and force-send-now live in the reconciler / supervisor.
//!
//! Attachments ride with a queued prompt: the enqueue POST carries the same
//! `PromptAttachmentUpload` shape as `/acp/prompt`, the bytes are validated and
//! buffered in the event store's pending-attachment table (keyed by the prompt
//! id, outside the seq-keyed retention prune), and the drain reloads and
//! forwards them. A per-session byte cap bounds how much a client can buffer.

use std::sync::Arc;

use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use super::acp::{read_only_block, validate_attachments};
use crate::acp::protocol::PromptAttachmentUpload;
use crate::acp::state::PromptAttachmentRef;
use crate::server::session_service::EditQueuedOutcome;
use crate::server::AppState;

/// Cap on total queued-attachment bytes buffered per session. Generous enough
/// for several image follow-ups (`/acp/prompt` caps one prompt at 20 MiB) while
/// bounding what an undrained queue can hold on disk.
const MAX_QUEUED_ATTACHMENT_BYTES_PER_SESSION: u64 = 64 * 1024 * 1024;

/// Cap on queue depth per session. The queue lives on the `Instance`, and every
/// mutation rewrites the whole profile session file, so depth costs disk I/O on
/// each enqueue rather than just memory. Well above any plausible run of
/// follow-ups a person lines up behind one turn.
const MAX_QUEUED_PROMPTS_PER_SESSION: usize = 100;

/// Cap on a single queued prompt's text. Matches nothing upstream because
/// `/acp/prompt` streams straight to the agent, while this text is persisted to
/// the session file and rewritten on every subsequent queue mutation. 256 KiB
/// is far past a pasted stack trace and still bounds that rewrite.
const MAX_QUEUED_TEXT_BYTES: usize = 256 * 1024;

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
    /// Image/file attachments to deliver with the prompt when it drains. Same
    /// untrusted wire shape as `/acp/prompt`; `#[serde(default)]` keeps
    /// text-only enqueues working unchanged.
    #[serde(default)]
    pub attachments: Vec<PromptAttachmentUpload>,
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
    // A text-only prompt may be empty ONLY if it carries attachments (an
    // image-only follow-up). A truly empty enqueue is rejected.
    if req.text.trim().is_empty() && req.attachments.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty prompt").into_response();
    }
    if req.text.len() > MAX_QUEUED_TEXT_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "queued prompt text exceeds the {} KiB limit",
                MAX_QUEUED_TEXT_BYTES / 1024
            ),
        )
            .into_response();
    }
    // Depth cap. Re-enqueuing an existing id replaces that row rather than
    // adding one, so it must not count against a full queue.
    {
        let queue = state.session_service.queued_prompts_snapshot(&id).await;
        if queue.len() >= MAX_QUEUED_PROMPTS_PER_SESSION && !queue.iter().any(|q| q.id == req.id) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                format!("queue is full ({MAX_QUEUED_PROMPTS_PER_SESSION} prompts)"),
            )
                .into_response();
        }
    }

    // Decode + validate + capability-gate the attachments exactly as the live
    // prompt path does (size / MIME / count caps, image magic-byte sniff).
    let blobs = match validate_attachments(&state, &id, &req.attachments) {
        Ok(b) => b,
        Err((status, msg)) => return (status, msg).into_response(),
    };
    let created_at = req
        .created_at
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    match buffer_and_enqueue(
        &state,
        &id,
        &req.id,
        req.text,
        &blobs,
        req.origin_device,
        created_at,
    )
    .await
    {
        Ok(entry) => (StatusCode::OK, Json(entry)).into_response(),
        Err((status, msg)) => (status, msg).into_response(),
    }
}

/// Buffer already-validated attachment blobs under `prompt_id` and append the
/// prompt to the session's server-owned queue.
///
/// Shared by `queue_enqueue` and the prompt endpoint's `Queued` disposition
/// (Tier 3), so a prompt the daemon decides to park is byte-for-byte the same
/// queue row a client would have created itself: same per-session cap, same
/// idempotent-by-id replace, same blob bookkeeping.
#[allow(clippy::too_many_arguments)]
pub(super) async fn buffer_and_enqueue(
    state: &Arc<AppState>,
    id: &str,
    prompt_id: &str,
    text: String,
    blobs: &[crate::acp::event_store::AttachmentBlob],
    origin_device: Option<String>,
    created_at: String,
) -> Result<crate::acp::state::QueuedPromptEntry, (StatusCode, String)> {
    // Per-session buffer cap: reject rather than let an undrained queue grow
    // without bound. Re-enqueuing the same id replaces its blobs, so subtract
    // what this prompt already holds before checking headroom.
    if !blobs.is_empty() {
        let incoming: u64 = blobs.iter().map(|b| b.data.len() as u64).sum();
        let existing_for_prompt: u64 = state
            .acp_event_store
            .load_pending_attachments_for_ref(id, prompt_id)
            .iter()
            .map(|b| b.data.len() as u64)
            .sum();
        let session_total = state.acp_event_store.pending_attachment_bytes(id);
        let projected = session_total.saturating_sub(existing_for_prompt) + incoming;
        if projected > MAX_QUEUED_ATTACHMENT_BYTES_PER_SESSION {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "queued attachments exceed the {} MiB per-session limit",
                    MAX_QUEUED_ATTACHMENT_BYTES_PER_SESSION / (1024 * 1024)
                ),
            ));
        }
    }

    // Buffer the bytes keyed by the prompt id, then hand the metadata-only refs
    // to the store. Re-enqueue is idempotent: clear any prior blobs for this id
    // first so a re-post with a different attachment set replaces cleanly.
    let refs: Vec<PromptAttachmentRef> = blobs
        .iter()
        .map(|b| PromptAttachmentRef {
            id: b.id.clone(),
            kind: b.kind,
            mime_type: b.mime_type.clone(),
            name: b.name.clone(),
            size: b.data.len() as u64,
        })
        .collect();
    // Unconditional, not gated on whether this request carried attachments:
    // `enqueue_prompt` replaces the row's refs with whatever came in, so a
    // re-enqueue that drops the attachments would otherwise orphan the prior
    // blobs, holding bytes against the per-session cap that nothing can ever
    // deliver or reclaim before the 24h sweep.
    state
        .acp_event_store
        .delete_pending_attachments_for_ref(id, prompt_id);
    for blob in blobs {
        state
            .acp_event_store
            .record_pending_attachment(id, prompt_id, blob);
    }

    match state
        .session_service
        .enqueue_prompt(
            id,
            prompt_id.to_string(),
            text,
            refs,
            origin_device,
            created_at,
        )
        .await
    {
        Some(entry) => Ok(entry),
        None => {
            // Session vanished between the existence check and the enqueue;
            // drop any blobs we just buffered so they don't leak.
            state
                .acp_event_store
                .delete_pending_attachments_for_ref(id, prompt_id);
            Err((StatusCode::NOT_FOUND, "session not found".to_string()))
        }
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
    // Same bound as enqueue: an edit is another write of this text into the
    // session file, so it cannot be a way around the cap.
    if req.text.len() > MAX_QUEUED_TEXT_BYTES {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "queued prompt text exceeds the {} KiB limit",
                MAX_QUEUED_TEXT_BYTES / 1024
            ),
        )
            .into_response();
    }
    match state
        .session_service
        .edit_queued_prompt(&id, prompt_id, req.text)
        .await
    {
        EditQueuedOutcome::Updated => StatusCode::NO_CONTENT.into_response(),
        EditQueuedOutcome::NotFound => {
            (StatusCode::NOT_FOUND, "queued prompt not found").into_response()
        }
        // Same rule as enqueue: a row with neither text nor attachments cannot
        // be delivered. The drain retires such a row rather than wedging on it
        // now, but silently discarding what the user typed is worse than a 400.
        EditQueuedOutcome::WouldEmpty => (StatusCode::BAD_REQUEST, "empty prompt").into_response(),
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
