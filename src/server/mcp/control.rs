//! Tools that create, drive and read sessions, for an agent orchestrating others. Each one
//! calls the function its HTTP route calls, so both surfaces decide the same way.

use axum::http::StatusCode;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::CallToolResult;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{read_only_refusal, tool_error, McpServer};
use crate::acp::dispatch::PromptDispatch;
use crate::daemon::{CreateSessionBody, SessionResponse};
use crate::server::api::sessions::{
    capture_terminal_output, create_session_with, send_terminal_message, CaptureError,
    SendKeysError,
};
use crate::server::api::{submit_prompt, PromptSubmission, PromptSubmitError, PromptSubmitted};
use crate::server::session_service::{SendTurnError, SessionCaller};
use crate::session::View;

/// Transcript events read per `read_output` call; the caller pages on with `next_since`.
const READ_PAGE: usize = 2000;

/// Pane lines `read_output` captures when the caller names none, as `GET /output` does.
const DEFAULT_LINES: u32 = 200;

/// The create fields a model may set. Hook trust, callbacks, command and environment
/// overrides, fork and import sources and UI-only fields stay with the dashboard and CLI.
#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct CreateSessionParams {
    /// Project directory the session works in.
    path: String,
    /// Agent to launch, e.g. `claude` or `codex`.
    tool: String,
    title: Option<String>,
    /// `terminal` (the default) or `structured`.
    #[serde(default)]
    view: View,
    /// The ACP agent for a structured session, when it differs from `tool`.
    agent_name: Option<String>,
    agent_model: Option<String>,
    agent_effort: Option<String>,
    /// Group path. A parent's group replaces it.
    #[serde(default)]
    group: String,
    /// Work in a git worktree instead of `path` itself.
    #[serde(default)]
    worktree_enabled: bool,
    worktree_branch: Option<String>,
    #[serde(default)]
    create_new_branch: bool,
    base_branch: Option<String>,
    /// Run the agent in a sandbox container.
    #[serde(default)]
    sandbox: bool,
    sandbox_image: Option<String>,
    /// Let the agent act without asking for approval.
    #[serde(default)]
    yolo_mode: bool,
    profile: Option<String>,
    /// First prompt for the new session.
    prompt: Option<String>,
    /// Make the session a sub-session of this top-level session, in the same profile.
    parent_session_id: Option<String>,
    /// Retry key: a repeated create with the same key answers with the first one's session.
    idempotency_key: Option<String>,
}

/// What happened to `create_session`'s `prompt`.
#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum PromptDelivery {
    /// No prompt was given.
    None,
    /// Stored with the structured session; delivered once its worker runs.
    PendingInitialTurn,
    /// Typed into the terminal session's pane.
    Sent,
    /// The terminal session was not running, so nothing was typed.
    NotRunning,
    /// Typing it failed; see `prompt_error`.
    Failed,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CreatedSession {
    session: SessionResponse,
    /// Whether the prompt was sent or stored for delivery.
    prompt_sent: bool,
    prompt_delivery: PromptDelivery,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_error: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SendParams {
    session_id: String,
    message: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum Disposition {
    /// Started a turn.
    Sent,
    /// Joined the turn already running.
    Steered,
    /// Parked on the session's queue; it starts a turn once the current one ends.
    Queued,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SendOutcome {
    disposition: Disposition,
    /// Why it was queued: `turn_active`, `cancelling`, `compacting` or `worker_down`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// The queue row holding a queued prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    queued_id: Option<String>,
    /// Structured view only: the event seq the prompt was recorded at, for `read_output`'s
    /// `since`. Absent for a terminal session, for a queued prompt, and for a sent prompt
    /// whose event was not recorded, in which case nothing marks where its turn began.
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct ReadOutputParams {
    session_id: String,
    /// Structured view: only what happened after this event seq. Defaults to 0.
    since: Option<u64>,
    /// Structured view: the raw event list instead of rendered text.
    #[serde(default)]
    raw: bool,
    /// Terminal view: how many pane lines to capture, 1 to 2000. Defaults to 200.
    lines: Option<u32>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct Output {
    view: View,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    /// `raw` only: `{seq, event}` objects, oldest first.
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<Vec<Value>>,
    /// Structured view: pass as `since` to read on from here.
    #[serde(skip_serializing_if = "Option::is_none")]
    next_since: Option<u64>,
    /// Structured view: more events follow `next_since` already.
    has_more: bool,
}

#[rmcp::tool_router(router = control_router, vis = "pub(super)")]
impl McpServer {
    #[rmcp::tool(
        description = "Create a session, optionally with a first prompt. A structured session \
            stores the prompt and delivers it once its worker runs; a terminal session waits \
            for its agent and types it. Repository hooks that need trust are refused with \
            `hooks_need_trust`: a human must approve them in the AoE dashboard."
    )]
    async fn create_session(
        &self,
        Parameters(params): Parameters<CreateSessionParams>,
    ) -> Result<Json<CreatedSession>, CallToolResult> {
        if let Some(refusal) = read_only_refusal(&self.state) {
            return Err(refusal);
        }
        let structured = params.view == View::Structured;
        // A terminal prompt is typed into the pane once the agent starts, so it gets what
        // `send` does to keys.
        let prompt = if structured {
            params.prompt.clone().filter(|p| !p.trim().is_empty())
        } else {
            params.prompt.clone().and_then(terminal_text)
        };
        let body = create_body(&params).map_err(|e| tool_error("invalid_request", e))?;
        let initial_turn = if structured { prompt.clone() } else { None };
        let wait_ready = !structured && prompt.is_some();
        let response =
            create_session_with(self.state.clone(), false, wait_ready, body, initial_turn).await;
        let session: SessionResponse = success_body(response).await.map_err(|e| *e)?;
        let (prompt_sent, prompt_delivery, prompt_error) = match prompt {
            None => (false, PromptDelivery::None, None),
            Some(_) if structured => (true, PromptDelivery::PendingInitialTurn, None),
            // A session that just failed to start is not restarted behind the caller's back.
            Some(text) => {
                match send_terminal_message(&self.state.session_service, &session.id, text, false)
                    .await
                {
                    Ok(_) => {
                        self.publish().await;
                        (true, PromptDelivery::Sent, None)
                    }
                    Err(e) => match e.error {
                        SendKeysError::NotRunning => (false, PromptDelivery::NotRunning, None),
                        other => {
                            let refusal = terminal_refusal(&session.id, other).await;
                            let message = refusal
                                .structured_content
                                .as_ref()
                                .and_then(|c| c["message"].as_str())
                                .map(str::to_string);
                            (false, PromptDelivery::Failed, message)
                        }
                    },
                }
            }
        };
        Ok(Json(CreatedSession {
            session,
            prompt_sent,
            prompt_delivery,
            prompt_error,
        }))
    }

    #[rmcp::tool(
        description = "Send a prompt to a session. A structured session starts a turn, joins \
            the running one, or queues it, and reports which, with the prompt's event seq. A \
            terminal session gets it typed into its pane."
    )]
    async fn send(
        &self,
        Parameters(params): Parameters<SendParams>,
    ) -> Result<Json<SendOutcome>, CallToolResult> {
        if let Some(refusal) = read_only_refusal(&self.state) {
            return Err(refusal);
        }
        let id = params.session_id;
        let Some(structured) = self.view_of(&id).await.map(|v| v == View::Structured) else {
            return Err(not_found(&id));
        };
        if structured {
            let submission = PromptSubmission {
                text: &params.message,
                uploads: &[],
                prompt_id: None,
                queued_prompt_id: None,
                no_revive: false,
            };
            let submitted = submit_prompt(&self.state, &SessionCaller::User, &id, submission)
                .await
                .map_err(|e| prompt_refusal(&id, e))?;
            return Ok(Json(send_outcome(submitted)));
        }
        let Some(message) = terminal_text(params.message) else {
            return Err(tool_error("invalid_request", "The message is empty"));
        };
        if let Err(e) = send_terminal_message(&self.state.session_service, &id, message, true).await
        {
            return Err(terminal_refusal(&id, e.error).await);
        }
        self.publish().await;
        Ok(Json(SendOutcome {
            disposition: Disposition::Sent,
            reason: None,
            queued_id: None,
            seq: None,
        }))
    }

    #[rmcp::tool(
        description = "Read a session's output. A structured session returns its transcript \
            since an event seq as text (the last 48 KB of a page), or the raw events, with \
            `next_since` to read on from. A terminal session returns a capture of its pane."
    )]
    async fn read_output(
        &self,
        Parameters(params): Parameters<ReadOutputParams>,
    ) -> Result<Json<Output>, CallToolResult> {
        let id = params.session_id;
        let instance = self
            .state
            .instances
            .read()
            .await
            .iter()
            .find(|i| i.id == id)
            .cloned();
        let Some(instance) = instance else {
            return Err(not_found(&id));
        };
        if instance.is_structured() {
            let since = params.since.unwrap_or(0);
            let page = self
                .state
                .acp_event_store
                .replay_page(&id, since, Some(READ_PAGE));
            let next_since = Some(page.last_scanned_seq.unwrap_or(since));
            let (text, events) = if params.raw {
                let events = page
                    .events
                    .iter()
                    .map(|(seq, event)| json!({"seq": seq, "event": event}))
                    .collect();
                (None, Some(events))
            } else {
                let (text, _, _) = crate::session::conversation_summary::extract_transcript_delta(
                    &page.events,
                    since,
                );
                (Some(text), None)
            };
            return Ok(Json(Output {
                view: View::Structured,
                text,
                events,
                next_since,
                has_more: page.has_more,
            }));
        }
        let lines = params.lines.unwrap_or(DEFAULT_LINES).clamp(1, 2000) as usize;
        let text = capture_terminal_output(instance, lines, false)
            .await
            .map_err(|e| match e {
                CaptureError::NotRunning => {
                    tool_error("not_running", "The terminal session is not running")
                }
                CaptureError::Tmux(e) => tool_error("internal", format!("tmux: {e:#}")),
                CaptureError::Panicked(e) => tool_error("internal", e),
            })?;
        Ok(Json(Output {
            view: View::Terminal,
            text: Some(text),
            events: None,
            next_since: None,
            has_more: false,
        }))
    }
}

impl McpServer {
    async fn view_of(&self, id: &str) -> Option<View> {
        self.state
            .instances
            .read()
            .await
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.view)
    }

    /// Mutations reach the dashboard through a published snapshot, as the HTTP routes do.
    async fn publish(&self) {
        if let Err(error) = self.state.runtime.publish(&self.state).await {
            tracing::debug!(target: "server.runtime", %error, "runtime publish after MCP mutation failed");
        }
    }
}

fn create_body(params: &CreateSessionParams) -> Result<CreateSessionBody, String> {
    serde_json::from_value(json!({
        "path": params.path,
        "tool": params.tool,
        "title": params.title,
        "view": params.view,
        "agent_name": params.agent_name,
        "agent_model": params.agent_model,
        "agent_effort": params.agent_effort,
        "group": params.group,
        "worktree_enabled": params.worktree_enabled,
        "worktree_branch": params.worktree_branch,
        "create_new_branch": params.create_new_branch,
        "base_branch": params.base_branch,
        "sandbox": params.sandbox,
        "sandbox_image": params.sandbox_image,
        "yolo_mode": params.yolo_mode,
        "profile": params.profile,
        "parent_session_id": params.parent_session_id,
        "idempotency_key": params.idempotency_key,
    }))
    .map_err(|e| e.to_string())
}

/// The created session from a create response, or the refusal it carries.
async fn success_body(
    response: axum::response::Response,
) -> Result<SessionResponse, Box<CallToolResult>> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|e| Box::new(tool_error("internal", e.to_string())))?;
    if !status.is_success() {
        return Err(Box::new(refusal(status, &bytes)));
    }
    serde_json::from_slice(&bytes).map_err(|e| Box::new(tool_error("internal", e.to_string())))
}

/// A refused HTTP response as a tool error: the codes a caller can act on pass through, and
/// the rest are named by status. A plain-text body becomes the message.
fn refusal(status: StatusCode, body: &[u8]) -> CallToolResult {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    let http_error = json
        .as_ref()
        .and_then(|v| v["error"].as_str())
        .map(str::to_string);
    let message = json
        .as_ref()
        .and_then(|v| v["message"].as_str())
        .map(str::to_string)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_string());
    match http_error.as_deref() {
        Some("hooks_need_trust") => {
            let v = json.unwrap_or_default();
            CallToolResult::structured_error(json!({
                "code": "hooks_need_trust",
                "message": "This repository's hooks need a human to review and trust them in \
                    the AoE dashboard before a session can be created here.",
                "on_create": v["on_create"],
                "on_launch": v["on_launch"],
                "on_destroy": v["on_destroy"],
            }))
        }
        Some(code @ ("parent_not_found" | "parent_is_sub_session" | "read_only")) => {
            tool_error(code, message)
        }
        _ => {
            let code = match status {
                StatusCode::NOT_FOUND => "not_found",
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => "busy",
                s if s.is_server_error() => "internal",
                _ => "invalid_request",
            };
            CallToolResult::structured_error(json!({
                "code": code,
                "message": message,
                "http_status": status.as_u16(),
                "http_error": http_error,
            }))
        }
    }
}

fn send_outcome(submitted: PromptSubmitted) -> SendOutcome {
    let (disposition, reason) = match submitted.dispatch {
        PromptDispatch::Sent => (Disposition::Sent, None),
        PromptDispatch::Steered => (Disposition::Steered, None),
        PromptDispatch::Queued { reason } => (
            Disposition::Queued,
            serde_json::to_value(reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string)),
        ),
    };
    SendOutcome {
        disposition,
        reason,
        queued_id: submitted.queued_id,
        seq: submitted.seq,
    }
}

fn prompt_refusal(id: &str, error: PromptSubmitError) -> CallToolResult {
    match error {
        PromptSubmitError::SessionNotFound
        | PromptSubmitError::Turn(SendTurnError::SessionNotFound) => not_found(id),
        PromptSubmitError::WorkerNotReady
        | PromptSubmitError::Turn(SendTurnError::WorkerNotReady) => tool_error(
            "busy",
            "The session's agent is not ready yet; try again shortly",
        ),
        PromptSubmitError::Turn(SendTurnError::ResumeFailed(e)) => tool_error(
            "busy",
            format!("The session's agent could not be resumed: {e}"),
        ),
        // The queue is full; it drains as turns end.
        PromptSubmitError::Enqueue(StatusCode::TOO_MANY_REQUESTS, message) => {
            tool_error("busy", message)
        }
        PromptSubmitError::Blocked(blocked) => tool_error("invalid_request", blocked.to_string()),
        PromptSubmitError::Enqueue(_, message) | PromptSubmitError::Attachments(_, message) => {
            tool_error("invalid_request", message)
        }
        PromptSubmitError::Turn(SendTurnError::Send(e))
        | PromptSubmitError::Turn(SendTurnError::ModeApplication(e)) => {
            tool_error("send_failed", e.to_string())
        }
        // Unreachable for a user caller that names no queue row and allows revival.
        PromptSubmitError::RevivalRefused
        | PromptSubmitError::Turn(SendTurnError::RevivalRefused)
        | PromptSubmitError::Turn(SendTurnError::NotOwner)
        | PromptSubmitError::QueuedPromptGone
        | PromptSubmitError::QueuedPromptChanged
        | PromptSubmitError::QueuedAttachmentsMissing => {
            tool_error("send_failed", "The prompt was refused")
        }
    }
}

/// Keys typed into a pane, as `POST /send` takes them: paste delimiters stripped so text
/// can't end the bracketed paste early, trailing line breaks dropped, nothing if empty.
fn terminal_text(mut text: String) -> Option<String> {
    crate::server::live_ws::strip_paste_escapes(&mut text);
    let len = text.trim_end_matches(['\r', '\n']).len();
    text.truncate(len);
    (!text.trim().is_empty()).then_some(text)
}

/// A refused terminal send as a tool error. A refusal from reviving the session carries the
/// HTTP response `POST /send` would give, so it maps like any other refusal.
async fn terminal_refusal(id: &str, error: SendKeysError) -> CallToolResult {
    match error {
        SendKeysError::NotFound | SendKeysError::Gone => not_found(id),
        SendKeysError::NotRunning => {
            tool_error("not_running", "The terminal session is not running")
        }
        SendKeysError::Transient(status) => {
            tool_error("busy", format!("The session is {status:?}; try again"))
        }
        SendKeysError::Preparation { response, .. } => {
            let status = response.status();
            match axum::body::to_bytes(response.into_body(), usize::MAX).await {
                Ok(bytes) => refusal(status, &bytes),
                Err(e) => tool_error("internal", e.to_string()),
            }
        }
        SendKeysError::StructuredView => tool_error(
            "invalid_request",
            "The session no longer has a terminal agent",
        ),
        SendKeysError::Blocked(blocked) => tool_error("invalid_request", blocked.to_string()),
        SendKeysError::Tmux(e) => tool_error("send_failed", format!("tmux: {e:#}")),
        SendKeysError::Internal => tool_error("internal", "Internal error"),
    }
}

fn not_found(id: &str) -> CallToolResult {
    tool_error("not_found", format!("No session {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The codes a caller acts on pass through, the rest are named by status, and a
    /// plain-text body becomes the message.
    #[test]
    fn refusals_keep_actionable_codes_and_fall_back_by_status() {
        let hooks = json!({
            "error": "hooks_need_trust", "message": "Resubmit with trust_hooks",
            "on_create": ["make setup"], "on_launch": [], "on_destroy": [],
        });
        // (status, body, code, message contains)
        let cases = [
            (
                StatusCode::FORBIDDEN,
                hooks.to_string(),
                "hooks_need_trust",
                "AoE dashboard",
            ),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": "parent_not_found", "message": "No session x"}).to_string(),
                "parent_not_found",
                "No session x",
            ),
            (
                StatusCode::BAD_REQUEST,
                json!({"error": "validation_failed", "message": "bad title"}).to_string(),
                "invalid_request",
                "bad title",
            ),
            (
                StatusCode::BAD_REQUEST,
                "Choose only one source".into(),
                "invalid_request",
                "Choose only one source",
            ),
            (StatusCode::NOT_FOUND, String::new(), "not_found", ""),
            (
                StatusCode::TOO_MANY_REQUESTS,
                "slow down".into(),
                "busy",
                "slow down",
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                String::new(),
                "internal",
                "",
            ),
        ];
        for (status, body, code, message) in cases {
            let result = refusal(status, body.as_bytes());
            assert_eq!(result.is_error, Some(true), "{body}");
            let content = result.structured_content.unwrap();
            assert_eq!(content["code"], code, "{body}");
            assert!(
                content["message"].as_str().unwrap().contains(message),
                "{body}: {content}"
            );
        }
        let content = refusal(StatusCode::FORBIDDEN, hooks.to_string().as_bytes())
            .structured_content
            .unwrap();
        assert_eq!(content["on_create"], json!(["make setup"]));
        assert!(content.get("trust_hooks").is_none());
    }

    /// Keys reach a pane as `POST /send` sends them, so a prompt can't close the bracketed
    /// paste and run the rest as raw keys.
    #[test]
    fn terminal_text_strips_paste_delimiters_and_trailing_breaks() {
        // (input, keys sent)
        let cases = [
            ("review the diff\n", Some("review the diff")),
            ("one\r\ntwo\r\n", Some("one\r\ntwo")),
            ("first\x1b[201~rm -rf ~\n", Some("firstrm -rf ~")),
            ("\x1b[200~pasted\x1b[201~", Some("pasted")),
            ("\n\r\n", None),
            ("\x1b[200~\x1b[201~", None),
        ];
        for (input, want) in cases {
            assert_eq!(
                terminal_text(input.to_string()).as_deref(),
                want,
                "{input:?}"
            );
        }
    }

    /// A refused revive keeps the HTTP refusal's code, so a hook approval reads as one
    /// instead of "try again"; a full queue is retryable.
    #[tokio::test]
    async fn refused_terminal_sends_and_full_queues_map_like_http() {
        use axum::response::IntoResponse;
        let preparation = |status: StatusCode, body: axum::response::Response| {
            let mut response = body;
            *response.status_mut() = status;
            SendKeysError::Preparation {
                response: Box::new(response),
                detail: json!({}),
            }
        };
        let hooks = axum::Json(json!({
            "error": "hooks_need_trust", "message": "trust them",
            "on_create": [], "on_launch": ["npm start"], "on_destroy": [],
        }))
        .into_response();
        // (error, code)
        let cases = [
            (
                preparation(StatusCode::FORBIDDEN, hooks),
                "hooks_need_trust",
            ),
            (
                preparation(StatusCode::CONFLICT, "resume_failed".into_response()),
                "invalid_request",
            ),
            (
                preparation(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "lifecycle busy".into_response(),
                ),
                "busy",
            ),
            (SendKeysError::NotRunning, "not_running"),
            (SendKeysError::Gone, "not_found"),
        ];
        for (error, code) in cases {
            let result = terminal_refusal("s", error).await;
            assert_eq!(result.structured_content.unwrap()["code"], code);
        }
        let full = prompt_refusal(
            "s",
            PromptSubmitError::Enqueue(StatusCode::TOO_MANY_REQUESTS, "queue is full".into()),
        );
        assert_eq!(full.structured_content.unwrap()["code"], "busy");
    }
}
