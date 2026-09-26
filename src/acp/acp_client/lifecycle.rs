//! Classifies agent updates into the turn-level facts the watchdogs act on.

use crate::acp::agent_profiles;
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::{
    ContentBlock, SessionUpdate, ToolCallContent, ToolCallStatus,
};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use tokio::sync::mpsc;
use tracing::trace;

use super::raw_input::wakeup_event_from_raw;
use super::update_events::{is_compact_completion, is_compact_failure, is_compact_start};

#[derive(Debug, Clone)]
pub(crate) enum LifecycleSignal {
    /// Transcript progress that resets the silent-orphan timer.
    Progress,
    /// A tool call started or moved to `InProgress`. `is_background_task`
    /// carries the SDK `run_in_background` flag from `raw_input`.
    ToolStarted {
        id: String,
        is_background_task: bool,
    },
    /// A tool call reached `Completed` or `Failed`. `off_protocol_work` is
    /// only detected on successful completions.
    ToolCompleted {
        id: String,
        succeeded: bool,
        off_protocol_work: Option<OffProtocolWorkKind>,
    },
    /// Cost-populated `UsageUpdate`, the adapter's end-of-turn accounting
    /// marker. Not progress.
    TerminalUsage,
    /// `ScheduleWakeup` registered an absolute wake time.
    WakeupPending {
        at: chrono::DateTime<chrono::Utc>,
    },
    CompactionStarted,
    CompactionCompleted,
    /// The adapter emits this after the turn's own terminal on a cancel, so
    /// it must never read as the start of new work.
    CompactionFailed,
}

/// `None` for ambient updates (mode, commands, usage without cost) that must
/// not reset the watchdog timer.
pub(super) fn classify_lifecycle_signal(update: &SessionUpdate) -> Option<LifecycleSignal> {
    match update {
        SessionUpdate::UsageUpdate(u) if u.cost.is_some() => Some(LifecycleSignal::TerminalUsage),
        SessionUpdate::AgentMessageChunk(chunk) => {
            // Completion is checked before start so marker drift cannot
            // misroute an end as a fresh start.
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
        SessionUpdate::ToolCall(tc) => Some(LifecycleSignal::ToolStarted {
            id: tc.tool_call_id.0.to_string(),
            is_background_task: tc
                .raw_input
                .as_ref()
                .and_then(|v| v.get("run_in_background"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        }),
        SessionUpdate::ToolCallUpdate(update) => {
            let id = update.tool_call_id.0.to_string();
            Some(match update.fields.status {
                // Failed content can echo a marker, so only completions count.
                Some(ToolCallStatus::Completed) => LifecycleSignal::ToolCompleted {
                    id,
                    succeeded: true,
                    off_protocol_work: detect_off_protocol_work_completed(&update.fields.content),
                },
                Some(ToolCallStatus::Failed) => LifecycleSignal::ToolCompleted {
                    id,
                    succeeded: false,
                    off_protocol_work: None,
                },
                // `InProgress` never carries `raw_input`; the watchdog ORs
                // the flag with the original `ToolCall`'s.
                Some(ToolCallStatus::InProgress) => LifecycleSignal::ToolStarted {
                    id,
                    is_background_task: false,
                },
                _ => LifecycleSignal::Progress,
            })
        }
        _ => None,
    }
}

/// Work the agent keeps doing after its visible tool call completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffProtocolWorkKind {
    /// SDK `Agent` tool with `isAsync: true`.
    AsyncAgent,
    /// SDK `Bash` with `run_in_background: true`.
    BackgroundCommand,
    /// SDK `ScheduleWakeup`; survives `TerminalUsage` because a wake outlasts
    /// the turn's accounting frame.
    ScheduledWakeup,
    /// `/compact`; bounded by the turn and dropped on `TerminalUsage`.
    Compaction,
}

/// Ensures exactly one path publishes each turn's terminal event. Epochs allow
/// a later turn to be claimed without letting an older path clear its state.
pub(crate) struct TerminalClaim {
    epoch: AtomicU64,
    /// Epoch whose terminal was published; `0` means none.
    claimed_for: AtomicU64,
}

impl TerminalClaim {
    pub(crate) fn new() -> Self {
        Self {
            epoch: AtomicU64::new(1),
            claimed_for: AtomicU64::new(0),
        }
    }

    pub(super) fn begin_turn(&self) {
        self.epoch.fetch_add(1, AtomicOrdering::AcqRel);
    }

    /// `false` when another path already published this turn's terminal.
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

    pub(super) fn claimed(&self) -> bool {
        self.claimed_for.load(AtomicOrdering::Acquire) == self.epoch.load(AtomicOrdering::Acquire)
    }
}

/// A signal tagged with the prompt epoch current when its notification
/// arrived, so the prompt loop can discard signals from an earlier prompt.
#[derive(Debug, Clone)]
pub(crate) struct LifecycleEnvelope {
    pub epoch: u64,
    pub signal: LifecycleSignal,
}

/// Forward signals only while their prompt-loop consumer is active. Between
/// prompts nothing drains the channel, so an awaited send would block every
/// later notification.
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
        // Signals are never dropped for backpressure; `send` only fails once
        // the prompt loop is gone.
        if tx.send(LifecycleEnvelope { epoch, signal }).await.is_err() {
            trace!(
                target: "acp.protocol",
                session = session_label,
                "lifecycle channel closed; dropping signal"
            );
        }
    }
}

/// Matches SDK markers only at line starts so echoed output does not extend
/// watchdog grace.
pub(super) fn detect_off_protocol_work_completed(
    content: &Option<Vec<ToolCallContent>>,
) -> Option<OffProtocolWorkKind> {
    content
        .iter()
        .flatten()
        .filter_map(|block| match block {
            ToolCallContent::Content(c) => match &c.content {
                ContentBlock::Text(t) => Some(t.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .flat_map(str::lines)
        .find_map(|line| {
            let line = line.trim_start();
            if line.starts_with("Async agent launched successfully") {
                Some(OffProtocolWorkKind::AsyncAgent)
            } else if line.starts_with("Command running in background with ID: ") {
                Some(OffProtocolWorkKind::BackgroundCommand)
            } else {
                None
            }
        })
}

/// Wake signal from a non-failed `ScheduleWakeup` tool update. The initial
/// `ToolCall` is excluded because the tool may still fail; `InProgress` is
/// accepted because the adapter strips `raw_input` from `Completed`.
pub(super) fn wakeup_lifecycle_signal_from_update(
    update: &SessionUpdate,
    profile: &agent_profiles::AgentProfile,
) -> Option<LifecycleSignal> {
    if !profile.supports_wakeup_tools {
        return None;
    }
    let SessionUpdate::ToolCallUpdate(u) = update else {
        return None;
    };
    if matches!(u.fields.status, Some(ToolCallStatus::Failed))
        || u.fields.title.as_deref() != Some("ScheduleWakeup")
    {
        return None;
    }
    match wakeup_event_from_raw(u.fields.raw_input.as_ref()?)? {
        Event::WakeupScheduled { at, .. } => Some(LifecycleSignal::WakeupPending { at }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::text_chunk;
    use agent_client_protocol::schema::v1::{
        Content, ToolCall, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn tool_update(status: ToolCallStatus, text: &str) -> SessionUpdate {
        SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tc-1",
            ToolCallUpdateFields::new()
                .status(status)
                .content(vec![ToolCallContent::Content(Content::new(text))]),
        ))
    }

    #[tokio::test]
    async fn lifecycle_forwarding_only_while_prompt_active() {
        // #2888: between prompts a full channel must not block the handler.
        let (tx, _rx) = mpsc::channel::<LifecycleEnvelope>(1);
        tx.try_send(LifecycleEnvelope {
            epoch: 0,
            signal: LifecycleSignal::Progress,
        })
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for _ in 0..200 {
                forward_lifecycle_signals(
                    false,
                    &tx,
                    0,
                    Some(LifecycleSignal::Progress),
                    None,
                    "test",
                )
                .await;
            }
        })
        .await
        .expect("between-prompt signals must not block on a full lifecycle channel");

        let (tx, mut rx) = mpsc::channel::<LifecycleEnvelope>(8);
        let wake = LifecycleSignal::WakeupPending {
            at: chrono::Utc::now(),
        };
        forward_lifecycle_signals(
            true,
            &tx,
            7,
            Some(LifecycleSignal::Progress),
            Some(wake),
            "t",
        )
        .await;
        let first = rx.try_recv().unwrap();
        assert_eq!(first.epoch, 7);
        assert!(matches!(first.signal, LifecycleSignal::Progress));
        let second = rx.try_recv().unwrap();
        assert!(matches!(
            second.signal,
            LifecycleSignal::WakeupPending { .. }
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn classify_lifecycle_signal_cases() {
        let cases = [
            ("Compacting...", "CompactionStarted"),
            ("Compacting completed.", "CompactionCompleted"),
            ("regular assistant output", "Progress"),
            (
                "\n\nCompacting failed: API Error: Request was aborted.",
                "CompactionFailed",
            ),
            (
                "\n\nCompacting failed: Not enough messages to compact.",
                "CompactionFailed",
            ),
            ("\n\nCompacting failed.", "CompactionFailed"),
            ("the compaction failed earlier", "Progress"),
        ];
        for (text, expected) in cases {
            let sig = classify_lifecycle_signal(&text_chunk(text, Some("m"))).unwrap();
            assert_eq!(format!("{sig:?}"), expected, "{text:?}");
        }

        let tool_cases = [
            (
                ToolCallStatus::Completed,
                "Async agent launched successfully. agentId: a1",
                true,
                Some(OffProtocolWorkKind::AsyncAgent),
            ),
            (
                ToolCallStatus::Completed,
                "Command running in background with ID: bg. Output: /tmp/x",
                true,
                Some(OffProtocolWorkKind::BackgroundCommand),
            ),
            (ToolCallStatus::Completed, "ls /tmp/foo done", true, None),
            (
                ToolCallStatus::Failed,
                "Async agent launched successfully. agentId: not-real",
                false,
                None,
            ),
        ];
        for (status, text, want_ok, want_work) in tool_cases {
            match classify_lifecycle_signal(&tool_update(status, text)) {
                Some(LifecycleSignal::ToolCompleted {
                    id,
                    succeeded,
                    off_protocol_work,
                }) => {
                    assert_eq!(id, "tc-1");
                    assert_eq!(succeeded, want_ok, "{text}");
                    assert_eq!(off_protocol_work, want_work, "{text}");
                }
                other => panic!("expected ToolCompleted, got {other:?}"),
            }
        }

        for (input, expected) in [
            (
                serde_json::json!({ "command": "npm i", "run_in_background": true }),
                true,
            ),
            (serde_json::json!({ "command": "ls" }), false),
        ] {
            let mut tc = ToolCall::new("tc-bg", "Bash");
            tc.raw_input = Some(input);
            match classify_lifecycle_signal(&SessionUpdate::ToolCall(tc)) {
                Some(LifecycleSignal::ToolStarted {
                    id,
                    is_background_task,
                }) => {
                    assert_eq!(id, "tc-bg");
                    assert_eq!(is_background_task, expected);
                }
                other => panic!("expected ToolStarted, got {other:?}"),
            }
        }

        {
            let wake = |status| {
                SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    "tc-wake",
                    ToolCallUpdateFields::new()
                        .status(status)
                        .title("ScheduleWakeup".to_string())
                        .raw_input(serde_json::json!({ "delaySeconds": 60 })),
                ))
            };
            for (status, expected) in [
                (ToolCallStatus::Completed, true),
                (ToolCallStatus::InProgress, true),
                (ToolCallStatus::Failed, false),
            ] {
                let sig =
                    wakeup_lifecycle_signal_from_update(&wake(status), &agent_profiles::CLAUDE);
                assert_eq!(
                    matches!(sig, Some(LifecycleSignal::WakeupPending { .. })),
                    expected,
                    "{status:?}"
                );
            }
            let mut tc = ToolCall::new("tc-wake-2", "ScheduleWakeup");
            tc.raw_input = Some(serde_json::json!({ "delaySeconds": 60 }));
            assert!(wakeup_lifecycle_signal_from_update(
                &SessionUpdate::ToolCall(tc),
                &agent_profiles::CLAUDE
            )
            .is_none());
        }
    }

    #[test]
    fn detect_off_protocol_work_matches_line_prefixes_only() {
        let cases: [(Option<&str>, Option<OffProtocolWorkKind>); 7] = [
            (
                Some("Async agent launched successfully.\nagentId: af2a (internal ID)"),
                Some(OffProtocolWorkKind::AsyncAgent),
            ),
            (
                Some("Command running in background with ID: bgx. Output: /tmp/x"),
                Some(OffProtocolWorkKind::BackgroundCommand),
            ),
            (
                Some("\n  Command running in background with ID: btest."),
                Some(OffProtocolWorkKind::BackgroundCommand),
            ),
            (Some("abc1234 first commit\nabc1235 second"), None),
            (
                Some("user typed: Command running in background with ID: x"),
                None,
            ),
            (
                Some("log line: Async agent launched successfully but not"),
                None,
            ),
            (None, None),
        ];
        for (text, expected) in cases {
            let content = text.map(|t| vec![ToolCallContent::Content(Content::new(t))]);
            assert_eq!(
                detect_off_protocol_work_completed(&content),
                expected,
                "{text:?}"
            );
        }
        assert!(detect_off_protocol_work_completed(&Some(vec![])).is_none());
    }

    /// #3190: a per-connection claim left a second turn with no terminal.
    #[test]
    fn terminal_claim_is_per_turn_not_per_connection() {
        let claim = TerminalClaim::new();
        assert!(!claim.claimed());
        assert!(claim.claim());
        assert!(claim.claimed());
        assert!(!claim.claim());
        claim.begin_turn();
        assert!(!claim.claimed());
        assert!(claim.claim());
        assert!(!claim.claim());
        claim.begin_turn();
        assert!(claim.claim());
    }
}
