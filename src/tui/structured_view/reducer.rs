//! Server-fed view state for the native TUI structured view: the daemon's
//! folded control state plus its ordered transcript rows. Nothing here
//! reduces the raw event stream any more.
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
//! CONTROL state (turn flags, pending approvals / elicitations, usage, mode,
//! available commands, the plan snapshot, the compaction / cancel phases)
//! arrives the same way since Tier 1.3: the daemon folds `AcpState` once and
//! pushes it as a `reduced_state` frame, which
//! [`AcpTranscript::apply_reduced_state`] adopts wholesale. Notes:
//!
//! - Approvals are pure control state (`pending_approvals`), rendered as the
//!   modal approval shelf, not interleaved into the transcript. This matches
//!   the migrated web, whose `ActivityRow` union carries no approval kind:
//!   the tool card the approval gates already sits in `server_rows`.
//! - The two `resolve_*_locally` helpers are the only optimism left: they hide
//!   a card the user just answered until the server's list catches up.
//! - The "context lost, re-prime?" banner is derived from the rows rather than
//!   latched from an event. See [`AcpTranscript::context_primer_pending`].

use crate::acp::elicitations::ElicitationQuestion;
use crate::acp::state::{
    AcpState, AvailableCommand, DiffPreview, ModeInfo, PlanStepStatus, SessionUsage,
};
use crate::acp::transcript::{
    patch_transcript_row, upsert_transcript_row, TranscriptDelta, TranscriptRow, TranscriptRowKind,
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
    /// Live status banner ("thinking…" / "compacting…"), shown only while a
    /// turn runs. Derived from the server's phase in `apply_reduced_state`.
    pub status_text: Option<String>,
    /// Id of the agent's currently selected mode. `None` until the agent
    /// advertises one.
    pub current_mode: Option<String>,
    /// Permission modes the agent advertised (`ModesAvailable`). Drives
    /// the `m` mode picker; empty when the agent never announced any.
    pub available_modes: Vec<ModeInfo>,
    /// Slash commands the agent has advertised. Drives the composer's
    /// `/` picker (followup #1018).
    pub available_commands: Vec<AvailableCommand>,
    /// Nonces of approvals / elicitations the user resolved here, held until
    /// the daemon's own pending list stops carrying them. See
    /// [`Self::apply_reduced_state`].
    locally_resolved: Vec<String>,
    /// Whether the agent is mid-turn. The composer reads it to decide whether
    /// Enter sends now or parks the prompt in the daemon's queue.
    pub turn_active: bool,
    /// Whether the agent accepts `_session/steering`. When true the composer
    /// sends a mid-turn prompt straight through instead of parking it: the
    /// daemon injects it into the running turn. Re-derived as `false` on a
    /// respawn onto an adapter that lacks the capability, so it cannot go
    /// stale. See #2805.
    pub steering: bool,
    /// Whether a `/compact` cycle is running. The adapter goes silent for 90
    /// to 170 seconds in that window, so the composer must park a send rather
    /// than steer it: a summarization turn has nothing to steer and never
    /// answers the injected message. See #3219.
    pub compacting: bool,
    /// Whether a `session/cancel` is in flight. Only consulted by the
    /// composer's park decision: the daemon reads a prompt arriving mid-cancel
    /// as a wedged agent and escalates to a runner restart, so a steerable
    /// agent must still park here rather than route Stop-then-type into that
    /// path. See #2805 / #1727.
    pub cancelling: bool,
    /// Latest context-window usage / cost snapshot the agent reported.
    /// Rendered as a token meter in the status line, mirroring the web
    /// composer's usage chip.
    pub usage: Option<SessionUsage>,
    /// Latest plan snapshot. Kept separate from the append-only transcript so
    /// repeated progress updates render as one sticky summary instead of a
    /// growing stack of near-identical checklists.
    pub current_plan: Vec<PlanLine>,
    /// Set when the WS layer reports `{"kind":"lagged"}`; the view layer
    /// rebuilds the rows via `?view=rows`. See [`Self::drop_rows`].
    pub lagged: bool,
    /// Highest seq applied from a `reduced_state` frame. Used as the `since`
    /// cursor for reconnect and to drop stale frames.
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
    pub outcome: ToolOutcome,
    /// Empty string when the agent didn't ship a content body; the
    /// view layer falls back to a status word in that case.
    pub content: String,
}

/// How a tool call ended. `Stopped` is not a failure: it is the turn-end
/// sweep closing a call the adapter left open (#1646), so it reads neutral,
/// matching the web's third tool-card status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolOutcome {
    Ok,
    Error,
    Stopped,
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
    Error,
}

impl AcpTranscript {
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
            locally_resolved: Vec::new(),
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

    /// Drop the transcript rows ahead of a `?view=rows` rebuild, used when the
    /// daemon signals `lagged` and rows we never saw may have been evicted.
    /// Control state is left alone here because the daemon repairs it at the
    /// source: on a lag it re-folds the session from the event store and
    /// pushes a corrected `reduced_state`, so the next frame is authoritative.
    pub fn drop_rows(&mut self) {
        self.server_rows.clear();
    }

    /// Optimistically clear an approval card by nonce after the resolve
    /// POST succeeded (204) or the daemon reported the nonce already gone
    /// (404), instead of waiting on the `ApprovalResolved` broadcast, which
    /// the seq dedupe can swallow and leave the card stuck. Mirrors the
    /// `ApprovalResolved` event arm. See #1821.
    pub fn resolve_approval_locally(&mut self, nonce: &str) {
        self.pending_approvals.retain(|p| p.nonce != nonce);
        self.locally_resolved.push(nonce.to_string());
    }

    /// Optimistically clear a pending elicitation after the skip/cancel
    /// POST succeeded (or 404'd), mirroring `resolve_approval_locally`.
    pub fn resolve_elicitation_locally(&mut self, nonce: &str) {
        self.pending_elicitations.retain(|p| p.nonce != nonce);
        self.locally_resolved.push(nonce.to_string());
    }

    /// Mark `lagged = true`. The view layer notices this (the status line
    /// shows a "broadcast lagged" banner) and triggers a /replay refetch,
    /// which reseeds `server_rows` via `?view=rows`.
    pub fn set_lagged(&mut self) {
        self.lagged = true;
    }

    /// Adopt the daemon's folded control state, carried by a WS
    /// `reduced_state` frame on connect and after every event. This is the
    /// whole control reduction now: nothing here folds raw events, so the
    /// native view and the web render the same server-derived truth.
    ///
    /// A frame older than the last one applied is dropped, so a snapshot that
    /// races live deltas cannot rewind the view.
    ///
    /// `unchanged` names cold fields the server omitted because this
    /// connection already holds them; they arrive as empty defaults, so
    /// adopting them blindly would blank the pickers.
    pub fn apply_reduced_state(&mut self, seq: u64, state: AcpState, unchanged: &[String]) {
        if seq < self.last_seq {
            tracing::debug!(
                target: "acp.tui.reducer",
                session = %self.session_id,
                seq,
                last_seq = self.last_seq,
                "dropped stale reduced_state frame"
            );
            return;
        }
        self.last_seq = seq;

        self.agent_name = Some(state.agent.0);
        self.turn_active = state.turn_active;
        self.steering = state.steering;
        self.cancelling = state.cancelling;
        self.compacting = state.compacting;
        self.usage = state.usage;
        let holds = |field: &str| unchanged.iter().any(|f| f == field);
        if !holds("available_commands") {
            self.available_commands = state.available_commands;
        }
        if !holds("available_modes") {
            self.available_modes = state.available_modes;
        }
        self.current_mode = state.current_mode_id;
        self.current_plan = state
            .current_plan
            .map(|plan| {
                plan.steps
                    .into_iter()
                    .map(|s| PlanLine {
                        title: s.title,
                        status: s.status,
                    })
                    .collect()
            })
            .unwrap_or_default();

        // The status banner only renders while a turn is running, so the
        // phases that end one (stopped, startup / prompt errors) never reach
        // the screen. Compaction outranks thinking: the adapter goes silent
        // for 90 to 170 seconds there and the user needs to know why.
        self.status_text = if state.compacting {
            Some("compacting…".to_string())
        } else if state.thinking.is_some() {
            Some("thinking…".to_string())
        } else {
            None
        };

        // A locally-resolved card stays hidden until the daemon's own list
        // agrees. The shelf clears on the resolve POST's 204/404 rather than
        // waiting for the broadcast (#1821), and without this filter the very
        // next event's reduced state would paint the card straight back.
        let still_pending: Vec<&str> = state
            .pending_approvals
            .iter()
            .map(|a| a.nonce.0.as_str())
            .chain(
                state
                    .pending_elicitations
                    .iter()
                    .map(|e| e.nonce.0.as_str()),
            )
            .collect();
        self.locally_resolved
            .retain(|n| still_pending.contains(&n.as_str()));

        self.pending_approvals = state
            .pending_approvals
            .into_iter()
            .filter(|a| !self.locally_resolved.contains(&a.nonce.0))
            .map(|a| PendingApproval {
                nonce: a.nonce.0,
                title: a.tool_call.name,
                kind: a.tool_call.kind,
                args: a.tool_call.args_preview,
                destructive: a.destructive,
            })
            .collect();
        self.pending_elicitations = state
            .pending_elicitations
            .into_iter()
            .filter(|e| !self.locally_resolved.contains(&e.nonce.0))
            .map(|e| PendingElicitation {
                nonce: e.nonce.0,
                message: e.message,
                questions: e.questions,
            })
            .collect();
    }

    /// Whether the model lost its context and the next prompt re-primes it.
    /// Derived from the server rows (the newest `context_reset` row with no
    /// prompt after it) rather than latched from a raw event, so it survives
    /// a reconnect without any client-side reduction.
    pub fn context_primer_pending(&self) -> bool {
        self.server_rows
            .iter()
            .rev()
            .find_map(|row| match row.kind {
                TranscriptRowKind::ContextReset => Some(true),
                TranscriptRowKind::UserPrompt | TranscriptRowKind::UserDiffComments => Some(false),
                _ => None,
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::approvals::{Approval, Nonce};
    use crate::acp::elicitations::Elicitation;
    use crate::acp::state::{
        AcpSessionId, AgentName, Event, Plan, PlanStep, ThinkingSignal, ToolCall,
    };
    use crate::acp::transcript::{TranscriptModel, TranscriptRowKind};
    use chrono::Utc;

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
    /// `?view=rows`. Lets a test feed realistic rows without hand-writing
    /// `TranscriptRow` literals.
    fn server_rows(events: &[Event]) -> Vec<TranscriptRow> {
        let mut m = TranscriptModel::new();
        for (i, e) in events.iter().enumerate() {
            m.apply_event(i as u64 + 1, e);
        }
        m.rows().to_vec()
    }

    /// The daemon's control state, folded from `events` exactly as the WS
    /// handler does before emitting a `reduced_state` frame.
    fn reduced(events: &[Event]) -> AcpState {
        let mut s = AcpState::new(
            AcpSessionId("s-1".into()),
            AgentName("claude".into()),
            Some("claude-opus-5".into()),
        );
        for e in events {
            s.apply_event(e.clone()).expect("apply ok");
        }
        s
    }

    fn approval(nonce: &str) -> Approval {
        Approval {
            nonce: Nonce(nonce.into()),
            tool_call: tool("t-1", "Edit a file"),
            destructive: true,
            requested_at: Utc::now(),
            resolved: None,
        }
    }

    /// Every control field the view renders comes off the frame verbatim: the
    /// TUI holds no derivation of its own any more, so a mapping slip here is
    /// the whole class of bug this tier can still introduce.
    #[test]
    fn reduced_state_maps_every_rendered_control_field() {
        let mut t = AcpTranscript::new("s-1");
        let mut s = reduced(&[
            Event::UserPromptSent {
                prompt_id: None,
                text: "go".into(),
                attachments: vec![],
            },
            Event::PromptCapabilities {
                steering: true,
                image: false,
                audio: false,
                embedded_context: false,
                load_session: None,
            },
            Event::CancelRequested {
                escalates_at: Utc::now(),
            },
            Event::ModesAvailable {
                current_mode_id: "plan".into(),
                modes: vec![ModeInfo {
                    id: "plan".into(),
                    name: "Plan".into(),
                    description: None,
                }],
            },
            Event::AvailableCommandsUpdated {
                commands: vec![AvailableCommand {
                    name: "review".into(),
                    description: "Review".into(),
                    accepts_input: true,
                }],
            },
            Event::PlanUpdated {
                plan: Plan {
                    plan_id: "p-1".into(),
                    version: 1,
                    steps: vec![PlanStep {
                        id: "s-1".into(),
                        title: "Step one".into(),
                        detail: None,
                        status: PlanStepStatus::InProgress,
                    }],
                },
            },
            Event::UsageUpdated {
                usage: SessionUsage {
                    used: 5_000,
                    size: 200_000,
                    cost: None,
                },
            },
        ]);
        s.pending_approvals = vec![approval("n-1")];
        s.pending_elicitations = vec![Elicitation {
            nonce: Nonce("n-2".into()),
            message: "Which one?".into(),
            title: None,
            description: None,
            tool_call_id: None,
            questions: Vec::new(),
            requested_at: Utc::now(),
            resolved: None,
        }];

        t.apply_reduced_state(9, s, &[]);

        assert_eq!(t.last_seq, 9);
        assert_eq!(t.agent_name.as_deref(), Some("claude"));
        assert!(t.turn_active && t.steering && t.cancelling);
        assert!(!t.compacting);
        assert_eq!(t.usage.as_ref().map(|u| u.used), Some(5_000));
        assert_eq!(t.current_mode.as_deref(), Some("plan"));
        assert_eq!(t.available_modes.len(), 1);
        assert_eq!(t.available_commands.len(), 1);
        assert_eq!(t.current_plan.len(), 1);
        assert_eq!(t.current_plan[0].title, "Step one");
        // Approvals carry the whole request: the shelf renders from these
        // fields, since the transcript holds no approval row.
        assert_eq!(t.pending_approvals.len(), 1);
        assert_eq!(t.pending_approvals[0].nonce, "n-1");
        assert_eq!(t.pending_approvals[0].title, "Edit a file");
        assert_eq!(t.pending_approvals[0].kind, "execute");
        assert_eq!(t.pending_approvals[0].args, "ls");
        assert!(t.pending_approvals[0].destructive);
        assert_eq!(t.pending_elicitations.len(), 1);
        assert_eq!(t.pending_elicitations[0].message, "Which one?");
    }

    /// The banner only shows inside a running turn, so the phases that end
    /// one never reach it; compaction outranks thinking because the adapter
    /// goes silent for minutes there.
    #[test]
    fn status_banner_follows_the_server_phase() {
        let cases: [(&str, Option<ThinkingSignal>, bool, Option<&str>); 4] = [
            ("idle", None, false, None),
            (
                "thinking",
                Some(ThinkingSignal {
                    started_at: Utc::now(),
                }),
                false,
                Some("thinking…"),
            ),
            ("compacting", None, true, Some("compacting…")),
            (
                "compacting outranks thinking",
                Some(ThinkingSignal {
                    started_at: Utc::now(),
                }),
                true,
                Some("compacting…"),
            ),
        ];
        for (label, thinking, compacting, expected) in cases {
            let mut t = AcpTranscript::new("s-1");
            let mut s = reduced(&[]);
            s.thinking = thinking;
            s.compacting = compacting;
            t.apply_reduced_state(1, s, &[]);
            assert_eq!(t.status_text.as_deref(), expected, "{label}");
        }
    }

    /// The server omits cold fields this connection already holds (a ~30 KB
    /// command list re-sent after every event dominated the socket). They
    /// arrive as empty defaults, so adopting them blindly would blank the
    /// slash and mode pickers mid-session.
    #[test]
    fn omitted_cold_fields_keep_their_current_value() {
        let mut t = AcpTranscript::new("s-1");
        let full = reduced(&[
            Event::AvailableCommandsUpdated {
                commands: vec![AvailableCommand {
                    name: "review".into(),
                    description: "Review".into(),
                    accepts_input: false,
                }],
            },
            Event::ModesAvailable {
                current_mode_id: "plan".into(),
                modes: vec![ModeInfo {
                    id: "plan".into(),
                    name: "Plan".into(),
                    description: None,
                }],
            },
        ]);
        t.apply_reduced_state(1, full, &[]);
        assert_eq!(t.available_commands.len(), 1);
        assert_eq!(t.available_modes.len(), 1);

        // The next frame omits both, naming them as unchanged.
        t.apply_reduced_state(
            2,
            reduced(&[]),
            &[
                "available_commands".to_string(),
                "available_modes".to_string(),
            ],
        );
        assert_eq!(t.available_commands.len(), 1, "commands survived");
        assert_eq!(t.available_modes.len(), 1, "modes survived");

        // A frame that does NOT name them is authoritative, including empty.
        t.apply_reduced_state(3, reduced(&[]), &[]);
        assert!(t.available_commands.is_empty());
        assert!(t.available_modes.is_empty());
    }

    /// A snapshot that races live deltas must not rewind the view.
    #[test]
    fn stale_reduced_state_frame_is_dropped() {
        let mut t = AcpTranscript::new("s-1");
        let mut live = reduced(&[]);
        live.turn_active = true;
        t.apply_reduced_state(7, live, &[]);
        assert!(t.turn_active);

        t.apply_reduced_state(3, reduced(&[]), &[]);
        assert!(t.turn_active, "an older frame cannot clear a live turn");
        assert_eq!(t.last_seq, 7);

        // The same seq is applied: the connect snapshot lands on the seq the
        // socket dialled from, and it is the authority there.
        t.apply_reduced_state(7, reduced(&[]), &[]);
        assert!(!t.turn_active);
    }

    /// The shelf clears on the resolve POST's 204/404 rather than waiting for
    /// the broadcast (#1821). Without the optimistic filter the next event's
    /// reduced state would paint the answered card straight back.
    #[test]
    fn locally_resolved_card_stays_hidden_until_the_server_agrees() {
        let mut t = AcpTranscript::new("s-1");
        let mut with_approval = reduced(&[]);
        with_approval.pending_approvals = vec![approval("n-1")];
        t.apply_reduced_state(1, with_approval.clone(), &[]);
        assert_eq!(t.pending_approvals.len(), 1);

        t.resolve_approval_locally("n-1");
        assert!(t.pending_approvals.is_empty());

        // An unrelated event still lists the approval: the daemon has not
        // processed the resolve yet.
        t.apply_reduced_state(2, with_approval, &[]);
        assert!(
            t.pending_approvals.is_empty(),
            "a locally-resolved card must not come back"
        );

        // Once the daemon drops it, the local memory of it goes too, so a
        // later approval reusing the nonce is not swallowed.
        t.apply_reduced_state(3, reduced(&[]), &[]);
        assert!(t.pending_approvals.is_empty());
        let mut again = reduced(&[]);
        again.pending_approvals = vec![approval("n-1")];
        t.apply_reduced_state(4, again, &[]);
        assert_eq!(t.pending_approvals.len(), 1, "the filter must not latch");
    }

    /// Derived from the rows, so it survives a reconnect with no client-side
    /// latch: a reset with no prompt after it means the context is gone.
    #[test]
    fn context_primer_pending_tracks_the_newest_reset_or_prompt() {
        let prompt = || Event::UserPromptSent {
            prompt_id: None,
            text: "go".into(),
            attachments: vec![],
        };
        let reset = || Event::SessionContextReset {
            reason: "worker restarted".into(),
        };
        let cases: [(&str, Vec<Event>, bool); 4] = [
            ("empty", vec![], false),
            ("reset with nothing after", vec![prompt(), reset()], true),
            ("prompt re-primed it", vec![reset(), prompt()], false),
            (
                "the newest reset wins",
                vec![reset(), prompt(), reset()],
                true,
            ),
        ];
        for (label, events, expected) in cases {
            let mut t = AcpTranscript::new("s-1");
            t.merge_server_rows(server_rows(&events));
            assert_eq!(t.context_primer_pending(), expected, "{label}");
        }
    }

    #[test]
    fn set_lagged_flags_without_touching_rows() {
        let mut t = AcpTranscript::new("s-1");
        t.set_lagged();
        assert!(t.lagged);
        assert!(t.server_rows.is_empty());
    }

    /// A lag rebuilds the rows but leaves control state alone: every frame is
    /// a whole-state snapshot, so a gap in the event stream cannot stale it.
    #[test]
    fn drop_rows_clears_the_buffer_and_keeps_control_state() {
        let mut t = AcpTranscript::new("s-1");
        let mut s = reduced(&[]);
        s.turn_active = true;
        s.pending_approvals = vec![approval("n-1")];
        t.apply_reduced_state(4, s, &[]);
        t.merge_server_rows(server_rows(&[Event::AgentMessageChunk {
            text: "hi".into(),
        }]));
        assert!(!t.server_rows.is_empty());

        t.drop_rows();
        assert!(t.server_rows.is_empty());
        assert!(t.turn_active, "a lag does not end the running turn");
        assert_eq!(t.pending_approvals.len(), 1, "the shelf survives a lag");
        assert_eq!(t.last_seq, 4, "the reconnect cursor survives a lag");
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
