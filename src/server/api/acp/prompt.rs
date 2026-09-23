//! Turn submission, cancellation, and approval/elicitation answers.
//!
//! Every turn-starting handler claims the session's prompt submission guard
//! before its first side effect, so a concurrent `/acp/cancel` (which claims the
//! same guard) can never overtake the prompt it is meant to stop.

use crate::acp::approvals::Nonce;
use crate::acp::dispatch::{PromptDispatch, QueueReason};
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::protocol::{
    ApprovalDecisionWire, DiffCommentsPromptRequest, PromptRequest, ResolveApprovalRequest,
};
use crate::server::session_service::{PromptTouch, SendTurnError, SendTurnRequest, SessionCaller};

use super::*;

/// A cancel that waited this long on the submission guard is logged.
const CANCEL_SUBMISSION_WAIT_WARN: std::time::Duration = std::time::Duration::from_millis(500);

/// `POST /api/sessions/{id}/acp/prompt` success body, e.g.
/// `{"disposition":"queued","reason":"cancelling","queued_id":"..."}`.
#[derive(Debug, serde::Serialize)]
pub struct PromptDispatchResponse {
    #[serde(flatten)]
    pub dispatch: PromptDispatch,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_id: Option<String>,
}

/// 503 for a worker that could not be resumed; the client retries.
fn resume_failed_response(err: &SupervisorError) -> Response {
    let body = match err {
        SupervisorError::CapacityFull { current, limit } => {
            format!("worker_capacity_full ({current}/{limit})")
        }
        e => format!("worker_not_ready: {e}"),
    };
    (StatusCode::SERVICE_UNAVAILABLE, body).into_response()
}

fn worker_not_ready() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, "worker_not_ready").into_response()
}

/// `no_revive` refused: reviving (archived/snoozed/idle-dormant wake, or a
/// stopped worker) was required to accept this prompt and the caller asked
/// not to. Distinct from `worker_not_ready`, which is transient and worth
/// retrying; this is a standing precondition until something else revives
/// the session.
fn no_revive_refused() -> Response {
    (
        StatusCode::CONFLICT,
        "no_revive: reviving the session is required to accept this prompt",
    )
        .into_response()
}

pub async fn acp_prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<PromptRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    let woke_idle_dormant = match state
        .session_service
        .touch_and_wake_on_prompt(&id, req.no_revive)
        .await
    {
        PromptTouch::Touched { idle_dormant } => idle_dormant,
        PromptTouch::RevivalRefused => return no_revive_refused(),
    };
    // Validated before publishing or resuming, so a rejected prompt leaves no
    // trace in the transcript and spawns no worker.
    let attachments = match validate_attachments(&state, &id, &req.attachments) {
        Ok(a) => a,
        Err((code, msg)) => return (code, msg).into_response(),
    };
    // Decided under the guard so the decision and its dispatch are atomic
    // (#3621). The wake already cleared the dormant marker, hence the flag.
    let dispatch = state
        .session_service
        .prompt_dispatch_under_submission(&id, woke_idle_dormant)
        .await;
    // Refused before touching the pending-turn/queue state below, so a
    // rejected prompt leaves both untouched (#4081 review).
    if req.no_revive
        && matches!(
            dispatch,
            PromptDispatch::Queued {
                reason: QueueReason::WorkerDown
            }
        )
    {
        return no_revive_refused();
    }
    if let PromptDispatch::Queued { reason } = dispatch {
        // A fresh prompt supersedes a queued rate-limit continuation (#3028).
        // Only once queueing is certain: a `no_revive` refusal above must
        // leave it in place (#4081 review).
        state.session_service.clear_pending_initial_turn(&id).await;
        let prompt_id = req
            .prompt_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        return match super::super::queue::buffer_and_enqueue(
            &state,
            &id,
            &prompt_id,
            req.text.clone(),
            &attachments,
            None,
            chrono::Utc::now().to_rfc3339(),
        )
        .await
        {
            Ok(entry) => (
                StatusCode::ACCEPTED,
                Json(PromptDispatchResponse {
                    dispatch,
                    queued_id: Some(entry.id),
                }),
            )
                .into_response(),
            Err((status, msg)) => {
                tracing::warn!(
                    target: "http.api.acp",
                    session = %id,
                    ?reason,
                    "prompt dispatch chose the queue but enqueue failed: {msg}"
                );
                (status, msg).into_response()
            }
        };
    }
    let outcome = state
        .session_service
        .send_turn(
            &SessionCaller::User,
            &id,
            SendTurnRequest {
                text: &req.text,
                attachments: &attachments,
                woke_idle_dormant,
                prompt_id: req.prompt_id.clone(),
                synthesized: false,
                no_revive: req.no_revive,
            },
        )
        .await;
    // A fresh prompt supersedes a queued rate-limit continuation (#3028),
    // but only once delivery is certain: a `no_revive` refusal must leave
    // the pending turn in place (#4081 review).
    if !matches!(outcome, Err(SendTurnError::RevivalRefused)) {
        state.session_service.clear_pending_initial_turn(&id).await;
    }
    match outcome {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(PromptDispatchResponse {
                dispatch,
                queued_id: None,
            }),
        )
            .into_response(),
        Err(SendTurnError::SessionNotFound) => session_not_found(),
        Err(SendTurnError::ResumeFailed(e)) => resume_failed_response(&e),
        Err(SendTurnError::WorkerNotReady) => worker_not_ready(),
        Err(SendTurnError::RevivalRefused) => no_revive_refused(),
        // Unreachable for a User caller.
        Err(SendTurnError::NotOwner) => {
            (StatusCode::FORBIDDEN, "session not owned by caller").into_response()
        }
        Err(SendTurnError::ModeApplication(e)) => {
            supervisor_error_response("mode application failed", &e)
        }
        Err(SendTurnError::Send(e)) => supervisor_error_response("prompt failed", &e),
    }
}

/// Why a diff-comments prompt cannot start a turn now; the dialog keeps the
/// comments so the user can retry.
fn diff_comments_not_now(reason: QueueReason) -> Response {
    let message = match reason {
        QueueReason::WorkerDown => return worker_not_ready(),
        QueueReason::TurnActive => {
            "the agent is mid-turn; wait for it to finish, then send the comments again"
        }
        QueueReason::Cancelling => {
            "the turn is still cancelling; send the comments again once it stops"
        }
        QueueReason::Compacting => {
            "the agent is compacting its context; send the comments again when it finishes"
        }
    };
    (StatusCode::CONFLICT, message).into_response()
}

/// `POST /api/sessions/{id}/acp/prompt/diff-comments`: records a typed
/// `UserDiffCommentsPrompt` for the transcript and forwards only
/// `assembled_markdown` to the agent.
pub async fn acp_prompt_diff_comments(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<DiffCommentsPromptRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(&id)
        .await
    else {
        return session_not_found();
    };
    let woke_idle_dormant = state
        .session_service
        .touch_and_wake_on_prompt(&id, false)
        .await
        .idle_dormant();
    let dispatch = state
        .session_service
        .prompt_dispatch_under_submission(&id, woke_idle_dormant)
        .await;
    // There is no queue row for a typed review, so refuse rather than publish
    // a card the agent would then reject as busy.
    if let PromptDispatch::Queued { reason } = dispatch {
        return diff_comments_not_now(reason);
    }
    // Idle-dormant sessions and rate-limit parks are sendable with no live worker.
    let needs_resume = woke_idle_dormant || !state.acp_supervisor.is_running(&id).await;
    if needs_resume {
        use crate::server::acp_reconciler::ResumeTrigger;
        match crate::server::acp_reconciler::trigger_resume_background(&state.session_service, &id)
            .await
        {
            Ok(ResumeTrigger::NotFound) => return session_not_found(),
            Ok(_) => {}
            Err(e) => return resume_failed_response(&e),
        }
    }
    // Publish only once a worker exists, or the card strands on "running" (#3172).
    if let Err(e) = state.acp_supervisor.wait_until_ready(&id).await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("worker_not_ready: {e}"),
        )
            .into_response();
    }
    state
        .acp_supervisor
        .publish_user_diff_comments_prompt(
            &id,
            req.intro,
            req.outro,
            req.is_multi_repo,
            req.comments,
            req.assembled_markdown.clone(),
        )
        .await;
    match state
        .acp_supervisor
        .send_prompt(&id, &req.assembled_markdown, &[])
        .await
    {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(SupervisorError::UnknownSession(_)) if needs_resume => worker_not_ready(),
        Err(e) => supervisor_error_response("prompt failed", &e),
    }
}

/// Serve a persisted prompt attachment, scoped by session id.
pub async fn acp_attachment(
    State(state): State<Arc<AppState>>,
    Path((id, attachment_id)): Path<(String, String)>,
) -> impl IntoResponse {
    use axum::http::header;
    match state.acp_event_store.load_attachment(&id, &attachment_id) {
        Some((mime, bytes)) => (
            [
                (header::CONTENT_TYPE, mime),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                (
                    header::CACHE_CONTROL,
                    "private, max-age=31536000, immutable".to_string(),
                ),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "attachment not found").into_response(),
    }
}

/// Cancel the in-flight turn, ordered behind any prompt submission in flight.
/// The guard can also be held by a queue drain or a rename, so a long wait is
/// logged: the UI shows no progress meanwhile.
pub async fn acp_cancel(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let waited_from = std::time::Instant::now();
    let _submission = state
        .session_service
        .prompt_submission_for_session(&id)
        .await;
    let waited = waited_from.elapsed();
    if waited >= CANCEL_SUBMISSION_WAIT_WARN {
        tracing::warn!(
            target: "http.api.acp",
            session = %id,
            waited_ms = waited.as_millis(),
            "cancel waited on the session's prompt submission; the UI showed no \
             progress for that long"
        );
    }
    match state.acp_supervisor.cancel_prompt(&id).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        Err(e) => supervisor_error_response("cancel failed", &e),
    }
}

/// Escape hatch for a stuck spinner (#1100). Deliberately skips the submission
/// guard so it never queues.
pub async fn acp_force_end_turn(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    state.acp_supervisor.force_end_turn(&id).await;
    StatusCode::ACCEPTED.into_response()
}

pub async fn resolve_approval(
    State(state): State<Arc<AppState>>,
    Path((id, nonce_str)): Path<(String, String)>,
    req: Result<Json<ResolveApprovalRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let nonce = Nonce(nonce_str.clone());
    // A dismissal must not go through option matching, which would send the
    // first reject option as the user's answer (#3741).
    let outcome = match req.decision {
        ApprovalDecisionWire::Cancelled => state.acp_supervisor.cancel_permission(&id, nonce).await,
        decision => {
            state
                .acp_supervisor
                .resolve_permission(&id, nonce, decision.into(), req.option_id)
                .await
        }
    };
    match outcome {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        // The nonce echo lets clients match the 404 to the card (#1821).
        Err(SupervisorError::Acp(AcpError::UnknownNonce)) => (
            StatusCode::NOT_FOUND,
            format!("no pending approval with nonce {nonce_str}"),
        )
            .into_response(),
        Err(e) => supervisor_error_response("resolve failed", &e),
    }
}

/// Resolve a pending `AskUserQuestion` elicitation.
pub async fn resolve_elicitation(
    State(state): State<Arc<AppState>>,
    Path((id, nonce_str)): Path<(String, String)>,
    req: Result<Json<ElicitationResolution>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(resolution) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let nonce = Nonce(nonce_str.clone());
    match state
        .acp_supervisor
        .resolve_elicitation(&id, nonce, resolution)
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(SupervisorError::Acp(AcpError::UnknownNonce)) => (
            StatusCode::NOT_FOUND,
            format!("no pending elicitation with nonce {nonce_str}"),
        )
            .into_response(),
        // The elicitation stays pending, so the client can correct and resubmit.
        Err(SupervisorError::Acp(AcpError::InvalidAnswer(msg))) => {
            (StatusCode::UNPROCESSABLE_ENTITY, msg).into_response()
        }
        Err(e) => supervisor_error_response("resolve failed", &e),
    }
}

#[cfg(test)]
mod tests;
