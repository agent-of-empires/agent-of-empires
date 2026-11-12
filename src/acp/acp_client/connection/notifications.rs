//! Turn state shared between the notification handler and the command loop,
//! and the `session/update` handler itself.

use crate::acp::agent_profiles::AgentProfile;
use crate::acp::background_agent::{spawn_tailer, TailerStart, TranscriptSource};
use crate::acp::state::{BackgroundTaskLaunch, Event, SubagentRecord};
use crate::acp::task_transcript::{spawn_task_tailer, ActiveTasks};
use agent_client_protocol::v1::schema::{
    ContentBlock, SessionId, SessionNotification, SessionUpdate,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::debug;

use crate::acp::acp_client::aoe_ingress::{AoeIngress, Ingested};
use crate::acp::acp_client::between_prompt::BetweenPromptTracker;
use crate::acp::acp_client::control::{publish_adopted_completion, DaemonControlClient};
use crate::acp::acp_client::lifecycle::{
    forward_lifecycle_signals, LifecycleEnvelope, TerminalClaim,
};
use crate::acp::acp_client::rate_limit::record_rate_limit_report;
use crate::acp::acp_client::session_identity::{PendingIngress, SessionIngress};
use crate::acp::acp_client::session_sandbox::SessionSandbox;
use crate::acp::acp_client::tool_context::{
    restore_wake_tool_title, shell_command, update_tool_context_cache, ToolCallContextCache,
    ToolContextCache,
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
    /// Set from an import's `session/load` until the first prompt, when
    /// replayed user messages are the only record of them (#2276). Any other
    /// `user_message_chunk` is the agent echoing a prompt already recorded.
    pub(super) seed_user_replay: AtomicBool,
    /// The last AoE prompt's text and when it ended (`0` while running), to
    /// drop an echo that trails its `Stopped`.
    pub(super) last_prompt: std::sync::Mutex<(String, i64)>,
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
    /// Merged snapshot per subagent session; see `subagents.rs`.
    pub(super) subagents: std::sync::Mutex<HashMap<String, SubagentRecord>>,
    bg_transcript_source: TranscriptSource,
    pub(super) bg_tasks: Arc<ActiveTasks>,
    pub(super) aoe: AoeIngress,
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
            seed_user_replay: AtomicBool::new(false),
            last_prompt: Default::default(),
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
            subagents: Default::default(),
            bg_transcript_source,
            bg_tasks: Arc::default(),
            aoe: AoeIngress::default(),
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

    /// A user chunk repeating the prompt AoE just finished, arriving within
    /// the grace after its `Stopped`.
    fn is_late_echo(&self, update: &SessionUpdate) -> bool {
        const LATE_ECHO_GRACE_MS: i64 = 2_000;
        let SessionUpdate::UserMessageChunk(chunk) = update else {
            return false;
        };
        let ContentBlock::Text(text) = &chunk.content else {
            return false;
        };
        let (prompt, ended_ms) = &*self.last_prompt.lock().expect("last prompt mutex poisoned");
        *ended_ms != 0 && now_ms() - ended_ms < LATE_ECHO_GRACE_MS && text.text.trim() == prompt
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
        notification: SessionNotification,
        local_prompt_signals: bool,
    ) {
        // A frame the contract consumes without delivering (a lease renewal)
        // still proves the agent is alive.
        self.last_event_at.store(now_ms(), Ordering::Relaxed);
        self.settle_discarded().await;
        let epoch = self.prompt_epoch.load(Ordering::Relaxed);
        let ingested = self
            .aoe
            .ingest(notification, local_prompt_signals, epoch, now_ms());
        self.handle_ingested(ingested).await;
    }

    /// Frames the session identity dropped still count in the proxy's
    /// numbering, so a tailer frame anchored after them is not held for
    /// adapter output that already happened.
    pub(super) async fn settle_discarded(&self) {
        let skipped = self.ingress.take_discarded_adapter_frames();
        if skipped > 0 {
            let ingested = self.aoe.skip_frames(skipped, now_ms());
            self.handle_ingested(ingested).await;
        }
    }

    async fn handle_ingested(&self, ingested: Ingested) {
        let (frames, lost) = match ingested {
            Ingested::Deliver(frames) => (frames, None),
            Ingested::Lost(reason) => (Vec::new(), Some(reason)),
            Ingested::DeliverThenLost(frames, reason) => (frames, Some(reason)),
        };
        for frame in frames {
            self.handle_adapter_notification(
                frame.notification,
                frame.local_prompt_signals,
                frame.epoch,
            )
            .await;
        }
        if let Some(reason) = lost {
            tracing::warn!(
                target: "acp.protocol",
                session = %self.session_label,
                ?reason,
                "agent integration lost; ignoring further frames"
            );
            self.emit(Event::IntegrationLost { reason }).await;
        }
    }

    async fn handle_adapter_notification(
        &self,
        mut notification: SessionNotification,
        local_prompt_signals: bool,
        epoch: u64,
    ) {
        // Subagent traffic never feeds the root session's dedup, compaction
        // or watchdog state: a busy child would hold its parent's turn open.
        let from_child = self.ingress.child_route(&notification.session_id).is_some();
        match notification.update {
            SessionUpdate::SubagentUpdate(update) => {
                return self
                    .handle_subagent_update(notification.session_id, update)
                    .await;
            }
            SessionUpdate::SessionMessage(message) => {
                return self
                    .handle_session_message(notification.session_id, message)
                    .await;
            }
            SessionUpdate::SessionMessageChunk(_) => return,
            update if from_child => {
                return self
                    .handle_child_notification(notification.session_id, update)
                    .await;
            }
            update => notification.update = update,
        }
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
        // `epoch` is the prompt current when the frame arrived, so its signals
        // belong to that prompt even if the frame was held.
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
        // A chunk while an AoE prompt is in flight echoes it (#5224). Idle, it
        // is a Remote Control message from another client.
        let user_echo =
            prompt_active || !local_prompt_signals || self.is_late_echo(&notification.update);
        let mut events = if matches!(notification.update, SessionUpdate::UserMessageChunk(_))
            && user_echo
            && !self.seed_user_replay.load(Ordering::Relaxed)
        {
            Vec::new()
        } else {
            map_update_to_events(notification.update, self.profile)
        };
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
        for mut event in events {
            if failed_main_turn && matches!(event, Event::ThinkingStarted) {
                continue;
            }
            if let Event::BackgroundAgentLaunched {
                agent_id,
                output_file,
                ..
            } = &event
            {
                // A subagents session gets the agent's state from its child
                // session, so nothing tails the transcript.
                if !suppressing && !output_file.is_empty() && !self.profile.supports_subagents {
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
                tool_call_id: Some(call),
                description,
                workflow: None,
                ..
            } = &mut event
            {
                if let Some(command) = shell_command(&self.tool_context_cache, call) {
                    *description = command;
                }
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
            // Only `TaskStop` ends a task here; the tailer publishes the end.
            if let Event::BackgroundTaskFinished { task_id, .. } = &event {
                self.bg_tasks.stop(task_id);
                continue;
            }
            update_tool_context_cache(
                &self.tool_context_cache,
                &mut event,
                &update_for_tool_context,
            );
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{ContentChunk, TextContent};

    fn echo_shared(prompt_in_flight: bool) -> (Shared, mpsc::Receiver<Event>) {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(8);
        let shared = Shared::new(
            event_tx,
            "s-echo".to_string(),
            &crate::acp::agent_profiles::CLAUDE,
            lifecycle_tx,
            Arc::new(TerminalClaim::new()),
            Arc::new(AtomicBool::new(prompt_in_flight)),
            None,
            Arc::new(SessionIngress::new(Some(SessionId::from("native")))),
            None,
            Arc::default(),
            None,
            Arc::default(),
        );
        (shared, event_rx)
    }

    async fn user_chunk_events(
        shared: Shared,
        mut event_rx: mpsc::Receiver<Event>,
        local_prompt_signals: bool,
    ) -> Vec<Event> {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("hello")));
        shared
            .handle_notification(
                SessionNotification::new("native", SessionUpdate::UserMessageChunk(chunk)),
                local_prompt_signals,
            )
            .await;
        drop(shared);
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            events.push(event);
        }
        events
    }

    async fn user_echo_events(
        seed_user_replay: bool,
        prompt_in_flight: bool,
        local_prompt_signals: bool,
    ) -> Vec<Event> {
        let (shared, event_rx) = echo_shared(prompt_in_flight);
        shared
            .seed_user_replay
            .store(seed_user_replay, Ordering::Relaxed);
        user_chunk_events(shared, event_rx, local_prompt_signals).await
    }

    #[tokio::test]
    async fn user_message_chunk_records_replay_and_remote_messages_only() {
        // Echo of an AoE prompt is already recorded locally (#5224).
        assert!(user_echo_events(false, true, true).await.is_empty());
        // Replayed update committed with the session belongs to no live message.
        assert!(user_echo_events(false, false, false).await.is_empty());
        // Import replay and an idle Remote Control message are recorded.
        for (seed, in_flight) in [(true, true), (true, false), (false, false)] {
            assert!(
                matches!(
                    user_echo_events(seed, in_flight, true).await.as_slice(),
                    [Event::UserPromptSent { text, .. }] if text == "hello"
                ),
                "seed={seed} in_flight={in_flight}"
            );
        }
    }

    #[tokio::test]
    async fn echo_trailing_its_stopped_is_dropped_within_the_grace_only() {
        // (prompt text, ms since it ended, recorded)
        for (prompt, ago_ms, recorded) in [
            ("hello", 100, false),
            ("hello", 10_000, true),
            ("other", 100, true),
        ] {
            let (shared, event_rx) = echo_shared(false);
            *shared.last_prompt.lock().unwrap() = (prompt.to_string(), now_ms() - ago_ms);
            let events = user_chunk_events(shared, event_rx, true).await;
            assert_eq!(
                !events.is_empty(),
                recorded,
                "{prompt} ended {ago_ms}ms ago"
            );
        }
    }
}
