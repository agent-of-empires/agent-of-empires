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
use crate::acp::acp_client::control::{publish_adopted_completion, DaemonControlClient};
use crate::acp::acp_client::lifecycle::{
    forward_lifecycle_signals, LifecycleEnvelope, TerminalClaim,
};
use crate::acp::acp_client::rate_limit::record_rate_limit_report;
use crate::acp::acp_client::session_identity::{PendingIngress, SessionIngress};
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
    pub(super) connection_state: Arc<super::ConnectionState>,
    control: Option<Arc<DaemonControlClient>>,
    pending_responders: crate::acp::acp_client::pending::PendingResponders,
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
    pub(super) local_prompt_settled: tokio::sync::Notify,
    /// Bumped before each prompt so stale lifecycle envelopes are discarded.
    pub(super) prompt_epoch: AtomicU64,
    lifecycle_tx: mpsc::Sender<LifecycleEnvelope>,
    pub(super) between_prompt: BetweenPromptTracker,
    /// Reset epochs (unix seconds) per rejected quota window. Cleared at each
    /// prompt and drained when a turn parks; a window that later reports a
    /// non-rejected status is removed (#3028, #3152).
    pub(super) rate_limit_rejections: std::sync::Mutex<HashMap<String, i64>>,
    /// Scoped to one turn; see `AgentMessageDedup` (#2281).
    pub(super) agent_msg_dedup: std::sync::Mutex<AgentMessageDedup>,
    compaction: std::sync::Mutex<CompactionTracker>,
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
        connection_state: Arc<super::ConnectionState>,
        control: Option<Arc<DaemonControlClient>>,
        pending_responders: crate::acp::acp_client::pending::PendingResponders,
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
            connection_state,
            control,
            pending_responders,
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
            local_prompt_settled: tokio::sync::Notify::new(),
            prompt_epoch: AtomicU64::new(0),
            lifecycle_tx,
            between_prompt: BetweenPromptTracker::default(),
            rate_limit_rejections: Default::default(),
            agent_msg_dedup: Default::default(),
            compaction: Default::default(),
            tool_context_cache: Arc::new(std::sync::Mutex::new(ToolCallContextCache::default())),
            bg_transcript_source,
            bg_tasks: Arc::default(),
            claude_home,
        }
    }

    pub(super) async fn emit(&self, mut event: Event) {
        let Ok(permit) = self.event_tx.reserve().await else {
            return;
        };
        if let Event::AcpSessionAssigned {
            connection_ready, ..
        } = &mut event
        {
            let _admission = self
                .connection_state
                .admission
                .lock()
                .expect("connection admission lock");
            self.connection_state.established();
            *connection_ready &= self.connection_state.loss().is_none();
            if *connection_ready {
                self.connection_state.usable.store(true, Ordering::Release);
            }
            permit.send(event);
        } else {
            permit.send(event);
        }
    }

    /// Inferred idle/watchdog terminals recheck native admission after channel
    /// backpressure. Genuine correlated outcomes use `emit` instead.
    pub(super) async fn emit_while_available<const N: usize>(&self, events: [Event; N]) -> bool {
        let Ok(mut permits) = self.event_tx.reserve_many(N).await else {
            return false;
        };
        self.connection_state
            .admit(|| {
                for event in events {
                    permits.next().expect("reserved event permit").send(event);
                }
            })
            .is_ok()
    }

    pub(super) async fn wait_local_prompt_settled(&self) {
        loop {
            let settled = self.local_prompt_settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            if self.prompt_epoch.load(Ordering::Acquire) == 0
                || !self.prompt_in_flight.load(Ordering::Acquire)
            {
                return;
            }
            settled.await;
        }
    }

    pub(super) async fn publish_connection_unavailable(&self) {
        self.wait_local_prompt_settled().await;
        if self.connection_state.is_stopping() {
            return;
        }
        if let Some(observation) = self.connection_state.take_unreported_loss() {
            self.pending_responders.lock().await.clear();
            self.terminal_claim.claim();
            self.adopted_turn_active.store(false, Ordering::Release);
            self.between_prompt.deactivate();
            self.emit(Event::AcpConnectionUnavailable {
                source: observation.source,
                cause: observation.cause,
                phase: observation.phase,
                native_subject: None,
            })
            .await;
        }
    }

    /// Publish the native session the lifecycle selected and apply the
    /// updates buffered while it was still pending.
    pub(super) async fn commit_session(
        &self,
        id: SessionId,
        connection_ready: bool,
    ) -> Result<(), agent_client_protocol::Error> {
        let _guard = self.ingress.fence.write().await;
        let replay = self.ingress.finish(Some(id.clone()))?;
        self.emit(Event::AcpSessionAssigned {
            acp_session_id: id.0.to_string(),
            connection_ready,
            native_subject: None,
        })
        .await;
        self.apply_replay(replay).await;
        Ok(())
    }

    /// Replayed updates carry no local prompt. Private barriers retain their
    /// exact position without blocking SDK ingress on initial establishment.
    pub(super) async fn apply_replay(&self, replay: Vec<PendingIngress>) {
        for entry in replay {
            self.apply_ingress(entry).await;
        }
    }

    pub(super) async fn handle_ingress_marker(
        &self,
        marker: PendingIngress,
    ) -> agent_client_protocol::Result<()> {
        if matches!(&marker, PendingIngress::NativeLost) {
            self.connection_state.mark_ordered_loss();
            if let Some(control) = self.control.as_ref() {
                control.deliver_native_lost();
            }
        }
        if let Some(marker) = self.ingress.marker(marker)? {
            if matches!(&marker, PendingIngress::NativeLost) {
                self.connection_state.wait_completed_outcomes().await;
            }
            let _guard = self.ingress.fence.read().await;
            if let Some(marker) = self.ingress.marker(marker)? {
                self.apply_ingress(marker).await;
            }
        }
        Ok(())
    }

    async fn apply_ingress(&self, entry: PendingIngress) {
        match entry {
            PendingIngress::Update(notification) => {
                self.handle_notification(*notification, false).await
            }
            PendingIngress::Auth(status) => {
                self.emit(Event::AuthStatusUpdated {
                    status: Some(status),
                })
                .await;
            }
            PendingIngress::PromptCompleted(adopted) => {
                if let Some(control) = self.control.as_ref() {
                    let turn = adopted.turn;
                    if publish_adopted_completion(
                        *adopted,
                        &self.event_tx,
                        &control.completion,
                        &self.prompt_in_flight,
                        &self.terminal_claim,
                    )
                    .await
                    {
                        self.adopted_turn_active.store(false, Ordering::Release);
                        self.between_prompt
                            .deactivate_unsuccessful_turn(&self.terminal_claim, turn);
                    }
                }
            }
            PendingIngress::NativeLost => self.publish_connection_unavailable().await,
        }
    }

    pub(super) async fn finish_unavailable_ingress(&self) {
        let _guard = self.ingress.fence.write().await;
        if let Ok(replay) = self.ingress.finish(self.ingress.current()) {
            self.apply_replay(replay).await;
        }
    }

    /// Re-tail sub-agents a previous daemon launched and left unresolved.
    pub(super) fn resume_background_tailing(&self, launches: Vec<(String, String)>) {
        for (agent_id, output_file) in launches {
            spawn_tailer(
                agent_id,
                output_file,
                self.bg_transcript_source.clone(),
                self.event_tx.clone(),
                self.between_prompt.bg_agents.clone(),
                TailerStart::Resumed,
            );
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
        let prompt_active = self.prompt_in_flight.load(Ordering::Acquire);
        let failed_main_turn = !prompt_active && self.terminal_claim.failed();
        if !prompt_active
            && self.connection_state.loss().is_none()
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
        let mut events = map_update_to_events(notification.update, self.profile);
        events.extend(restored_wake_event);
        // Signals go first: if the event send backpressures, the watchdog must
        // not evaluate without a suppression-bearing signal.
        forward_lifecycle_signals(
            local_prompt_signals
                && prompt_active
                && epoch != 0
                && self.connection_state.loss().is_none(),
            &self.lifecycle_tx,
            epoch,
            lifecycle,
            wakeup,
            &self.session_label,
        )
        .await;
        for event in events {
            if failed_main_turn && matches!(event, Event::ThinkingStarted) {
                continue;
            }
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
                tracing::trace!(
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
                workflow,
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
                        workflow: workflow.clone(),
                        workflow_agents: Default::default(),
                    },
                    session_id.clone(),
                    self.claude_home.clone(),
                    self.bg_tasks.clone(),
                    self.between_prompt.bg_agents.clone(),
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
                self.between_prompt.bg_agents.clone(),
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
