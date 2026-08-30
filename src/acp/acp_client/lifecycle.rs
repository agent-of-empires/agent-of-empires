//! Lifecycle signals: classifying agent updates into the turn-level facts
//! the watchdogs and the connection task act on.

use crate::acp::agent_profiles;
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::ContentBlock;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use tokio::sync::mpsc;
use tracing::trace;

use super::raw_input::wakeup_event_from_raw;
use super::update_events::{is_compact_completion, is_compact_failure, is_compact_start};

/// Classification of an inbound ACP `SessionUpdate` for the silent-
/// orphan watchdog state machine. Sent from the notification handler
/// to the prompt loop via a dedicated mpsc; the prompt loop owns the
/// `Instant`-based timers and the tool-id map, so the handler doesn't
/// need to touch shared atomics on the hot path.
#[derive(Debug, Clone)]
pub(crate) enum LifecycleSignal {
    /// Transcript-producing event that resets the silent-orphan timer:
    /// `AgentMessageChunk`, `AgentThoughtChunk`, `Plan`, or a
    /// non-terminal `ToolCallUpdate` other than `InProgress`.
    Progress,
    /// A new tool call has started (`SessionUpdate::ToolCall`) or
    /// transitioned to `InProgress` (`ToolCallUpdate`). Added to the
    /// prompt-loop's `tool_calls_in_flight` map; while non-empty, the
    /// watchdog stays suppressed so long-running tools (npm install,
    /// Playwright runs, Task subagents) never false-positive.
    /// `is_background_task` carries the Claude SDK `run_in_background`
    /// flag from `raw_input` when available; the prompt loop uses it to
    /// flip `off_protocol_work_seen` at completion time even if the
    /// completion content marker is stripped or reshaped. See #1401.
    ToolStarted {
        id: String,
        is_background_task: bool,
    },
    /// A tool call reached terminal status (`Completed` or `Failed`).
    /// Removed from `tool_calls_in_flight`; when the map drains to
    /// empty after at least one progress event, the watchdog arms.
    /// `off_protocol_work` is `Some(_)` when the completion content
    /// text carries one of the Claude SDK markers detected by
    /// `detect_off_protocol_work_completed`. The matching `ToolStarted`'s
    /// `is_background_task` flag is only honored when `succeeded == true`
    /// (the prompt loop branches on this in `apply_signal`); a failed
    /// background launch must not pin the watchdog open for 30 minutes.
    /// See #1360, #1401, and upstream
    /// `agentclientprotocol/claude-agent-acp#336`.
    ToolCompleted {
        id: String,
        succeeded: bool,
        off_protocol_work: Option<OffProtocolWorkKind>,
    },
    /// Cost-populated `UsageUpdate`: claude-agent-acp's "wrap up
    /// accounting" marker. Switches the effective grace from the
    /// vendor-agnostic default to the accelerated value for this
    /// prompt only. Does NOT count as progress (it's accounting
    /// telemetry, not lifecycle), so the silent-orphan timer keeps
    /// running from the previous progress event.
    TerminalUsage,
    /// The Claude SDK `ScheduleWakeup` tool registered an absolute wake
    /// timestamp. Suppresses the watchdog until `at + base_grace`,
    /// converted to a monotonic `Instant` deadline at signal receipt so
    /// wall-clock jumps don't perturb the suppression. After the
    /// deadline the watchdog rearms with its normal grace. See #1401.
    WakeupPending { at: chrono::DateTime<chrono::Utc> },
    /// The `/compact` cycle started ("Compacting..." text chunk). Latches
    /// `OffProtocolWorkKind::Compaction` so the silent summarization window
    /// keeps the off-protocol grace floor instead of the base grace, which
    /// otherwise cancels a large compaction after 120s. See #2898.
    CompactionStarted,
    /// The `/compact` cycle finished ("Compacting completed." text chunk).
    /// Clears the compaction suppression so any continued work in the same
    /// turn recovers on the normal grace. See #2898.
    CompactionCompleted,
    /// The `/compact` cycle ended without replacing the context
    /// ("Compacting failed..." text chunk): the user cancelled it, the API
    /// call errored, or there was too little to summarize. Clears the
    /// compaction suppression exactly like `CompactionCompleted`; the two
    /// are distinct only so the failure is readable in the logs.
    ///
    /// The adapter emits this marker AFTER the turn's own terminal on a
    /// cancel, so classifying it as ordinary `Progress` made the
    /// between-prompt watchdog read it as the agent starting a
    /// self-initiated turn, leaving the session Running for the full
    /// stall grace with nothing actually running. See #3219 follow-up.
    CompactionFailed,
}

/// Classify a `SessionUpdate` into a `LifecycleSignal`, or `None` for
/// ambient state (mode changes, available_commands, raw metadata,
/// usage-without-cost) that shouldn't influence the silent-orphan
/// watchdog timer. Out-of-band notifications must NOT reset the timer:
/// claude-agent-acp can interleave mode and command refreshes mid-turn
/// or after final accounting, and treating those as progress would
/// mask the exact wedge the watchdog is designed to detect. See #1240.
pub(super) fn classify_lifecycle_signal(
    update: &agent_client_protocol::schema::v1::SessionUpdate,
) -> Option<LifecycleSignal> {
    use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, ToolCallStatus};
    match update {
        SessionUpdate::UsageUpdate(u) if u.cost.is_some() => Some(LifecycleSignal::TerminalUsage),
        SessionUpdate::AgentMessageChunk(chunk) => {
            // /compact surfaces only as text chunks; detect its start/end
            // markers so the watchdog suppresses the silent summarization
            // window instead of cancelling it. Check completion before start
            // so a future marker-wording drift can't misroute the end as a
            // fresh start. Everything else is ordinary progress. See #2898.
            if let ContentBlock::Text(t) = &chunk.content {
                if is_compact_completion(&t.text) {
                    return Some(LifecycleSignal::CompactionCompleted);
                }
                if is_compact_failure(&t.text) {
                    return Some(LifecycleSignal::CompactionFailed);
                }
                if is_compact_start(&t.text) {
                    return Some(LifecycleSignal::CompactionStarted);
                }
            }
            Some(LifecycleSignal::Progress)
        }
        SessionUpdate::AgentThoughtChunk(_) | SessionUpdate::Plan(_) => {
            Some(LifecycleSignal::Progress)
        }
        SessionUpdate::ToolCall(tc) => {
            let is_background_task = tc
                .raw_input
                .as_ref()
                .and_then(|v| v.as_object())
                .and_then(|obj| obj.get("run_in_background"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some(LifecycleSignal::ToolStarted {
                id: tc.tool_call_id.0.to_string(),
                is_background_task,
            })
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            match update.fields.status {
                Some(ToolCallStatus::Completed) => {
                    // Only successful completions can mark a real off-protocol
                    // work launch. A `Failed` update may carry arbitrary error
                    // content that happens to mention one of the SDK markers
                    // (e.g. as part of a stack trace or echoed input), and
                    // treating that as off-protocol work would pin the watchdog
                    // suppression for 30 minutes even though no background work
                    // is actually running. See CodeRabbit feedback on PR #1364.
                    let off_protocol_work =
                        detect_off_protocol_work_completed(&update.fields.content);
                    Some(LifecycleSignal::ToolCompleted {
                        id,
                        succeeded: true,
                        off_protocol_work,
                    })
                }
                Some(ToolCallStatus::Failed) => Some(LifecycleSignal::ToolCompleted {
                    id,
                    succeeded: false,
                    off_protocol_work: None,
                }),
                Some(ToolCallStatus::InProgress) => Some(LifecycleSignal::ToolStarted {
                    id,
                    // `InProgress` updates never carry the original
                    // `raw_input` so we cannot re-derive the flag here.
                    // `apply_signal` ORs this with any existing
                    // metadata so a later `InProgress` cannot
                    // overwrite a `true` from the original `ToolCall`.
                    is_background_task: false,
                }),
                _ => Some(LifecycleSignal::Progress),
            }
        }
        _ => None,
    }
}

/// Kind of off-protocol work the daemon has observed during the current
/// prompt. Both variants flip the silent-orphan watchdog to its
/// `OFF_PROTOCOL_WORK_GRACE_FLOOR` window so a legitimately quiet turn
/// doesn't get cancelled. See #1360 (`AsyncAgent`) and #1401
/// (`BackgroundCommand`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffProtocolWorkKind {
    /// Claude SDK `Agent` tool with `isAsync: true`. Sub-agent runs inside
    /// the claude binary, polled via an internal SDK channel that is
    /// invisible at the ACP layer.
    AsyncAgent,
    /// Claude SDK `Bash` tool with `run_in_background: true`. The visible
    /// `ToolCall` completes immediately while the underlying subprocess
    /// keeps running off-protocol; the agent polls later via `BashOutput`.
    BackgroundCommand,
    /// Claude SDK `ScheduleWakeup` tool: the agent deliberately parks the
    /// turn until a future wake time (a monitor or `/loop` run). The turn
    /// stays in-flight while the agent is intentionally idle, so the
    /// silent-orphan watchdog must not treat the quiet window as a wedge.
    /// Like `AsyncAgent` (and unlike `BackgroundCommand`) this survives a
    /// `TerminalUsage` marker, because a scheduled wake legitimately
    /// outlasts the turn's final accounting frame. See #1360, #1401, and
    /// the monitor-killed-by-watchdog regression.
    ScheduledWakeup,
    /// The Claude adapter's `/compact` command. It hides a long context
    /// summarization API call behind a single "Compacting..." text chunk
    /// with no further ACP progress until "Compacting completed.". Bounded
    /// by the turn: dropped on `TerminalUsage` (like `BackgroundCommand`)
    /// and on the explicit completion marker, so a lost `PromptResponse`
    /// still self-heals on the fast grace rather than holding the 30-minute
    /// floor. See #2898.
    Compaction,
}

/// Ensures exactly one path publishes each turn's terminal event. Epochs allow
/// a later turn to be claimed without letting an older path clear its state.
pub(crate) struct TerminalClaim {
    /// Incremented for each turn that begins on this connection.
    epoch: AtomicU64,
    /// Epoch whose terminal has already been published. `0` means none, and
    /// `epoch` starts at 1, so the first turn is claimable.
    claimed_for: AtomicU64,
}

impl TerminalClaim {
    pub(crate) fn new() -> Self {
        Self {
            epoch: AtomicU64::new(1),
            claimed_for: AtomicU64::new(0),
        }
    }

    /// A turn is starting; its terminal is unclaimed.
    pub(super) fn begin_turn(&self) {
        self.epoch.fetch_add(1, AtomicOrdering::AcqRel);
    }

    /// Take ownership of the current turn's terminal event. `false` when some
    /// other path already published it for this same turn, in which case the
    /// caller must not emit.
    pub(super) fn claim(&self) -> bool {
        let epoch = self.epoch.load(AtomicOrdering::Acquire);
        loop {
            let claimed = self.claimed_for.load(AtomicOrdering::Acquire);
            if claimed == epoch {
                return false;
            }
            if self
                .claimed_for
                .compare_exchange(
                    claimed,
                    epoch,
                    AtomicOrdering::AcqRel,
                    AtomicOrdering::Acquire,
                )
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Whether the current turn's terminal has already been published.
    pub(super) fn claimed(&self) -> bool {
        self.claimed_for.load(AtomicOrdering::Acquire) == self.epoch.load(AtomicOrdering::Acquire)
    }
}

/// Tagged lifecycle signal carried over the watchdog mpsc. The
/// `epoch` field is captured at signal-construction time from the
/// shared `current_prompt_epoch` atomic; the prompt loop discards
/// envelopes whose epoch doesn't match the prompt currently being
/// drained. This keeps a notification handler parked on a full
/// channel from leaking its previous-prompt signal into the next
/// prompt's watchdog state when it eventually unblocks. See #1401
/// post-impl review.
#[derive(Debug, Clone)]
pub(crate) struct LifecycleEnvelope {
    pub epoch: u64,
    pub signal: LifecycleSignal,
}

/// Deliver without dropping signals that affect watchdog correctness. Progress
/// tries the nonblocking path first because it arrives in bursts.
pub(super) async fn send_lifecycle_signal(
    tx: &mpsc::Sender<LifecycleEnvelope>,
    env: LifecycleEnvelope,
    session_label: &str,
) {
    match &env.signal {
        LifecycleSignal::Progress => {
            let env = match tx.try_send(env) {
                Ok(()) => return,
                Err(mpsc::error::TrySendError::Full(env)) => env,
                Err(mpsc::error::TrySendError::Closed(_)) => return,
            };
            if tx.send(env).await.is_err() {
                trace!(
                    target: "acp.protocol",
                    session = session_label,
                    "lifecycle channel closed; dropping Progress fallback"
                );
            }
        }
        _ => {
            if tx.send(env).await.is_err() {
                trace!(
                    target: "acp.protocol",
                    session = session_label,
                    "lifecycle channel closed; dropping load-bearing signal"
                );
            }
        }
    }
}

/// Forward signals only while their prompt-loop consumer is active. Between
/// prompts they would fill the channel and block all later notifications; the
/// separate idle watchdog reads atomics instead.
pub(super) async fn forward_lifecycle_signals(
    prompt_active: bool,
    tx: &mpsc::Sender<LifecycleEnvelope>,
    epoch: u64,
    lifecycle: Option<LifecycleSignal>,
    wakeup: Option<LifecycleSignal>,
    session_label: &str,
) {
    if !prompt_active {
        return;
    }
    for signal in [lifecycle, wakeup].into_iter().flatten() {
        send_lifecycle_signal(tx, LifecycleEnvelope { epoch, signal }, session_label).await;
    }
}

/// Detect SDK markers for async agents and background commands whose work
/// continues after the visible tool call. Match only line prefixes so ordinary
/// command output containing the marker does not extend watchdog grace.
pub(super) fn detect_off_protocol_work_completed(
    content: &Option<Vec<agent_client_protocol::schema::v1::ToolCallContent>>,
) -> Option<OffProtocolWorkKind> {
    use agent_client_protocol::schema::v1::ToolCallContent;
    let blocks = content.as_ref()?;
    for block in blocks {
        let ToolCallContent::Content(c) = block else {
            continue;
        };
        let ContentBlock::Text(t) = &c.content else {
            continue;
        };
        for line in t.text.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("Async agent launched successfully") {
                return Some(OffProtocolWorkKind::AsyncAgent);
            }
            if trimmed.starts_with("Command running in background with ID: ") {
                return Some(OffProtocolWorkKind::BackgroundCommand);
            }
        }
    }
    None
}

/// Derive a `LifecycleSignal::WakeupPending` from a `SessionUpdate`.
///
/// The watchdog must not suppress on every `Event::WakeupScheduled`:
/// the initial `ToolCall` frame is emitted eagerly before the tool is
/// known to have succeeded, and a later `Failed` status means no
/// wakeup was ever registered. Either case would let a real adapter
/// wedge masquerade as a pending wake and pin the prompt open for
/// `delay + base_grace`.
///
/// Gate: source must be a `ToolCallUpdate` whose status is NOT
/// `Failed` (Completed or InProgress are both acceptable; real
/// `claude-agent-acp` lands the raw_input on an interim
/// `ToolCallUpdate` before the final `Completed` arrives, and the
/// Completed update often strips `raw_input`, so requiring strictly
/// `Completed` would miss the wakeup in production). The title
/// must be `ScheduleWakeup` and `raw_input.delaySeconds` must
/// parse.
///
/// UI emit of `Event::WakeupScheduled` keeps its current best-effort
/// behavior so the sidebar countdown lights up immediately. See
/// CodeRabbit review on PR #1406.
pub(super) fn wakeup_lifecycle_signal_from_update(
    update: &agent_client_protocol::schema::v1::SessionUpdate,
    profile: &agent_profiles::AgentProfile,
) -> Option<LifecycleSignal> {
    use agent_client_protocol::schema::v1::{SessionUpdate, ToolCallStatus};
    if !profile.supports_wakeup_tools {
        return None;
    }
    let SessionUpdate::ToolCallUpdate(u) = update else {
        return None;
    };
    if matches!(u.fields.status, Some(ToolCallStatus::Failed)) {
        return None;
    }
    if u.fields.title.as_deref() != Some("ScheduleWakeup") {
        return None;
    }
    let raw = u.fields.raw_input.as_ref()?;
    match wakeup_event_from_raw(raw)? {
        Event::WakeupScheduled { at, .. } => Some(LifecycleSignal::WakeupPending { at }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::text_chunk;
    use agent_client_protocol::schema::v1::SessionUpdate;

    #[tokio::test]
    async fn between_prompt_signals_do_not_block_on_a_full_lifecycle_channel() {
        // Reproduces #2888: no prompt in flight means nothing drains the
        // lifecycle channel; once it is full, an unguarded awaited send
        // parks the notification handler forever and every notification
        // behind it queues invisibly until the next prompt. The guard must
        // skip the send entirely, so a between-prompt burst larger than
        // the channel capacity completes without blocking.
        let (tx, _rx) = mpsc::channel::<LifecycleEnvelope>(2);
        for i in 0..2 {
            tx.try_send(LifecycleEnvelope {
                epoch: 0,
                signal: LifecycleSignal::ToolStarted {
                    id: format!("fill-{i}"),
                    is_background_task: false,
                },
            })
            .expect("pre-fill fits the channel");
        }
        let burst = async {
            for i in 0..200 {
                forward_lifecycle_signals(
                    false,
                    &tx,
                    0,
                    Some(LifecycleSignal::ToolStarted {
                        id: i.to_string(),
                        is_background_task: false,
                    }),
                    None,
                    "test",
                )
                .await;
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), burst)
            .await
            .expect("between-prompt signals must not block on a full lifecycle channel");
    }

    #[tokio::test]
    async fn active_prompt_signals_still_reach_the_lifecycle_channel() {
        let (tx, mut rx) = mpsc::channel::<LifecycleEnvelope>(8);
        forward_lifecycle_signals(
            true,
            &tx,
            7,
            Some(LifecycleSignal::Progress),
            Some(LifecycleSignal::WakeupPending {
                at: chrono::Utc::now(),
            }),
            "test",
        )
        .await;
        let first = rx.try_recv().expect("lifecycle signal forwarded");
        assert_eq!(first.epoch, 7);
        assert!(matches!(first.signal, LifecycleSignal::Progress));
        let second = rx.try_recv().expect("wakeup signal forwarded");
        assert!(matches!(
            second.signal,
            LifecycleSignal::WakeupPending { .. }
        ));
        assert!(rx.try_recv().is_err(), "no extra envelopes");
    }

    #[test]
    fn classify_lifecycle_signal_routes_compaction_markers() {
        // "Compacting..." arms the compaction floor; "Compacting completed."
        // clears it; ordinary text stays plain progress. See #2898.
        assert!(matches!(
            classify_lifecycle_signal(&text_chunk("Compacting...", Some("m1"))),
            Some(LifecycleSignal::CompactionStarted)
        ));
        assert!(matches!(
            classify_lifecycle_signal(&text_chunk("Compacting completed.", Some("m2"))),
            Some(LifecycleSignal::CompactionCompleted)
        ));
        assert!(matches!(
            classify_lifecycle_signal(&text_chunk("regular assistant output", Some("m3"))),
            Some(LifecycleSignal::Progress)
        ));
        // Every shape of the failure marker the adapter interpolates a
        // reason into. Classifying these as Progress is what left a
        // cancelled compaction's session stuck Running.
        for text in [
            "\n\nCompacting failed: API Error: Request was aborted.",
            "\n\nCompacting failed: Not enough messages to compact.",
            "\n\nCompacting failed.",
        ] {
            assert!(
                matches!(
                    classify_lifecycle_signal(&text_chunk(text, Some("m4"))),
                    Some(LifecycleSignal::CompactionFailed)
                ),
                "{text:?}"
            );
        }
        // Prose that merely mentions compaction must stay plain progress.
        assert!(matches!(
            classify_lifecycle_signal(&text_chunk("the compaction failed earlier", Some("m5"))),
            Some(LifecycleSignal::Progress)
        ));
    }

    #[test]
    fn detect_off_protocol_work_completed_matches_async_agent_prefix() {
        use agent_client_protocol::schema::v1::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "Async agent launched successfully.\nagentId: af2a6a5d46bc21f91 (internal ID)",
        ))];
        assert_eq!(
            detect_off_protocol_work_completed(&Some(blocks)),
            Some(OffProtocolWorkKind::AsyncAgent)
        );
    }

    #[test]
    fn detect_off_protocol_work_completed_matches_background_command_prefix() {
        use agent_client_protocol::schema::v1::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "Command running in background with ID: bgxe33hwb. Output is being written to: /tmp/x",
        ))];
        assert_eq!(
            detect_off_protocol_work_completed(&Some(blocks)),
            Some(OffProtocolWorkKind::BackgroundCommand)
        );
    }

    #[test]
    fn detect_off_protocol_work_completed_none_on_regular_completion() {
        use agent_client_protocol::schema::v1::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "abc1234 first commit\nabc1235 second commit",
        ))];
        assert!(detect_off_protocol_work_completed(&Some(blocks)).is_none());
    }

    #[test]
    fn detect_off_protocol_work_completed_none_on_none_content() {
        assert!(detect_off_protocol_work_completed(&None).is_none());
    }

    #[test]
    fn detect_off_protocol_work_completed_none_on_empty_content() {
        assert!(detect_off_protocol_work_completed(&Some(vec![])).is_none());
    }

    #[test]
    fn detect_off_protocol_work_completed_ignores_echoed_marker_mid_line() {
        // CodeRabbit regression on PR #1406: a regular foreground Bash that
        // prints the SDK marker substring as part of its output (e.g.
        // an echo or grep that includes the phrase) must NOT trip
        // off-protocol suppression. Match anchors at the start of a
        // line, not anywhere in the content.
        use agent_client_protocol::schema::v1::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "user typed: Command running in background with ID: pretend\nbye",
        ))];
        assert!(detect_off_protocol_work_completed(&Some(blocks)).is_none());

        let blocks2 = vec![ToolCallContent::Content(Content::new(
            "log line: Async agent launched successfully but actually not",
        ))];
        assert!(detect_off_protocol_work_completed(&Some(blocks2)).is_none());
    }

    #[test]
    fn detect_off_protocol_work_completed_matches_marker_on_indented_line() {
        // The marker may not be the first character of the block;
        // a leading newline or whitespace must not break detection
        // as long as the marker starts the (trimmed) line.
        use agent_client_protocol::schema::v1::{Content, ToolCallContent};
        let blocks = vec![ToolCallContent::Content(Content::new(
            "\n  Command running in background with ID: btest. log: /tmp/x",
        ))];
        assert_eq!(
            detect_off_protocol_work_completed(&Some(blocks)),
            Some(OffProtocolWorkKind::BackgroundCommand)
        );
    }

    #[test]
    fn wakeup_lifecycle_signal_from_completed_tool_call_update() {
        use agent_client_protocol::schema::v1::{
            ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .title("ScheduleWakeup".to_string())
            .raw_input(serde_json::json!({ "delaySeconds": 60 }));
        let update = ToolCallUpdate::new("tc-wake-1", fields);
        let sig = wakeup_lifecycle_signal_from_update(
            &SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(matches!(sig, Some(LifecycleSignal::WakeupPending { .. })));
    }

    #[test]
    fn wakeup_lifecycle_signal_none_on_initial_tool_call() {
        // The initial `ToolCall` frame is NOT yet a completion; the
        // tool could still fail. Watchdog suppression must wait until
        // a successful ToolCallUpdate { Completed }. See CodeRabbit
        // review on PR #1406.
        use agent_client_protocol::schema::v1::ToolCall;
        let mut tc = ToolCall::new("tc-wake-2", "ScheduleWakeup");
        tc.raw_input = Some(serde_json::json!({ "delaySeconds": 60 }));
        let sig = wakeup_lifecycle_signal_from_update(
            &SessionUpdate::ToolCall(tc),
            &agent_profiles::CLAUDE,
        );
        assert!(sig.is_none());
    }

    #[test]
    fn wakeup_lifecycle_signal_none_on_failed_completion() {
        // A failed ScheduleWakeup means no wakeup was actually
        // registered; suppressing for `delay + base_grace` would
        // hide a real adapter wedge.
        use agent_client_protocol::schema::v1::{
            ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .title("ScheduleWakeup".to_string())
            .raw_input(serde_json::json!({ "delaySeconds": 60 }));
        let update = ToolCallUpdate::new("tc-wake-3", fields);
        let sig = wakeup_lifecycle_signal_from_update(
            &SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(sig.is_none());
    }

    #[test]
    fn wakeup_lifecycle_signal_fires_on_in_progress_with_raw_input() {
        // Real `claude-agent-acp` typically populates `raw_input` on an
        // interim `ToolCallUpdate { status: InProgress }` and strips
        // it from the final `Completed` frame. Requiring strictly
        // Completed status would lose the wakeup; we gate only on
        // not-Failed.
        use agent_client_protocol::schema::v1::{
            ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::InProgress)
            .title("ScheduleWakeup".to_string())
            .raw_input(serde_json::json!({ "delaySeconds": 60 }));
        let update = ToolCallUpdate::new("tc-wake-4", fields);
        let sig = wakeup_lifecycle_signal_from_update(
            &SessionUpdate::ToolCallUpdate(update),
            &agent_profiles::CLAUDE,
        );
        assert!(matches!(sig, Some(LifecycleSignal::WakeupPending { .. })));
    }

    #[test]
    fn classify_lifecycle_signal_marks_async_agent_completion() {
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "Async agent launched successfully. agentId: async-test-1",
            ))]);
        let update = ToolCallUpdate::new("tc-async-1", fields);
        match classify_lifecycle_signal(&SessionUpdate::ToolCallUpdate(update)) {
            Some(LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            }) => {
                assert_eq!(id, "tc-async-1");
                assert!(succeeded);
                assert_eq!(off_protocol_work, Some(OffProtocolWorkKind::AsyncAgent));
            }
            other => panic!(
                "expected ToolCompleted {{ off_protocol_work: Some(AsyncAgent) }}, got {other:?}"
            ),
        }
    }

    #[test]
    fn classify_lifecycle_signal_marks_background_command_completion() {
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "Command running in background with ID: bgtest. Output is being written to: /tmp/x",
            ))]);
        let update = ToolCallUpdate::new("tc-bg-1", fields);
        match classify_lifecycle_signal(&SessionUpdate::ToolCallUpdate(update)) {
            Some(LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            }) => {
                assert_eq!(id, "tc-bg-1");
                assert!(succeeded);
                assert_eq!(
                    off_protocol_work,
                    Some(OffProtocolWorkKind::BackgroundCommand)
                );
            }
            other => panic!(
                "expected ToolCompleted {{ off_protocol_work: Some(BackgroundCommand) }}, got {other:?}"
            ),
        }
    }

    #[test]
    fn classify_lifecycle_signal_clears_off_protocol_on_regular_completion() {
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Completed)
            .content(vec![ToolCallContent::Content(Content::new(
                "ls /tmp/foo done",
            ))]);
        let update = ToolCallUpdate::new("tc-bash-1", fields);
        match classify_lifecycle_signal(&SessionUpdate::ToolCallUpdate(update)) {
            Some(LifecycleSignal::ToolCompleted {
                id,
                succeeded,
                off_protocol_work,
            }) => {
                assert_eq!(id, "tc-bash-1");
                assert!(succeeded);
                assert!(off_protocol_work.is_none());
            }
            other => panic!("expected ToolCompleted {{ off_protocol_work: None }}, got {other:?}"),
        }
    }

    #[test]
    fn classify_lifecycle_signal_failed_ignores_off_protocol_marker() {
        use agent_client_protocol::schema::v1::{
            Content, ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields,
        };
        let fields = ToolCallUpdateFields::new()
            .status(ToolCallStatus::Failed)
            .content(vec![ToolCallContent::Content(Content::new(
                "Async agent launched successfully. agentId: not-real",
            ))]);
        let update = ToolCallUpdate::new("tc-failed-1", fields);
        match classify_lifecycle_signal(&SessionUpdate::ToolCallUpdate(update)) {
            Some(LifecycleSignal::ToolCompleted {
                succeeded,
                off_protocol_work,
                ..
            }) => {
                assert!(!succeeded, "Failed updates must mark succeeded=false");
                assert!(
                    off_protocol_work.is_none(),
                    "Failed updates must not activate off-protocol suppression"
                );
            }
            other => panic!("expected ToolCompleted, got {other:?}"),
        }
    }

    #[test]
    fn lifecycle_envelope_round_trips_epoch_and_signal() {
        // Smoke test that `LifecycleEnvelope` carries both fields as
        // expected: the prompt-loop discard path keys off `epoch`
        // mismatch, so a regression that loses or zeroes the field
        // would silently break cross-prompt stale-signal protection.
        let env = LifecycleEnvelope {
            epoch: 42,
            signal: LifecycleSignal::WakeupPending {
                at: chrono::Utc::now(),
            },
        };
        assert_eq!(env.epoch, 42);
        assert!(matches!(env.signal, LifecycleSignal::WakeupPending { .. }));
    }

    #[test]
    fn classify_lifecycle_signal_tool_call_carries_run_in_background_flag() {
        use agent_client_protocol::schema::v1::ToolCall;
        let mut tc = ToolCall::new("tc-bg-2", "Bash");
        tc.raw_input = Some(serde_json::json!({
            "command": "npm install",
            "run_in_background": true,
        }));
        match classify_lifecycle_signal(&SessionUpdate::ToolCall(tc)) {
            Some(LifecycleSignal::ToolStarted {
                id,
                is_background_task,
            }) => {
                assert_eq!(id, "tc-bg-2");
                assert!(
                    is_background_task,
                    "raw_input.run_in_background=true must propagate"
                );
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    #[test]
    fn classify_lifecycle_signal_tool_call_defaults_run_in_background_false() {
        use agent_client_protocol::schema::v1::ToolCall;
        let mut tc = ToolCall::new("tc-fg-1", "Bash");
        tc.raw_input = Some(serde_json::json!({ "command": "ls" }));
        match classify_lifecycle_signal(&SessionUpdate::ToolCall(tc)) {
            Some(LifecycleSignal::ToolStarted {
                is_background_task, ..
            }) => assert!(!is_background_task),
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    /// The one-shot guard this replaced could be claimed once per CONNECTION,
    /// so a second turn on the same connection found it taken and its
    /// watchdog skipped the emit, leaving the session rendering Running with
    /// no terminal event. Reachable by reattaching to an in-flight turn (the
    /// control reader claims for the adopted turn) and then letting the agent
    /// resume itself again. Each turn must own its own claim. See #3190 and
    /// PR #3192 review.
    #[test]
    fn terminal_claim_is_per_turn_not_per_connection() {
        let claim = TerminalClaim::new();

        // First turn: claimable exactly once.
        assert!(!claim.claimed());
        assert!(claim.claim(), "first turn's terminal is unclaimed");
        assert!(claim.claimed());
        assert!(
            !claim.claim(),
            "a second path must not double-publish the same turn's terminal"
        );

        // Second turn on the same connection: its own terminal.
        claim.begin_turn();
        assert!(
            !claim.claimed(),
            "a new turn must not inherit the previous turn's claim"
        );
        assert!(claim.claim(), "second turn's terminal is claimable");
        assert!(!claim.claim());

        // And it keeps working, so a long-lived connection cannot run out.
        claim.begin_turn();
        assert!(claim.claim());
    }
}
