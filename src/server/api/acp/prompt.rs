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
use crate::server::api::sessions::cityhall_block_non_structured;

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
    /// The prompt's event seq, when it was sent and recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
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

/// `no_revive` refused: reviving (snoozed/idle-dormant wake, or a
/// stopped worker) was required to accept this prompt and the caller asked
/// not to. Distinct from `worker_not_ready`, which is transient and worth
/// retrying; this is a standing precondition until something else revives
/// the session.
fn no_revive_refused() -> Response {
    (
        StatusCode::CONFLICT,
        crate::daemon::ApiErrorCode::NoRevive.header(),
        "no_revive: reviving the session is required to accept this prompt",
    )
        .into_response()
}

/// The queue row a "Send now" named is gone: the drain or another tab already delivered it.
fn queued_prompt_gone() -> Response {
    (StatusCode::CONFLICT, "queued_prompt_gone").into_response()
}

/// A queued row is sent as stored: other text would be accepted for, or credited to, a row
/// that holds something else.
fn queued_prompt_changed() -> Response {
    (
        StatusCode::CONFLICT,
        "queued_prompt_changed: the queued row differs from the request",
    )
        .into_response()
}

/// The row's attachment bytes expired; sending it now would drop them.
fn queued_attachments_missing() -> Response {
    (
        StatusCode::CONFLICT,
        "queued_attachments_missing: the row's attachment bytes are no longer stored",
    )
        .into_response()
}

pub async fn acp_prompt(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    req: Result<Json<PromptRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    let Json(req) = match req {
        Ok(j) => j,
        Err(rej) => return rej.into_response(),
    };
    let submission = PromptSubmission {
        text: &req.text,
        uploads: &req.attachments,
        prompt_id: req.prompt_id.clone(),
        queued_prompt_id: req.queued_prompt_id.clone(),
        no_revive: req.no_revive,
    };
    match submit_prompt(&state, &SessionCaller::User, &id, submission).await {
        Ok(PromptSubmitted {
            dispatch,
            queued_id,
            seq,
        }) => (
            StatusCode::ACCEPTED,
            Json(PromptDispatchResponse {
                dispatch,
                queued_id,
                seq,
            }),
        )
            .into_response(),
        Err(PromptSubmitError::SessionNotFound) => session_not_found(),
        Err(PromptSubmitError::RevivalRefused) => no_revive_refused(),
        Err(PromptSubmitError::WorkerNotReady) => worker_not_ready(),
        Err(PromptSubmitError::Blocked(blocked)) => {
            crate::server::api::start_blocked_response(blocked)
        }
        Err(PromptSubmitError::Attachments(code, msg))
        | Err(PromptSubmitError::Enqueue(code, msg)) => (code, msg).into_response(),
        Err(PromptSubmitError::QueuedPromptGone) => queued_prompt_gone(),
        Err(PromptSubmitError::QueuedPromptChanged) => queued_prompt_changed(),
        Err(PromptSubmitError::QueuedAttachmentsMissing) => queued_attachments_missing(),
        Err(PromptSubmitError::Turn(e)) => match e {
            SendTurnError::SessionNotFound => session_not_found(),
            SendTurnError::ResumeFailed(e) => resume_failed_response(&e),
            SendTurnError::WorkerNotReady | SendTurnError::WorkerLostAfterPublish => {
                worker_not_ready()
            }
            SendTurnError::RevivalRefused => no_revive_refused(),
            // Unreachable for a User caller.
            SendTurnError::NotOwner => {
                (StatusCode::FORBIDDEN, "session not owned by caller").into_response()
            }
            SendTurnError::ModeApplication(e) => {
                supervisor_error_response("mode application failed", &e)
            }
            SendTurnError::Send(e) => supervisor_error_response("prompt failed", &e),
        },
    }
}

/// A prompt for a structured session, from any surface that submits on the user's
/// behalf.
pub(crate) struct PromptSubmission<'a> {
    pub text: &'a str,
    pub uploads: &'a [crate::acp::protocol::PromptAttachmentUpload],
    pub prompt_id: Option<String>,
    /// "Send now" for a row already in the session's queue.
    pub queued_prompt_id: Option<String>,
    pub no_revive: bool,
}

/// What [`submit_prompt`] did with a prompt.
pub(crate) struct PromptSubmitted {
    pub dispatch: PromptDispatch,
    /// The queue row holding it, when `dispatch` is `Queued`.
    pub queued_id: Option<String>,
    /// The seq of its `UserPromptSent` event. `None` when it was queued, or when it was
    /// sent but the event was not persisted.
    pub seq: Option<u64>,
}

/// Why [`submit_prompt`] did not deliver a prompt.
///
/// Every `Turn` error for which [`SendTurnError::published`] holds comes after the pending
/// initial turn was cleared, and the transcript may show a prompt the agent never received.
/// The other variants leave the transcript, the queue and the pending initial turn as they
/// were, though the session may already have been woken.
pub(crate) enum PromptSubmitError {
    SessionNotFound,
    RevivalRefused,
    WorkerNotReady,
    Blocked(crate::session::StartBlocked),
    Attachments(StatusCode, String),
    QueuedPromptGone,
    QueuedPromptChanged,
    QueuedAttachmentsMissing,
    /// The dispatch chose the queue and the enqueue was refused, e.g. a full queue (429).
    Enqueue(StatusCode, String),
    Turn(SendTurnError),
}

/// Submit a prompt to a structured session: wake it, decide whether the prompt is
/// sent, steered into the running turn or queued, then queue or send it.
pub(crate) async fn submit_prompt(
    state: &Arc<AppState>,
    caller: &SessionCaller,
    id: &str,
    req: PromptSubmission<'_>,
) -> Result<PromptSubmitted, PromptSubmitError> {
    let Some(_submission) = state
        .session_service
        .prompt_submission_for_session(id)
        .await
    else {
        return Err(PromptSubmitError::SessionNotFound);
    };
    let woke_idle_dormant = match state
        .session_service
        .touch_and_wake_on_prompt(id, req.no_revive)
        .await
    {
        PromptTouch::Touched { idle_dormant } => idle_dormant,
        PromptTouch::RevivalRefused => return Err(PromptSubmitError::RevivalRefused),
        PromptTouch::WorkerNotReady => return Err(PromptSubmitError::WorkerNotReady),
        PromptTouch::Blocked(blocked) => return Err(PromptSubmitError::Blocked(blocked)),
    };
    // Validated before publishing or resuming, so a rejected prompt leaves no
    // trace in the transcript and spawns no worker.
    // A named queue row is delivered with its stored bytes, so request uploads are not read.
    let uploads: &[_] = if req.queued_prompt_id.is_some() {
        &[]
    } else {
        req.uploads
    };
    let attachments = validate_attachments(state, id, uploads)
        .map_err(|(code, msg)| PromptSubmitError::Attachments(code, msg))?;
    // "Send now" delivers a stored row under its stored sender. The row is only consumed
    // once the turn is accepted, so any refusal leaves it for a retry.
    let queued_row = match &req.queued_prompt_id {
        Some(queued_id) => {
            let row = state
                .session_service
                .queued_prompts_snapshot(id)
                .await
                .into_iter()
                .find(|r| &r.id == queued_id);
            match row {
                Some(row) => Some(row),
                None => return Err(PromptSubmitError::QueuedPromptGone),
            }
        }
        None => None,
    };
    if let Some(row) = &queued_row {
        if row.text != req.text {
            return Err(PromptSubmitError::QueuedPromptChanged);
        }
    }
    let origin_plugin = queued_row.as_ref().and_then(|r| r.origin_plugin.clone());
    // Decided under the guard so the decision and its dispatch are atomic
    // (#3621). The wake already cleared the dormant marker, hence the flag.
    let dispatch = state
        .session_service
        .prompt_dispatch_under_submission(id, req.text, woke_idle_dormant, req.no_revive)
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
        return Err(PromptSubmitError::RevivalRefused);
    }
    if let PromptDispatch::Queued { reason } = dispatch {
        // A fresh prompt supersedes a queued rate-limit continuation (#3028),
        // but only once it is queued: a refusal must leave it in place (#4081 review).
        // The named row is already the parked copy; queueing again would duplicate it.
        if let Some(row) = &queued_row {
            state.session_service.clear_pending_initial_turn(id).await;
            return Ok(PromptSubmitted {
                dispatch,
                queued_id: Some(row.id.clone()),
                seq: None,
            });
        }
        let prompt_id = req
            .prompt_id
            .clone()
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        return match super::super::queue::buffer_and_enqueue(
            state,
            id,
            super::super::queue::QueuedPrompt {
                prompt_id: &prompt_id,
                text: req.text.to_string(),
                blobs: &attachments,
                origin_device: None,
                origin_plugin: origin_plugin.clone(),
                supersede_pending_turn: true,
            },
        )
        .await
        {
            Ok(entry) => {
                tracing::debug!(
                    target: "acp.queue",
                    session = %id,
                    ?reason,
                    queued_id = %entry.id,
                    "prompt queued"
                );
                Ok(PromptSubmitted {
                    dispatch,
                    queued_id: Some(entry.id),
                    seq: None,
                })
            }
            Err((status, msg)) => {
                tracing::warn!(
                    target: "http.api.acp",
                    session = %id,
                    ?reason,
                    "prompt dispatch chose the queue but enqueue failed: {msg}"
                );
                Err(PromptSubmitError::Enqueue(status, msg))
            }
        };
    }
    // A named row is delivered with the bytes the daemon stored for it, never the request's.
    let attachments = match &queued_row {
        Some(row) => {
            let store = Arc::clone(&state.acp_event_store);
            let (session_id, row_id) = (id.to_string(), row.id.clone());
            let stored = tokio::task::spawn_blocking(move || {
                store.load_pending_attachments_for_ref(&session_id, &row_id)
            })
            .await
            .unwrap_or_default();
            if stored.len() != row.attachments.len() {
                return Err(PromptSubmitError::QueuedAttachmentsMissing);
            }
            stored
        }
        None => attachments,
    };
    let row_caller;
    let caller = match &origin_plugin {
        Some(plugin_id) => {
            row_caller = SessionCaller::PluginMessage {
                plugin_id: plugin_id.clone(),
            };
            &row_caller
        }
        None => caller,
    };
    let outcome = state
        .session_service
        .send_turn(
            caller,
            id,
            SendTurnRequest {
                resume_authority: crate::process::worker_registry::ResumeAuthority::Explicit,
                text: req.text,
                attachments: &attachments,
                woke_idle_dormant,
                prompt_id: req.prompt_id.clone(),
                synthesized: false,
                no_revive: req.no_revive,
            },
        )
        .await;
    // A fresh prompt supersedes a queued rate-limit continuation (#3028),
    // but only once it is published: a prompt refused before that must leave
    // the pending turn in place (#4081 review, #5253).
    if outcome.as_ref().err().is_none_or(SendTurnError::published) {
        state.session_service.clear_pending_initial_turn(id).await;
    }
    let seq = outcome.map_err(PromptSubmitError::Turn)?;
    tracing::info!(
        target: "acp.queue",
        session = %id,
        ?dispatch,
        text_len = req.text.len(),
        attachments = attachments.len(),
        send_now = queued_row.is_some(),
        ?origin_plugin,
        "prompt sent"
    );
    if let Some(row) = &queued_row {
        state
            .session_service
            .take_queued_prompt(id, row.id.clone())
            .await;
    }
    Ok(PromptSubmitted {
        dispatch,
        queued_id: None,
        seq,
    })
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
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
        .touch_and_wake_on_prompt(&id, false)
        .await
    {
        PromptTouch::Touched { idle_dormant } => idle_dormant,
        PromptTouch::RevivalRefused => return no_revive_refused(),
        PromptTouch::WorkerNotReady => return worker_not_ready(),
        PromptTouch::Blocked(blocked) => {
            return crate::server::api::start_blocked_response(blocked)
        }
    };
    let dispatch = state
        .session_service
        .prompt_dispatch_under_submission(&id, "", woke_idle_dormant, false)
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
        match crate::server::acp_reconciler::trigger_resume_background(
            &state.session_service,
            &id,
            crate::process::worker_registry::ResumeAuthority::Explicit,
        )
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
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
        Ok(()) => {
            tracing::info!(
                target: "acp.supervisor",
                session = %id,
                waited_ms = waited.as_millis(),
                "turn cancelled"
            );
            StatusCode::ACCEPTED.into_response()
        }
        Err(e) => {
            tracing::warn!(target: "acp.supervisor", session = %id, error = %e, "turn cancel failed");
            supervisor_error_response("cancel failed", &e)
        }
    }
}

/// Escape hatch for a stuck spinner (#1100). Deliberately skips the submission
/// guard so it never queues.
pub async fn acp_force_end_turn(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
    if let Some(resp) = read_only_block(&state) {
        return resp;
    }
    tracing::info!(target: "acp.supervisor", session = %id, "turn force-ended");
    state.acp_supervisor.force_end_turn(&id).await;
    StatusCode::ACCEPTED.into_response()
}

pub async fn resolve_approval(
    State(state): State<Arc<AppState>>,
    Path((id, nonce_str)): Path<(String, String)>,
    req: Result<Json<ResolveApprovalRequest>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
            crate::daemon::ApiErrorCode::PendingTargetGone.header(),
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
    if let Some(resp) = cityhall_block_non_structured(&state, &id).await {
        return resp;
    }
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
            crate::daemon::ApiErrorCode::PendingTargetGone.header(),
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
