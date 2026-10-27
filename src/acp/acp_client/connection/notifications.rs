//! Turn state shared between the notification handler and the command loop,
//! and the `session/update` handler itself.

use crate::acp::agent_profiles::AgentProfile;
use crate::acp::background_agent::{spawn_tailer, TailerStart, TranscriptSource};
use crate::acp::state::{BackgroundTaskLaunch, Event};
use crate::acp::task_transcript::{spawn_task_tailer, ActiveTasks};
use agent_client_protocol::schema::v1::{SessionId, SessionNotification, SessionUpdate};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::debug;

use crate::acp::acp_client::between_prompt::BetweenPromptTracker;
use crate::acp::acp_client::lifecycle::{
    forward_lifecycle_signals, LifecycleEnvelope, TerminalClaim,
};
use crate::acp::acp_client::omp_tasks::OmpTaskTracker;
use crate::acp::acp_client::rate_limit::record_rate_limit_report;
use crate::acp::acp_client::session_identity::SessionIngress;
use crate::acp::acp_client::session_sandbox::SessionSandbox;
use crate::acp::acp_client::tool_context::{
    restore_wake_tool_title, update_tool_context_cache, ToolCallContextCache, ToolContextCache,
};
use crate::acp::acp_client::transcript_filter::{is_transcript_event, transcript_event_kind};
use crate::acp::acp_client::update_events::{
    map_update_to_events, qualify_compaction_id, restored_wake_tool_event, AgentMessageDedup,
    CompactionFold, CompactionTracker,
};
use crate::acp::acp_client::watchdog::classify_watchdog_notification_signals;

pub(super) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(super) struct Shared {
    pub(super) event_tx: mpsc::Sender<Event>,
    /// Admits only the lifecycle-selected native session; everything the
    /// agent sends for another id is refused or buffered here (#3937).
    pub(super) ingress: Arc<SessionIngress>,
    pub(super) session_label: String,
    pub(super) profile: &'static AgentProfile,
    /// Set between a successful `session/load` and the first prompt, while
    /// the adapter replays history the event store already holds.
    pub(super) suppress_history_replay: AtomicBool,
    /// Epoch-ms of the last inbound notification, for the resume-idle
    /// watchdog.
    pub(super) last_event_at: AtomicI64,
    /// A lifecycle-bearing notification arrived after attach. Ambient
    /// updates do not prove turn progress.
    pub(super) first_event_after_attach: AtomicBool,
    pub(super) prompt_sent_since_attach: AtomicBool,
    /// Ensures exactly one path publishes each turn's terminal `Stopped`.
    pub(super) terminal_claim: Arc<TerminalClaim>,
    /// A turn adopted mid-flight on resume, which no local prompt owns
    /// (#2899).
    pub(super) adopted_turn_active: AtomicBool,
    /// While set, the per-prompt watchdog owns idle detection.
    pub(super) prompt_in_flight: Arc<AtomicBool>,
    /// Bumped before each prompt so stale lifecycle envelopes are discarded.
    pub(super) prompt_epoch: AtomicU64,
    lifecycle_tx: mpsc::Sender<LifecycleEnvelope>,
    pub(super) between_prompt: BetweenPromptTracker,
    /// Reset epochs (unix seconds) per rejected quota window. Cleared at each
    /// prompt and drained when a turn parks; a window that later reports a
    /// non-rejected status is removed (#3028, #3152).
    pub(super) rate_limit_rejections: std::sync::Mutex<HashMap<String, i64>>,
    /// A stored-session rejection already emitted its context reset.
    pub(super) context_reset_emitted: AtomicBool,
    /// Scoped to one turn; see `AgentMessageDedup` (#2281).
    pub(super) agent_msg_dedup: std::sync::Mutex<AgentMessageDedup>,
    compaction: std::sync::Mutex<CompactionTracker>,
    omp_tasks: std::sync::Mutex<OmpTaskTracker>,
    pub(super) tool_context_cache: ToolContextCache,
    bg_transcript_source: TranscriptSource,
    pub(super) bg_tasks: Arc<ActiveTasks>,
    claude_home: Option<std::path::PathBuf>,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        event_tx: mpsc::Sender<Event>,
        session_label: String,
        profile: &'static AgentProfile,
        lifecycle_tx: mpsc::Sender<LifecycleEnvelope>,
        terminal_claim: Arc<TerminalClaim>,
        prompt_in_flight: Arc<AtomicBool>,
        sandbox: Option<&SessionSandbox>,
        ingress: Arc<SessionIngress>,
        claude_home: Option<std::path::PathBuf>,
    ) -> Self {
        // A sandboxed session's sub-agent transcripts live in the container.
        let bg_transcript_source = match sandbox {
            Some(sandbox) => TranscriptSource::Container {
                runtime: crate::containers::get_container_runtime().base.binary,
                container: sandbox.container_name.clone(),
            },
            None => TranscriptSource::Host,
        };
        Self {
            event_tx,
            ingress,
            session_label,
            profile,
            suppress_history_replay: AtomicBool::new(false),
            last_event_at: AtomicI64::new(now_ms()),
            first_event_after_attach: AtomicBool::new(false),
            prompt_sent_since_attach: AtomicBool::new(false),
            terminal_claim,
            adopted_turn_active: AtomicBool::new(false),
            prompt_in_flight,
            prompt_epoch: AtomicU64::new(0),
            lifecycle_tx,
            between_prompt: BetweenPromptTracker::default(),
            rate_limit_rejections: Default::default(),
            context_reset_emitted: AtomicBool::new(false),
            agent_msg_dedup: Default::default(),
            compaction: Default::default(),
            omp_tasks: Default::default(),
            tool_context_cache: Arc::new(std::sync::Mutex::new(ToolCallContextCache::default())),
            bg_transcript_source,
            bg_tasks: Arc::default(),
            claude_home,
        }
    }

    pub(super) async fn emit(&self, event: Event) {
        let _ = self.event_tx.send(event).await;
    }

    /// Publish the native session the lifecycle selected and apply the
    /// updates buffered while it was still pending.
    pub(super) async fn commit_session(
        &self,
        id: SessionId,
    ) -> Result<(), agent_client_protocol::Error> {
        let _guard = self.ingress.fence.lock().await;
        let replay = self.ingress.finish(Some(id.clone()))?;
        self.emit(Event::AcpSessionAssigned {
            acp_session_id: id.0.to_string(),
        })
        .await;
        self.apply_replay(replay).await;
        Ok(())
    }

    /// Replayed updates carry no local prompt, so they only rebuild state.
    pub(super) async fn apply_replay(&self, replay: Vec<SessionNotification>) {
        for notification in replay {
            self.handle_notification(notification, false).await;
        }
    }

    /// Restore durable identities before any resumed notification is admitted.
    pub(super) fn seed_background_tracking(&self, seed: Vec<(String, bool)>) {
        let mut tracker = self
            .omp_tasks
            .lock()
            .expect("omp task tracker mutex poisoned");
        let mut running = self
            .between_prompt
            .bg_agents
            .lock()
            .expect("background agent mutex poisoned");
        for (agent_id, terminal) in seed {
            tracker.seed(agent_id.clone(), terminal);
            if !terminal {
                running.insert(agent_id);
            }
        }
    }

    /// Resume transcript tailers or structured tracking for a surviving runner.
    pub(super) fn resume_background_tailing(
        &self,
        launches: Vec<crate::acp::event_store::UnresolvedBackgroundAgentLaunch>,
    ) {
        for launch in launches {
            if !launch.wire_tracked && !launch.output_file.is_empty() {
                spawn_tailer(
                    launch.agent_id,
                    launch.output_file,
                    self.bg_transcript_source.clone(),
                    self.event_tx.clone(),
                    self.between_prompt.bg_agents.clone(),
                    TailerStart::Resumed,
                );
            }
        }
    }

    pub(super) fn reset_message_dedup(&self) {
        self.agent_msg_dedup
            .lock()
            .expect("agent message dedup mutex poisoned")
            .reset();
    }

    /// `local_prompt_signals` is false for a replayed update committed with
    /// the session: it belongs to no local prompt, so it must not reach the
    /// per-prompt watchdog.
    pub(super) async fn handle_notification(
        &self,
        mut notification: SessionNotification,
        local_prompt_signals: bool,
    ) {
        self.last_event_at.store(now_ms(), Ordering::Relaxed);
        let suppressing = self.suppress_history_replay.load(Ordering::Relaxed);
        // Drop the adapter's leaked restatement before anything sees it
        // (#2281). Replayed history resets rather than feeds the deduper.
        {
            let mut dedup = self
                .agent_msg_dedup
                .lock()
                .expect("agent message dedup mutex poisoned");
            if suppressing {
                dedup.reset();
            } else if dedup.observe(&notification.update) {
                debug!(
                    target: "acp.protocol",
                    session = %self.session_label,
                    "dropping leaked consolidated agent_message_chunk restatement (#2281)"
                );
                return;
            }
        }
        // Replayed compactions carry fresh ids and are dropped as transcript.
        if !suppressing {
            qualify_compaction_id(&notification.session_id, &mut notification.update);
            let fold = self
                .compaction
                .lock()
                .expect("compaction tracker mutex poisoned")
                .observe(&mut notification.update);
            match fold {
                CompactionFold::Pass => {}
                CompactionFold::Skip => return,
                CompactionFold::Patch(event) => return self.emit(*event).await,
            }
        }
        let restored_title =
            restore_wake_tool_title(&self.tool_context_cache, &notification.update);
        let restored_wake_event = restored_title
            .as_deref()
            .and_then(|title| restored_wake_tool_event(self.profile, &notification.update, title));
        // One epoch per notification, so its signals belong to the prompt
        // that was current when it arrived.
        let epoch = self.prompt_epoch.load(Ordering::Relaxed);
        let (lifecycle, wakeup) = classify_watchdog_notification_signals(
            &notification.update,
            restored_title.as_deref(),
            self.profile,
            suppressing,
        );
        if lifecycle.is_some() || wakeup.is_some() {
            self.first_event_after_attach.store(true, Ordering::Relaxed);
        }
        let prompt_active = self.prompt_in_flight.load(Ordering::Relaxed);
        if !prompt_active
            && self.between_prompt.observe(
                lifecycle.as_ref(),
                wakeup.as_ref(),
                now_ms(),
                self.adopted_turn_active.load(Ordering::Relaxed),
                &self.terminal_claim,
            )
        {
            debug!(
                target: "acp.protocol",
                session = %self.session_label,
                "between-prompt watchdog armed for an agent-initiated turn"
            );
        }
        self.capture_rate_limit(&notification.update);
        let session_id = notification.session_id.0.to_string();
        let update_for_tool_context = notification.update.clone();
        let task_events = self
            .omp_tasks
            .lock()
            .expect("omp task tracker mutex poisoned")
            .observe(&notification.update, suppressing);
        let mut events = map_update_to_events(notification.update, self.profile);
        events.extend(restored_wake_event);
        events.extend(task_events);
        // Signals go first: if the event send backpressures, the watchdog must
        // not evaluate without a suppression-bearing signal.
        forward_lifecycle_signals(
            local_prompt_signals && prompt_active && epoch != 0,
            &self.lifecycle_tx,
            epoch,
            lifecycle,
            wakeup,
            &self.session_label,
        )
        .await;
        for event in events {
            if let Event::BackgroundAgentLaunched {
                agent_id,
                output_file,
                ..
            } = &event
            {
                if !suppressing && !output_file.is_empty() {
                    spawn_tailer(
                        agent_id.clone(),
                        output_file.clone(),
                        self.bg_transcript_source.clone(),
                        self.event_tx.clone(),
                        self.between_prompt.bg_agents.clone(),
                        TailerStart::Live,
                    );
                }
            }
            // During replay only transcript events are dropped; ambient state
            // and lifecycle events still pass so pickers stay current.
            if suppressing && is_transcript_event(&event) {
                debug!(
                    target: "acp.protocol",
                    session = %self.session_label,
                    kind = transcript_event_kind(&event),
                    "dropping post-load history-replay event"
                );
                continue;
            }
            if let Event::BackgroundTaskStarted {
                task_id,
                tool_call_id,
                ..
            } = &event
            {
                if !matches!(self.bg_transcript_source, TranscriptSource::Host) {
                    continue;
                }
                spawn_task_tailer(
                    BackgroundTaskLaunch {
                        task_id: task_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                    },
                    session_id.clone(),
                    self.claude_home.clone(),
                    self.bg_tasks.clone(),
                    self.event_tx.downgrade(),
                    TailerStart::Live,
                    Some(event),
                )
                .await;
                if self.event_tx.is_closed() {
                    break;
                }
                continue;
            }
            match &event {
                Event::BackgroundAgentLaunched {
                    agent_id,
                    wire_tracked: true,
                    ..
                } => {
                    self.between_prompt
                        .bg_agents
                        .lock()
                        .expect("background agent mutex poisoned")
                        .insert(agent_id.clone());
                }
                Event::BackgroundAgentCompleted { agent_id, .. } => {
                    self.between_prompt
                        .bg_agents
                        .lock()
                        .expect("background agent mutex poisoned")
                        .remove(agent_id);
                }
                _ => {}
            }
            update_tool_context_cache(&self.tool_context_cache, &event, &update_for_tool_context);
            if self.event_tx.send(event).await.is_err() {
                break;
            }
        }
    }

    /// Re-tail tasks a previous daemon left open: their end may already be in
    /// the transcript, so each tailer reads it from the start.
    pub(super) async fn resume_task_tailing(
        &self,
        session_id: &str,
        tasks: Vec<BackgroundTaskLaunch>,
    ) {
        if !matches!(self.bg_transcript_source, TranscriptSource::Host) {
            return;
        }
        for task in tasks {
            spawn_task_tailer(
                task,
                session_id.to_string(),
                self.claude_home.clone(),
                self.bg_tasks.clone(),
                self.event_tx.downgrade(),
                TailerStart::Resumed,
                None,
            )
            .await;
        }
    }

    /// Only rejections are retained (#3152); every observation is logged as
    /// the breadcrumb for a wrong reset time.
    fn capture_rate_limit(&self, update: &SessionUpdate) {
        let SessionUpdate::UsageUpdate(u) = update else {
            return;
        };
        let Some(raw) = u.meta.as_ref().and_then(|m| m.get("_claude/rateLimit")) else {
            return;
        };
        let retained = record_rate_limit_report(&self.rate_limit_rejections, &u.meta);
        debug!(
            target: "acp.protocol",
            session = %self.session_label,
            observed = %raw,
            retained,
            "observed adapter rate-limit meta"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::agent_profiles::OMP;
    use serde_json::json;

    fn task_notification(status: &str, child_status: &str) -> SessionNotification {
        serde_json::from_value(json!({
            "sessionId": "native",
            "update": {
                "sessionUpdate": "tool_call_update", "toolCallId": "batch", "status": status,
                "rawOutput": {"details": {"async": {"type": "task"}, "progress": [{
                    "id": "Child", "agent": "task", "status": child_status,
                    "resolvedModel": "provider/model", "assignment": "Inspect code",
                    "toolCount": 1, "recentTools": [{"tool": "read", "args": "src"}],
                    "recentOutput": ["Preview"]
                }]}}
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn omp_tasks_resume_preserves_terminal_siblings_and_settles_unresolved() {
        use crate::acp::state::{AcpState, BackgroundAgentStatus};

        let ended_at = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
        let mut state = AcpState::default();
        let mut original = OmpTaskTracker::default();
        let initial = task_notification("completed", "completed");
        for mut event in original.observe(&initial.update, false) {
            if let Event::BackgroundAgentCompleted { ended_at: at, .. } = &mut event {
                *at = ended_at;
            }
            state.apply_event(state.last_seq.saturating_add(1), event).unwrap();
        }
        let mut pending = serde_json::to_value(task_notification("completed", "running")).unwrap();
        pending["update"]["rawOutput"]["details"]["progress"][0]["id"] = json!("Pending");
        let pending: SessionNotification = serde_json::from_value(pending).unwrap();
        for event in original.observe(&pending.update, false) {
            state.apply_event(state.last_seq.saturating_add(1), event).unwrap();
        }

        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(32);
        let shared = Shared::new(
            event_tx,
            "test".into(),
            &OMP,
            lifecycle_tx,
            Arc::new(TerminalClaim::new()),
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            Arc::new(SessionIngress::new(Some(SessionId::from("native")))),
        );
        shared.seed_background_tracking(vec![("Child".into(), true), ("Pending".into(), false)]);
        assert_eq!(
            *shared.between_prompt.bg_agents.lock().unwrap(),
            std::collections::HashSet::from(["Pending".to_string()])
        );

        let mut aggregate = serde_json::to_value(initial).unwrap();
        let mut completed_pending = serde_json::to_value(pending).unwrap()["update"]["rawOutput"]
            ["details"]["progress"][0]
            .clone();
        completed_pending["status"] = json!("completed");
        aggregate["update"]["rawOutput"]["details"]["progress"]
            .as_array_mut()
            .unwrap()
            .push(completed_pending);
        let aggregate: SessionNotification = serde_json::from_value(aggregate).unwrap();
        shared.handle_notification(aggregate.clone(), false).await;
        let mut completions = 0;
        while let Ok(event) = event_rx.try_recv() {
            assert!(!matches!(&event, Event::BackgroundAgentLaunched { .. }));
            if let Event::BackgroundAgentCompleted { agent_id, .. } = &event {
                assert_eq!(agent_id, "Pending");
                completions += 1;
            }
            state.apply_event(state.last_seq.saturating_add(1), event).unwrap();
        }
        assert_eq!(completions, 1);
        assert_eq!(state.background_agents.len(), 2);
        assert_eq!(state.background_agents[0].ended_at, Some(ended_at));
        assert!(state
            .background_agents
            .iter()
            .all(|agent| agent.status == BackgroundAgentStatus::Completed));
        assert!(shared.between_prompt.bg_agents.lock().unwrap().is_empty());

        shared.handle_notification(aggregate, false).await;
        while let Ok(event) = event_rx.try_recv() {
            assert!(!matches!(
                event,
                Event::BackgroundAgentLaunched { .. }
                    | Event::BackgroundAgentProgress { .. }
                    | Event::BackgroundAgentCompleted { .. }
            ));
        }
    }

    #[tokio::test]
    async fn omp_tasks_preserve_tool_cards_without_transcript_tailing() {
        let (event_tx, mut event_rx) = mpsc::channel(32);
        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(32);
        let shared = Shared::new(
            event_tx,
            "test".into(),
            &OMP,
            lifecycle_tx,
            Arc::new(TerminalClaim::new()),
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            Arc::new(SessionIngress::new(Some(SessionId::from("native")))),
        );
        shared
            .handle_notification(task_notification("completed", "running"), false)
            .await;
        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::ToolCallCompleted { .. })));
        assert!(events.iter().any(|e| matches!(e, Event::BackgroundAgentLaunched { output_file, wire_tracked: true, .. } if output_file.is_empty())));
        assert!(!events
            .iter()
            .any(|e| matches!(e, Event::BackgroundAgentCompleted { .. })));
        assert!(shared
            .between_prompt
            .bg_agents
            .lock()
            .unwrap()
            .contains("Child"));

        shared
            .handle_notification(task_notification("completed", "completed"), false)
            .await;
        let mut terminal = 0;
        while let Ok(event) = event_rx.try_recv() {
            assert!(!matches!(event, Event::BackgroundAgentLaunched { .. }));
            terminal += usize::from(matches!(event, Event::BackgroundAgentCompleted { .. }));
        }
        assert_eq!(terminal, 1);
        assert!(!shared
            .between_prompt
            .bg_agents
            .lock()
            .unwrap()
            .contains("Child"));

        shared
            .suppress_history_replay
            .store(true, Ordering::Relaxed);
        shared
            .handle_notification(task_notification("completed", "running"), false)
            .await;
        assert!(event_rx.try_recv().is_err());
    }
}
