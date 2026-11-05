//! Turn state shared between the notification handler and the command loop,
//! and the `session/update` handler itself.

use crate::acp::agent_profiles::AgentProfile;
use crate::acp::background_agent::{spawn_tailer, TailerStart, TranscriptSource};
use crate::acp::establishment::{
    NativeEstablishmentReceipt, NativeResourceAuthority, NativeResourceLocation,
    NativeResourceSelector, NativeWorkflowSelector,
};
use crate::acp::event_sender::{EventMetadata, EventScope, EventSender};
use crate::acp::state::{BackgroundTaskLaunch, Event};
use crate::acp::task_transcript::{spawn_task_tailer, ActiveTasks};
use agent_client_protocol::schema::v1::{
    SessionId, SessionNotification, SessionUpdate, ToolCallStatus,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::debug;

use crate::acp::acp_client::between_prompt::BetweenPromptTracker;
use crate::acp::acp_client::control::{
    publish_adopted_completion, AdoptedCompletionPublication, DaemonControlClient,
};
use crate::acp::acp_client::lifecycle::{
    classify_lifecycle_signal, forward_lifecycle_signals, LifecycleEnvelope, LifecycleSignal,
    TerminalClaim,
};
use crate::acp::acp_client::rate_limit::{captured_rate_limit_resets_at, record_rate_limit_report};
use crate::acp::acp_client::session_identity::{IngressUpdate, PendingIngress, SessionIngress};
use crate::acp::acp_client::session_sandbox::SessionSandbox;
use crate::acp::acp_client::tool_context::{
    capture_tool_context_input, restore_wake_tool_title, update_tool_context_cache,
    ToolCallContextCache, ToolContextCache,
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
    pub(super) event_tx: EventSender,
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
    /// Native runtime uses only current-qualified updates; original history folds separately.
    compaction: std::sync::Mutex<CompactionTracker>,
    pub(super) tool_context_cache: ToolContextCache,
    bg_transcript_source: TranscriptSource,
    pub(super) bg_tasks: Arc<ActiveTasks>,
    claude_home: Option<std::path::PathBuf>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        // Readers must release strong senders before the SDK drain can join.
        self.bg_tasks.clear();
        self.between_prompt.bg_agents.clear();
    }
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        event_tx: EventSender,
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

    pub(super) fn native_compacting(&self) -> bool {
        self.compaction
            .lock()
            .expect("compaction tracker mutex poisoned")
            .is_active()
    }

    pub(super) fn begin_native_compaction_original(
        &self,
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
        attempt: crate::acp::establishment::AttemptId,
    ) {
        self.compaction
            .lock()
            .expect("compaction tracker mutex poisoned")
            .begin_original(owner, attempt);
    }

    pub(super) async fn emit(&self, event: Event) {
        let constructor = self
            .control
            .as_ref()
            .and_then(|_| self.ingress.constructor_scope());
        self.emit_with_constructor(event, constructor).await;
    }

    pub(super) async fn emit_with_constructor(
        &self,
        mut event: Event,
        constructor: Option<Arc<crate::acp::establishment::NativeConstructorObservation>>,
    ) {
        let sender = match &constructor {
            Some(constructor) => self.event_tx.with_constructor(Arc::clone(constructor)),
            None => self.event_tx.historical(),
        };
        let Ok(permit) = sender.reserve().await else {
            return;
        };
        let current = constructor
            .as_deref()
            .map(|constructor| constructor.try_publication_guard())
            .transpose();
        let stale = current
            .as_ref()
            .is_err_and(|error| !error.is::<crate::process::worker_registry::RegistryBusy>());
        let permit = if stale { permit.historical() } else { permit };
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
                self.connection_state.record_usable();
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
            let _ = self
                .event_tx
                .send_control_plane_loss(Arc::clone(&self.connection_state), observation)
                .await;
        }
    }

    pub(super) async fn activate_session(
        &self,
        id: SessionId,
    ) -> Result<(), agent_client_protocol::Error> {
        if self.ingress.is_current_stable(&id) {
            return Ok(());
        }
        let _guard = self.ingress.fence.write().await;
        let replay = self.ingress.finish(Some(id))?;
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
        if matches!(&marker, PendingIngress::Replayed(_)) {
            if let Some(marker) = self.ingress.marker(marker)? {
                let _guard = self.ingress.fence.write().await;
                if let Some(marker) = self.ingress.marker(marker)? {
                    self.apply_ingress(marker).await;
                }
            }
            return Ok(());
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
            PendingIngress::Auth(auth) => {
                let mut metadata = EventMetadata::history();
                metadata.native_origin = auth.evidence.physical_source.map(|source| source.witness);
                if let Some(context) = &auth.current_scope {
                    metadata.scope = EventScope::SessionCurrent;
                    metadata.native_origin = context.origin();
                }
                let sender = self
                    .event_tx
                    .with_scoped_metadata(metadata, auth.current_scope);
                let _ = sender
                    .send(Event::AuthStatusUpdated {
                        status: Some(auth.status),
                    })
                    .await;
            }
            PendingIngress::PromptCompleted(adopted) => {
                if let Some(control) = self.control.as_ref() {
                    if let AdoptedCompletionPublication::Settled(turn) = publish_adopted_completion(
                        *adopted,
                        &self.event_tx,
                        control,
                        &self.prompt_in_flight,
                        &self.terminal_claim,
                        || {
                            captured_rate_limit_resets_at(
                                &self.rate_limit_rejections,
                                chrono::Utc::now(),
                            )
                        },
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
            PendingIngress::Replayed(marker) => {
                if let Some(control) = &self.control {
                    control.mark_session_replayed(marker);
                }
            }
        }
    }

    pub(super) async fn finish_unavailable_ingress(&self) {
        let _guard = self.ingress.fence.write().await;
        if let Ok(replay) = self.ingress.finish(self.ingress.current()) {
            self.apply_replay(replay).await;
        }
    }

    /// Re-tail sub-agents a previous daemon launched and left unresolved.
    pub(super) async fn resume_background_tailing(
        &self,
        launches: Vec<crate::acp::event_store::UnresolvedBackgroundAgentLaunch>,
        receipt: Option<Arc<NativeEstablishmentReceipt>>,
    ) {
        let Some(receipt) = receipt.as_deref() else {
            return;
        };
        for launch in launches {
            let Some(witness) = launch.witness else {
                continue;
            };
            let authority = match NativeResourceAuthority::adopt(witness, receipt) {
                Ok(authority) => authority,
                Err(error) => {
                    tracing::warn!(target: "acp.resource", agent = %launch.agent_id, %error,
                        "retained tracking allocation was not adopted");
                    continue;
                }
            };
            let NativeResourceSelector::Agent { source, .. } = authority.selector() else {
                continue;
            };
            let Some(source) = TranscriptSource::from_location(source) else {
                tracing::warn!(target: "acp.resource", agent = %launch.agent_id,
                    "retained transcript runtime is unsupported");
                continue;
            };
            spawn_tailer(
                (launch.agent_id, launch.output_file),
                source,
                self.event_tx.clone(),
                Arc::clone(&self.between_prompt.bg_agents),
                TailerStart::Resumed,
                authority,
                None,
            )
            .await;
        }
    }

    pub(super) fn reset_message_dedup(&self) {
        if self.control.is_some() {
            return;
        }
        self.agent_msg_dedup
            .lock()
            .expect("agent message dedup mutex poisoned")
            .reset();
    }

    pub(super) async fn handle_notification(
        &self,
        update: IngressUpdate,
        local_prompt_signals: bool,
    ) {
        if self.control.is_none() {
            self.handle_sdk_notification(update.notification, local_prompt_signals)
                .await;
            return;
        }
        let IngressUpdate {
            mut notification,
            evidence,
            current_scope,
            metadata_scope,
            resource_start,
            history,
            suppress_replay,
        } = update;
        let mut metadata = EventMetadata::history();
        metadata.native_origin = evidence.physical_source.map(|source| source.witness);
        metadata.reported_session_id = Some(notification.session_id.clone());
        metadata.prompt_observation = evidence.prompt_observation;
        let historical = self.event_tx.with_metadata(metadata.clone());
        if let Some(history) = &history {
            if !suppress_replay
                && history
                    .dedup
                    .lock()
                    .expect("original message dedup")
                    .observe(&notification.update)
            {
                return;
            }
        }
        if !suppress_replay {
            qualify_compaction_id(&notification.session_id, &mut notification.update);
            let compaction_lifecycle = matches!(
                &notification.update,
                SessionUpdate::AgentMessageChunk(_) | SessionUpdate::CompactionUpdate(_)
            ) && matches!(
                classify_lifecycle_signal(&notification.update),
                Some(
                    LifecycleSignal::CompactionStarted
                        | LifecycleSignal::CompactionCompleted
                        | LifecycleSignal::CompactionFailed
                )
            );
            if let Some(context) = current_scope.as_ref().filter(|_| compaction_lifecycle) {
                let pending = self
                    .compaction
                    .lock()
                    .expect("compaction tracker mutex poisoned")
                    .observe_current(&mut notification.update, Arc::clone(context));
                #[cfg(test)]
                if pending {
                    if let Some(entered) = self
                        .connection_state
                        .test_prompt_admission_busy
                        .lock()
                        .unwrap()
                        .take()
                    {
                        let _ = entered.send(());
                    }
                }
                #[cfg(not(test))]
                let _ = pending;
                #[cfg(test)]
                if let Some(folded) = crate::acp::acp_client::steer::COMPACTION_FOLD_OBSERVED
                    .lock()
                    .unwrap()
                    .remove(&(Arc::as_ptr(&self.connection_state) as usize))
                {
                    let _ = folded.send(());
                }
            }
            let fold = history.as_ref().map(|history| {
                history
                    .compaction
                    .lock()
                    .expect("original compaction")
                    .observe(&mut notification.update)
            });
            match fold {
                Some(CompactionFold::Skip) => return,
                Some(CompactionFold::Patch(event)) => {
                    let _ = historical.send(*event).await;
                    return;
                }
                _ => {}
            }
        }
        if !suppress_replay && history.is_some() && current_scope.is_some() {
            self.capture_rate_limit(&notification.update);
        }
        if !suppress_replay && history.is_some() {
            let tool_activity = match &notification.update {
                SessionUpdate::ToolCall(tool) => Some((
                    tool.tool_call_id.0.as_ref(),
                    !matches!(
                        tool.status,
                        ToolCallStatus::Completed | ToolCallStatus::Failed
                    ),
                )),
                SessionUpdate::ToolCallUpdate(tool) => tool.fields.status.map(|status| {
                    (
                        tool.tool_call_id.0.as_ref(),
                        !matches!(status, ToolCallStatus::Completed | ToolCallStatus::Failed),
                    )
                }),
                _ => None,
            };
            if let Some((id, in_flight)) = tool_activity {
                let _ = super::command_loop::current_effect_wait(
                    current_scope.as_deref(),
                    true,
                    &self.connection_state,
                    || self.between_prompt.record_native_tool(id, in_flight),
                )
                .await;
            }
        }
        let session_metadata = matches!(
            &notification.update,
            SessionUpdate::CurrentModeUpdate(_)
                | SessionUpdate::UsageUpdate(_)
                | SessionUpdate::AvailableCommandsUpdate(_)
                | SessionUpdate::ConfigOptionUpdate(_)
        );
        let compaction_sender =
            current_scope
                .as_ref()
                .filter(|_| !suppress_replay)
                .map(|context| {
                    let mut metadata = metadata.clone();
                    metadata.scope = EventScope::SessionCurrent;
                    metadata.native_origin = context.origin();
                    self.event_tx
                        .with_scoped_metadata(metadata, Some(Arc::clone(context)))
                });
        let sender = if let (true, Some(context)) = (session_metadata, current_scope) {
            metadata.scope = EventScope::SessionCurrent;
            metadata.native_origin = context.origin();
            self.event_tx.with_scoped_metadata(metadata, Some(context))
        } else if let Some(context) = metadata_scope {
            self.event_tx.with_constructor(context)
        } else {
            historical
        };
        let restored = history
            .as_ref()
            .and_then(|history| {
                restore_wake_tool_title(&history.tool_context, &notification.update)
            })
            .and_then(|title| restored_wake_tool_event(self.profile, &notification.update, &title));
        let native_session = notification.session_id;
        let mut tool_input = history
            .as_ref()
            .and_then(|_| capture_tool_context_input(&notification.update));
        let events = map_update_to_events(notification.update, self.profile)
            .into_iter()
            .chain(restored);
        // Late callback bookkeeping remains history, not a new main turn.
        let failed_main_turn =
            !self.prompt_in_flight.load(Ordering::Acquire) && self.terminal_claim.failed();
        for event in events {
            if failed_main_turn && matches!(event, Event::ThinkingStarted) {
                continue;
            }
            if suppress_replay && is_transcript_event(&event) {
                continue;
            }
            let sender = if matches!(
                event,
                Event::ConversationCompactionStarted
                    | Event::ConversationCompacted
                    | Event::ConversationCompactionFailed
            ) {
                compaction_sender.as_ref().unwrap_or(&sender)
            } else {
                &sender
            };
            if let Some(start) = &resource_start {
                if let Event::BackgroundAgentLaunched {
                    agent_id,
                    tool_call_id,
                    output_file,
                    ..
                } = &event
                {
                    if !output_file.is_empty() {
                        let source = match &self.bg_transcript_source {
                            TranscriptSource::Host => NativeResourceLocation::Host,
                            TranscriptSource::Container { runtime, container } => {
                                NativeResourceLocation::Container {
                                    runtime: runtime.to_string(),
                                    container: container.clone(),
                                }
                            }
                        };
                        if let Ok(authority) = NativeResourceAuthority::start(
                            Arc::clone(start),
                            NativeResourceSelector::Agent {
                                agent_id: agent_id.clone(),
                                tool_call_id: tool_call_id.clone(),
                                output_file: output_file.clone(),
                                source,
                            },
                        ) {
                            spawn_tailer(
                                (agent_id.clone(), output_file.clone()),
                                self.bg_transcript_source.clone(),
                                sender.clone(),
                                Arc::clone(&self.between_prompt.bg_agents),
                                TailerStart::Live,
                                authority,
                                Some(event),
                            )
                            .await;
                            continue;
                        }
                    }
                }
                if let (
                    Event::BackgroundTaskStarted {
                        task_id,
                        tool_call_id,
                        workflow,
                        ..
                    },
                    TranscriptSource::Host,
                    Some(home),
                ) = (&event, &self.bg_transcript_source, &self.claude_home)
                {
                    let selector = NativeResourceSelector::Task {
                        task_id: task_id.clone(),
                        tool_call_id: tool_call_id.clone(),
                        session_id: native_session.0.to_string(),
                        claude_home: home.clone(),
                        workflow: workflow.as_ref().map(|workflow| NativeWorkflowSelector {
                            run_id: workflow.run_id.clone(),
                            transcript_dir: workflow.transcript_dir.clone(),
                        }),
                    };
                    if let Ok(authority) =
                        NativeResourceAuthority::start(Arc::clone(start), selector)
                    {
                        spawn_task_tailer(
                            BackgroundTaskLaunch {
                                task_id: task_id.clone(),
                                tool_call_id: tool_call_id.clone(),
                                workflow: workflow.clone(),
                                workflow_agents: Default::default(),
                                witness: None,
                            },
                            native_session.0.to_string(),
                            Some(home.clone()),
                            Arc::clone(&self.bg_tasks),
                            Arc::clone(&self.between_prompt.bg_agents),
                            sender.downgrade(),
                            TailerStart::Live,
                            Some(event),
                            authority,
                        )
                        .await;
                        continue;
                    }
                }
            }
            if let Some(history) = &history {
                update_tool_context_cache(&history.tool_context, &event, &mut tool_input);
            }
            if sender.send(event).await.is_err() {
                break;
            }
        }
    }

    /// `local_prompt_signals` is false for a replayed update committed with
    /// the session: it belongs to no local prompt, so it must not reach the
    /// per-prompt watchdog.
    async fn handle_sdk_notification(
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
        let mut tool_input = capture_tool_context_input(&notification.update);
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
            update_tool_context_cache(&self.tool_context_cache, &event, &mut tool_input);
            if self.event_tx.send(event).await.is_err() {
                break;
            }
        }
    }

    /// Retained selectors, not current configuration, choose the resumed source.
    pub(super) async fn resume_task_tailing(
        &self,
        tasks: Vec<BackgroundTaskLaunch>,
        receipt: Option<Arc<NativeEstablishmentReceipt>>,
    ) {
        let Some(receipt) = receipt.as_deref() else {
            return;
        };
        for task in tasks {
            let Some(witness) = task.witness.as_ref() else {
                continue;
            };
            let authority = match NativeResourceAuthority::adopt(Arc::clone(witness), receipt) {
                Ok(authority) => authority,
                Err(error) => {
                    tracing::warn!(target: "acp.resource", task = %task.task_id, %error,
                        "retained tracking allocation was not adopted");
                    continue;
                }
            };
            let NativeResourceSelector::Task {
                session_id,
                claude_home,
                ..
            } = authority.selector()
            else {
                continue;
            };
            let native_session = session_id.clone();
            let home = Some(claude_home.clone());
            spawn_task_tailer(
                task,
                native_session,
                home,
                Arc::clone(&self.bg_tasks),
                Arc::clone(&self.between_prompt.bg_agents),
                self.event_tx.downgrade(),
                TailerStart::Resumed,
                None,
                authority,
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
