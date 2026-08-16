//! Control-state reducer over `AcpBroadcastFrame` plus a server-fed
//! transcript row buffer, for the native TUI structured view.
//!
//! Since Tier 4 the ORDERED TRANSCRIPT (assistant messages, tool cards,
//! dividers, elicitation answers) is owned by the daemon: it folds the
//! event stream through `TranscriptModel` (`src/acp/transcript.rs`) once and
//! streams the built `TranscriptRow`s over the WS transcript channel (a
//! `transcript_snapshot` on connect, a `transcript_delta` per live event)
//! and via `GET /acp/replay?view=rows`. This reducer holds those rows in
//! `server_rows` and reconciles them by id; `render.rs` projects them to the
//! TUI's text presentation. The web reducer (`web/src/hooks/useAcpSession.ts`)
//! is the authoritative reference for this split.
//!
//! What still lives here is CONTROL state: whether a turn is active, the
//! pending approvals / elicitations, usage, mode, available commands, the
//! plan snapshot, and the compaction / cancel phases. Server-owned control
//! state (`reduced_state`, Tier 1.3) is not consumed yet, so the raw frames
//! are still folded for it. Notes:
//!
//! - Approvals are pure control state (`pending_approvals`), rendered as the
//!   modal approval shelf, not interleaved into the transcript. This matches
//!   the migrated web, whose `ActivityRow` union carries no approval kind:
//!   the tool card the approval gates already sits in `server_rows`.
//! - `AvailableCommandsUpdated` is retained for the composer's slash picker.
//! - `SessionContextReset` flips `context_primer_pending` so the view layer
//!   can offer the "paste a context primer" affordance.

use crate::acp::elicitations::ElicitationQuestion;
use crate::acp::protocol::AcpBroadcastFrame;
use crate::acp::state::{
    AvailableCommand, DiffPreview, Event, ModeInfo, PlanStepStatus, SessionUsage,
};
use crate::acp::transcript::{
    patch_transcript_row, upsert_transcript_row, TranscriptDelta, TranscriptRow,
};

#[derive(Debug, Clone)]
pub struct AcpTranscript {
    pub session_id: String,
    /// Friendly session title hydrated from `/api/sessions`.
    pub session_title: Option<String>,
    /// Resolved ACP registry key shown in the header. Updated when the backend
    /// switches mid-session.
    pub agent_name: Option<String>,
    /// The daemon-owned ordered transcript, reconciled by row id from the WS
    /// `transcript_snapshot` / `transcript_delta` channel and the
    /// `?view=rows` replay. `render.rs` projects these to text; nothing here
    /// builds them. See the module docs.
    pub server_rows: Vec<TranscriptRow>,
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
    /// Whether the agent accepts `_session/steering`, from the latest
    /// `PromptCapabilities`. When true the composer sends a mid-turn
    /// prompt straight through instead of parking it: the daemon injects
    /// it into the running turn. Rebuilt by `/replay` like `turn_active`,
    /// and re-emitted as `false` on a respawn onto an adapter that lacks
    /// the capability, so it cannot go stale. See #2805.
    pub steering: bool,
    /// Whether a `/compact` cycle is running, from
    /// `ConversationCompactionStarted` until the matching
    /// `ConversationCompacted` or the turn's `Stopped`. The adapter goes
    /// silent for 90 to 170 seconds in that window, so the composer must
    /// park a send instead of steering it: a summarization turn has
    /// nothing to steer and never answers the injected message. Rebuilt
    /// by `/replay` like `turn_active`. See #3219.
    pub compacting: bool,
    /// Whether a `session/cancel` is in flight, from `CancelRequested`
    /// until the turn's `Stopped`. Only consulted by the composer's park
    /// decision: the daemon reads a prompt arriving mid-cancel as a
    /// wedged agent and escalates to a runner restart, so a steerable
    /// agent must still park here rather than route Stop-then-type into
    /// that path. See #2805 / #1727.
    pub cancelling: bool,
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
}

/// A tool card the renderer builds from a server `tool_start` row (plus its
/// paired terminal row). No longer produced by this reducer; it is a pure
/// presentation view-model that `render::render_tool_lines` consumes, kept
/// here so the render helpers keep their existing shape.
#[derive(Debug, Clone)]
pub struct ToolCallRow {
    pub name: String,
    /// ACP `ToolKind` lowercased (`read` / `edit` / `delete` / `execute`
    /// / …), forwarded from `ToolCall::kind`. Drives the per-kind
    /// renderer in `render_tool_lines`; empty string falls back to the
    /// generic one-liner.
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
pub struct PlanLine {
    pub title: String,
    pub status: PlanStepStatus,
}

/// A pending approval, control state that drives the modal approval shelf.
/// Carries the full request payload (previously read off the inline
/// `ApprovalRow`) so the shelf renders without an approval transcript row:
/// the daemon transcript emits none, since the gated tool card already sits
/// in `server_rows`.
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub nonce: String,
    pub title: String,
    pub kind: String,
    pub args: String,
    pub destructive: bool,
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

/// Styling class for a divider / notice line the transcript projection
/// renders (session cleared, compacted, summary, context reset, ...).
#[derive(Debug, Clone, Copy)]
pub enum NoteKind {
    Info,
    Warning,
}

impl AcpTranscript {
    /// Whether an arriving user prompt is a message steered into the turn
    /// already running rather than the start of a new one (#2805).
    ///
    /// The daemon injects a mid-turn prompt via `_session/steering`
    /// instead of starting a turn for it, so the same condition the
    /// composer used to send it identifies it on the way back. Such a
    /// prompt must not run the fresh-turn bookkeeping: no new turn began,
    /// and the running turn's `Stopped` still owns the state it built up.
    /// Call before mutating `turn_active`.
    fn is_steered_continuation(&self) -> bool {
        self.turn_active && self.steering
    }

    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            session_title: None,
            agent_name: None,
            server_rows: Vec::new(),
            pending_approvals: Vec::new(),
            pending_elicitations: Vec::new(),
            status_text: None,
            current_mode: None,
            available_modes: Vec::new(),
            available_commands: Vec::new(),
            context_primer_pending: false,
            steering: false,
            cancelling: false,
            compacting: false,
            turn_active: false,
            usage: None,
            current_plan: Vec::new(),
            lagged: false,
            last_seq: 0,
        }
    }

    /// Reconcile a batch of server-folded transcript rows (a WS
    /// `transcript_snapshot`, or a `?view=rows` replay) into `server_rows`
    /// by id. Idempotent, so the connect snapshot overlapping the initial
    /// replay is a no-op. See [`upsert_transcript_row`].
    pub fn merge_server_rows(&mut self, rows: Vec<TranscriptRow>) {
        for row in rows {
            upsert_transcript_row(&mut self.server_rows, row);
        }
    }

    /// Apply one incremental transcript row change (a WS `transcript_delta`):
    /// `Append`/`Patch` upsert by id, `Remove` drops the row. `Append` uses
    /// the same reconcile as the snapshot so a live append that raced a
    /// replayed row does not double it.
    pub fn apply_transcript_delta(&mut self, delta: TranscriptDelta) {
        match delta {
            TranscriptDelta::Append(row) => upsert_transcript_row(&mut self.server_rows, row),
            TranscriptDelta::Patch { row, .. } => patch_transcript_row(&mut self.server_rows, row),
            TranscriptDelta::Remove(id) => self.server_rows.retain(|r| r.id != id),
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
    pub fn resolve_approval_locally(&mut self, nonce: &str) {
        self.pending_approvals.retain(|p| p.nonce != nonce);
    }

    /// Optimistically clear a pending elicitation after the skip/cancel
    /// POST succeeded (or 404'd), mirroring `resolve_approval_locally`.
    pub fn resolve_elicitation_locally(&mut self, nonce: &str) {
        self.pending_elicitations.retain(|p| p.nonce != nonce);
    }

    /// Mark `lagged = true`. The view layer notices this (the status line
    /// shows a "broadcast lagged" banner) and triggers a /replay refetch,
    /// which reseeds `server_rows` via `?view=rows`.
    pub fn set_lagged(&mut self) {
        self.lagged = true;
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

    /// Fold one event into CONTROL state only. The ordered transcript is
    /// server-owned since Tier 4, so every arm that used to build an
    /// `ActivityRow` (assistant messages, tool cards, dividers, elicitation
    /// answers) is gone; what remains is turn / approval / elicitation /
    /// usage / mode / plan / phase state. Events that carry no control state
    /// (message chunks, the tool lifecycle, notices, summaries) are no-ops
    /// here: their rows arrive over the transcript channel.
    fn apply_event(&mut self, event: &Event) {
        match event {
            Event::UserPromptSent { .. } | Event::UserDiffCommentsPrompt { .. } => {
                // Sending a prompt dismisses any context-primer hint and opens
                // the turn. A steered mid-turn prompt is not a fresh turn, so
                // it must not clear the running turn's pending cancel; a genuine
                // fresh turn supersedes a stale one (#1727 / #2805). Web routes
                // both prompt kinds through the same `applyNewTurnResets`.
                self.context_primer_pending = false;
                let steered = self.is_steered_continuation();
                self.turn_active = true;
                if !steered {
                    self.cancelling = false;
                }
            }
            Event::ThinkingStarted => {
                self.status_text = Some("thinking…".to_string());
                // Deliberately does NOT clear `cancelling`, even though it
                // sets `turn_active`. This fires repeatedly *within* a
                // running turn, so clearing here would drop the pending
                // cancel the moment the agent emits its next thought,
                // which is exactly when a user is waiting on a stop.
                self.turn_active = true;
            }
            Event::ThinkingEnded => {
                if self.status_text.as_deref() == Some("thinking…") {
                    self.status_text = None;
                }
            }
            Event::ApprovalRequested { approval } => {
                // Approvals are pure control state: the shelf renders from
                // this list, and the gated tool card is already a server row.
                self.pending_approvals.push(PendingApproval {
                    nonce: approval.nonce.0.clone(),
                    title: approval.tool_call.name.clone(),
                    kind: approval.tool_call.kind.clone(),
                    args: approval.tool_call.args_preview.clone(),
                    destructive: approval.destructive,
                });
            }
            Event::ApprovalResolved { nonce, .. } => {
                self.pending_approvals.retain(|p| p.nonce != nonce.0);
            }
            Event::ElicitationRequested { elicitation } => {
                self.pending_elicitations.push(PendingElicitation {
                    nonce: elicitation.nonce.0.clone(),
                    message: elicitation.message.clone(),
                    questions: elicitation.questions.clone(),
                });
            }
            Event::ElicitationResolved { nonce, .. } => {
                // The server transcript emits the `elicitation_answered` row;
                // here we only clear the pending card. Idempotent on a
                // re-broadcast (cancel-on-teardown racing a POST).
                self.pending_elicitations.retain(|p| p.nonce != nonce.0);
            }
            Event::PlanUpdated { plan } => {
                self.current_plan = plan
                    .steps
                    .iter()
                    .map(|s| PlanLine {
                        title: s.title.clone(),
                        status: s.status.clone(),
                    })
                    .collect();
            }
            Event::Stopped { reason } => {
                self.status_text = Some(format!("stopped: {reason}"));
                self.turn_active = false;
                self.cancelling = false;
                // The turn is over however it ended, so any compaction it
                // was running is over too. This is the self-healing clear:
                // a dropped completion marker, a killed worker, or a user
                // cancel all arrive here. See #3219.
                self.compacting = false;
            }
            Event::AgentStartupError { .. } => {
                self.status_text = Some("startup error".to_string());
                self.turn_active = false;
                self.cancelling = false;
            }
            Event::PromptRuntimeError { .. } => {
                self.status_text = Some("prompt error".to_string());
                self.turn_active = false;
                self.cancelling = false;
            }
            Event::SessionContextReset { .. } => {
                self.context_primer_pending = true;
            }
            Event::SessionCleared => {
                // /clear wiped the model's memory: drop session-scoped
                // capability caches the agent no longer recognises. The
                // divider row itself arrives over the transcript channel.
                self.available_commands.clear();
                self.current_mode = None;
            }
            Event::ConversationCompactionStarted => {
                // The adapter is about to go silent for 90 to 170 seconds.
                // Latch the phase so the composer parks a send rather than
                // steering it into a turn that will never answer it. See #3219.
                self.compacting = true;
                self.status_text = Some("compacting…".to_string());
            }
            Event::ConversationCompacted => {
                self.compacting = false;
                // Drop the pre-compaction usage snapshot: the model's context
                // is now a summary, so the latched "160k/200k" describes a
                // window that no longer exists. The web reducer nulls it at the
                // same boundary. See #3253.
                self.usage = None;
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
                // The daemon refused the prompt (e.g. read-only mode); no turn
                // started, so clear the busy flag the optimistic submit path
                // may have set. `cancelling` deliberately survives: a rejection
                // is not a turn boundary, and clearing here would let the next
                // send take the steering path into the daemon's escalation.
                self.turn_active = false;
            }
            Event::UsageUpdated { usage } => {
                // Latest snapshot wins; the agent typically resends after each
                // turn. Rendered as the status-line token meter.
                self.usage = Some(usage.clone());
            }
            Event::PromptCapabilities { steering, .. } => {
                self.steering = *steering;
            }
            Event::CancelRequested { .. } => {
                self.cancelling = true;
            }
            Event::AgentSwitched { to, .. } => {
                self.agent_name = Some(to.clone());
                self.current_plan.clear();
            }
            // Everything else carries no control state for the native view:
            // the assistant-message stream, the whole tool lifecycle, the
            // divider / summary / rate-limit-resume / mode-switch-failed
            // notices, and the events other surfaces own. Their transcript
            // rows (where any) arrive over the server transcript channel.
            Event::SessionTitleSuggested { .. }
            | Event::AgentMessageChunk { .. }
            | Event::ToolCallStarted { .. }
            | Event::ToolCallUpdated { .. }
            | Event::ToolCallContent { .. }
            | Event::ToolCallCompleted { .. }
            | Event::TodoListUpdated { .. }
            | Event::IncompatibleAgent { .. }
            | Event::ConversationSummary { .. }
            | Event::AcpSessionAssigned { .. }
            | Event::RateLimitAutoResumed { .. }
            | Event::ModeSwitchFailed { .. }
            | Event::DiffEmitted { .. }
            | Event::RateLimit { .. }
            | Event::RawAgentUpdate { .. }
            | Event::BackgroundAgentLaunched { .. }
            | Event::BackgroundAgentProgress { .. }
            | Event::BackgroundAgentCompleted { .. }
            | Event::WakeupScheduled { .. }
            | Event::MonitorArmed { .. }
            | Event::ConfigOptionsUpdated { .. }
            | Event::ConfigOptionSwitchFailed { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::approvals::{Approval, ApprovalDecision, Nonce};
    use crate::acp::state::{Plan, PlanStep, PlanStepStatus, SessionMode, ToolCall};
    use crate::acp::transcript::{TranscriptModel, TranscriptRowKind};
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

    /// Build the server's transcript rows for a sequence of events, the way
    /// the daemon does before shipping them over the WS transcript channel /
    /// `?view=rows`. Lets a reducer test feed realistic rows without
    /// hand-writing `TranscriptRow` literals.
    fn server_rows(events: &[Event]) -> Vec<TranscriptRow> {
        let mut m = TranscriptModel::new();
        for (i, e) in events.iter().enumerate() {
            m.apply_event(i as u64 + 1, e);
        }
        m.rows().to_vec()
    }

    /// The composer parks a mid-turn send while a cancel is pending even
    /// on a steerable agent (#2805). Both directions matter: a
    /// `cancelling` left set would park every later mid-turn send on a
    /// session that had been stopped once, while one cleared too early
    /// lets the next send take the steering path into the daemon's
    /// wedged-agent escalation.
    #[test]
    fn cancelling_tracks_the_turn_not_the_rejection() {
        let cancel = || Event::CancelRequested {
            escalates_at: Utc::now(),
        };

        // A rejection is not a turn boundary. The daemon rejects a
        // mid-cancel prompt and then escalates, so the cancel is still
        // pending and its real `Stopped` is still to come.
        let mut t = AcpTranscript::new("s-1");
        assert!(!t.cancelling);
        t.apply(&frame(1, cancel()));
        assert!(t.cancelling);
        t.apply(&frame(
            2,
            Event::PromptRejected {
                reason: "agent_busy".into(),
                text: "hi".into(),
            },
        ));
        assert!(
            t.cancelling,
            "a rejected prompt must not clear a pending cancel"
        );

        // `ThinkingStarted` also sets `turn_active`, but it fires within
        // a running turn, so it must NOT count as a fresh turn: clearing
        // there would drop the cancel the moment the agent thinks again.
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(1, cancel()));
        t.apply(&frame(2, Event::ThinkingStarted));
        assert!(
            t.cancelling,
            "mid-turn thinking must not clear a pending cancel"
        );

        // Terminal turn events clear it, and so does either fresh-turn
        // branch: web routes both prompt kinds through the same
        // `applyNewTurnResets`.
        let clearing = [
            Event::Stopped {
                reason: "cancelled".into(),
            },
            Event::AgentStartupError {
                message: "boom".into(),
            },
            Event::PromptRuntimeError {
                message: "boom".into(),
            },
            Event::UserPromptSent {
                prompt_id: None,
                text: "next turn".into(),
                attachments: Vec::new(),
            },
            Event::UserDiffCommentsPrompt {
                intro: "look".into(),
                outro: "thanks".into(),
                is_multi_repo: false,
                comments: Vec::new(),
                assembled_markdown: "next turn".into(),
            },
        ];
        for event in clearing {
            let label = format!("{event:?}");
            let mut t = AcpTranscript::new("s-1");
            t.apply(&frame(1, cancel()));
            t.apply(&frame(2, event));
            assert!(!t.cancelling, "{label} must clear the pending cancel");
        }
    }

    /// #3219: the compaction phase latches on the start marker and clears
    /// on exactly two events. A mid-compaction `UserPromptSent` must NOT
    /// clear it, or a prompt confirmed during the silent window would
    /// re-arm the force-end hatch while the compaction is still running.
    #[test]
    fn compaction_phase_clears_only_on_completion_or_stopped() {
        let started = || Event::ConversationCompactionStarted;
        let cases = [
            (Event::ConversationCompacted, false),
            (
                Event::Stopped {
                    reason: "prompt_complete".into(),
                },
                false,
            ),
            (
                Event::Stopped {
                    reason: "cancelled".into(),
                },
                false,
            ),
            // A steered follow-up the daemon confirmed mid-compaction.
            (
                Event::UserPromptSent {
                    prompt_id: None,
                    text: "also check the tests".into(),
                    attachments: Vec::new(),
                },
                true,
            ),
            // Ordinary streaming inside the window changes nothing.
            (Event::ThinkingStarted, true),
        ];
        for (event, expected) in cases {
            let label = format!("{event:?}");
            let mut t = AcpTranscript::new("s-1");
            t.apply(&frame(1, started()));
            assert!(t.compacting, "the start marker must latch the phase");
            t.apply(&frame(2, event));
            assert_eq!(t.compacting, expected, "after {label}");
        }
    }

    #[test]
    fn user_prompt_opens_turn_and_builds_no_local_rows() {
        // The transcript rows are server-owned now: a UserPromptSent mutates
        // only control state (turn open, primer hint cleared) and appends
        // nothing to the local buffer.
        let mut t = AcpTranscript::new("s-1");
        t.context_primer_pending = true;
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                prompt_id: None,
                text: "hi".into(),
                attachments: Vec::new(),
            },
        ));
        assert!(t.turn_active);
        assert!(!t.context_primer_pending);
        assert!(t.server_rows.is_empty(), "reducer must not build rows");
        assert_eq!(t.last_seq, 1);
    }

    #[test]
    fn current_mode_keeps_real_id_when_legacy_enum_follows() {
        // acp_client emits [CurrentModeChanged{real id}, ModeChanged{enum}].
        // An OpenCode `build`/custom agent has no SessionMode variant, so the
        // enum coerces to Default; the raw id must survive. See #1827.
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
    fn approval_request_enriches_pending_and_resolution_clears() {
        // Approvals are control state: the request adds an enriched pending
        // entry (title/kind/args/destructive for the shelf) and never a row;
        // the resolution clears it.
        let mut t = AcpTranscript::new("s-1");
        let mut tc = tool("t-1", "Bash");
        tc.kind = "execute".into();
        tc.args_preview = r#"{"command":"rm -rf /"}"#.into();
        let approval = Approval {
            nonce: Nonce("nonce-1".into()),
            tool_call: tc,
            destructive: true,
            requested_at: Utc::now(),
            resolved: None,
        };
        t.apply(&frame(1, Event::ApprovalRequested { approval }));
        assert_eq!(t.pending_approvals.len(), 1);
        let p = &t.pending_approvals[0];
        assert_eq!(p.nonce, "nonce-1");
        assert_eq!(p.title, "Bash");
        assert_eq!(p.kind, "execute");
        assert!(p.destructive);
        assert!(p.args.contains("rm -rf"));
        assert!(t.server_rows.is_empty(), "no inline approval row");
        t.apply(&frame(
            2,
            Event::ApprovalResolved {
                nonce: Nonce("nonce-1".into()),
                decision: ApprovalDecision::Allow,
            },
        ));
        assert!(t.pending_approvals.is_empty());
        assert!(t.server_rows.is_empty());
    }

    #[test]
    fn elicitation_request_and_resolution_track_pending_only() {
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
        // No local rows: the `elicitation_answered` trace is a server row.
        assert!(t.server_rows.is_empty());
        t.apply(&frame(
            2,
            Event::ElicitationResolved {
                nonce: Nonce("e-1".into()),
                outcome: ElicitationOutcome::Declined,
                answers: Vec::new(),
            },
        ));
        assert!(t.pending_elicitations.is_empty());
        assert!(t.server_rows.is_empty());
    }

    #[test]
    fn resolve_approval_locally_clears_card_without_broadcast() {
        // #1821: the optimistic clear removes the pending approval without an
        // ApprovalResolved frame, since the broadcast can be swallowed by the
        // seq dedupe.
        let mut t = AcpTranscript::new("s-1");
        let approval = Approval {
            nonce: Nonce("approval-correlation-id".into()),
            tool_call: tool("t-1", "Bash"),
            destructive: true,
            requested_at: Utc::now(),
            resolved: None,
        };
        let nonce = approval.nonce.0.to_string();
        t.apply(&frame(1, Event::ApprovalRequested { approval }));
        assert_eq!(t.pending_approvals.len(), 1);

        t.resolve_approval_locally(&nonce);
        assert!(t.pending_approvals.is_empty());

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
        // Replay-vs-live overlap can deliver the same seq twice; the reducer
        // dedupes on seq, so a re-delivered older frame cannot mutate control
        // state. A lower seq is dropped too.
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            2,
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
                    used: 9_999,
                    size: 200_000,
                    cost: None,
                },
            },
        ));
        t.apply(&frame(
            1,
            Event::UsageUpdated {
                usage: SessionUsage {
                    used: 5,
                    size: 200_000,
                    cost: None,
                },
            },
        ));
        assert_eq!(t.usage.as_ref().map(|u| u.used), Some(1_000));
        assert_eq!(t.last_seq, 2);
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
        t.apply(&frame(
            2,
            Event::UserPromptSent {
                prompt_id: None,
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
        assert!(t.server_rows.is_empty());
        assert_eq!(t.current_plan.len(), 1);
        assert_eq!(t.current_plan[0].title, "Step one");
    }

    #[test]
    fn set_lagged_flags_without_touching_rows() {
        let mut t = AcpTranscript::new("s-1");
        t.set_lagged();
        assert!(t.lagged);
        assert!(t.server_rows.is_empty());
    }

    #[test]
    fn turn_active_tracks_prompt_and_stop_edges() {
        let mut t = AcpTranscript::new("s-1");
        assert!(!t.turn_active, "fresh transcript is idle");
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                prompt_id: None,
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
                prompt_id: None,
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
                prompt_id: None,
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
    fn reset_returns_to_idle_and_drops_rows() {
        let mut t = AcpTranscript::new("s-1");
        t.apply(&frame(
            1,
            Event::UserPromptSent {
                prompt_id: None,
                text: "go".into(),
                attachments: vec![],
            },
        ));
        t.merge_server_rows(server_rows(&[Event::AgentMessageChunk {
            text: "hi".into(),
        }]));
        assert!(t.turn_active);
        assert!(!t.server_rows.is_empty());
        t.reset();
        assert!(!t.turn_active, "reset drops derived turn state for replay");
        assert!(t.server_rows.is_empty(), "reset drops the row buffer");
        assert_eq!(t.session_id, "s-1");
        assert_eq!(t.last_seq, 0);
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
        // A compaction rewrites the model's context, so the replaced snapshot
        // no longer describes anything. Matching the web reducer.
        t.apply(&frame(3, Event::ConversationCompacted));
        assert!(t.usage.is_none());
    }

    #[test]
    fn merge_server_rows_upserts_by_id_and_guards_rich_tool_start() {
        // The snapshot / `?view=rows` reconcile is idempotent by id and must
        // not let a sparse synth `tool_start` clobber a richer one already
        // buffered (the #1713/#2711 seam, mirrored from web `mergeServerRows`).
        let mut t = AcpTranscript::new("s-1");
        let rich = TranscriptModel::new();
        let mut m = rich;
        let mut tc = tool("dup", "Bash");
        tc.args_preview = r#"{"x":1}"#.into();
        m.apply_event(1, &Event::ToolCallStarted { tool_call: tc });
        let rich_rows = m.rows().to_vec();
        t.merge_server_rows(rich_rows.clone());
        // Re-applying the same rows is a no-op (idempotent overlap).
        t.merge_server_rows(rich_rows);
        assert_eq!(
            t.server_rows
                .iter()
                .filter(|r| r.kind == TranscriptRowKind::ToolStart)
                .count(),
            1
        );
        // A sparse synth start for the same id must not overwrite the args.
        let mut sparse = TranscriptModel::new();
        sparse.apply_event(
            1,
            &Event::ToolCallStarted {
                tool_call: ToolCall {
                    kind: "other".into(),
                    args_preview: String::new(),
                    ..tool("dup", "Bash")
                },
            },
        );
        t.merge_server_rows(sparse.rows().to_vec());
        let start = t
            .server_rows
            .iter()
            .find(|r| r.kind == TranscriptRowKind::ToolStart)
            .unwrap();
        assert_eq!(
            start.tool.as_ref().unwrap().args_preview,
            r#"{"x":1}"#,
            "richer args survive the merge"
        );
    }

    #[test]
    fn apply_transcript_delta_appends_patches_and_removes() {
        let mut t = AcpTranscript::new("s-1");
        let rows = server_rows(&[
            Event::UserPromptSent {
                prompt_id: None,
                text: "hi".into(),
                attachments: Vec::new(),
            },
            Event::AgentMessageChunk { text: "one".into() },
        ]);
        // Append each row via a delta.
        for row in rows.clone() {
            t.apply_transcript_delta(TranscriptDelta::Append(row));
        }
        assert_eq!(t.server_rows.len(), 2);
        // An Append with an id already present is idempotent, not a dupe.
        t.apply_transcript_delta(TranscriptDelta::Append(rows[0].clone()));
        assert_eq!(t.server_rows.len(), 2);
        // Patch replaces the matching row's payload.
        let mut patched = rows[1].clone();
        patched.text = "one-edited".into();
        t.apply_transcript_delta(TranscriptDelta::Patch {
            id: patched.id.clone(),
            row: patched.clone(),
        });
        assert_eq!(
            t.server_rows
                .iter()
                .find(|r| r.id == patched.id)
                .unwrap()
                .text,
            "one-edited"
        );
        // Remove drops it.
        t.apply_transcript_delta(TranscriptDelta::Remove(patched.id.clone()));
        assert!(!t.server_rows.iter().any(|r| r.id == patched.id));
        assert_eq!(t.server_rows.len(), 1);
    }
}
