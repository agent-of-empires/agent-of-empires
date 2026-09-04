//! TranscriptModel: the server-side render model for the structured-view
//! transcript.
//!
//! `AcpState` (in `state.rs`) is the control-state reducer: it tracks the
//! session's live status (turn active, pending approvals, usage, mode, ...)
//! but deliberately drops the ordered activity stream (`AgentMessageChunk`
//! text, tool-card lifecycle, dividers). Two separate reducers reconstruct
//! that ordered stream today: the web reducer (`web/src/lib/acpTypes.ts`,
//! `applyEvent`) and the native TUI reducer (`src/tui/structured_view/reducer.rs`,
//! `AcpTranscript`). They duplicate the tool-call lifecycle merge, the
//! suppression of AskUserQuestion tool cards, the open-tool sweep on turn
//! end, the divider synthesis, and the paging-seam dedupe.
//!
//! This module centralizes that fold so the daemon can own the transcript
//! once and serve every client. It is the superset of the two reducers, with
//! the web reducer as the authoritative reference. It emits [`TranscriptDelta`]
//! values from each event so a client can apply incremental updates instead of
//! re-reducing the whole log.
//!
//! Scope: this owns the ordered *rows* only. Everything that is control-state
//! (turn active, pending approvals, rate limits, usage, mode, config options)
//! stays on `AcpState`. The model tracks a small slice of turn state
//! internally (`turn_active`, `turn_has_output`, `steering`) solely to decide
//! the `empty_output` notice and the divider suppression, mirroring the web
//! reducer's `turnHasOutput` / `turnActive` reads.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::elicitations::ElicitationAnswer;
use super::state::{DiffComment, Event, PromptAttachmentRef, ToolCall, ToolOutputBlock};

/// One renderable row of the transcript. A stable superset of the web
/// `ActivityRow` union and the TUI `ActivityRow` enum: the same shape serves
/// web's group-at-render and the TUI's group-in-reduce, so a client picks its
/// grouping from `group_id` without re-deriving it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptRow {
    /// Stable row id. Deterministic where the web reducer's is (`start-<id>`,
    /// `done-<id>`, `user-seq-<seq>`, `cleared-<seq>`, ...), so a client that
    /// keys rows by id reconciles a live append against a replayed row.
    pub id: String,
    /// Grouping key. Consecutive `AgentMessageChunk` rows share one group so a
    /// client renders them as one assistant bubble; a tool call's rows share
    /// `tool-<tool_call_id>`; every other row gets a fresh group. See
    /// `TranscriptModel::message_group`.
    pub group_id: String,
    pub kind: TranscriptRowKind,
    pub at: DateTime<Utc>,
    pub text: String,
    /// Set on the four tool-lifecycle kinds so a client can pair a completion
    /// with its start without parsing the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Full tool payload, present on `tool_start` rows so a client can pick a
    /// per-kind renderer without a lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolCall>,
    /// Structured completion payload on `tool_complete` / `tool_error` rows
    /// (media/resource blocks the agent shipped only at completion). Empty for
    /// text-only completions, which render from `text`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output: Vec<ToolOutputBlock>,
    /// Attachment refs on a `user_prompt` row (metadata only; bytes are
    /// fetched lazily). Stage B materializes the replay URL from these.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<PromptAttachmentRef>,
    /// Structured payload on a `user_diff_comments` row; `text` holds the
    /// assembled markdown as the fallback / agent-visible body.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff_comments: Option<DiffCommentsPayload>,
    /// Display-ready answers on an `elicitation_answered` row.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub elicitation_answers: Vec<ElicitationAnswer>,
    /// True on a `tool_complete` row that is the synchronous launch of an
    /// async sub-agent (Claude `Task` with `isAsync`). The client routes it to
    /// a neutral background-dispatch card and drops the marker body.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub async_subagent: bool,
}

/// The kind discriminant for a [`TranscriptRow`]. Mirrors the web
/// `ActivityRow["kind"]` union with one deliberate omission: there is no
/// `thinking` row. Neither reducer appends a thinking transcript row (the web
/// reducer only sets `state.thinking`, the TUI only sets `status_text`); it is
/// control-state, already tracked on `AcpState.thinking`. Adding an unused
/// variant would be dead code, so it is left to `AcpState`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptRowKind {
    ToolStart,
    ToolComplete,
    ToolError,
    ToolStopped,
    Message,
    UserPrompt,
    UserDiffComments,
    ElicitationAnswered,
    EmptyOutput,
    ContextReset,
    SessionCleared,
    Compacted,
    Summary,
    /// An error or lifecycle notice the user needs in the timeline: a failed
    /// startup, a turn that died mid-flight, an adapter refusing a mode
    /// switch, or the reconciler auto-resuming a rate-limit park. The native
    /// view renders these inline where it once built them itself; the web
    /// renders the same information as dismissible banners from its own
    /// control state and skips these rows. See #1722 / #3152 / #1233.
    Notice,
}

/// Structured payload for a `user_diff_comments` row. Field-for-field the
/// `UserDiffCommentsPrompt` event minus the assembled markdown (which lives on
/// the row's `text`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiffCommentsPayload {
    pub intro: String,
    pub outro: String,
    pub is_multi_repo: bool,
    pub comments: Vec<DiffComment>,
}

/// An incremental change to the ordered row list, emitted per event so a
/// client applies updates without re-reducing the whole log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TranscriptDelta {
    /// A new row was appended at the end.
    Append(TranscriptRow),
    /// The row with `id` changed. Carries the full new row so the client
    /// replaces by id; the only fields the model ever mutates in place are a
    /// `tool_start` row's `tool` / `text` / `at` (from a duplicate start or a
    /// `ToolCallUpdated`).
    Patch { id: String, row: TranscriptRow },
    /// The row with this id was removed (an AskUserQuestion tool card
    /// superseded by its elicitation form).
    Remove(String),
}

/// Folds the ACP `Event` stream into an ordered [`TranscriptRow`] list. One
/// instance per session, single-writer, exactly like `AcpState`.
#[derive(Debug, Clone)]
pub struct TranscriptModel {
    rows: Vec<TranscriptRow>,
    /// Fast membership test for row ids (seq disambiguation, answer dedupe).
    row_ids: HashSet<String>,
    /// tool_call_ids that reached a terminal row (`tool_complete` /
    /// `tool_error` / `tool_stopped`), so the sweep and a re-completion do not
    /// double-close.
    terminal_tools: HashSet<String>,
    /// Streaming output buffered by `ToolCallContent`, keyed by tool_call_id.
    /// Drained on completion (as a fallback) or by the open-tool sweep.
    tool_outputs: HashMap<String, String>,
    /// tool_call_ids surfaced as an elicitation (AskUserQuestion). Their tool
    /// cards are suppressed; the elicitation form is the real UI.
    elicitation_tool_ids: HashSet<String>,
    /// The group id of the run of consecutive `AgentMessageChunk` rows still
    /// open, or `None` after any non-chunk event closed it.
    open_message_group: Option<String>,
    /// Monotonic counter behind [`TranscriptModel::fresh_group`].
    group_counter: u64,
    /// Highest seq consumed; frames whose seq is not greater are dropped so
    /// reconnect-replay can re-deliver without double-applying. Matches both
    /// reducers.
    last_seq: u64,
    /// Whether a turn is in flight, for the `empty_output` / context_reset
    /// gates only. Opened by a fresh (non-steered) prompt / `ThinkingStarted`,
    /// closed by `Stopped` / startup error.
    turn_active: bool,
    /// Whether the running turn has produced any visible output, so `Stopped`
    /// can fire the "no output" notice. Reset by a fresh prompt.
    turn_has_output: bool,
    /// Latest `PromptCapabilities.steering`, so a mid-turn prompt is treated
    /// as a steered continuation rather than a fresh turn.
    steering: bool,
}

impl Default for TranscriptModel {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptModel {
    pub fn new() -> Self {
        Self {
            rows: Vec::new(),
            row_ids: HashSet::new(),
            terminal_tools: HashSet::new(),
            tool_outputs: HashMap::new(),
            elicitation_tool_ids: HashSet::new(),
            open_message_group: None,
            group_counter: 0,
            last_seq: 0,
            turn_active: false,
            turn_has_output: false,
            steering: false,
        }
    }

    /// The ordered rows, oldest first.
    pub fn rows(&self) -> &[TranscriptRow] {
        &self.rows
    }

    /// Highest seq consumed.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// Apply one event at `seq`, returning the row changes it produced. Frames
    /// whose seq is not strictly greater than the last consumed seq are dropped
    /// (reconnect-replay overlap), returning no deltas.
    pub fn apply_event(&mut self, seq: u64, event: &Event) -> Vec<TranscriptDelta> {
        if seq <= self.last_seq {
            return Vec::new();
        }
        self.last_seq = seq;

        // Any non-chunk event closes the open assistant-message run, so the
        // next chunk starts a fresh group. Mirrors the TUI reducer flushing
        // its pending chunk on every non-chunk event.
        if !matches!(event, Event::AgentMessageChunk { .. }) {
            self.open_message_group = None;
        }

        match event {
            Event::AgentMessageChunk { text } => {
                let group_id = self.message_group();
                self.turn_has_output = true;
                vec![self.append(TranscriptRow::new(
                    format!("msg-{seq}"),
                    group_id,
                    TranscriptRowKind::Message,
                    text.clone(),
                ))]
            }
            Event::UserPromptSent {
                text,
                attachments,
                prompt_id,
            } => {
                self.begin_turn();
                let group_id = self.fresh_group();
                // When the client minted a prompt id, key the row by it so an
                // optimistic client row reconciles by id; otherwise fall back
                // to the seq-derived id both reducers use.
                let id = match prompt_id {
                    Some(pid) if !pid.is_empty() => pid.clone(),
                    _ => format!("user-seq-{seq}"),
                };
                let mut row =
                    TranscriptRow::new(id, group_id, TranscriptRowKind::UserPrompt, text.clone());
                row.attachments = attachments.clone();
                vec![self.append(row)]
            }
            Event::UserDiffCommentsPrompt {
                intro,
                outro,
                is_multi_repo,
                comments,
                assembled_markdown,
            } => {
                self.begin_turn();
                let group_id = self.fresh_group();
                let mut row = TranscriptRow::new(
                    format!("user-seq-{seq}"),
                    group_id,
                    TranscriptRowKind::UserDiffComments,
                    assembled_markdown.clone(),
                );
                row.diff_comments = Some(DiffCommentsPayload {
                    intro: intro.clone(),
                    outro: outro.clone(),
                    is_multi_repo: *is_multi_repo,
                    comments: comments.clone(),
                });
                vec![self.append(row)]
            }
            Event::ToolCallStarted { tool_call } => self.on_tool_started(tool_call),
            Event::ToolCallCompleted {
                tool_call_id,
                is_error,
                content,
                output,
                completed_at,
                async_subagent,
            } => self.on_tool_completed(
                seq,
                tool_call_id,
                *is_error,
                content,
                output,
                *completed_at,
                *async_subagent,
            ),
            Event::ToolCallContent {
                tool_call_id,
                content,
            } => {
                // Latest snapshot wins (per ACP, content is a replacement).
                // Buffered until completion / sweep; no row of its own.
                self.tool_outputs
                    .insert(tool_call_id.clone(), content.clone());
                Vec::new()
            }
            Event::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                started_at,
                diffs,
            } => self.on_tool_updated(
                tool_call_id,
                title.as_deref(),
                args_preview.as_deref(),
                *started_at,
                diffs.as_deref(),
            ),
            Event::ElicitationRequested { elicitation } => {
                let Some(tcid) = elicitation.tool_call_id.as_ref() else {
                    return Vec::new();
                };
                self.elicitation_tool_ids.insert(tcid.clone());
                // Strip any tool card the AskUserQuestion call already produced;
                // the elicitation form replaces it.
                self.remove_rows_for_tool(tcid)
            }
            Event::ElicitationResolved { nonce, answers, .. } => {
                let id = format!("elicitation-{}", nonce.0);
                if answers.is_empty() || self.row_ids.contains(&id) {
                    return Vec::new();
                }
                let group_id = self.fresh_group();
                let text = answers
                    .iter()
                    .map(|a| format!("{}: {}", a.question, a.answer))
                    .collect::<Vec<_>>()
                    .join("\n");
                let mut row =
                    TranscriptRow::new(id, group_id, TranscriptRowKind::ElicitationAnswered, text);
                row.elicitation_answers = answers.clone();
                vec![self.append(row)]
            }
            Event::Stopped { .. } => {
                let was_active = self.turn_active;
                let had_output = self.turn_has_output;
                let mut deltas = self.sweep_open_tools(seq);
                // A turn that opened but produced nothing visible gets a notice
                // (some slash commands emit no chunk and no tool call). Read
                // the pre-event turn state, exactly like the web reducer.
                if was_active && !had_output {
                    let group_id = self.fresh_group();
                    deltas.push(self.append(TranscriptRow::new(
                        format!("empty-{seq}"),
                        group_id,
                        TranscriptRowKind::EmptyOutput,
                        "Command produced no output.".to_string(),
                    )));
                }
                self.turn_active = false;
                deltas
            }
            // Turn-ending errors sweep open tools but never add the empty-output
            // notice: the failure itself is the turn's visible product.
            Event::AgentStartupError { message } => {
                let mut deltas = self.sweep_open_tools(seq);
                self.turn_active = false;
                self.turn_has_output = true;
                let group_id = self.fresh_group();
                deltas.push(self.append(TranscriptRow::new(
                    format!("notice-{seq}"),
                    group_id,
                    TranscriptRowKind::Notice,
                    format!("agent startup failed: {message}"),
                )));
                deltas
            }
            // The structured payload drives a dedicated remediation screen, so
            // the timeline stays quiet here.
            Event::IncompatibleAgent { .. } => {
                let deltas = self.sweep_open_tools(seq);
                self.turn_active = false;
                deltas
            }
            Event::AgentSwitched { from, to, reason } => {
                // Close any tool the prior backend left open before the divider,
                // so the order is start -> stopped -> divider.
                let mut deltas = self.sweep_open_tools(seq);
                let group_id = self.fresh_group();
                deltas.push(self.append(TranscriptRow::new(
                    format!("agent-switched-{seq}"),
                    group_id,
                    // The web reducer renders the handoff with the session_cleared
                    // divider kind rather than a dedicated one.
                    TranscriptRowKind::SessionCleared,
                    format!("Switched structured view agent from {from} to {to} ({reason})."),
                )));
                deltas
            }
            Event::SessionCleared => {
                let group_id = self.fresh_group();
                vec![self.append(TranscriptRow::new(
                    format!("cleared-{seq}"),
                    group_id,
                    TranscriptRowKind::SessionCleared,
                    "Conversation cleared, the model no longer remembers earlier turns."
                        .to_string(),
                ))]
            }
            Event::ConversationCompacted => {
                let group_id = self.fresh_group();
                vec![self.append(TranscriptRow::new(
                    format!("compacted-{seq}"),
                    group_id,
                    TranscriptRowKind::Compacted,
                    "Conversation compacted; earlier turns above are summarised in the model's context."
                        .to_string(),
                ))]
            }
            Event::SessionContextReset { reason } => {
                // Suppress the divider on a session that never saw a prompt:
                // session/load failing on a 0-prompt session is expected, not an
                // incident. Events arrive in seq order, so a scan captures every
                // earlier prompt.
                let has_prior_prompt = self.rows.iter().any(|r| {
                    matches!(
                        r.kind,
                        TranscriptRowKind::UserPrompt | TranscriptRowKind::UserDiffComments
                    )
                });
                if !has_prior_prompt {
                    return Vec::new();
                }
                let text = if reason.is_empty() {
                    "Conversation context reset; agent transcript was unavailable.".to_string()
                } else {
                    reason.clone()
                };
                let group_id = self.fresh_group();
                // The reset row is this turn's visible product; without this the
                // empty-output fallback would stack under the boundary.
                self.turn_has_output = true;
                vec![self.append(TranscriptRow::new(
                    format!("reset-{seq}"),
                    group_id,
                    TranscriptRowKind::ContextReset,
                    text,
                ))]
            }
            Event::PromptRuntimeError { message } => {
                let group_id = self.fresh_group();
                self.turn_has_output = true;
                vec![self.append(TranscriptRow::new(
                    format!("notice-{seq}"),
                    group_id,
                    TranscriptRowKind::Notice,
                    format!("prompt failed: {message}"),
                ))]
            }
            Event::ModeSwitchFailed { mode_id, reason } => {
                let group_id = self.fresh_group();
                vec![self.append(TranscriptRow::new(
                    format!("notice-{seq}"),
                    group_id,
                    TranscriptRowKind::Notice,
                    format!("mode switch to \"{mode_id}\" failed: {reason}"),
                ))]
            }
            Event::RateLimitAutoResumed { resets_at, manual } => {
                let group_id = self.fresh_group();
                let how = if *manual { "resumed" } else { "auto-resumed" };
                vec![self.append(TranscriptRow::new(
                    format!("notice-{seq}"),
                    group_id,
                    TranscriptRowKind::Notice,
                    format!("{how} at {resets_at} after rate-limit park"),
                ))]
            }
            Event::ConversationSummary { text, .. } => {
                let group_id = self.fresh_group();
                vec![self.append(TranscriptRow::new(
                    format!("summary-{seq}"),
                    group_id,
                    TranscriptRowKind::Summary,
                    text.clone(),
                ))]
            }
            // Opens the turn but is not itself output (the web reducer counts
            // ThinkingStarted as output so a pure-reasoning turn is not flagged
            // empty). No transcript row of its own.
            Event::ThinkingStarted => {
                self.turn_active = true;
                self.turn_has_output = true;
                Vec::new()
            }
            Event::PromptCapabilities { steering, .. } => {
                self.steering = *steering;
                Vec::new()
            }
            // Everything else is control-state (handled by AcpState) or carries
            // no transcript row: PlanUpdated, TodoListUpdated, ThinkingEnded,
            // approvals, DiffEmitted, rate limit / usage / mode / config,
            // background agents, cancel / rejected, wakeup / monitor,
            // AcpSessionAssigned, ConversationCompactionStarted, RawAgentUpdate,
            // SessionTitleSuggested.
            _ => Vec::new(),
        }
    }

    fn on_tool_started(&mut self, tool_call: &ToolCall) -> Vec<TranscriptDelta> {
        // An AskUserQuestion tool call is rendered by its elicitation card; if
        // the elicitation arrived first, drop the redundant start entirely.
        if self.elicitation_tool_ids.contains(&tool_call.id) {
            return Vec::new();
        }
        // A duplicate start for the same id (e.g. once full args are known) must
        // not clobber richer data already on the row with a sparser frame; merge
        // in place instead of appending a second card.
        if let Some(idx) = self.find_tool_start(&tool_call.id) {
            let existing = self.rows[idx]
                .tool
                .clone()
                .unwrap_or_else(|| tool_call.clone());
            let merged = merge_tool_start(&existing, tool_call);
            self.rows[idx].tool = Some(merged.clone());
            self.rows[idx].text = merged.name.clone();
            self.rows[idx].at = merged.started_at;
            return vec![TranscriptDelta::Patch {
                id: self.rows[idx].id.clone(),
                row: self.rows[idx].clone(),
            }];
        }
        self.turn_has_output = true;
        let mut row = TranscriptRow::new(
            format!("start-{}", tool_call.id),
            format!("tool-{}", tool_call.id),
            TranscriptRowKind::ToolStart,
            tool_call.name.clone(),
        );
        row.at = tool_call.started_at;
        row.tool_call_id = Some(tool_call.id.clone());
        row.tool = Some(tool_call.clone());
        vec![self.append(row)]
    }

    #[allow(clippy::too_many_arguments)]
    fn on_tool_completed(
        &mut self,
        seq: u64,
        tool_call_id: &str,
        is_error: bool,
        content: &str,
        output: &[ToolOutputBlock],
        completed_at: DateTime<Utc>,
        async_subagent: bool,
    ) -> Vec<TranscriptDelta> {
        // The AskUserQuestion completion is owned by its elicitation card; drop
        // it so no transcript card materializes.
        if self.elicitation_tool_ids.contains(tool_call_id) {
            return Vec::new();
        }
        let mut deltas = Vec::new();
        // A completion with no preceding start would render no card. Synthesize a
        // minimal start first so the card appears (#1713).
        if self.find_tool_start(tool_call_id).is_none() {
            let synth = synth_tool_start_row(tool_call_id, None, None, completed_at);
            self.turn_has_output = true;
            deltas.push(self.append(synth));
        }
        // Prefer content shipped with the completion; fall back to whatever
        // streamed earlier; only use the status word when neither carried text.
        let buffered = self.tool_outputs.remove(tool_call_id).unwrap_or_default();
        let text = if !content.is_empty() {
            content.to_string()
        } else if !buffered.is_empty() {
            buffered
        } else if is_error {
            "tool failed".to_string()
        } else {
            "completed".to_string()
        };
        // Some adapters reuse a tool_call_id after reconnecting; disambiguate a
        // later completion row with the seq so ids stay unique.
        let base_id = format!("done-{tool_call_id}");
        let row_id = if self.row_ids.contains(&base_id) {
            format!("{base_id}-{seq}")
        } else {
            base_id
        };
        self.terminal_tools.insert(tool_call_id.to_string());
        let mut row = TranscriptRow::new(
            row_id,
            format!("tool-{tool_call_id}"),
            if is_error {
                TranscriptRowKind::ToolError
            } else {
                TranscriptRowKind::ToolComplete
            },
            text,
        );
        row.at = completed_at;
        row.tool_call_id = Some(tool_call_id.to_string());
        row.output = output.to_vec();
        row.async_subagent = async_subagent;
        deltas.push(self.append(row));
        deltas
    }

    fn on_tool_updated(
        &mut self,
        tool_call_id: &str,
        title: Option<&str>,
        args_preview: Option<&str>,
        started_at: Option<DateTime<Utc>>,
        diffs: Option<&[super::state::DiffPreview]>,
    ) -> Vec<TranscriptDelta> {
        // An update with no preceding start would be dropped; synthesize one so
        // the update lands and a card renders (#1713). The synth carries the
        // update's fields directly, so no follow-up patch is needed.
        if self.find_tool_start(tool_call_id).is_none() {
            let mut synth = synth_tool_start_row(
                tool_call_id,
                title,
                args_preview,
                started_at.unwrap_or_else(Utc::now),
            );
            if let Some(d) = diffs {
                if !d.is_empty() {
                    if let Some(tc) = synth.tool.as_mut() {
                        tc.diffs = d.to_vec();
                    }
                }
            }
            self.turn_has_output = true;
            return vec![self.append(synth)];
        }
        let idx = self
            .find_tool_start(tool_call_id)
            .expect("tool_start present (checked above)");
        let row = &mut self.rows[idx];
        if let Some(t) = title {
            row.text = t.to_string();
        }
        if let Some(tc) = row.tool.as_mut() {
            if let Some(t) = title {
                tc.name = t.to_string();
            }
            if let Some(a) = args_preview {
                tc.args_preview = a.to_string();
            }
            if let Some(s) = started_at {
                tc.started_at = s;
            }
            // Per ACP, a non-empty diff list replaces wholesale; None/empty
            // leaves an earlier frame's diffs intact (#1721).
            if let Some(d) = diffs {
                if !d.is_empty() {
                    tc.diffs = d.to_vec();
                }
            }
        }
        vec![TranscriptDelta::Patch {
            id: row.id.clone(),
            row: row.clone(),
        }]
    }

    /// Close any `tool_start` that never received a terminal row by
    /// synthesizing a `tool_stopped` for each, draining any buffered streamed
    /// output into it. Called from every turn-ending arm; a dangling tool is
    /// "stopped", not "done" or "failed", because its outcome was never
    /// reported (#1646).
    fn sweep_open_tools(&mut self, seq: u64) -> Vec<TranscriptDelta> {
        // Collect open ids first (immutable borrow) so the appends below do not
        // re-scan the rows they add. Dedupe by tool_call_id because pre-fix logs
        // can carry duplicate tool_start rows for one call.
        let mut open: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = self.terminal_tools.clone();
        for row in &self.rows {
            if row.kind != TranscriptRowKind::ToolStart {
                continue;
            }
            let Some(id) = row.tool_call_id.as_ref() else {
                continue;
            };
            if seen.insert(id.clone()) {
                open.push(id.clone());
            }
        }
        let now = Utc::now();
        let mut deltas = Vec::new();
        for id in open {
            self.terminal_tools.insert(id.clone());
            let buffered = self.tool_outputs.remove(&id).unwrap_or_default();
            let mut row = TranscriptRow::new(
                format!("stopped-{id}-{seq}"),
                format!("tool-{id}"),
                TranscriptRowKind::ToolStopped,
                buffered,
            );
            row.at = now;
            row.tool_call_id = Some(id);
            deltas.push(self.append(row));
        }
        deltas
    }

    /// Remove every row carrying `tool_call_id` (an AskUserQuestion card
    /// superseded by its elicitation form). Rebuilds the id set since indices
    /// shift.
    fn remove_rows_for_tool(&mut self, tool_call_id: &str) -> Vec<TranscriptDelta> {
        let mut removed = Vec::new();
        self.rows.retain(|r| {
            if r.tool_call_id.as_deref() == Some(tool_call_id) {
                removed.push(TranscriptDelta::Remove(r.id.clone()));
                false
            } else {
                true
            }
        });
        if !removed.is_empty() {
            self.row_ids = self.rows.iter().map(|r| r.id.clone()).collect();
        }
        removed
    }

    /// Push a row, keeping the id set current, and return its `Append` delta.
    fn append(&mut self, row: TranscriptRow) -> TranscriptDelta {
        self.row_ids.insert(row.id.clone());
        self.rows.push(row.clone());
        TranscriptDelta::Append(row)
    }

    /// The group id for the current assistant-message run, allocating a fresh
    /// one when no run is open.
    fn message_group(&mut self) -> String {
        if let Some(g) = self.open_message_group.clone() {
            return g;
        }
        let g = self.fresh_group();
        self.open_message_group = Some(g.clone());
        g
    }

    fn fresh_group(&mut self) -> String {
        self.group_counter += 1;
        format!("g{}", self.group_counter)
    }

    /// Turn bookkeeping shared by both user-prompt kinds. A steered
    /// continuation (a mid-turn prompt injected into a running steerable turn)
    /// is not a fresh turn, so it keeps the output the turn already earned; a
    /// genuine fresh turn resets it. Mirrors the web reducer's
    /// `applyNewTurnResets` gate.
    fn begin_turn(&mut self) {
        let steered_continuation = self.turn_active && self.steering;
        self.turn_active = true;
        if !steered_continuation {
            self.turn_has_output = false;
        }
    }

    fn find_tool_start(&self, tool_call_id: &str) -> Option<usize> {
        self.rows.iter().position(|r| {
            r.kind == TranscriptRowKind::ToolStart
                && r.tool_call_id.as_deref() == Some(tool_call_id)
        })
    }
}

impl TranscriptRow {
    fn new(id: String, group_id: String, kind: TranscriptRowKind, text: String) -> Self {
        Self {
            id,
            group_id,
            kind,
            at: Utc::now(),
            text,
            tool_call_id: None,
            tool: None,
            output: Vec::new(),
            attachments: Vec::new(),
            diff_comments: None,
            elicitation_answers: Vec::new(),
            async_subagent: false,
        }
    }
}

/// Build a minimal `tool_start` row for a tool call we never saw start. Some
/// agents (Gemini's permission flow) emit updates/completions with no start
/// frame; synthesizing one keeps the card visible (#1713).
fn synth_tool_start_row(
    tool_call_id: &str,
    name: Option<&str>,
    args_preview: Option<&str>,
    started_at: DateTime<Utc>,
) -> TranscriptRow {
    let name = match name {
        Some(n) if !n.is_empty() => n.to_string(),
        _ => "tool call".to_string(),
    };
    let tool = ToolCall {
        id: tool_call_id.to_string(),
        name: name.clone(),
        kind: "other".to_string(),
        args_preview: args_preview.unwrap_or_default().to_string(),
        started_at,
        parent_tool_call_id: None,
        memory_recall: None,
        diffs: Vec::new(),
    };
    let mut row = TranscriptRow::new(
        format!("start-{tool_call_id}"),
        format!("tool-{tool_call_id}"),
        TranscriptRowKind::ToolStart,
        name,
    );
    row.at = started_at;
    row.tool_call_id = Some(tool_call_id.to_string());
    row.tool = Some(tool);
    row
}

/// Merge a duplicate `ToolCallStarted` into the existing row's payload without
/// clobbering richer data with a sparser frame. A permission start (#1713)
/// carries an empty args and `kind: "other"`; a later real start for the same
/// id must win, but a real start that arrived first must not be overwritten by
/// the sparse permission start.
///
/// The web reducer additionally preserves `raw_name` (#3070), but the Rust
/// `ToolCall` has no such field, so that branch is not portable here.
///
/// `pub(crate)` so the native TUI's server-row merge (which folds
/// `?view=rows` pages and WS snapshots by id) can guard a rich `tool_start`
/// against a sparse synth start from a later page, matching the web's
/// `mergeServerRows`.
pub(crate) fn merge_tool_start(prev: &ToolCall, incoming: &ToolCall) -> ToolCall {
    ToolCall {
        id: incoming.id.clone(),
        name: if !incoming.name.is_empty() {
            incoming.name.clone()
        } else {
            prev.name.clone()
        },
        kind: if !incoming.kind.is_empty() && incoming.kind != "other" {
            incoming.kind.clone()
        } else {
            prev.kind.clone()
        },
        args_preview: if !incoming.args_preview.trim().is_empty() {
            incoming.args_preview.clone()
        } else {
            prev.args_preview.clone()
        },
        started_at: if incoming.started_at > prev.started_at {
            incoming.started_at
        } else {
            prev.started_at
        },
        parent_tool_call_id: incoming
            .parent_tool_call_id
            .clone()
            .or_else(|| prev.parent_tool_call_id.clone()),
        memory_recall: incoming
            .memory_recall
            .clone()
            .or_else(|| prev.memory_recall.clone()),
        diffs: if !incoming.diffs.is_empty() {
            incoming.diffs.clone()
        } else {
            prev.diffs.clone()
        },
    }
}

/// Reconcile one server-folded row into a client's row buffer by id, the
/// Rust twin of the web's `mergeServerRows` per-row step. A new id appends
/// in order; an existing id is replaced, except that two `tool_start` rows
/// for one id are merged so a sparse synth start folded on a later
/// `?view=rows` page cannot clobber a richer start already buffered
/// (#1713/#2711). Idempotent, so it absorbs the WS/replay overlap without a
/// seq gate.
pub(crate) fn upsert_transcript_row(rows: &mut Vec<TranscriptRow>, incoming: TranscriptRow) {
    let Some(idx) = rows.iter().position(|r| r.id == incoming.id) else {
        rows.push(incoming);
        return;
    };
    let prev = &rows[idx];
    if prev.kind == TranscriptRowKind::ToolStart && incoming.kind == TranscriptRowKind::ToolStart {
        if let (Some(prev_tool), Some(inc_tool)) = (prev.tool.as_ref(), incoming.tool.as_ref()) {
            let merged = merge_tool_start(prev_tool, inc_tool);
            let name = merged.name.clone();
            let started_at = merged.started_at;
            let row = &mut rows[idx];
            row.text = name;
            row.at = started_at;
            row.tool = Some(merged);
            return;
        }
    }
    rows[idx] = incoming;
}

/// Replace the row with `row.id` by the server's authoritative new row (a
/// `Patch` delta carries the full row), appending when the id is absent
/// (e.g. a Patch that lands before its Append after a reconnect). The Rust
/// twin of the web's `patchServerRow`.
pub(crate) fn patch_transcript_row(rows: &mut Vec<TranscriptRow>, row: TranscriptRow) {
    match rows.iter().position(|r| r.id == row.id) {
        Some(idx) => rows[idx] = row,
        None => rows.push(row),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::approvals::Nonce;
    use crate::acp::elicitations::Elicitation;
    use crate::acp::elicitations::ElicitationOutcome;
    use crate::acp::state::{DiffPreview, MemoryRecall, PromptAttachmentKind};
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(secs, 0).single().expect("valid ts")
    }

    fn tool(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            kind: "execute".into(),
            args_preview: "{}".into(),
            started_at: at(100),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        }
    }

    fn completed(id: &str, is_error: bool, content: &str) -> Event {
        Event::ToolCallCompleted {
            tool_call_id: id.into(),
            is_error,
            content: content.into(),
            output: Vec::new(),
            completed_at: at(200),
            async_subagent: false,
        }
    }

    fn prompt(text: &str) -> Event {
        Event::UserPromptSent {
            text: text.into(),
            attachments: Vec::new(),
            prompt_id: None,
        }
    }

    fn elicitation(nonce: &str, tool_call_id: Option<&str>) -> Elicitation {
        Elicitation {
            nonce: Nonce(nonce.into()),
            message: "Pick one".into(),
            title: None,
            description: None,
            tool_call_id: tool_call_id.map(|s| s.into()),
            questions: Vec::new(),
            requested_at: at(50),
            resolved: None,
        }
    }

    /// Apply a sequence of events with auto-incrementing seq (starting at 1)
    /// and return the final model, for tests that only inspect end state.
    fn fold(events: Vec<Event>) -> TranscriptModel {
        let mut m = TranscriptModel::new();
        for (i, e) in events.into_iter().enumerate() {
            m.apply_event(i as u64 + 1, &e);
        }
        m
    }

    fn kinds(m: &TranscriptModel) -> Vec<TranscriptRowKind> {
        m.rows().iter().map(|r| r.kind).collect()
    }

    fn row_by_id<'a>(m: &'a TranscriptModel, id: &str) -> Option<&'a TranscriptRow> {
        m.rows().iter().find(|r| r.id == id)
    }

    /// The native view used to build these notices itself; since Tier 4 the
    /// transcript is server-owned, so the daemon has to emit them or they
    /// vanish from the timeline with nothing else showing them.
    #[test]
    fn turn_level_failures_emit_a_notice_row() {
        fn resets_at_fixture() -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")
                .unwrap()
                .with_timezone(&chrono::Utc)
        }
        let expected_resume = format!(
            "auto-resumed at {} after rate-limit park",
            resets_at_fixture()
        );
        let expected_manual_resume =
            format!("resumed at {} after rate-limit park", resets_at_fixture());
        let cases: [(&str, Event, &str); 5] = [
            (
                "startup",
                Event::AgentStartupError {
                    message: "node missing".into(),
                },
                "agent startup failed: node missing",
            ),
            (
                "runtime",
                Event::PromptRuntimeError {
                    message: "stream died".into(),
                },
                "prompt failed: stream died",
            ),
            (
                "mode switch",
                Event::ModeSwitchFailed {
                    mode_id: "bypassPermissions".into(),
                    reason: "denied".into(),
                },
                "mode switch to \"bypassPermissions\" failed: denied",
            ),
            (
                "rate-limit resume",
                Event::RateLimitAutoResumed {
                    resets_at: resets_at_fixture(),
                    manual: false,
                },
                expected_resume.as_str(),
            ),
            (
                "manual rate-limit resume: RESUME NOW is the user's own, \
                 so the notice must not call it automatic (#3688)",
                Event::RateLimitAutoResumed {
                    resets_at: resets_at_fixture(),
                    manual: true,
                },
                expected_manual_resume.as_str(),
            ),
        ];
        for (label, event, expected) in cases {
            let mut m = TranscriptModel::new();
            m.apply_event(1, &event);
            let row = m
                .rows()
                .iter()
                .find(|r| r.kind == TranscriptRowKind::Notice)
                .unwrap_or_else(|| panic!("{label}: no notice row"));
            assert_eq!(row.text, expected, "{label}");
        }
    }

    #[test]
    fn drops_frames_at_or_below_last_seq() {
        let mut m = TranscriptModel::new();
        let d1 = m.apply_event(1, &prompt("hi"));
        assert_eq!(d1.len(), 1);
        // Replay-vs-live overlap re-delivers seq 1; it must be a no-op.
        let d_dup = m.apply_event(1, &prompt("ignored"));
        assert!(d_dup.is_empty());
        // A lower seq is dropped too.
        let d_low = m.apply_event(0, &prompt("older"));
        assert!(d_low.is_empty());
        assert_eq!(m.rows().len(), 1);
        assert_eq!(m.rows()[0].text, "hi");
        assert_eq!(m.last_seq(), 1);
    }

    #[test]
    fn user_prompt_appends_authoritatively_with_attachments() {
        // The server does no optimism: every UserPromptSent appends a fresh
        // authoritative row (no placeholder promotion).
        let ev = Event::UserPromptSent {
            text: "look".into(),
            attachments: vec![PromptAttachmentRef {
                id: "att-1".into(),
                kind: PromptAttachmentKind::Image,
                mime_type: "image/png".into(),
                name: Some("shot.png".into()),
                size: 1234,
            }],
            prompt_id: None,
        };
        let mut m = TranscriptModel::new();
        let deltas = m.apply_event(3, &ev);
        assert_eq!(deltas.len(), 1);
        assert!(matches!(deltas[0], TranscriptDelta::Append(_)));
        let row = &m.rows()[0];
        assert_eq!(row.id, "user-seq-3");
        assert_eq!(row.kind, TranscriptRowKind::UserPrompt);
        assert_eq!(row.attachments.len(), 1);
        assert_eq!(row.attachments[0].id, "att-1");
    }

    #[test]
    fn client_minted_prompt_id_keys_the_user_row() {
        // A client-minted prompt_id becomes the row id so an optimistic client
        // row reconciles by id; absent/empty falls back to the seq-derived id.
        let cases: [(Option<&str>, &str); 3] = [
            (Some("cmp-abc"), "cmp-abc"),
            (None, "user-seq-7"),
            (Some(""), "user-seq-7"),
        ];
        for (pid, expect_id) in cases {
            let ev = Event::UserPromptSent {
                text: "hi".into(),
                attachments: Vec::new(),
                prompt_id: pid.map(|s| s.to_string()),
            };
            let mut m = TranscriptModel::new();
            m.apply_event(7, &ev);
            assert_eq!(m.rows()[0].id, expect_id, "prompt_id={pid:?}");
            assert_eq!(m.rows()[0].kind, TranscriptRowKind::UserPrompt);
        }
    }

    #[test]
    fn user_diff_comments_row_carries_structured_payload() {
        let ev = Event::UserDiffCommentsPrompt {
            intro: "look".into(),
            outro: "thanks".into(),
            is_multi_repo: true,
            comments: Vec::new(),
            assembled_markdown: "# body".into(),
        };
        let m = fold(vec![ev]);
        let row = &m.rows()[0];
        assert_eq!(row.kind, TranscriptRowKind::UserDiffComments);
        assert_eq!(row.id, "user-seq-1");
        assert_eq!(row.text, "# body");
        let dc = row.diff_comments.as_ref().expect("diff_comments payload");
        assert!(dc.is_multi_repo);
        assert_eq!(dc.intro, "look");
    }

    #[test]
    fn agent_message_chunks_are_one_row_each_with_shared_group_until_broken() {
        // One row per chunk (matching the web reducer), but consecutive chunks
        // share a group so a client renders them as one bubble; a non-chunk
        // event breaks the run.
        let m = fold(vec![
            Event::AgentMessageChunk {
                text: "Hello".into(),
            },
            Event::AgentMessageChunk {
                text: ", world".into(),
            },
            Event::ThinkingStarted,
            Event::AgentMessageChunk {
                text: "second".into(),
            },
        ]);
        let msgs: Vec<&TranscriptRow> = m
            .rows()
            .iter()
            .filter(|r| r.kind == TranscriptRowKind::Message)
            .collect();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text, "Hello");
        assert_eq!(
            msgs[0].group_id, msgs[1].group_id,
            "consecutive chunks group"
        );
        assert_ne!(
            msgs[1].group_id, msgs[2].group_id,
            "a non-chunk event starts a new group"
        );
    }

    #[test]
    fn tool_start_then_completion_are_separate_rows() {
        let m = fold(vec![
            Event::ToolCallStarted {
                tool_call: tool("t-1", "Bash"),
            },
            completed("t-1", false, "abc first\ndef second\n"),
        ]);
        assert_eq!(
            kinds(&m),
            vec![
                TranscriptRowKind::ToolStart,
                TranscriptRowKind::ToolComplete
            ]
        );
        let start = row_by_id(&m, "start-t-1").expect("start row");
        assert_eq!(start.tool.as_ref().unwrap().name, "Bash");
        let done = row_by_id(&m, "done-t-1").expect("done row");
        assert_eq!(done.text, "abc first\ndef second\n");
        // start and completion share the tool group.
        assert_eq!(start.group_id, done.group_id);
    }

    #[test]
    fn completion_text_fallbacks() {
        // Table: (buffered content via ToolCallContent, completion content,
        // is_error) -> expected row text + kind.
        struct Case {
            buffered: Option<&'static str>,
            content: &'static str,
            is_error: bool,
            expect_text: &'static str,
            expect_kind: TranscriptRowKind,
        }
        let cases = [
            // Completion content wins over buffered.
            Case {
                buffered: Some("streamed"),
                content: "final",
                is_error: false,
                expect_text: "final",
                expect_kind: TranscriptRowKind::ToolComplete,
            },
            // Falls back to buffered ToolCallContent when completion is empty.
            Case {
                buffered: Some("line1\nline2\n"),
                content: "",
                is_error: false,
                expect_text: "line1\nline2\n",
                expect_kind: TranscriptRowKind::ToolComplete,
            },
            // No content anywhere -> status word.
            Case {
                buffered: None,
                content: "",
                is_error: false,
                expect_text: "completed",
                expect_kind: TranscriptRowKind::ToolComplete,
            },
            // Error with no content -> "tool failed" and error kind.
            Case {
                buffered: None,
                content: "",
                is_error: true,
                expect_text: "tool failed",
                expect_kind: TranscriptRowKind::ToolError,
            },
        ];
        for (i, c) in cases.iter().enumerate() {
            let mut m = TranscriptModel::new();
            let mut seq = 0u64;
            let mut next = || {
                seq += 1;
                seq
            };
            m.apply_event(
                next(),
                &Event::ToolCallStarted {
                    tool_call: tool("t", "Bash"),
                },
            );
            if let Some(b) = c.buffered {
                m.apply_event(
                    next(),
                    &Event::ToolCallContent {
                        tool_call_id: "t".into(),
                        content: b.into(),
                    },
                );
            }
            m.apply_event(next(), &completed("t", c.is_error, c.content));
            let done = m
                .rows()
                .iter()
                .find(|r| {
                    r.tool_call_id.as_deref() == Some("t") && r.kind != TranscriptRowKind::ToolStart
                })
                .expect("done row");
            assert_eq!(done.text, c.expect_text, "case {i}");
            assert_eq!(done.kind, c.expect_kind, "case {i}");
            // Buffer is drained so a replay cannot double-render it.
            assert!(
                !m.tool_outputs.contains_key("t"),
                "case {i}: buffer drained"
            );
        }
    }

    #[test]
    fn async_subagent_flag_rides_the_completion_row() {
        let ev = Event::ToolCallCompleted {
            tool_call_id: "task-1".into(),
            is_error: false,
            content: "Async agent launched successfully".into(),
            output: Vec::new(),
            completed_at: at(200),
            async_subagent: true,
        };
        let m = fold(vec![
            Event::ToolCallStarted {
                tool_call: tool("task-1", "Task"),
            },
            ev,
        ]);
        let done = row_by_id(&m, "done-task-1").expect("done row");
        assert!(done.async_subagent);
    }

    #[test]
    fn synthesizes_a_start_when_completion_has_no_start() {
        // #1713: a completion with no preceding start must still render a card.
        let m = fold(vec![completed("orphan-1", false, "done output")]);
        assert_eq!(
            kinds(&m),
            vec![
                TranscriptRowKind::ToolStart,
                TranscriptRowKind::ToolComplete
            ]
        );
        assert!(row_by_id(&m, "start-orphan-1").is_some());
        assert_eq!(row_by_id(&m, "done-orphan-1").unwrap().text, "done output");
        // A synthesized card counts as turn output.
        assert!(m.turn_has_output);
    }

    #[test]
    fn synthesizes_a_start_when_update_has_no_start() {
        // #1713: an update with no preceding start synthesizes one carrying the
        // update's title/args.
        let ev = Event::ToolCallUpdated {
            tool_call_id: "orphan-2".into(),
            title: Some("run_shell_command".into()),
            args_preview: Some(r#"{"command":"ls"}"#.into()),
            started_at: None,
            diffs: None,
        };
        let m = fold(vec![ev]);
        let start = row_by_id(&m, "start-orphan-2").expect("synth start");
        assert_eq!(start.tool.as_ref().unwrap().name, "run_shell_command");
        assert_eq!(
            start.tool.as_ref().unwrap().args_preview,
            r#"{"command":"ls"}"#
        );
    }

    #[test]
    fn duplicate_start_merges_without_clobbering_richer_args() {
        // A sparse permission start must not overwrite a real start's args/kind,
        // and the later timestamp wins. Emits a Patch, not a second row.
        let mut m = TranscriptModel::new();
        m.apply_event(
            1,
            &Event::ToolCallStarted {
                tool_call: ToolCall {
                    args_preview: r#"{"x":1}"#.into(),
                    started_at: at(110),
                    ..tool("dup", "Bash")
                },
            },
        );
        let deltas = m.apply_event(
            2,
            &Event::ToolCallStarted {
                tool_call: ToolCall {
                    kind: "other".into(),
                    args_preview: "".into(),
                    started_at: at(105),
                    ..tool("dup", "Bash")
                },
            },
        );
        assert_eq!(
            m.rows()
                .iter()
                .filter(|r| r.kind == TranscriptRowKind::ToolStart)
                .count(),
            1
        );
        assert_eq!(deltas.len(), 1);
        assert!(matches!(deltas[0], TranscriptDelta::Patch { .. }));
        let row = row_by_id(&m, "start-dup").unwrap();
        let tc = row.tool.as_ref().unwrap();
        assert_eq!(tc.args_preview, r#"{"x":1}"#, "richer args survive");
        assert_eq!(
            tc.kind, "execute",
            "richer kind survives the sparse 'other'"
        );
        assert_eq!(tc.started_at, at(110), "the later started_at wins");
    }

    #[test]
    fn duplicate_start_carries_parent_and_memory_through_merge() {
        let mut m = TranscriptModel::new();
        m.apply_event(
            1,
            &Event::ToolCallStarted {
                tool_call: ToolCall {
                    args_preview: "".into(),
                    ..tool("dup2", "Bash")
                },
            },
        );
        m.apply_event(
            2,
            &Event::ToolCallStarted {
                tool_call: ToolCall {
                    parent_tool_call_id: Some("parent-9".into()),
                    memory_recall: Some(MemoryRecall {
                        mode: "recall".into(),
                        paths: vec!["/a".into()],
                        synthesized_text: None,
                    }),
                    args_preview: r#"{"y":2}"#.into(),
                    ..tool("dup2", "Bash")
                },
            },
        );
        let tc = row_by_id(&m, "start-dup2").unwrap().tool.as_ref().unwrap();
        assert_eq!(tc.parent_tool_call_id.as_deref(), Some("parent-9"));
        assert_eq!(tc.memory_recall.as_ref().unwrap().mode, "recall");
    }

    #[test]
    fn tool_update_patches_the_matching_row_only() {
        let mut m = TranscriptModel::new();
        m.apply_event(
            1,
            &Event::ToolCallStarted {
                tool_call: ToolCall {
                    args_preview: r#"{"k":1}"#.into(),
                    ..tool("other", "Other")
                },
            },
        );
        m.apply_event(
            2,
            &Event::ToolCallStarted {
                tool_call: tool("target", "Terminal"),
            },
        );
        let deltas = m.apply_event(
            3,
            &Event::ToolCallUpdated {
                tool_call_id: "target".into(),
                title: None,
                args_preview: Some(r#"{"command":"git log"}"#.into()),
                started_at: None,
                diffs: None,
            },
        );
        assert!(
            matches!(deltas.as_slice(), [TranscriptDelta::Patch { id, .. }] if id == "start-target")
        );
        // Non-matching row untouched.
        let other = row_by_id(&m, "start-other").unwrap();
        assert_eq!(other.tool.as_ref().unwrap().args_preview, r#"{"k":1}"#);
        // Matching row patched; title None leaves the name intact.
        let target = row_by_id(&m, "start-target").unwrap();
        assert_eq!(target.tool.as_ref().unwrap().name, "Terminal");
        assert_eq!(
            target.tool.as_ref().unwrap().args_preview,
            r#"{"command":"git log"}"#
        );
    }

    #[test]
    fn tool_update_diffs_replace_wholesale_but_none_preserves() {
        // #1721: Some(non-empty) replaces the diff list; None leaves it intact.
        let diff = |path: &str| DiffPreview {
            path: path.into(),
            old_text: None,
            new_text: Some("x".into()),
            created_at: at(100),
        };
        let mut m = TranscriptModel::new();
        m.apply_event(
            1,
            &Event::ToolCallStarted {
                tool_call: ToolCall {
                    kind: "edit".into(),
                    diffs: vec![diff("a.rs")],
                    ..tool("t-1", "Edit")
                },
            },
        );
        // Text-only update must not erase the initial diff.
        m.apply_event(
            2,
            &Event::ToolCallUpdated {
                tool_call_id: "t-1".into(),
                title: Some("Edit a.rs".into()),
                args_preview: None,
                started_at: None,
                diffs: None,
            },
        );
        assert_eq!(
            row_by_id(&m, "start-t-1")
                .unwrap()
                .tool
                .as_ref()
                .unwrap()
                .diffs,
            vec![diff("a.rs")]
        );
        // Some replaces wholesale.
        m.apply_event(
            3,
            &Event::ToolCallUpdated {
                tool_call_id: "t-1".into(),
                title: None,
                args_preview: None,
                started_at: None,
                diffs: Some(vec![diff("b.rs")]),
            },
        );
        let diffs = &row_by_id(&m, "start-t-1")
            .unwrap()
            .tool
            .as_ref()
            .unwrap()
            .diffs;
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].path, "b.rs");
    }

    #[test]
    fn completion_row_id_is_seq_disambiguated_on_reuse() {
        // Some adapters reuse a tool_call_id; the first completion keeps the
        // historical id, later ones are disambiguated by seq so ids stay unique.
        let m = fold(vec![
            Event::ToolCallStarted {
                tool_call: tool("t", "Bash"),
            },
            completed("t", false, "first"),
            completed("t", false, "second"),
        ]);
        assert!(row_by_id(&m, "done-t").is_some());
        assert!(
            row_by_id(&m, "done-t-3").is_some(),
            "second completion disambiguated by seq"
        );
        assert_eq!(
            m.rows()
                .iter()
                .filter(|r| r.kind == TranscriptRowKind::ToolComplete)
                .count(),
            2
        );
    }

    #[test]
    fn ask_user_question_tool_card_suppressed_when_tool_started_first() {
        let mut m = TranscriptModel::new();
        // Tool call arrives first -> a transcript row appears.
        m.apply_event(
            1,
            &Event::ToolCallStarted {
                tool_call: tool("tc-ask", "Asking"),
            },
        );
        assert!(m
            .rows()
            .iter()
            .any(|r| r.tool_call_id.as_deref() == Some("tc-ask")));
        // The matching elicitation strips the row (Remove delta) and remembers
        // the id.
        let deltas = m.apply_event(
            2,
            &Event::ElicitationRequested {
                elicitation: elicitation("e-1", Some("tc-ask")),
            },
        );
        assert!(matches!(deltas.as_slice(), [TranscriptDelta::Remove(id)] if id == "start-tc-ask"));
        assert!(!m
            .rows()
            .iter()
            .any(|r| r.tool_call_id.as_deref() == Some("tc-ask")));
        // A later completion for the same id produces no card.
        let after = m.apply_event(3, &completed("tc-ask", false, "x"));
        assert!(after.is_empty());
        assert!(!m
            .rows()
            .iter()
            .any(|r| r.tool_call_id.as_deref() == Some("tc-ask")));
    }

    #[test]
    fn ask_user_question_tool_card_suppressed_when_elicitation_arrived_first() {
        let mut m = TranscriptModel::new();
        m.apply_event(
            1,
            &Event::ElicitationRequested {
                elicitation: elicitation("e-1", Some("tc-ask")),
            },
        );
        let deltas = m.apply_event(
            2,
            &Event::ToolCallStarted {
                tool_call: tool("tc-ask", "Asking"),
            },
        );
        assert!(deltas.is_empty(), "the redundant start frame is dropped");
        assert!(!m
            .rows()
            .iter()
            .any(|r| r.tool_call_id.as_deref() == Some("tc-ask")));
    }

    #[test]
    fn sweep_closes_open_tools_on_turn_end() {
        // #1646: a tool left open when the turn ends becomes tool_stopped, with
        // any buffered streamed output drained into it.
        let m = fold(vec![
            prompt("run"),
            Event::ToolCallStarted {
                tool_call: tool("open-1", "LongTask"),
            },
            Event::ToolCallContent {
                tool_call_id: "open-1".into(),
                content: "partial out".into(),
            },
            Event::Stopped {
                reason: "prompt_complete".into(),
            },
        ]);
        let stopped = m
            .rows()
            .iter()
            .find(|r| r.kind == TranscriptRowKind::ToolStopped)
            .expect("stopped row");
        assert_eq!(stopped.tool_call_id.as_deref(), Some("open-1"));
        assert_eq!(stopped.text, "partial out");
        assert_eq!(stopped.id, "stopped-open-1-4");
        assert!(!m.tool_outputs.contains_key("open-1"), "buffer drained");
    }

    #[test]
    fn sweep_leaves_completed_tools_and_does_not_double_close() {
        // A completed tool is untouched; a second Stopped does not re-close.
        let mut m = fold(vec![
            Event::ToolCallStarted {
                tool_call: tool("a", "A"),
            },
            Event::ToolCallStarted {
                tool_call: tool("b", "B"),
            },
            completed("a", false, ""),
            Event::Stopped {
                reason: "user_stopped".into(),
            },
        ]);
        let stopped: Vec<&str> = m
            .rows()
            .iter()
            .filter(|r| r.kind == TranscriptRowKind::ToolStopped)
            .map(|r| r.tool_call_id.as_deref().unwrap())
            .collect();
        assert_eq!(stopped, vec!["b"], "only the still-open tool is swept");
        // Second Stopped adds no further tool_stopped rows.
        m.apply_event(
            10,
            &Event::Stopped {
                reason: "prompt_complete".into(),
            },
        );
        assert_eq!(
            m.rows()
                .iter()
                .filter(|r| r.kind == TranscriptRowKind::ToolStopped)
                .count(),
            1
        );
    }

    #[test]
    fn every_turn_ending_arm_sweeps_open_tools() {
        // The sweep is reason-independent: Stopped, the two startup-error arms,
        // and AgentSwitched all close a dangling tool.
        let closers = [
            Event::Stopped {
                reason: "cancelled".into(),
            },
            Event::AgentStartupError {
                message: "boom".into(),
            },
            Event::IncompatibleAgent {
                detail: crate::acp::state::StartupErrorDetail::UnsupportedProtocolVersion {
                    expected: "1".into(),
                    received: "2".into(),
                },
            },
            Event::AgentSwitched {
                from: "claude".into(),
                to: "codex".into(),
                reason: "rate_limit".into(),
            },
        ];
        for closer in closers {
            let label = format!("{closer:?}");
            let m = fold(vec![
                Event::ToolCallStarted {
                    tool_call: tool("t1", "T"),
                },
                closer,
            ]);
            assert_eq!(
                m.rows()
                    .iter()
                    .filter(|r| r.kind == TranscriptRowKind::ToolStopped)
                    .count(),
                1,
                "{label}"
            );
        }
    }

    #[test]
    fn empty_output_notice_gated_on_turn_output() {
        // Table: events after the opening prompt -> whether an empty_output row
        // is appended on the trailing Stopped.
        struct Case {
            mid: Vec<Event>,
            expect_notice: bool,
        }
        let cases = [
            // Turn opened, produced nothing -> notice.
            Case {
                mid: vec![],
                expect_notice: true,
            },
            // Produced a chunk -> no notice.
            Case {
                mid: vec![Event::AgentMessageChunk { text: "hi".into() }],
                expect_notice: false,
            },
            // ThinkingStarted counts as output -> no notice.
            Case {
                mid: vec![Event::ThinkingStarted],
                expect_notice: false,
            },
            // A surfaced runtime error suppresses the generic notice.
            Case {
                mid: vec![Event::PromptRuntimeError {
                    message: "boom".into(),
                }],
                expect_notice: false,
            },
        ];
        for (i, c) in cases.into_iter().enumerate() {
            let mut evs = vec![prompt("/usage")];
            evs.extend(c.mid);
            evs.push(Event::Stopped {
                reason: "prompt_complete".into(),
            });
            let m = fold(evs);
            let has_notice = m
                .rows()
                .iter()
                .any(|r| r.kind == TranscriptRowKind::EmptyOutput);
            assert_eq!(has_notice, c.expect_notice, "case {i}");
        }
    }

    #[test]
    fn steered_continuation_keeps_turn_output_so_no_empty_notice() {
        // #2805: a steered mid-turn prompt is not a fresh turn, so it must not
        // reset the output flag the running turn already earned.
        let m = fold(vec![
            Event::PromptCapabilities {
                image: false,
                audio: false,
                embedded_context: false,
                steering: true,
            },
            prompt("read src/acp"),
            Event::AgentMessageChunk {
                text: "on it".into(),
            },
            prompt("actually just acp_client.rs"),
            Event::Stopped {
                reason: "prompt_complete".into(),
            },
        ]);
        assert!(!m
            .rows()
            .iter()
            .any(|r| r.kind == TranscriptRowKind::EmptyOutput));
        // Both prompts still appended their authoritative rows.
        assert_eq!(
            m.rows()
                .iter()
                .filter(|r| r.kind == TranscriptRowKind::UserPrompt)
                .count(),
            2
        );
    }

    #[test]
    fn divider_rows_render_per_event() {
        // Table: event -> (expected row id, expected kind). One divider per
        // event; text is asserted separately where load-bearing.
        let cases: Vec<(Event, &str, TranscriptRowKind)> = vec![
            (
                Event::SessionCleared,
                "cleared-1",
                TranscriptRowKind::SessionCleared,
            ),
            (
                Event::ConversationCompacted,
                "compacted-1",
                TranscriptRowKind::Compacted,
            ),
            (
                Event::ConversationSummary {
                    text: "recap".into(),
                    summarized_until_seq: 0,
                },
                "summary-1",
                TranscriptRowKind::Summary,
            ),
            (
                Event::AgentSwitched {
                    from: "claude".into(),
                    to: "codex".into(),
                    reason: "rate_limit".into(),
                },
                "agent-switched-1",
                TranscriptRowKind::SessionCleared,
            ),
        ];
        for (ev, id, kind) in cases {
            let label = format!("{ev:?}");
            let m = fold(vec![ev]);
            let row = row_by_id(&m, id).unwrap_or_else(|| panic!("{label}: row {id}"));
            assert_eq!(row.kind, kind, "{label}");
        }
        // The summary row carries the generated text verbatim.
        let m = fold(vec![Event::ConversationSummary {
            text: "did the thing".into(),
            summarized_until_seq: 0,
        }]);
        assert_eq!(row_by_id(&m, "summary-1").unwrap().text, "did the thing");
    }

    #[test]
    fn context_reset_divider_suppressed_without_a_prior_prompt() {
        // A 0-prompt session's session/load failure is expected, not an
        // incident; no divider. With a prior prompt, the reason is rendered.
        let none = fold(vec![Event::SessionContextReset {
            reason: "load failed".into(),
        }]);
        assert!(none.rows().is_empty());

        let with_prompt = fold(vec![
            prompt("hi"),
            Event::SessionContextReset {
                reason: "session/load failed: bad id".into(),
            },
        ]);
        let row = with_prompt.rows().last().unwrap();
        assert_eq!(row.kind, TranscriptRowKind::ContextReset);
        assert!(row.text.contains("session/load failed"));

        // An empty reason falls back to the canned message.
        let fallback = fold(vec![
            prompt("hi"),
            Event::SessionContextReset { reason: "".into() },
        ]);
        assert!(fallback
            .rows()
            .last()
            .unwrap()
            .text
            .contains("context reset"));
    }

    #[test]
    fn elicitation_answer_row_appends_dedupes_and_skips_empty() {
        let accept = |answers: Vec<ElicitationAnswer>| Event::ElicitationResolved {
            nonce: Nonce("el-1".into()),
            outcome: ElicitationOutcome::Accepted,
            answers,
        };
        // Accepted with answers -> a keyed row.
        let mut m = TranscriptModel::new();
        m.apply_event(
            1,
            &accept(vec![ElicitationAnswer {
                question: "Proceed?".into(),
                answer: "Yes".into(),
            }]),
        );
        let row = row_by_id(&m, "elicitation-el-1").expect("answer row");
        assert_eq!(row.kind, TranscriptRowKind::ElicitationAnswered);
        assert_eq!(row.elicitation_answers.len(), 1);
        assert_eq!(row.text, "Proceed?: Yes");
        // A re-broadcast is deduped by id.
        let dup = m.apply_event(
            2,
            &accept(vec![ElicitationAnswer {
                question: "Proceed?".into(),
                answer: "Yes".into(),
            }]),
        );
        assert!(dup.is_empty());
        assert_eq!(
            m.rows()
                .iter()
                .filter(|r| r.kind == TranscriptRowKind::ElicitationAnswered)
                .count(),
            1
        );

        // Empty answers (skip / cancel / teardown) add no row.
        let skipped = fold(vec![Event::ElicitationResolved {
            nonce: Nonce("el-2".into()),
            outcome: ElicitationOutcome::Declined,
            answers: Vec::new(),
        }]);
        assert!(skipped.rows().is_empty());
    }

    #[test]
    fn tool_call_content_buffers_without_a_row() {
        let deltas = {
            let mut m = TranscriptModel::new();
            m.apply_event(
                1,
                &Event::ToolCallStarted {
                    tool_call: tool("t", "Bash"),
                },
            );
            m.apply_event(
                2,
                &Event::ToolCallContent {
                    tool_call_id: "t".into(),
                    content: "streaming".into(),
                },
            )
        };
        assert!(deltas.is_empty(), "buffering emits no delta");
    }

    #[test]
    fn control_only_events_produce_no_rows() {
        // A representative slice of the control-state events AcpState owns must
        // add nothing to the transcript.
        let m = fold(vec![
            Event::PlanUpdated {
                plan: crate::acp::state::Plan {
                    plan_id: "p".into(),
                    version: 1,
                    steps: Vec::new(),
                },
            },
            Event::ThinkingEnded,
            Event::RawAgentUpdate {
                payload: serde_json::json!({"x": 1}),
            },
            Event::TodoListUpdated { todos: Vec::new() },
            Event::ConversationCompactionStarted,
        ]);
        assert!(m.rows().is_empty());
        assert_eq!(m.last_seq(), 5);
    }
}
