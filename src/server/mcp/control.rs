//! Tools that create, drive and read sessions, for an agent orchestrating others. Each one
//! calls the function its HTTP route calls, so both surfaces decide the same way.

use axum::http::StatusCode;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::CallToolResult;
use rmcp::service::{RequestContext, RoleServer};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{caller_session_id, read_only_refusal, tool_error, McpServer};
use crate::acp::dispatch::PromptDispatch;
use crate::daemon::{CreateSessionBody, SessionResponse};
use crate::server::api::sessions::{
    capture_terminal_output, create_session_with, send_terminal_message, CaptureError,
    CheckedSendPane, CreateOrigin, SendKeysError, SendPane,
};
use crate::server::api::{submit_prompt, PromptSubmission, PromptSubmitError, PromptSubmitted};
use crate::server::session_service::{SendTurnError, SessionCaller};
use crate::session::{Instance, Status, View, CAPTURE_FAILED_RULE};

/// Transcript events read per `read_output` call; the caller pages on with `next_since`.
const READ_PAGE: usize = 2000;

/// Most rendered text one `read_output` page returns, the transcript renderer's own cap. A page
/// stops at the last event that fits whole, so following `next_since` skips nothing.
const TEXT_PAGE_BYTES: usize = crate::session::conversation_summary::MAX_INPUT_BYTES;

/// How long a terminal `create_session` waits for its agent's input prompt before typing the
/// first prompt. A dialog (such as Claude's folder-trust question) stops it at once instead.
const PROMPT_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Pane lines `read_output` captures when the caller names none, as `GET /output` does.
const DEFAULT_LINES: u32 = 200;

/// A create's prompt result: sent, how, why not, and `prompt_error_code`.
type PromptOutcome = (bool, PromptDelivery, Option<String>, Option<&'static str>);

/// The code for a terminal pane whose input box holds a person's draft.
const INPUT_LINE_BUSY: &str = "input_line_busy";

/// Why a `create_session` prompt did not reach a session another one manages.
const IGNORED_PROMPT: &str =
    "The session is ignored because another session manages it, so nothing was typed. Take \
     ownership back, then `send` the prompt.";

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
    /// Defaults to the calling session when it is top-level, in that profile, and no `group`
    /// is given.
    parent_session_id: Option<String>,
    /// Retry key: a repeated create with the same key answers with the first one's session, and
    /// does nothing else (the prompt is not delivered again).
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
    /// Typed into the terminal session's pane, once its agent was at its input prompt.
    Sent,
    /// The terminal session was not running, so nothing was typed.
    NotRunning,
    /// The terminal agent is waiting on a dialog (such as Claude's folder-trust question), so
    /// nothing was typed; a person answers it in the terminal, then `send` the prompt.
    BlockedWaiting,
    /// The terminal agent did not reach its input prompt in time, so nothing was typed.
    NotReady,
    /// The terminal session was ignored, because another session manages it, before the
    /// prompt could be typed: nothing was typed.
    Ignored,
    /// The create was a retry of an earlier one with the same `idempotency_key`, so the prompt
    /// was not attempted by this call; the first create's outcome is unknown here.
    Replayed,
    /// Typing it failed; see `prompt_error`.
    Failed,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct CreatedSession {
    session: SessionResponse,
    /// False when `idempotency_key` matched an earlier create, whose session this is.
    fresh: bool,
    /// Whether the prompt was sent or stored for delivery.
    prompt_sent: bool,
    prompt_delivery: PromptDelivery,
    /// Why the prompt was not delivered, for `failed`, `blocked_waiting` and `not_ready`.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_error: Option<String>,
    /// `input_line_busy` when the prompt was refused because the input box held a person's
    /// draft, as `send` names it.
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_error_code: Option<&'static str>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SendParams {
    session_id: String,
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SendToParentParams {
    message: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub(super) struct SuggestSessionParams {
    /// Short title of the suggested session.
    title: String,
    /// The first prompt the suggested session starts with.
    prompt: String,
    /// Why this is worth doing now.
    #[serde(default)]
    reason: String,
    /// Registered Project name or path the session belongs in.
    project: Option<String>,
    /// A stable key; suggesting the same one again replaces that suggestion.
    id: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub(super) struct SuggestOutcome {
    /// The suggestion's id.
    id: String,
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
            stores the prompt and delivers it once its worker runs; a terminal session types it \
            once its agent is at its input prompt, and never into a dialog (`blocked_waiting`: \
            a person answers the dialog, then `send` the prompt). Repository hooks that need trust are refused with \
            `hooks_need_trust`: a human must approve them in the AoE dashboard."
    )]
    async fn create_session(
        &self,
        Parameters(mut params): Parameters<CreateSessionParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<CreatedSession>, CallToolResult> {
        if let Some(refusal) = read_only_refusal(&self.state) {
            return Err(refusal);
        }
        if params.parent_session_id.is_none() && params.group.is_empty() {
            params.parent_session_id = self
                .caller_as_parent(&context, params.profile.as_deref())
                .await;
        }
        let structured = params.view == View::Structured;
        // A terminal prompt is typed into the pane once the agent starts, so it gets what
        // `send` does to keys.
        let prompt = if structured {
            params.prompt.clone().filter(|p| !p.trim().is_empty())
        } else {
            params.prompt.clone().and_then(terminal_text)
        };
        // `send` refuses the same input, so a create does not quietly drop it.
        if params.prompt.is_some() && prompt.is_none() {
            return Err(tool_error("invalid_request", "The prompt is empty"));
        }
        let body = create_body(&params).map_err(|e| tool_error("invalid_request", e))?;
        let initial_turn = prompt
            .clone()
            .filter(|_| structured)
            .map(|text| (text, crate::daemon::PromptOrigin::Mcp));
        let wait_ready = !structured && prompt.is_some();
        // Names this create in failure logs and responses, as an HTTP request's id does.
        let reference = format!("mcp-{}", uuid::Uuid::new_v4());
        let response = create_session_with(
            self.state.clone(),
            false,
            &reference,
            wait_ready,
            body,
            CreateOrigin {
                initial_turn,
                ..CreateOrigin::wizard(crate::daemon::PromptOrigin::Mcp)
            },
        )
        .await;
        let (session, fresh) = success_body(response).await.map_err(|e| *e)?;
        // Ignore outranks the readiness wait: a person who ignores the new row while its pane
        // comes up did not ask for this prompt to land there.
        let ignored =
            prompt.is_some() && fresh && !structured && self.session_is_ignored(&session.id).await;
        let (prompt_sent, prompt_delivery, prompt_error, prompt_error_code) = match prompt {
            None => (false, PromptDelivery::None, None, None),
            Some(_) if !fresh => (false, PromptDelivery::Replayed, None, None),
            Some(_) if structured => (true, PromptDelivery::PendingInitialTurn, None, None),
            Some(_) if ignored => (
                false,
                PromptDelivery::Ignored,
                Some(IGNORED_PROMPT.to_string()),
                None,
            ),
            Some(text) => {
                let (readiness, checked_pane) =
                    wait_for_input_prompt(&self.state, &session.id).await;
                // The wait can outlast a person's ignore, so it is checked again before
                // the readiness result decides what the caller is told.
                if self.session_is_ignored(&session.id).await {
                    (
                        false,
                        PromptDelivery::Ignored,
                        Some(IGNORED_PROMPT.to_string()),
                        None,
                    )
                } else {
                    self.deliver_terminal_prompt(&session.id, text, readiness, checked_pane)
                        .await
                }
            }
        };
        Ok(Json(CreatedSession {
            session,
            fresh,
            prompt_sent,
            prompt_delivery,
            prompt_error,
            prompt_error_code,
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
        self.send_to(params.session_id, params.message)
            .await
            .map(Json)
            .map_err(|e| *e)
    }

    #[rmcp::tool(
        description = "Suggest a session for the user to start: it appears under Suggested \
            sessions in the dashboard and the TUI until they start or dismiss it. Give a \
            title, the prompt the session starts with, and why. Suggesting the same `id` again \
            replaces it."
    )]
    async fn suggest_session(
        &self,
        Parameters(params): Parameters<SuggestSessionParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<SuggestOutcome>, CallToolResult> {
        if let Some(refusal) = read_only_refusal(&self.state) {
            return Err(refusal);
        }
        let pushed_by = match caller_session_id(&context) {
            Some(id) => self
                .state
                .instances
                .read()
                .await
                .iter()
                .find(|inst| inst.id == id)
                .map(|inst| inst.title.clone()),
            None => None,
        }
        .unwrap_or_else(|| "Agent".to_string());
        let input = crate::plugin::suggestions::AgentPush {
            id: params.id,
            title: params.title,
            reason: params.reason,
            prompt: params.prompt,
            project: params.project,
            pushed_by,
        };
        crate::server::api::push_agent_suggestion(&self.state, input)
            .await
            .map(|id| Json(SuggestOutcome { id }))
            .map_err(|(_, code, message)| tool_error(code, message))
    }

    #[rmcp::tool(
        description = "Send a prompt to the session that created this one, as `send` does, \
            with the same result. Listed only for a session with a parent."
    )]
    async fn send_to_parent(
        &self,
        Parameters(params): Parameters<SendToParentParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<Json<SendOutcome>, CallToolResult> {
        if let Some(refusal) = read_only_refusal(&self.state) {
            return Err(refusal);
        }
        let Some(parent) = self.caller_parent(&context).await else {
            return Err(tool_error(
                "no_parent",
                "The calling session has no live parent session to send to",
            ));
        };
        self.send_to(parent, params.message)
            .await
            .map(Json)
            .map_err(|e| *e)
    }

    #[rmcp::tool(
        description = "Read a session's output. A structured session returns its transcript \
            since an event seq as text, or the raw events, with `next_since` to read on from \
            without skipping anything. A text page holds whole events up to 48 KB; one event \
            larger than that comes back whole as the only event on its page. A terminal session \
            returns a capture of its pane."
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
                .replay_page(&id, since, Some(READ_PAGE))
                .map_err(|_| tool_error("internal", "Could not read stored output"))?;
            if params.raw {
                let events = page
                    .events
                    .iter()
                    .map(|stored| json!({"seq": stored.seq, "event": stored.event.as_ref()}))
                    .collect();
                return Ok(Json(Output {
                    view: View::Structured,
                    text: None,
                    events: Some(events),
                    next_since: Some(page.last_scanned_seq.unwrap_or(since)),
                    has_more: page.has_more,
                }));
            }
            let (text, next_since, has_more) = rendered_page(page, since);
            return Ok(Json(Output {
                view: View::Structured,
                text: Some(text),
                events: None,
                next_since: Some(next_since),
                has_more,
            }));
        }
        let lines = params.lines.unwrap_or(DEFAULT_LINES).clamp(1, 2000) as usize;
        let text = capture_terminal_output(instance, lines, false)
            .await
            .map_err(|e| match e {
                CaptureError::NotRunning => {
                    tool_error("not_running", "The terminal session is not running")
                }
                CaptureError::Tmux(e) => {
                    tracing::error!(target: "http.api.sessions", session = %id, error = %e, "read_output: tmux error");
                    tool_error("internal", "Terminal capture failed")
                }
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
    async fn send_to(
        &self,
        id: String,
        message: String,
    ) -> Result<SendOutcome, Box<CallToolResult>> {
        let Some(structured) = self.view_of(&id).await.map(|v| v == View::Structured) else {
            return Err(Box::new(not_found(&id)));
        };
        if structured {
            if message.trim().is_empty() {
                return Err(Box::new(tool_error(
                    "invalid_request",
                    "The message is empty",
                )));
            }
            let submission = PromptSubmission {
                text: &message,
                uploads: &[],
                prompt_id: None,
                queued_prompt_id: None,
                no_revive: false,
                refuse_if_ignored: true,
                origin: crate::daemon::PromptOrigin::Mcp,
            };
            let submitted =
                match submit_prompt(&self.state, &SessionCaller::User, &id, submission).await {
                    Ok(submitted) => submitted,
                    Err(error) => return Err(Box::new(prompt_refusal(&id, error).await)),
                };
            return send_outcome(submitted)
                .map_err(|unavailability| Box::new(native_unavailable(unavailability)));
        }
        let Some(message) = terminal_text(message) else {
            return Err(Box::new(tool_error(
                "invalid_request",
                "The message is empty",
            )));
        };
        // Ignore outranks the pane probe: an ignored row whose pane shows a dialog would
        // report `waiting` forever, and answering that dialog would never unblock it.
        if self.session_is_ignored(&id).await {
            return Err(Box::new(
                terminal_refusal(&id, SendKeysError::Ignored).await,
            ));
        }
        // Keys sent into a dialog answer it (Enter accepts Claude's folder trust), so MCP refuses a
        // pane whose status rules detect one; an agent without status rules gets no check. HTTP
        // `POST /send` is left as is: a person at the dashboard sees the pane.
        let checked_pane = self.terminal_pane_refusal(&id).await?;
        // No revival: a restarted agent can open on a dialog the check above never saw.
        if let Err(e) = send_terminal_message(
            &self.state.session_service,
            &id,
            message.clone(),
            false,
            true,
            SendPane {
                checked: Some(checked_pane),
                draft_wait: std::time::Duration::ZERO,
            },
        )
        .await
        {
            return Err(Box::new(terminal_refusal(&id, e.error).await));
        }
        log_terminal_message_sent(&id, &message);
        self.publish().await;
        Ok(SendOutcome {
            disposition: Disposition::Sent,
            reason: None,
            queued_id: None,
            seq: None,
        })
    }

    /// The caller session's parent, when it is live in the caller's profile.
    pub(super) async fn caller_parent(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Option<String> {
        let caller = caller_session_id(context)?;
        let instances = self.state.instances.read().await;
        let caller = instances.iter().find(|i| i.id == caller)?;
        let parent = caller.parent_session_id.as_deref()?;
        instances
            .iter()
            .find(|i| {
                i.id == parent
                    && i.source_profile == caller.source_profile
                    && !i.is_trashed()
                    && !i.is_archived()
            })
            .map(|i| i.id.clone())
    }

    /// The caller, when an explicit parent naming it would pass `create_session`'s checks for
    /// `profile`; otherwise the create stays top-level, as it was before parents defaulted.
    async fn caller_as_parent(
        &self,
        context: &RequestContext<RoleServer>,
        profile: Option<&str>,
    ) -> Option<String> {
        let caller = caller_session_id(context)?;
        let profile = profile.map_or_else(|| self.state.served_profile().clone(), str::to_string);
        let instances = self.state.instances.read().await;
        instances
            .iter()
            .find(|i| {
                i.id == caller
                    && i.source_profile == profile
                    && !i.is_trashed()
                    && i.ensure_can_parent().is_ok()
            })
            .map(|i| i.id.clone())
    }

    /// The canonical ignore marker, which outranks the snapshot and any pane probe.
    async fn session_is_ignored(&self, id: &str) -> bool {
        match self.state.session_service.is_ignored_on_disk(id).await {
            Ok(ignored) => ignored,
            Err(error) => {
                tracing::warn!(target: "http.mcp", session = %id, %error,
                    "reading the canonical ignore marker failed");
                false
            }
        }
    }

    pub(super) async fn view_of(&self, id: &str) -> Option<View> {
        self.state
            .instances
            .read()
            .await
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.view)
    }

    /// What a create tells its caller about its terminal prompt once the wait for the agent's
    /// input prompt has settled as `readiness` on `checked_pane`: whether it was sent, how, why
    /// not, and the code for a refused draft.
    async fn deliver_terminal_prompt(
        &self,
        id: &str,
        text: String,
        readiness: PaneReadiness,
        checked_pane: Option<CheckedSendPane>,
    ) -> PromptOutcome {
        match readiness {
            PaneReadiness::Waiting => (
                false,
                PromptDelivery::BlockedWaiting,
                Some(
                    "The agent is waiting on a dialog in its terminal, such as a folder-trust \
                     question. Nothing was typed: a person answers it there, then `send` the \
                     prompt."
                        .to_string(),
                ),
                None,
            ),
            PaneReadiness::NotYet | PaneReadiness::Unreadable => (
                false,
                PromptDelivery::NotReady,
                Some(format!(
                    "The agent did not reach its input prompt within {} s, so nothing was \
                     typed. `send` the prompt once it is ready.",
                    PROMPT_READY_TIMEOUT.as_secs()
                )),
                None,
            ),
            PaneReadiness::Drafted => (
                false,
                PromptDelivery::NotReady,
                Some(crate::tmux::InputLine::BUSY_MESSAGE.to_string()),
                Some(INPUT_LINE_BUSY),
            ),
            PaneReadiness::NoPromptRule => (
                false,
                PromptDelivery::NotReady,
                Some(
                    "This agent has no status rules that show its input prompt, so nothing \
                     was typed. `send` the prompt instead."
                        .to_string(),
                ),
                None,
            ),
            PaneReadiness::Gone => (false, PromptDelivery::NotRunning, None, None),
            // A session that just failed to start is not restarted behind the caller's back.
            PaneReadiness::AtPrompt => {
                let Some(checked) = checked_pane else {
                    return (false, PromptDelivery::NotRunning, None, None);
                };
                match send_terminal_message(
                    &self.state.session_service,
                    id,
                    text.clone(),
                    false,
                    true,
                    SendPane {
                        checked: Some(checked),
                        draft_wait: std::time::Duration::ZERO,
                    },
                )
                .await
                {
                    Ok(_) => {
                        log_terminal_message_sent(id, &text);
                        self.publish().await;
                        (true, PromptDelivery::Sent, None, None)
                    }
                    Err(e) => match e.error {
                        SendKeysError::Ignored => (
                            false,
                            PromptDelivery::Ignored,
                            Some(IGNORED_PROMPT.to_string()),
                            None,
                        ),
                        SendKeysError::NotRunning => {
                            (false, PromptDelivery::NotRunning, None, None)
                        }
                        other => {
                            let code = matches!(other, SendKeysError::InputLineBusy)
                                .then_some(INPUT_LINE_BUSY);
                            let refusal = terminal_refusal(id, other).await;
                            let message = refusal
                                .structured_content
                                .as_ref()
                                .and_then(|c| c["message"].as_str())
                                .map(str::to_string);
                            (false, PromptDelivery::Failed, message, code)
                        }
                    },
                }
            }
        }
    }

    /// Why `send` refuses a terminal session's pane, read once, without waiting: its status rules
    /// detect a dialog or question waiting on a person, or its input box holds a person's draft.
    /// A check that fails to run counts as waiting, as does, for an agent with a status manifest,
    /// a pane that can't be captured. `send` is explicit, so an agent without one
    /// (`NoPromptRule`) is not refused. A session with no pane to check is refused as not
    /// running, so `send` never types into a pane that appeared after the check. Otherwise
    /// answers the pane it checked, for the send.
    async fn terminal_pane_refusal(
        &self,
        id: &str,
    ) -> Result<CheckedSendPane, Box<CallToolResult>> {
        let instance = self
            .state
            .instances
            .read()
            .await
            .iter()
            .find(|i| i.id == id)
            .cloned();
        let Some(instance) = instance else {
            return Err(Box::new(
                terminal_refusal(id, SendKeysError::NotFound).await,
            ));
        };
        let (readiness, pane) = tokio::task::spawn_blocking(move || pane_readiness(&instance))
            .await
            .unwrap_or((PaneReadiness::Unreadable, None));
        match readiness {
            PaneReadiness::Drafted => Err(Box::new(tool_error(
                INPUT_LINE_BUSY,
                crate::tmux::InputLine::BUSY_MESSAGE,
            ))),
            readiness if refuses_send(readiness) => Err(Box::new(tool_error(
                "waiting",
                "The agent is waiting on a dialog or question in its terminal, or its pane could \
                 not be read, so nothing was typed. A person answers a dialog there; retry once \
                 the pane can be read.",
            ))),
            _ => match pane {
                Some(pane) => Ok(pane),
                None => Err(Box::new(
                    terminal_refusal(id, SendKeysError::NotRunning).await,
                )),
            },
        }
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

/// The created session from a create response, or the refusal it carries, and whether this call
/// created it: `201` is a fresh create, `200` the answer to a retried `idempotency_key`.
async fn success_body(
    response: axum::response::Response,
) -> Result<(SessionResponse, bool), Box<CallToolResult>> {
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .map_err(|e| Box::new(tool_error("internal", e.to_string())))?;
    if !status.is_success() {
        return Err(Box::new(refusal(status, &bytes)));
    }
    let session = serde_json::from_slice(&bytes)
        .map_err(|e| Box::new(tool_error("internal", e.to_string())))?;
    Ok((session, status == StatusCode::CREATED))
}

/// What a terminal agent's pane shows, read with the rules the status poller uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneReadiness {
    /// A prompt-box rule matched the agent's own input prompt.
    AtPrompt,
    /// A rule matched a dialog or question waiting for a person.
    Waiting,
    /// The input box holds text a person typed and has not sent.
    Drafted,
    /// Neither yet: still starting, working, idle by a rule that does not see the prompt box, or
    /// the pane could not be captured.
    NotYet,
    /// The agent has no detection manifest, so nothing can show it is at its prompt.
    NoPromptRule,
    /// The pane could not be captured, so nothing is known about it.
    Unreadable,
    /// The session is gone, stopped or failed.
    Gone,
}

/// Waits up to [`PROMPT_READY_TIMEOUT`] for a terminal session's agent to reach its input
/// prompt, so a first prompt is never typed into a dialog. Returns at once on a dialog, with the
/// pane the last check read.
async fn wait_for_input_prompt(
    state: &std::sync::Arc<crate::server::AppState>,
    id: &str,
) -> (PaneReadiness, Option<CheckedSendPane>) {
    let deadline = tokio::time::Instant::now() + PROMPT_READY_TIMEOUT;
    loop {
        let instance = state
            .instances
            .read()
            .await
            .iter()
            .find(|i| i.id == id)
            .cloned();
        let Some(instance) = instance else {
            return (PaneReadiness::Gone, None);
        };
        let checked = tokio::task::spawn_blocking(move || pane_readiness(&instance))
            .await
            .unwrap_or((PaneReadiness::NotYet, None));
        let settled = !matches!(checked.0, PaneReadiness::NotYet | PaneReadiness::Unreadable);
        if settled || tokio::time::Instant::now() >= deadline {
            return checked;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// One fresh capture of the agent's pane, decided by its detection manifest, as `aoe session
/// show` reads it, and the pane it read, so the send types into that one. A stopped session is
/// `Gone` before any capture, whose failure would read as `Unreadable`.
fn pane_readiness(instance: &Instance) -> (PaneReadiness, Option<CheckedSendPane>) {
    if matches!(
        instance.status,
        Status::Error | Status::Stopped | Status::Deleting
    ) {
        return (PaneReadiness::Gone, None);
    }
    let Ok(session) = instance.tmux_session() else {
        return (PaneReadiness::Gone, None);
    };
    if !session.exists() || session.is_pane_dead() {
        return (PaneReadiness::Gone, None);
    }
    let pane = session
        .live_pane_target_with_deadline(&crate::tmux::TmuxCommandDeadline::new())
        .ok();
    let Some(checked) = pane
        .as_deref()
        .and_then(|pane| CheckedSendPane::capture(instance, session.name(), pane).ok())
    else {
        return (PaneReadiness::Unreadable, None);
    };
    let readiness = readiness_in(instance, &checked.session(), Some(checked.pane_id()));
    (readiness, Some(checked))
}

/// [`pane_readiness`] of the pane its lookup found. Without one nothing is captured: the
/// session's default target may be another pane than the one a send would type into.
fn readiness_in(
    instance: &Instance,
    session: &crate::tmux::Session,
    pane: Option<&str>,
) -> PaneReadiness {
    let Some(pane) = pane else {
        return PaneReadiness::Unreadable;
    };
    let (detected, input_line) =
        instance.detect_pane_and_input_line_now(&instance.source_profile, session, pane);
    let readiness = readiness_of(detected, input_line);
    #[cfg(test)]
    crate::tmux::test_helpers::after_input_check(pane);
    readiness
}

/// MCP `send` refuses a pane whose status rules detect a dialog, one holding a draft, or one it
/// could not read. An agent without status rules (`NoPromptRule`) gets no dialog check.
fn refuses_send(readiness: PaneReadiness) -> bool {
    matches!(
        readiness,
        PaneReadiness::Waiting | PaneReadiness::Drafted | PaneReadiness::Unreadable
    )
}

/// Only a rule that sees the prompt box counts as at the prompt: other idle rules (a finished
/// turn, an interrupt banner, the pane title) also hold while a dialog is on screen.
/// A dialog outranks a draft, which outranks the prompt box.
fn readiness_of(
    detected: Option<(Option<Status>, &str)>,
    input_line: crate::tmux::InputLine,
) -> PaneReadiness {
    match detected {
        None => PaneReadiness::NoPromptRule,
        Some((_, CAPTURE_FAILED_RULE)) => PaneReadiness::Unreadable,
        Some((Some(Status::Waiting), _)) => PaneReadiness::Waiting,
        _ if input_line == crate::tmux::InputLine::Draft => PaneReadiness::Drafted,
        Some((Some(Status::Idle), "ready_prompt" | "live_prompt_box")) => PaneReadiness::AtPrompt,
        Some(_) => PaneReadiness::NotYet,
    }
}

/// The audit line HTTP `POST /send` writes for keystrokes typed into a pane.
fn log_terminal_message_sent(id: &str, message: &str) {
    tracing::info!(target: "session.lifecycle", session = %id, len = message.len(), via = "mcp", "terminal message sent");
}

/// A structured session's transcript after `since` as one text page: the text, the cursor to
/// read on from, and whether more follows it.
pub(super) fn transcript_page(
    state: &crate::server::AppState,
    id: &str,
    since: u64,
) -> anyhow::Result<(String, u64, bool)> {
    let page = state
        .acp_event_store
        .replay_page(id, since, Some(READ_PAGE))?;
    Ok(rendered_page(page, since))
}

fn rendered_page(page: crate::acp::event_store::ReplayPage, since: u64) -> (String, u64, bool) {
    let text_page = text_page(&page.events, since);
    // A page cut short leaves events for the next call, whatever the store says.
    let next_since = match text_page.last_seq {
        Some(seq) if text_page.cut => seq,
        _ => page.last_scanned_seq.unwrap_or(since),
    };
    (text_page.text, next_since, page.has_more || text_page.cut)
}

/// One `read_output` text page: the longest run of whole events, from `since`, whose rendering
/// fits in [`TEXT_PAGE_BYTES`], or the first event alone when it does not fit.
struct TextPage {
    text: String,
    /// The last event rendered, when the page stops short of the events read.
    last_seq: Option<u64>,
    /// Fewer events were rendered than were read.
    cut: bool,
}

fn text_page(events: &[crate::acp::event_store::StoredEvent], since: u64) -> TextPage {
    use crate::session::conversation_summary::render_transcript_delta;
    let render = |n: usize| {
        render_transcript_delta(
            events[..n]
                .iter()
                .map(|stored| (stored.seq, stored.event.as_ref())),
            since,
        )
        .0
    };
    let whole = render(events.len());
    if whole.len() <= TEXT_PAGE_BYTES {
        return TextPage {
            text: whole,
            last_seq: None,
            cut: false,
        };
    }
    // The rendering only grows with the prefix, so the longest prefix that fits is found by
    // bisection. At least one event is always returned, whole, so the cursor moves.
    let (mut fits, mut over) = (0usize, events.len());
    while over - fits > 1 {
        let mid = fits + (over - fits) / 2;
        if render(mid).len() <= TEXT_PAGE_BYTES {
            fits = mid;
        } else {
            over = mid;
        }
    }
    let n = fits.max(1);
    TextPage {
        text: render(n),
        last_seq: Some(events[n - 1].seq),
        cut: n < events.len(),
    }
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
            let mut error = json!({
                "code": code,
                "message": message,
                "http_status": status.as_u16(),
                "http_error": http_error,
            });
            // A failed create names itself in the Daemon's log; pass that on so the two can be matched.
            if let Some(reference) = json.as_ref().and_then(|v| v["reference"].as_str()) {
                error["reference"] = reference.into();
            }
            CallToolResult::structured_error(error)
        }
    }
}

fn send_outcome(
    submitted: PromptSubmitted,
) -> Result<SendOutcome, crate::acp::state::AcpConnectionUnavailability> {
    let (disposition, reason) = match submitted.dispatch {
        PromptDispatch::Sent => (Disposition::Sent, None),
        PromptDispatch::Steered => (Disposition::Steered, None),
        PromptDispatch::Unavailable { unavailability } => {
            return Err(unavailability);
        }
        PromptDispatch::Queued { reason } => (
            Disposition::Queued,
            serde_json::to_value(reason)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string)),
        ),
    };
    Ok(SendOutcome {
        disposition,
        reason,
        queued_id: submitted.queued_id,
        seq: submitted.seq,
    })
}

fn native_unavailable(
    unavailability: crate::acp::state::AcpConnectionUnavailability,
) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "code": "native_unavailable",
        "message": "Native input is unavailable; read_output before resending",
        "unavailability": unavailability,
    }))
}

async fn prompt_refusal(id: &str, error: PromptSubmitError) -> CallToolResult {
    match error {
        PromptSubmitError::AdmissionRefused(response) => {
            let status = response.status();
            match axum::body::to_bytes(response.into_body(), usize::MAX).await {
                Ok(bytes) => refusal(status, &bytes),
                Err(error) => tool_error("internal", error.to_string()),
            }
        }
        PromptSubmitError::SessionNotFound
        | PromptSubmitError::Turn(SendTurnError::SessionNotFound) => not_found(id),
        PromptSubmitError::WorkerNotReady
        | PromptSubmitError::Turn(SendTurnError::WorkerNotReady)
        | PromptSubmitError::Turn(SendTurnError::AgentChanged) => tool_error(
            "busy",
            "The session's agent is not ready yet; try again shortly",
        ),
        PromptSubmitError::Turn(SendTurnError::WorkerLostAfterPublish) => tool_error(
            "send_failed",
            "The agent stopped after the prompt was recorded; read_output before resending it",
        ),
        PromptSubmitError::Turn(SendTurnError::ControlStateUnavailable) => tool_error(
            "control_state_unavailable",
            "Session control state could not be read",
        ),
        PromptSubmitError::Turn(SendTurnError::Unavailable(unavailability))
        | PromptSubmitError::Turn(SendTurnError::UnavailableAfterPublish(unavailability)) => {
            native_unavailable(unavailability)
        }
        PromptSubmitError::Turn(SendTurnError::ResumeFailed(e)) => tool_error(
            "busy",
            format!("The session's agent could not be resumed: {e}"),
        ),
        // The queue is full; it drains as turns end.
        PromptSubmitError::Enqueue(StatusCode::TOO_MANY_REQUESTS, message) => {
            tool_error("busy", message)
        }
        PromptSubmitError::Blocked(blocked) => tool_error("invalid_request", blocked.to_string()),
        PromptSubmitError::Ignored => tool_error(
            "ignored",
            "The session is ignored because another session manages it. A person must \
             reclaim ownership or unignore it before MCP sends a prompt.",
        ),
        // The prompt stays queued and goes out after the login; resending would queue it twice.
        PromptSubmitError::LoginRequired { blocked, queued_id } => tool_error(
            "send_failed",
            format!("{blocked}; the prompt is kept in the queue as {queued_id}, don't resend it"),
        ),
        PromptSubmitError::HookFailed { message, queued_id } => tool_error(
            "send_failed",
            format!("{message}; the prompt is kept in the queue as {queued_id}, don't resend it"),
        ),
        PromptSubmitError::Enqueue(_, message) | PromptSubmitError::Attachments(_, message) => {
            tool_error("invalid_request", message)
        }
        PromptSubmitError::Turn(SendTurnError::Send(e))
        | PromptSubmitError::Turn(SendTurnError::ModeApplication(e)) => {
            tool_error("send_failed", e.to_string())
        }
        // Unreachable for a user caller that names no queue row and allows revival, and
        // `StopFenced` only follows a foreground-origin prompt.
        PromptSubmitError::StopFenced
        | PromptSubmitError::RevivalRefused
        | PromptSubmitError::Turn(SendTurnError::RevivalRefused)
        | PromptSubmitError::Turn(SendTurnError::NotOwner)
        | PromptSubmitError::Turn(SendTurnError::LoginRequired(_))
        | PromptSubmitError::Turn(SendTurnError::HookFailed(_))
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
        SendKeysError::Preparation { detail, .. }
            if detail["error"] == "terminal_input_unavailable" =>
        {
            tool_error(
                "send_failed",
                "The checked terminal pane is no longer available for input",
            )
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
        SendKeysError::Ignored => tool_error(
            "ignored",
            "The session is ignored because another session manages it. A person must \
             reclaim ownership or unignore it before MCP sends a prompt.",
        ),
        SendKeysError::Blocked(blocked) => tool_error("invalid_request", blocked.to_string()),
        SendKeysError::InputLineBusy => {
            tool_error(INPUT_LINE_BUSY, crate::tmux::InputLine::BUSY_MESSAGE)
        }
        SendKeysError::Tmux(error) => {
            tracing::warn!(target: "http.api.sessions", session = %id, %error, "terminal send failed");
            tool_error("send_failed", "Terminal send failed")
        }
        SendKeysError::Internal => tool_error("internal", "Internal error"),
    }
}

pub(super) fn not_found(id: &str) -> CallToolResult {
    tool_error("not_found", format!("No session {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A create's prompt refused for a person's draft carries `input_line_busy`, whether the
    /// wait read the draft or the send did.
    #[tokio::test(flavor = "multi_thread")]
    #[serial_test::serial]
    async fn a_create_prompt_refused_for_a_draft_names_its_code() {
        use crate::tmux::test_helpers::{only_pane_id, TmuxTestSession, INPUT_BOX_SCRIPT};
        if !crate::tmux::test_helpers::tmux_available() {
            return;
        }
        let _guard = crate::session::test_support::isolate_app_dir();
        let shows = |pane: &str, text: &str| {
            crate::tmux::tmux_command()
                .args(["capture-pane", "-p", "-t", pane])
                .output()
                .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(text))
        };
        // (case, how the wait for the input prompt settled)
        for (case, readiness) in [
            ("drafted", PaneReadiness::Drafted),
            ("drafted-at-send", PaneReadiness::AtPrompt),
        ] {
            let mut instance = Instance::new(case, "/tmp/aoe-mcp-draft-code");
            instance.source_profile = "default".into();
            instance.view = View::Terminal;
            instance.status = Status::Idle;
            crate::server::test_support::seed_instances_on_disk_for_test(
                &instance.source_profile,
                vec![instance.clone()],
            );
            let session = instance.tmux_session().unwrap();
            let mut args: Vec<String> = ["new-session", "-d", "-s", session.name(), "sh", "-c"]
                .map(str::to_string)
                .to_vec();
            args.push(INPUT_BOX_SCRIPT.to_string());
            crate::tmux::utils::append_pane_base_index_args(&mut args, session.name());
            assert!(crate::tmux::tmux_command()
                .args(&args)
                .status()
                .unwrap()
                .success());
            let _cleanup = TmuxTestSession::from_name(session.name());
            let pane = only_pane_id(session.name());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !shows(&pane, "for shortcuts") {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{case}: the box shows"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            assert!(crate::tmux::tmux_command()
                .args(["send-keys", "-t", &pane, "-l", "half typed"])
                .status()
                .unwrap()
                .success());
            while !shows(&pane, "❯ half typed") {
                assert!(
                    std::time::Instant::now() < deadline,
                    "{case}: the draft shows"
                );
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            let state = crate::server::test_support::build_test_app_state(vec![instance.clone()]);
            crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
            let checked = CheckedSendPane::capture(&instance, session.name(), &pane).unwrap();

            let (sent, _, _, code) = McpServer::new(state)
                .deliver_terminal_prompt(
                    &instance.id,
                    "typed-by-create".into(),
                    readiness,
                    Some(checked),
                )
                .await;
            assert!(!sent, "{case}");
            assert_eq!(code, Some(INPUT_LINE_BUSY), "{case}");
            let screen = crate::tmux::tmux_command()
                .args(["capture-pane", "-p", "-t", &pane])
                .output()
                .unwrap();
            assert!(screen.status.success(), "{case}: the pane is captured");
            assert!(
                !String::from_utf8_lossy(&screen.stdout).contains("typed-by-create"),
                "{case}"
            );
        }
    }

    /// A pane the lookup didn't find is unreadable, even when the session's default target
    /// shows the agent at its prompt.
    #[test]
    #[serial_test::serial]
    fn readiness_without_a_looked_up_pane_is_unreadable() {
        use crate::tmux::test_helpers::{only_pane_id, TmuxTestSession, INPUT_BOX_SCRIPT};
        if !crate::tmux::test_helpers::tmux_available() {
            return;
        }
        let _guard = crate::session::test_support::isolate_app_dir();
        let mut instance = Instance::new("mcp-readiness", "/tmp/aoe-mcp-readiness");
        instance.source_profile = "default".into();
        let session = instance.tmux_session().unwrap();
        let mut args: Vec<String> = ["new-session", "-d", "-s", session.name(), "sh", "-c"]
            .map(str::to_string)
            .to_vec();
        args.push(INPUT_BOX_SCRIPT.to_string());
        crate::tmux::utils::append_pane_base_index_args(&mut args, session.name());
        assert!(crate::tmux::tmux_command()
            .args(&args)
            .status()
            .unwrap()
            .success());
        let _cleanup = TmuxTestSession::from_name(session.name());
        let pane = only_pane_id(session.name());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !session
            .try_capture_pane_in(&pane, 0)
            .is_ok_and(|screen| screen.contains("for shortcuts"))
        {
            assert!(std::time::Instant::now() < deadline, "the box shows");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        for (case, looked_up, want) in [
            ("looked-up", Some(pane.as_str()), PaneReadiness::AtPrompt),
            ("lookup-failed", None, PaneReadiness::Unreadable),
        ] {
            let readiness = readiness_in(&instance, &session, looked_up);
            assert_eq!(readiness, want, "{case}");
        }
    }

    /// The status and the input box come from one capture: a screen that changes after it can't
    /// pair the first frame's prompt box with the next frame's missing one, and a failed capture
    /// isn't followed by a status read alone.
    #[test]
    #[serial_test::serial]
    fn readiness_reads_status_and_draft_from_one_capture() {
        use crate::tmux::test_helpers::{only_pane_id, TmuxTestSession, INPUT_BOX_SCRIPT};
        if !crate::tmux::test_helpers::tmux_available() {
            return;
        }
        let _guard = crate::session::test_support::isolate_app_dir();
        // (case, tool, readiness captures that fail, replaced after its capture, want)
        for (case, tool, failures, replaced, want) in [
            (
                "draft-then-replaced",
                "claude",
                0,
                true,
                PaneReadiness::Drafted,
            ),
            (
                "first-capture-fails",
                "claude",
                1,
                false,
                PaneReadiness::Drafted,
            ),
            (
                "both-captures-fail",
                "claude",
                2,
                false,
                PaneReadiness::Unreadable,
            ),
            (
                "no-rules-both-fail",
                "no-manifest-agent",
                2,
                false,
                PaneReadiness::NoPromptRule,
            ),
        ] {
            let mut instance = Instance::new(
                &format!("mcp-one-capture-{case}"),
                "/tmp/aoe-mcp-one-capture",
            );
            instance.tool = tool.into();
            instance.source_profile = "default".into();
            let session = instance.tmux_session().unwrap();
            let mut args: Vec<String> = ["new-session", "-d", "-s", session.name(), "sh", "-c"]
                .map(str::to_string)
                .to_vec();
            args.push(INPUT_BOX_SCRIPT.to_string());
            crate::tmux::utils::append_pane_base_index_args(&mut args, session.name());
            assert!(crate::tmux::tmux_command()
                .args(&args)
                .status()
                .unwrap()
                .success());
            let _cleanup = TmuxTestSession::from_name(session.name());
            let pane = only_pane_id(session.name());
            let shows = |text: &str| {
                session
                    .try_capture_pane_in(&pane, 0)
                    .is_ok_and(|screen| screen.contains(text))
            };
            let wait_until_shown = |text: &str| {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while !shows(text) {
                    assert!(std::time::Instant::now() < deadline, "{case}: {text} shows");
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            };
            wait_until_shown("for shortcuts");
            assert!(crate::tmux::tmux_command()
                .args(["send-keys", "-t", &pane, "-l", "half typed"])
                .status()
                .unwrap()
                .success());
            wait_until_shown("❯ half typed");

            crate::tmux::test_helpers::fail_screen_captures(&pane, failures);
            if replaced {
                // After the capture, the pane is replaced by a screen with no input box.
                let replaced = pane.clone();
                crate::tmux::test_helpers::on_screen_captured(&pane, move || {
                    assert!(crate::tmux::tmux_command()
                        .args([
                            "respawn-pane",
                            "-k",
                            "-t",
                            &replaced,
                            "printf 'another screen\\n'; exec cat",
                        ])
                        .status()
                        .unwrap()
                        .success());
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                    while !crate::tmux::tmux_command()
                        .args(["capture-pane", "-p", "-t", &replaced])
                        .output()
                        .is_ok_and(|out| {
                            String::from_utf8_lossy(&out.stdout).contains("another screen")
                        })
                    {
                        assert!(std::time::Instant::now() < deadline, "the new screen shows");
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                });
            }
            let readiness = readiness_in(&instance, &session, Some(&pane));
            assert_eq!(readiness, want, "{case}");
        }
    }

    #[test]
    fn only_a_prompt_box_rule_reads_as_at_the_prompt() {
        use crate::tmux::InputLine::{Draft, Empty, Unknown};
        use PaneReadiness::*;
        // (detected, input line, readiness, whether MCP `send` refuses it)
        let cases = [
            (None, Unknown, NoPromptRule, false),
            (
                Some((Some(Status::Waiting), "trust_dialog")),
                Unknown,
                Waiting,
                true,
            ),
            (
                Some((Some(Status::Waiting), "trust_dialog")),
                Draft,
                Waiting,
                true,
            ),
            (
                Some((Some(Status::Idle), "ready_prompt")),
                Empty,
                AtPrompt,
                false,
            ),
            (
                Some((Some(Status::Idle), "ready_prompt")),
                Draft,
                Drafted,
                true,
            ),
            (
                Some((Some(Status::Idle), "live_prompt_box")),
                Unknown,
                AtPrompt,
                false,
            ),
            (
                Some((Some(Status::Idle), "osc_title_idle")),
                Unknown,
                NotYet,
                false,
            ),
            (
                Some((Some(Status::Idle), "completed_turn")),
                Unknown,
                NotYet,
                false,
            ),
            (
                Some((Some(Status::Idle), "interrupt_banner")),
                Unknown,
                NotYet,
                false,
            ),
            (
                Some((Some(Status::Idle), "no_rule")),
                Unknown,
                NotYet,
                false,
            ),
            (
                Some((Some(Status::Running), "spinner")),
                Unknown,
                NotYet,
                false,
            ),
            (
                Some((Some(Status::Running), "spinner")),
                Draft,
                Drafted,
                true,
            ),
            (Some((None, CAPTURE_FAILED_RULE)), Draft, Unreadable, true),
        ];
        for (detected, input_line, want, refused) in cases {
            assert_eq!(
                readiness_of(detected, input_line),
                want,
                "{detected:?} {input_line:?}"
            );
            assert_eq!(refuses_send(want), refused, "{want:?}");
        }
    }

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

        // A failed create's reference reaches the caller, to match the Daemon's log line.
        let failed = json!({
            "error": "create_failed", "message": "Failed to create session",
            "stage": "provisioning", "reference": "mcp-0f3a",
        });
        let content = refusal(StatusCode::BAD_REQUEST, failed.to_string().as_bytes())
            .structured_content
            .unwrap();
        assert_eq!(content["code"], "invalid_request");
        assert_eq!(content["reference"], "mcp-0f3a");
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
    /// instead of "try again"; a full queue is retryable, and a prompt already recorded is not.
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
            (SendKeysError::Ignored, "ignored"),
            (SendKeysError::InputLineBusy, "input_line_busy"),
            (SendKeysError::Gone, "not_found"),
        ];
        for (error, code) in cases {
            let result = terminal_refusal("s", error).await;
            assert_eq!(result.structured_content.unwrap()["code"], code);
        }
        let unavailable = crate::acp::state::AcpConnectionUnavailability {
            source: crate::acp::state::AcpConnectionSource::Sdk,
            cause: crate::acp::state::AcpConnectionCause::IncomingEnded,
            phase: crate::acp::state::AcpConnectionPhase::Established,
        };
        let prompt_cases = [
            (
                PromptSubmitError::Enqueue(StatusCode::TOO_MANY_REQUESTS, "queue is full".into()),
                "busy",
            ),
            (PromptSubmitError::WorkerNotReady, "busy"),
            (
                PromptSubmitError::Turn(SendTurnError::ControlStateUnavailable),
                "control_state_unavailable",
            ),
            // Already in the transcript, so a retry would record it twice.
            (
                PromptSubmitError::Turn(SendTurnError::WorkerLostAfterPublish),
                "send_failed",
            ),
            (
                PromptSubmitError::Turn(SendTurnError::Unavailable(unavailable)),
                "native_unavailable",
            ),
            (
                PromptSubmitError::Turn(SendTurnError::UnavailableAfterPublish(unavailable)),
                "native_unavailable",
            ),
        ];
        for (error, code) in prompt_cases {
            let result = prompt_refusal("s", error).await;
            let content = result.structured_content.unwrap();
            assert_eq!(content["code"], code);
            if code == "native_unavailable" {
                assert_eq!(content["unavailability"], json!(unavailable));
                assert!(content["message"]
                    .as_str()
                    .unwrap()
                    .contains("read_output before resending"));
            }
        }
        let result = prompt_refusal(
            "s",
            PromptSubmitError::AdmissionRefused(
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({
                        "error": "worker_capacity",
                        "message": "No worker capacity",
                    })),
                )
                    .into_response(),
            ),
        )
        .await;
        let content = result.structured_content.unwrap();
        assert_eq!(content["code"], "busy");
        assert_eq!(content["http_error"], "worker_capacity");
        assert_eq!(content["http_status"], 503);
        assert_eq!(content["message"], "No worker capacity");
    }
}
