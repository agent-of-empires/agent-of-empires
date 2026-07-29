//! Pure reducer over `AcpBroadcastFrame` → `AcpTranscript`.
//!
//! Mirrors the semantics of `web/src/hooks/useAcp.ts` but in
//! Rust + with the TUI's flat-row data shape. The server-side
//! `AcpState` in `src/acp/state.rs` is intentionally NOT a UI
//! reducer (it drops `AgentMessageChunk` text, for one), so the TUI
//! structured view owns its own activity accumulator.
//!
//! Design choices for the TUI MVP:
//!
//! - Rich tool-card breakdowns (per-kind layout, diff previews, file
//!   trees) are deferred to followup issues. Tool calls render as
//!   structured one-liner cards here; users can press `o` from the
//!   transcript pane to open the web view for full-fidelity inspection.
//! - `AvailableCommandsUpdated` is retained on the transcript even
//!   though the MVP composer doesn't surface a slash-command picker;
//!   the followup that adds slash autocomplete (#1018 followup) needs
//!   this list in place.
//! - `SessionContextReset` flips `context_primer_pending` so the view
//!   layer can offer the "paste a context primer" affordance.

use crate::acp::approvals::ApprovalDecision;
use crate::acp::elicitations::{ElicitationAnswer, ElicitationOutcome, ElicitationQuestion};
use crate::acp::protocol::AcpBroadcastFrame;
use crate::acp::state::{
    AvailableCommand, DiffPreview, Event, ModeInfo, PlanStepStatus, SessionUsage, ToolOutputBlock,
};

/// Render the structured completion payload as a single text block for the
/// native TUI, which can't display images/audio inline. Media variants
/// become a `[kind mime]` placeholder; text + text-resources show their
/// text; links/blobs show their uri. See #1818.
fn summarize_output_blocks(blocks: &[ToolOutputBlock]) -> String {
    blocks
        .iter()
        .map(|block| match block {
            ToolOutputBlock::Text { text } => text.clone(),
            ToolOutputBlock::Image { mime_type, .. } => format!("[image {mime_type}]"),
            ToolOutputBlock::Audio { mime_type, .. } => format!("[audio {mime_type}]"),
            ToolOutputBlock::ResourceLink { name, uri, .. } => format!("[link {name}: {uri}]"),
            ToolOutputBlock::Resource {
                uri,
                text: Some(text),
                ..
            } => format!("{text}\n[resource {uri}]"),
            ToolOutputBlock::Resource {
                uri, text: None, ..
            } => format!("[resource {uri}]"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone)]
pub struct AcpTranscript {
    pub session_id: String,
    /// Friendly session title hydrated from `/api/sessions`.
    pub session_title: Option<String>,
    /// Resolved ACP registry key shown in the header. Updated when the backend
    /// switches mid-session.
    pub agent_name: Option<String>,
    pub rows: Vec<ActivityRow>,
    pub pending_approvals: Vec<PendingApproval>,
    /// Pending `AskUserQuestion` elicitations. The native TUI does not
    /// render the answer form (that is web-only); it surfaces a notice and
    /// lets the user skip/cancel so the agent's turn never hangs. See the
    /// `ElicitationRequested` arm.
    pub pending_elicitations: Vec<PendingElicitation>,
    /// Live status banner (e.g. "thinking…", "ended: completed").
    pub status_text: Option<String>,
    /// Latest mode id the agent reported. `None` until the agent
    /// emits `ModesAvailable` / `CurrentModeChanged`.
    pub current_mode: Option<String>,
    /// Permission modes the agent advertised (`ModesAvailable`). Drives
    /// the `m` mode picker; empty when the agent never announced any.
    pub available_modes: Vec<ModeInfo>,
    /// Slash commands the agent has advertised. Drives the composer's
    /// `/` picker (followup #1018).
    pub available_commands: Vec<AvailableCommand>,
    /// Set after a `SessionContextReset`; the view layer drops a
    /// "context lost, re-prime?" banner until the user dismisses it
    /// or sends the next prompt.
    pub context_primer_pending: bool,
    /// Whether the agent is mid-turn, derived purely from daemon events:
    /// true on `UserPromptSent` / `ThinkingStarted`, false on `Stopped`
    /// / `AgentStartupError` / `PromptRejected`. Server truth (mirrors
    /// the web reducer's `turnActive`), so it lives here and is rebuilt
    /// by `/replay` after a `reset()`. The composer reads it to decide
    /// whether Enter sends now or parks the prompt in the local queue.
    pub turn_active: bool,
    /// Latest context-window usage / cost snapshot the agent reported.
    /// Rendered as a token meter in the status line, mirroring the web
    /// composer's usage chip.
    pub usage: Option<SessionUsage>,
    /// Latest plan snapshot. Kept separate from the append-only transcript so
    /// repeated progress updates render as one sticky summary instead of a
    /// growing stack of near-identical checklists.
    pub current_plan: Vec<PlanLine>,
    /// Set when the WS layer reports `{"kind":"lagged"}`; the view
    /// layer should clear and rehydrate via HTTP /replay.
    pub lagged: bool,
    /// Highest seq the reducer has consumed. Used as the `since`
    /// cursor for reconnect.
    pub last_seq: u64,
    /// Index into `rows` of the currently-growing `AgentMessage` row
    /// (so consecutive `AgentMessageChunk` events append in-place
    /// instead of fragmenting one assistant turn across many rows).
    /// Cleared on any non-chunk event.
    pending_message_idx: Option<usize>,
    /// Map of tool_call_id -> row index in `rows`. Lets
    /// `ToolCallCompleted` and `ToolCallUpdated` locate the row to
    /// mutate without scanning the entire activity feed.
    tool_idx: std::collections::HashMap<String, usize>,
    /// Map of approval nonce -> row index in `rows`. Same idea for
    /// `ApprovalResolved`.
    approval_idx: std::collections::HashMap<String, usize>,
}

#[derive(Debug, Clone)]
pub enum ActivityRow {
    UserPrompt(String),
    AgentMessage(String),
    ToolCall(ToolCallRow),
    Approval(ApprovalRow),
    /// The user's answers to an AskUserQuestion / elicitation form, kept
    /// in the transcript so the picked answer survives the card closing.
    /// See #2209.
    ElicitationAnswer(Vec<ElicitationAnswer>),
    Note {
        kind: NoteKind,
        text: String,
    },
}

#[derive(Debug, Clone)]
pub struct ToolCallRow {
    pub name: String,
    /// ACP `ToolKind` lowercased (`read` / `edit` / `delete` / `execute`
    /// / …), forwarded from `ToolCall::kind`. Drives the per-kind
    /// renderer in `render_tool_lines`; empty string falls back to the
    /// generic one-liner. `ToolCallUpdated` does not carry kind, so the
    /// value set at `ToolCallStarted` is authoritative for the row.
    pub kind: String,
    pub args: String,
    /// Structured per-file diffs the agent attached to the call (edit /
    /// apply_patch tools). When non-empty the renderer prefers these over
    /// the compact diff derived from `old_string`/`new_string` args.
    pub diffs: Vec<DiffPreview>,
    pub completed: Option<ToolCompletion>,
}

#[derive(Debug, Clone)]
pub struct ToolCompletion {
    pub ok: bool,
    /// Empty string when the agent didn't ship a content body; the
    /// view layer falls back to a status word in that case.
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct ApprovalRow {
    pub nonce: String,
    pub title: String,
    pub kind: String,
    pub args: String,
    pub destructive: bool,
    pub decision: Option<ApprovalDecision>,
}

#[derive(Debug, Clone)]
pub struct PlanLine {
    pub title: String,
    pub status: PlanStepStatus,
}

#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub nonce: String,
}

#[derive(Debug, Clone)]
pub struct PendingElicitation {
    pub nonce: String,
    /// Human-readable prompt (the question text, or a lead-in for a
    /// multi-question form).
    pub message: String,
    /// The form's fields, kept so the TUI can answer single-select
    /// questions natively via the `a` picker.
    pub questions: Vec<ElicitationQuestion>,
}

#[derive(Debug, Clone, Copy)]
pub enum NoteKind {
    Info,
    Warning,
    Error,
}

impl AcpTranscript {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            session_title: None,
            agent_name: None,
            rows: Vec::new(),
            pending_approvals: Vec::new(),
            pending_elicitations: Vec::new(),
            status_text: None,
            current_mode: None,
            available_modes: Vec::new(),
            available_commands: Vec::new(),
            context_primer_pending: false,
            turn_active: false,
            usage: None,
            current_plan: Vec::new(),
            lagged: false,
            last_seq: 0,
            pending_message_idx: None,
            tool_idx: std::collections::HashMap::new(),
            approval_idx: std::collections::HashMap::new(),
        }
    }

    /// Drop all accumulated state and start over. Used when the
    /// daemon signals `lagged` on the WebSocket and we need to
    /// rehydrate via HTTP /replay.
    pub fn reset(&mut self) {
        let session_id = std::mem::take(&mut self.session_id);
        let session_title = self.session_title.take();
        let agent_name = self.agent_name.take();
        *self = Self::new(session_id);
        self.session_title = session_title;
        self.agent_name = agent_name;
    }

    /// Optimistically clear an approval card by nonce after the resolve
    /// POST succeeded (204) or the daemon reported the nonce already gone
    /// (404), instead of waiting on the `ApprovalResolved` broadcast, which
    /// the seq dedupe can swallow and leave the card stuck. Mirrors the
    /// `ApprovalResolved` event arm. See #1821.
    pub fn resolve_approval_locally(&mut self, nonce: &str, decision: ApprovalDecision) {
        if let Some(&idx) = self.approval_idx.get(nonce) {
            if let Some(ActivityRow::Approval(row)) = self.rows.get_mut(idx) {
                row.decision = Some(decision);
            }
        }
        self.pending_approvals.retain(|p| p.nonce != nonce);
    }

    /// Optimistically clear a pending elicitation after the skip/cancel
    /// POST succeeded (or 404'd), mirroring `resolve_approval_locally`.
    pub fn resolve_elicitation_locally(&mut self, nonce: &str) {
        self.pending_elicitations.retain(|p| p.nonce != nonce);
    }

    /// Mark `lagged = true`. The view layer is responsible for
    /// noticing this and triggering a /replay refetch.
    pub fn set_lagged(&mut self) {
        self.lagged = true;
        self.rows.push(ActivityRow::Note {
            kind: NoteKind::Warning,
            text: "broadcast lagged; refetching transcript…".to_string(),
        });
    }

    /// Apply one broadcast frame.
    pub fn apply(&mut self, frame: &AcpBroadcastFrame) {
        if frame.seq <= self.last_seq && self.last_seq > 0 {
            // Already consumed; dedupe against the replay-vs-live
            // overlap. The web reducer does the same. Log at debug
            // so an unexpected drop (e.g. true reordering) leaves a
            // trail without spamming on every normal overlap.
            tracing::debug!(
                target: "acp.tui.reducer",
                session = %self.session_id,
                seq = frame.seq,
                last_seq = self.last_seq,
                "dropped duplicate or out-of-order frame"
            );
            return;
        }
        self.last_seq = frame.seq;
        self.apply_event(&frame.event);
    }

    fn apply_event(&mut self, event: &Event) {
        match event {
            // The daemon applies this legacy suggestion to Instance.title.
            // The authoritative current title is hydrated from /api/sessions.
            Event::SessionTitleSuggested { .. } => {}
            Event::AgentMessageChunk { text } => {
                if let Some(idx) = self.pending_message_idx {
                    if let Some(ActivityRow::AgentMessage(buf)) = self.rows.get_mut(idx) {
                        buf.push_str(text);
                        return;
                    }
                }
                self.rows.push(ActivityRow::AgentMessage(text.clone()));
                self.pending_message_idx = Some(self.rows.len() - 1);
            }
            Event::UserPromptSent { text, attachments } => {
                self.flush_pending_chunk();
                // The TUI structured view renders text only; note the
                // attachment count inline so a prompt sent from the web
                // composer with images doesn't look empty here.
                let row = if attachments.is_empty() {
                    text.clone()
                } else {
                    format!("{text} [{} attachment(s)]", attachments.len())
                };
                self.rows.push(ActivityRow::UserPrompt(row));
                // Sending a prompt dismisses any context-primer hint.
                self.context_primer_pending = false;
                self.turn_active = true;
            }
            Event::UserDiffCommentsPrompt {
                assembled_markdown, ..
            } => {
                // The TUI has no rich diff-comments card; render the
                // assembled markdown (exactly what the agent received) as
                // a plain user prompt row, same as UserPromptSent.
                self.flush_pending_chunk();
                self.rows
                    .push(ActivityRow::UserPrompt(assembled_markdown.clone()));
                self.context_primer_pending = false;
                self.turn_active = true;
            }
            Event::ThinkingStarted => {
                self.flush_pending_chunk();
                self.status_text = Some("thinking…".to_string());
                self.turn_active = true;
            }
            Event::ThinkingEnded => {
                self.flush_pending_chunk();
                if self.status_text.as_deref() == Some("thinking…") {
                    self.status_text = None;
                }
            }
            Event::ToolCallStarted { tool_call } => {
                self.flush_pending_chunk();
                let row = ToolCallRow {
                    name: tool_call.name.clone(),
                    kind: tool_call.kind.clone(),
                    args: tool_call.args_preview.clone(),
                    diffs: tool_call.diffs.clone(),
                    completed: None,
                };
                self.rows.push(ActivityRow::ToolCall(row));
                self.tool_idx
                    .insert(tool_call.id.clone(), self.rows.len() - 1);
            }
            Event::ToolCallUpdated {
                tool_call_id,
                title,
                args_preview,
                diffs,
                ..
            } => {
                if let Some(&idx) = self.tool_idx.get(tool_call_id) {
                    if let Some(ActivityRow::ToolCall(row)) = self.rows.get_mut(idx) {
                        if let Some(t) = title {
                            if !t.is_empty() {
                                row.name = t.clone();
                            }
                        }
                        if let Some(a) = args_preview {
                            if !a.is_empty() {
                                row.args = a.clone();
                            }
                        }
                        // `Some` replaces the diff list wholesale (per ACP,
                        // content is a replacement); `None` leaves the
                        // initial frame's diffs untouched. See #1721.
                        if let Some(d) = diffs {
                            row.diffs = d.clone();
                        }
                    }
                }
            }
            Event::ToolCallContent {
                tool_call_id,
                content,
            } => {
                // Streaming output: latest snapshot wins. Stash it on
                // the in-flight row as completion content so the user
                // sees progress even before the call completes.
                if let Some(&idx) = self.tool_idx.get(tool_call_id) {
                    if let Some(ActivityRow::ToolCall(row)) = self.rows.get_mut(idx) {
                        match row.completed.as_mut() {
                            Some(c) => c.content = content.clone(),
                            None => {
                                row.completed = Some(ToolCompletion {
                                    ok: true, // optimistic until ToolCallCompleted lands
                                    content: content.clone(),
                                });
                            }
                        }
                    }
                }
            }
            Event::ToolCallCompleted {
                tool_call_id,
                is_error,
                content,
                output,
                async_subagent,
                ..
            } => {
                self.flush_pending_chunk();
                if let Some(&idx) = self.tool_idx.get(tool_call_id) {
                    if let Some(ActivityRow::ToolCall(row)) = self.rows.get_mut(idx) {
                        // An async sub-agent launch completes immediately but
                        // the work runs off-protocol. Suppress the SDK marker
                        // body (it carries an internal agent id we must not
                        // surface) and label the row as a background dispatch
                        // instead of a finished tool call.
                        let text = if *async_subagent {
                            "runs in background".to_string()
                        } else if output.is_empty() {
                            content.clone()
                        } else {
                            summarize_output_blocks(output)
                        };
                        row.completed = Some(ToolCompletion {
                            ok: !is_error,
                            content: text,
                        });
                    }
                }
            }
            Event::ApprovalRequested { approval } => {
                self.flush_pending_chunk();
                let nonce = approval.nonce.0.clone();
                let row = ApprovalRow {
                    nonce: nonce.clone(),
                    title: approval.tool_call.name.clone(),
                    kind: approval.tool_call.kind.clone(),
                    args: approval.tool_call.args_preview.clone(),
                    destructive: approval.destructive,
                    decision: None,
                };
                self.rows.push(ActivityRow::Approval(row));
                let idx = self.rows.len() - 1;
                self.approval_idx.insert(nonce.clone(), idx);
                self.pending_approvals.push(PendingApproval { nonce });
            }
            Event::ApprovalResolved { nonce, decision } => {
                self.flush_pending_chunk();
                if let Some(&idx) = self.approval_idx.get(&nonce.0) {
                    if let Some(ActivityRow::Approval(row)) = self.rows.get_mut(idx) {
                        row.decision = Some(*decision);
                    }
                }
                self.pending_approvals.retain(|p| p.nonce != nonce.0);
            }
            Event::ElicitationRequested { elicitation } => {
                self.flush_pending_chunk();
                // Single-select questions are answerable natively via the
                // `a` picker; anything richer (free text, multi-select,
                // numbers) still points at the web form. The view layer
                // decides which hint applies from `questions`.
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Info,
                    text: format!(
                        "Agent asked a question: {}\nPress a to answer, s to skip, c to cancel (o opens the web form).",
                        elicitation.message
                    ),
                });
                self.pending_elicitations.push(PendingElicitation {
                    nonce: elicitation.nonce.0.clone(),
                    message: elicitation.message.clone(),
                    questions: elicitation.questions.clone(),
                });
            }
            Event::ElicitationResolved {
                nonce,
                outcome,
                answers,
            } => {
                self.flush_pending_chunk();
                // Gate on the card actually being pending: a resolved
                // elicitation can be re-broadcast (cancel-on-teardown racing a
                // POST; the store is lenient on the nonce), and replaying it
                // must not append a second row. The web reducer dedupes by row
                // id; here the pending card is the dedupe key. See #2209.
                let was_pending = self.pending_elicitations.iter().any(|p| p.nonce == nonce.0);
                self.pending_elicitations.retain(|p| p.nonce != nonce.0);
                if !was_pending {
                    return;
                }
                // Record what the user picked so the transcript keeps a
                // trace after the card closes. Skip (Decline) leaves a
                // short note; Cancel / teardown adds nothing. See #2209.
                if !answers.is_empty() {
                    self.rows
                        .push(ActivityRow::ElicitationAnswer(answers.clone()));
                } else if matches!(outcome, ElicitationOutcome::Declined) {
                    self.rows.push(ActivityRow::Note {
                        kind: NoteKind::Info,
                        text: "You skipped the question.".to_string(),
                    });
                }
            }
            Event::PlanUpdated { plan } => {
                self.flush_pending_chunk();
                self.current_plan = plan
                    .steps
                    .iter()
                    .map(|s| PlanLine {
                        title: s.title.clone(),
                        status: s.status.clone(),
                    })
                    .collect();
            }
            Event::TodoListUpdated { todos: _ } => {
                // TUI MVP omits the parallel todo list; agents almost
                // always echo it via Plan anyway. Followup issue.
            }
            Event::Stopped { reason } => {
                self.flush_pending_chunk();
                self.status_text = Some(format!("stopped: {reason}"));
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Info,
                    text: format!("agent stopped: {reason}"),
                });
                self.turn_active = false;
            }
            Event::AgentStartupError { message } => {
                self.flush_pending_chunk();
                self.status_text = Some("startup error".to_string());
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Error,
                    text: format!("agent startup failed: {message}"),
                });
                self.turn_active = false;
            }
            Event::PromptRuntimeError { message } => {
                self.flush_pending_chunk();
                self.status_text = Some("prompt error".to_string());
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Error,
                    text: format!("prompt failed: {message}"),
                });
                self.turn_active = false;
            }
            Event::IncompatibleAgent { .. } => {
                // Structured detail for the web structured view's StartupErrorScreen.
                // The TUI mirrors the textual signal via the parallel
                // AgentStartupError event the connection task emits, so the
                // reducer has nothing to do here.
            }
            Event::SessionContextReset { reason } => {
                self.flush_pending_chunk();
                self.context_primer_pending = true;
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Warning,
                    text: format!("context reset: {reason}"),
                });
            }
            Event::SessionCleared => {
                // /clear wiped the model's memory. Drop session-scoped
                // capability caches the agent no longer recognises and
                // surface a divider so the user sees the boundary. The
                // web UI folds pre-clear rows behind a disclosure; the
                // TUI just keeps them inline below the divider for now.
                // See #1101.
                self.flush_pending_chunk();
                self.available_commands.clear();
                self.current_mode = None;
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Warning,
                    text: "conversation cleared, the model no longer remembers earlier turns"
                        .into(),
                });
            }
            Event::ConversationCompacted => {
                // /compact replaced the model's context with a summary;
                // the model retains continuity, so this is informational
                // rather than a context-reset warning, and the primer
                // banner stays untouched. See #1109.
                self.flush_pending_chunk();
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Info,
                    text: "conversation compacted; earlier turns above are summarised in the model's context"
                        .into(),
                });
            }
            Event::ConversationSummary { text, .. } => {
                // aoe-generated recap of the conversation so far (see #2808).
                // Render it as an informational callout in the transcript;
                // it carries no model/session state to mutate.
                self.flush_pending_chunk();
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Info,
                    text: format!("summary of conversation so far:\n{text}"),
                });
            }
            Event::AcpSessionAssigned { acp_session_id } => {
                // Bookkeeping event; not surfaced to the user.
                let _ = acp_session_id;
            }
            Event::AvailableCommandsUpdated { commands } => {
                self.available_commands = commands.clone();
            }
            Event::ModesAvailable {
                current_mode_id,
                modes,
            } => {
                self.current_mode = Some(current_mode_id.clone());
                self.available_modes = modes.clone();
            }
            Event::CurrentModeChanged { current_mode_id } => {
                self.current_mode = Some(current_mode_id.clone());
            }
            Event::ModeChanged { mode } => {
                // Legacy hard-coded mode enum, always emitted right after a
                // CurrentModeChanged that already carries the real adapter mode
                // id. Only fall back to the coerced enum label when no raw id
                // was seen, so an OpenCode `build`/custom agent (which the enum
                // collapses to `Default`) keeps its real id in the title. See
                // #1827.
                if self.current_mode.is_none() {
                    self.current_mode = Some(format!("{mode:?}"));
                }
            }
            Event::PromptRejected { .. } => {
                // The daemon refused the prompt (e.g. read-only mode); no
                // turn started, so clear the busy flag the optimistic
                // submit path may have set. The richer rejected-prompt
                // renderer is followup work (see the no-op group below).
                self.turn_active = false;
            }
            Event::RateLimitAutoResumed { resets_at } => {
                // Timeline breadcrumb: the reconciler auto-resumed the
                // worker after the rate-limit reset elapsed. Surface it so
                // the transcript explains why the agent came back on its
                // own. See #1722.
                self.flush_pending_chunk();
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Info,
                    text: format!("auto-resumed after rate-limit reset ({resets_at})"),
                });
            }
            Event::UsageUpdated { usage } => {
                // Latest snapshot wins; the agent typically resends after
                // each turn. Rendered as the status-line token meter.
                self.usage = Some(usage.clone());
            }
            Event::ModeSwitchFailed { mode_id, reason } => {
                self.flush_pending_chunk();
                self.rows.push(ActivityRow::Note {
                    kind: NoteKind::Error,
                    text: format!("mode switch to \"{mode_id}\" failed: {reason}"),
                });
            }
            Event::DiffEmitted { .. }
            | Event::RateLimit { .. }
            | Event::RawAgentUpdate { .. }
            // Background async sub-agents surface in the web panel; the
            // native structured view has no panel yet, so these are
            // ambient no-ops here (followup work).
            | Event::BackgroundAgentLaunched { .. }
            | Event::BackgroundAgentProgress { .. }
            | Event::BackgroundAgentCompleted { .. }
            | Event::WakeupScheduled { .. }
            | Event::MonitorArmed { .. }
            | Event::CancelRequested { .. }
            | Event::PromptCapabilities { .. }
            | Event::ConfigOptionsUpdated { .. }
            | Event::ConfigOptionSwitchFailed { .. } => {
                // Surface as info notes for now; richer renderers are
                // followup work tracked in the plan's "out of scope".
            }
            Event::AgentSwitched { to, .. } => {
                self.agent_name = Some(to.clone());
                self.current_plan.clear();
            }
        }
    }

    fn flush_pending_chunk(&mut self) {
        self.pending_message_idx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::approvals::{Approval, Nonce};
    use crate::acp::state::{Plan, PlanStep, PlanStepStatus, SessionMode, ToolCall};
    use chrono::Utc;
    use std::sync::Arc;

    fn frame(seq: u64, event: Event) -> AcpBroadcastFrame {
        AcpBroadcastFrame {
            session_id: "s-1".into(),
            seq,
            event: Arc::new(event),
        }
    }

    fn tool(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            kind: "execute".into(),
            args_preview: "ls".into(),
            started_at: Utc::now(),
            parent_tool_call_id: None,
            memory_recall: None,
            diffs: Vec::new(),
        }
    }

    #[test]
    fn user_prompt_creates_row() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                text: "hi".into(),
                attachments: Vec::new(),
            },
        ));
        assert_eq!(t.rows.len(), 1);
        match &t.rows[0] {
            ActivityRow::UserPrompt(text) => assert_eq!(text, "hi"),
            _ => panic!("expected UserPrompt"),
        }
        assert_eq!(t.last_seq, 1);
    }

    #[test]
    fn chunks_accumulate_into_single_row() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::AgentMessageChunk {
                text: "Hello".into(),
            },
        ));
        t.apply(&frame(
            2,
            Event::AgentMessageChunk {
                text: ", world!".into(),
            },
        ));
        assert_eq!(t.rows.len(), 1);
        match &t.rows[0] {
            ActivityRow::AgentMessage(text) => assert_eq!(text, "Hello, world!"),
            _ => panic!("expected AgentMessage"),
        }
    }

    #[test]
    fn non_chunk_event_breaks_message_grouping() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::AgentMessageChunk {
                text: "First".into(),
            },
        ));
        t.apply(&frame(2, Event::ThinkingStarted));
        t.apply(&frame(
            3,
            Event::AgentMessageChunk {
                text: "Second".into(),
            },
        ));
        // First and Second land in distinct AgentMessage rows.
        let messages: Vec<&str> = t
            .rows
            .iter()
            .filter_map(|r| match r {
                ActivityRow::AgentMessage(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(messages, vec!["First", "Second"]);
    }

    #[test]
    fn current_mode_keeps_real_id_when_legacy_enum_follows() {
        // The acp_client emits [CurrentModeChanged{real id}, ModeChanged{enum}]
        // in that order. An OpenCode `build`/custom agent has no SessionMode
        // variant, so the enum coerces to Default; the raw id must survive.
        // See #1827.
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::CurrentModeChanged {
                current_mode_id: "build".into(),
            },
        ));
        t.apply(&frame(
            2,
            Event::ModeChanged {
                mode: SessionMode::Default,
            },
        ));
        assert_eq!(t.current_mode.as_deref(), Some("build"));
    }

    #[test]
    fn current_mode_falls_back_to_enum_when_no_raw_id() {
        // A bare ModeChanged with no preceding CurrentModeChanged still labels
        // the mode from the legacy enum.
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::ModeChanged {
                mode: SessionMode::Plan,
            },
        ));
        assert_eq!(t.current_mode.as_deref(), Some("Plan"));
    }

    #[test]
    fn tool_call_completion_mutates_existing_row() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::ToolCallStarted {
                tool_call: tool("t-1", "Bash"),
            },
        ));
        t.apply(&frame(
            2,
            Event::ToolCallCompleted {
                tool_call_id: "t-1".into(),
                is_error: false,
                content: "ok".into(),
                output: Vec::new(),
                completed_at: Utc::now(),
                async_subagent: false,
            },
        ));
        assert_eq!(t.rows.len(), 1);
        match &t.rows[0] {
            ActivityRow::ToolCall(row) => {
                let c = row.completed.as_ref().expect("completed");
                assert!(c.ok);
                assert_eq!(c.content, "ok");
            }
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn async_subagent_completion_hides_marker_body() {
        // An async sub-agent launch must not surface the SDK marker (it
        // carries an internal agent id); the row reads as a background
        // dispatch instead.
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::ToolCallStarted {
                tool_call: tool("t-1", "Task"),
            },
        ));
        t.apply(&frame(
            2,
            Event::ToolCallCompleted {
                tool_call_id: "t-1".into(),
                is_error: false,
                content: "Async agent launched successfully\nagentId: secret (internal ID)".into(),
                output: Vec::new(),
                completed_at: Utc::now(),
                async_subagent: true,
            },
        ));
        match &t.rows[0] {
            ActivityRow::ToolCall(row) => {
                let c = row.completed.as_ref().expect("completed");
                assert_eq!(c.content, "runs in background");
                assert!(!c.content.contains("agentId"));
                assert!(!c.content.contains("Async agent launched"));
            }
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn tool_call_started_carries_kind_to_row() {
        let mut t = AcpTranscript::new("s-1");
        let mut tc = tool("t-1", "Edit");
        tc.kind = "edit".into();
        tc.args_preview = r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#.into();
        t.apply(&frame(1, Event::ToolCallStarted { tool_call: tc }));
        match &t.rows[0] {
            ActivityRow::ToolCall(row) => {
                assert_eq!(row.kind, "edit");
                assert!(row.args.contains("old_string"));
            }
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn approval_request_and_resolution() {
        let mut t = AcpTranscript::new("s-1");
        let approval = Approval {
            nonce: Nonce("nonce-1".into()),
            tool_call: tool("t-1", "Bash"),
            destructive: true,
            requested_at: Utc::now(),
            resolved: None,
        };
        t.apply(&frame(1, Event::ApprovalRequested { approval }));
        assert_eq!(t.pending_approvals.len(), 1);
        assert_eq!(t.pending_approvals[0].nonce, "nonce-1");
        t.apply(&frame(
            2,
            Event::ApprovalResolved {
                nonce: Nonce("nonce-1".into()),
                decision: ApprovalDecision::Allow,
            },
        ));
        assert!(t.pending_approvals.is_empty());
        match &t.rows[0] {
            ActivityRow::Approval(row) => {
                assert_eq!(row.decision, Some(ApprovalDecision::Allow));
            }
            _ => panic!("expected Approval"),
        }
    }

    #[test]
    fn elicitation_request_notices_and_resolution_clears() {
        use crate::acp::elicitations::{Elicitation, ElicitationOutcome};
        let mut t = AcpTranscript::new("s-1");
        let elicitation = Elicitation {
            nonce: Nonce("e-1".into()),
            message: "Pick one".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: Utc::now(),
            resolved: None,
        };
        t.apply(&frame(1, Event::ElicitationRequested { elicitation }));
        assert_eq!(t.pending_elicitations.len(), 1);
        assert_eq!(t.pending_elicitations[0].nonce, "e-1");
        // The TUI surfaces a notice row with the answer/skip/cancel keys.
        assert!(matches!(
            t.rows.last(),
            Some(ActivityRow::Note { text, .. }) if text.contains("Press a to answer")
        ));
        t.apply(&frame(
            2,
            Event::ElicitationResolved {
                nonce: Nonce("e-1".into()),
                outcome: ElicitationOutcome::Declined,
                answers: Vec::new(),
            },
        ));
        assert!(t.pending_elicitations.is_empty());
        // A skip leaves a short note so the transcript records the choice.
        assert!(matches!(
            t.rows.last(),
            Some(ActivityRow::Note { text, .. }) if text.contains("skipped")
        ));
    }

    #[test]
    fn elicitation_accepted_records_answer_row() {
        use crate::acp::elicitations::{Elicitation, ElicitationAnswer, ElicitationOutcome};
        let mut t = AcpTranscript::new("s-1");
        let elicitation = Elicitation {
            nonce: Nonce("e-1".into()),
            message: "Pick one".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: Utc::now(),
            resolved: None,
        };
        t.apply(&frame(1, Event::ElicitationRequested { elicitation }));
        t.apply(&frame(
            2,
            Event::ElicitationResolved {
                nonce: Nonce("e-1".into()),
                outcome: ElicitationOutcome::Accepted,
                answers: vec![ElicitationAnswer {
                    question: "Proceed?".into(),
                    answer: "Yes".into(),
                }],
            },
        ));
        assert!(t.pending_elicitations.is_empty());
        // #2209: the picked answer must survive the card closing.
        match t.rows.last() {
            Some(ActivityRow::ElicitationAnswer(answers)) => {
                assert_eq!(answers.len(), 1);
                assert_eq!(answers[0].question, "Proceed?");
                assert_eq!(answers[0].answer, "Yes");
            }
            other => panic!("expected ElicitationAnswer row, got {other:?}"),
        }
    }

    #[test]
    fn resolve_approval_locally_clears_card_without_broadcast() {
        // #1821: the optimistic clear must remove the pending approval and
        // stamp the row decision without an ApprovalResolved frame, since
        // the broadcast can be swallowed by seq dedupe.
        let mut t = AcpTranscript::new("s-1");
        let approval = Approval {
            nonce: Nonce("approval-correlation-id".into()),
            tool_call: tool("t-1", "Bash"),
            destructive: true,
            requested_at: Utc::now(),
            resolved: None,
        };
        // Read the correlation id back from the request rather than passing a
        // literal, mirroring how the runtime resolves a card it actually
        // holds (and keeping the value out of any crypto-nonce heuristic).
        let nonce = approval.nonce.0.to_string();
        t.apply(&frame(1, Event::ApprovalRequested { approval }));
        assert_eq!(t.pending_approvals.len(), 1);

        t.resolve_approval_locally(&nonce, ApprovalDecision::Deny);
        assert!(t.pending_approvals.is_empty());
        match &t.rows[0] {
            ActivityRow::Approval(row) => {
                assert_eq!(row.decision, Some(ApprovalDecision::Deny));
            }
            _ => panic!("expected Approval"),
        }

        // A late ApprovalResolved for the same nonce is a harmless no-op.
        t.apply(&frame(
            2,
            Event::ApprovalResolved {
                nonce: Nonce(nonce.as_str().into()),
                decision: ApprovalDecision::Deny,
            },
        ));
        assert!(t.pending_approvals.is_empty());
    }

    #[test]
    fn duplicate_seq_is_ignored() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                text: "hi".into(),
                attachments: Vec::new(),
            },
        ));
        // Replay-vs-live overlap can deliver the same seq twice; the
        // reducer must dedupe.
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                text: "ignored".into(),
                attachments: Vec::new(),
            },
        ));
        assert_eq!(t.rows.len(), 1);
    }

    #[test]
    fn session_context_reset_sets_pending_primer_flag() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::SessionContextReset {
                reason: "session/load failed".into(),
            },
        ));
        assert!(t.context_primer_pending);
        // Sending a prompt clears the hint.
        t.apply(&frame(
            2,
            Event::UserPromptSent {
                text: "go".into(),
                attachments: Vec::new(),
            },
        ));
        assert!(!t.context_primer_pending);
    }

    #[test]
    fn available_commands_stored_for_future_slash_picker() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::AvailableCommandsUpdated {
                commands: vec![AvailableCommand {
                    name: "test".into(),
                    description: "run tests".into(),
                    accepts_input: false,
                }],
            },
        ));
        assert_eq!(t.available_commands.len(), 1);
        assert_eq!(t.available_commands[0].name, "test");
    }

    #[test]
    fn plan_update_replaces_sticky_plan_snapshot() {
        let mut t = AcpTranscript::new("s-1");
        let plan = Plan {
            plan_id: "p-1".into(),
            version: 1,
            steps: vec![PlanStep {
                id: "s-1".into(),
                title: "Step one".into(),
                detail: None,
                status: PlanStepStatus::Pending,
            }],
        };
        t.apply(&frame(1, Event::PlanUpdated { plan }));
        assert!(t.rows.is_empty());
        assert_eq!(t.current_plan.len(), 1);
        assert_eq!(t.current_plan[0].title, "Step one");
    }

    #[test]
    fn set_lagged_records_a_warning() {
        let mut t = AcpTranscript::new("s-1");
        t.set_lagged();
        assert!(t.lagged);
        assert_eq!(t.rows.len(), 1);
        match &t.rows[0] {
            ActivityRow::Note {
                kind: NoteKind::Warning,
                ..
            } => {}
            _ => panic!("expected warning note"),
        }
    }

    #[test]
    fn turn_active_tracks_prompt_and_stop_edges() {
        let mut t = AcpTranscript::new("s-1");
        assert!(!t.turn_active, "fresh transcript is idle");
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                text: "go".into(),
                attachments: vec![],
            },
        ));
        assert!(t.turn_active, "UserPromptSent opens the turn");
        t.apply(&frame(2, Event::ThinkingStarted));
        assert!(t.turn_active, "thinking keeps the turn open");
        t.apply(&frame(
            3,
            Event::Stopped {
                reason: "completed".into(),
            },
        ));
        assert!(!t.turn_active, "Stopped closes the turn");
    }

    #[test]
    fn turn_active_clears_on_startup_error_and_rejection() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                text: "go".into(),
                attachments: vec![],
            },
        ));
        t.apply(&frame(
            2,
            Event::AgentStartupError {
                message: "boom".into(),
            },
        ));
        assert!(!t.turn_active, "startup error ends any in-flight turn");

        t.apply(&frame(
            3,
            Event::UserPromptSent {
                text: "again".into(),
                attachments: vec![],
            },
        ));
        assert!(t.turn_active);
        t.apply(&frame(
            4,
            Event::PromptRejected {
                text: "again".into(),
                reason: "read-only".into(),
            },
        ));
        assert!(!t.turn_active, "a rejected prompt never started a turn");
    }

    #[test]
    fn reset_returns_to_idle() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                text: "go".into(),
                attachments: vec![],
            },
        ));
        assert!(t.turn_active);
        t.reset();
        assert!(!t.turn_active, "reset drops derived turn state for replay");
    }

    #[test]
    fn tool_call_update_replaces_diffs_wholesale_and_none_preserves() {
        let mut t = AcpTranscript::new("s-1");
        let mut tc = tool("t-1", "Edit");
        tc.diffs = vec![DiffPreview {
            path: "a.rs".into(),
            old_text: Some("old".into()),
            new_text: Some("new".into()),
            created_at: Utc::now(),
        }];
        t.apply(&frame(1, Event::ToolCallStarted { tool_call: tc }));
        // A text-only update (diffs: None) must not erase the initial diffs.
        t.apply(&frame(
            2,
            Event::ToolCallUpdated {
                tool_call_id: "t-1".into(),
                title: Some("Edit a.rs".into()),
                args_preview: None,
                started_at: None,
                diffs: None,
            },
        ));
        match &t.rows[0] {
            ActivityRow::ToolCall(row) => {
                assert_eq!(row.diffs.len(), 1);
                assert_eq!(row.diffs[0].path, "a.rs");
            }
            _ => panic!("expected ToolCall"),
        }
        // A Some(..) update replaces the list wholesale (#1721).
        t.apply(&frame(
            3,
            Event::ToolCallUpdated {
                tool_call_id: "t-1".into(),
                title: None,
                args_preview: None,
                started_at: None,
                diffs: Some(vec![DiffPreview {
                    path: "b.rs".into(),
                    old_text: None,
                    new_text: Some("fresh".into()),
                    created_at: Utc::now(),
                }]),
            },
        ));
        match &t.rows[0] {
            ActivityRow::ToolCall(row) => {
                assert_eq!(row.diffs.len(), 1);
                assert_eq!(row.diffs[0].path, "b.rs");
            }
            _ => panic!("expected ToolCall"),
        }
    }

    #[test]
    fn usage_updated_stores_latest_snapshot() {
        let mut t = AcpTranscript::new("s-1");
        assert!(t.usage.is_none());
        t.apply(&frame(
            1,
            Event::UsageUpdated {
                usage: SessionUsage {
                    used: 1_000,
                    size: 200_000,
                    cost: None,
                },
            },
        ));
        t.apply(&frame(
            2,
            Event::UsageUpdated {
                usage: SessionUsage {
                    used: 5_000,
                    size: 200_000,
                    cost: None,
                },
            },
        ));
        assert_eq!(t.usage.as_ref().map(|u| u.used), Some(5_000));
    }

    #[test]
    fn reset_clears_state_but_preserves_session_id() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                text: "hi".into(),
                attachments: Vec::new(),
            },
        ));
        t.reset();
        assert_eq!(t.session_id, "s-1");
        assert_eq!(t.last_seq, 0);
        assert!(t.rows.is_empty());
    }
}
