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
use crate::acp::protocol::AcpBroadcastFrame;
use crate::acp::state::{AvailableCommand, Event, PlanStepStatus, ToolOutputBlock};

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
    pub rows: Vec<ActivityRow>,
    pub pending_approvals: Vec<PendingApproval>,
    /// Live status banner (e.g. "thinking…", "ended: completed").
    pub status_text: Option<String>,
    /// Latest mode id the agent reported. `None` until the agent
    /// emits `ModesAvailable` / `CurrentModeChanged`.
    pub current_mode: Option<String>,
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
    Plan(Vec<PlanLine>),
    Note { kind: NoteKind, text: String },
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
            rows: Vec::new(),
            pending_approvals: Vec::new(),
            status_text: None,
            current_mode: None,
            available_commands: Vec::new(),
            context_primer_pending: false,
            turn_active: false,
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
        *self = Self::new(session_id);
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
                ..
            } => {
                self.flush_pending_chunk();
                if let Some(&idx) = self.tool_idx.get(tool_call_id) {
                    if let Some(ActivityRow::ToolCall(row)) = self.rows.get_mut(idx) {
                        // The terminal can't render images/audio inline, so a
                        // media completion shows a textual placeholder instead
                        // of collapsing to the status word. See #1818.
                        let text = if output.is_empty() {
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
            Event::PlanUpdated { plan } => {
                self.flush_pending_chunk();
                let lines: Vec<PlanLine> = plan
                    .steps
                    .iter()
                    .map(|s| PlanLine {
                        title: s.title.clone(),
                        status: s.status.clone(),
                    })
                    .collect();
                self.rows.push(ActivityRow::Plan(lines));
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
            Event::AcpSessionAssigned { acp_session_id } => {
                // Bookkeeping event; not surfaced to the user.
                let _ = acp_session_id;
            }
            Event::AvailableCommandsUpdated { commands } => {
                self.available_commands = commands.clone();
            }
            Event::ModesAvailable {
                current_mode_id, ..
            } => {
                self.current_mode = Some(current_mode_id.clone());
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
            Event::DiffEmitted { .. }
            | Event::RateLimit { .. }
            | Event::UsageUpdated { .. }
            | Event::RawAgentUpdate { .. }
            | Event::WakeupScheduled { .. }
            | Event::CancelRequested { .. }
            | Event::PromptCapabilities { .. }
            | Event::AgentSwitched { .. }
            | Event::ModeSwitchFailed { .. }
            | Event::ConfigOptionsUpdated { .. }
            | Event::ConfigOptionSwitchFailed { .. } => {
                // Surface as info notes for now; richer renderers are
                // followup work tracked in the plan's "out of scope".
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
    fn plan_update_creates_plan_row() {
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
        assert!(matches!(&t.rows[0], ActivityRow::Plan(lines) if lines.len() == 1));
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
