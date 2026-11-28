//! Turn state shared between the notification handler and the command loop,
//! and the `session/update` handler itself.

use super::subagents::BackgroundReport;
use crate::acp::agent_profiles::AgentProfile;
use crate::acp::background_agent::{spawn_tailer, TailerStart, TranscriptSource};
use crate::acp::establishment::{
    NativeEstablishmentReceipt, NativeResourceAuthority, NativeResourceLocation,
    NativeResourceSelector, NativeWorkflowSelector,
};
use crate::acp::event_sender::{EventMetadata, EventScope, EventSender};
use crate::acp::state::{BackgroundTaskLaunch, Event, SubagentRecord};
use crate::acp::task_transcript::{spawn_task_tailer, ActiveTasks};
use agent_client_protocol::schema::MaybeUndefined;
use agent_client_protocol::v1::schema::{
    ContentBlock, SessionId, SessionNotification, SessionUpdate, ToolCallStatus,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::debug;

use crate::acp::acp_client::aoe_ingress::{AoeIngress, Ingested};
use crate::acp::acp_client::between_prompt::BetweenPromptTracker;
use crate::acp::acp_client::control::{
    publish_adopted_completion, AdoptedCompletionPublication, DaemonControlClient,
};
use crate::acp::acp_client::lifecycle::{
    classify_lifecycle_signal, forward_lifecycle_signals, LifecycleEnvelope, LifecycleSignal,
    TerminalClaim,
};
use crate::acp::acp_client::rate_limit::{record_rate_limit_report, AgentTurnText};
use crate::acp::acp_client::session_identity::{IngressUpdate, PendingIngress, SessionIngress};
use crate::acp::acp_client::session_sandbox::SessionSandbox;
use crate::acp::acp_client::tool_context::{
    capture_tool_context_input, restore_wake_tool_title, shell_command, update_tool_context_cache,
    ToolCallContextCache, ToolContextCache,
};
use crate::acp::acp_client::transcript_filter::{is_transcript_event, transcript_event_kind};
use crate::acp::acp_client::update_events::{
    map_update_to_events, qualify_compaction_id, restored_wake_tool_event, AgentMessageDedup,
    CompactionFold, CompactionTracker,
};
use crate::acp::acp_client::watchdog::classify_watchdog_notification_signals;
use crate::acp::acp_client::SessionState;

pub(super) fn now_ms() -> i64 {
    crate::time::Timestamp::now().as_millis()
}

pub(super) struct Shared {
    pub(super) event_tx: EventSender,
    pub(super) state: Arc<SessionState>,
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
    /// SDK reset epochs, cleared at each prompt. Native captures belong to
    /// their admitted original and Written window in ingress.
    pub(super) rate_limit_rejections: Arc<std::sync::Mutex<HashMap<String, i64>>>,
    pub(super) cancel_deadline: Arc<std::sync::Mutex<Option<crate::acp::acp_client::CancelGrace>>>,
    #[cfg(test)]
    pub(super) cancel_send_gate: Arc<tokio::sync::RwLock<()>>,
    /// Scoped to one turn; see `AgentMessageDedup` (#2281).
    pub(super) agent_msg_dedup: std::sync::Mutex<AgentMessageDedup>,
    pub(super) agent_turn_text: std::sync::Mutex<AgentTurnText>,
    /// Native runtime uses only current-qualified updates; original history folds separately.
    compaction: std::sync::Mutex<CompactionTracker>,
    pub(super) tool_context_cache: ToolContextCache,
    /// Merged snapshot per subagent session; see `subagents.rs`.
    pub(super) subagents: std::sync::Mutex<HashMap<String, SubagentRecord>>,
    /// Background children whose card waits for their final answer; see
    /// `subagents.rs`.
    pub(super) background_reports: std::sync::Mutex<HashMap<String, BackgroundReport>>,
    /// Epoch-ms of the last validated child traffic.
    pub(super) subagent_activity_at: AtomicI64,
    /// Epoch-ms of each child's own last validated traffic.
    pub(super) subagent_seen: std::sync::Mutex<HashMap<String, i64>>,
    bg_transcript_source: TranscriptSource,
    pub(super) bg_tasks: Arc<ActiveTasks>,
    pub(super) aoe: AoeIngress,
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
        state: Arc<SessionState>,
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
            terminal_claim: state.terminal_claim.clone(),
            adopted_turn_active: AtomicBool::new(false),
            prompt_in_flight,
            local_prompt_settled: tokio::sync::Notify::new(),
            prompt_epoch: AtomicU64::new(0),
            lifecycle_tx,
            between_prompt: BetweenPromptTracker::default(),
            rate_limit_rejections: state.rate_limit_rejections.clone(),
            cancel_deadline: state.cancel_deadline.clone(),
            #[cfg(test)]
            cancel_send_gate: state.cancel_send_gate.clone(),
            agent_msg_dedup: Default::default(),
            agent_turn_text: Default::default(),
            compaction: Default::default(),
            tool_context_cache: Arc::new(std::sync::Mutex::new(ToolCallContextCache::default())),
            subagents: Default::default(),
            background_reports: Default::default(),
            subagent_activity_at: AtomicI64::new(0),
            subagent_seen: Default::default(),
            bg_transcript_source,
            bg_tasks: Arc::default(),
            aoe: AoeIngress::default(),
            claude_home,
            state,
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
        self.observe_turn_text(&event);
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
    pub(super) async fn emit_while_available(
        &self,
        events: impl IntoIterator<Item = Event>,
    ) -> bool {
        let turn = self.terminal_claim.current_turn();
        let failed = self.terminal_claim.failed();
        let _publication = self.state.publication.lock().await;
        for event in events {
            let Ok(permit) = self.event_tx.reserve().await else {
                return false;
            };
            if self
                .connection_state
                .admit(|| {
                    self.terminal_claim.publish_claimed(turn, failed, || {
                        permit.send(event);
                    })
                })
                .ok()
                != Some(true)
            {
                return false;
            }
        }
        true
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

    fn direct_proxy_exited(&self) -> bool {
        self.control.is_none() && self.connection_state.sdk_is_closing() && self.aoe.is_proxied()
    }

    pub(super) fn unavailable_ready(
        &self,
        observation: super::AcpConnectionUnavailability,
    ) -> Result<(), super::AcpError> {
        if self.direct_proxy_exited() {
            // A proved proxy exit must never trigger raw fallback.
            Ok(())
        } else {
            Err(super::AcpError::ConnectionUnavailable(observation))
        }
    }

    pub(super) async fn publish_connection_unavailable(&self) {
        self.wait_local_prompt_settled().await;
        if self.connection_state.is_stopping() {
            return;
        }
        if let Some(observation) = self.connection_state.take_unreported_loss() {
            self.pending_responders.lock().await.clear();
            if observation.source != crate::acp::state::AcpConnectionSource::Control {
                self.terminal_claim.claim();
                self.adopted_turn_active.store(false, Ordering::Release);
                self.between_prompt.deactivate();
            }
            if self.direct_proxy_exited() {
                let _ = self
                    .event_tx
                    .send(Event::IntegrationLost {
                        reason: crate::acp::state::IntegrationLostReason::ProxyExited,
                    })
                    .await;
            } else {
                let _ = self
                    .event_tx
                    .send_control_plane_loss(Arc::clone(&self.connection_state), observation)
                    .await;
            }
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
        let changed = self
            .ingress
            .current()
            .as_ref()
            .is_some_and(|current| current != &id);
        let replay = self.ingress.finish(Some(id))?;
        if changed {
            self.aoe.session_reset();
            self.settle_discarded().await;
        }
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

    /// Record traffic only after its dispatcher admits the source and replay fences.
    fn record_child_activity(&self, child: &SessionId) {
        let now = now_ms();
        let mut seen = self
            .subagent_seen
            .lock()
            .expect("subagent seen mutex poisoned");
        if let Some(last) = seen.get_mut(child.0.as_ref()) {
            *last = now;
        } else {
            seen.insert(child.0.to_string(), now);
        }
        self.subagent_activity_at.store(now, Ordering::Relaxed);
    }

    pub(super) async fn note_child_activity(
        &self,
        child: &SessionId,
        local_prompt_signals: bool,
        epoch: u64,
    ) {
        if self.control.is_some() {
            return;
        }
        self.record_child_activity(child);
        if self.prompt_in_flight.load(Ordering::Relaxed) {
            forward_lifecycle_signals(
                local_prompt_signals && epoch != 0,
                &self.lifecycle_tx,
                epoch,
                Some(LifecycleSignal::SubagentActivity),
                None,
                &self.session_label,
            )
            .await;
        } else {
            self.arm_for_subagent_activity().await;
        }
    }

    pub(super) async fn arm_for_subagent_activity(&self) {
        if self.control.is_some() {
            return;
        }
        let _publication = self.state.publication.lock().await;
        self.between_prompt.observe(
            Some(&LifecycleSignal::SubagentActivity),
            None,
            now_ms(),
            self.adopted_turn_active.load(Ordering::Relaxed),
            &self.terminal_claim,
        );
    }

    pub(super) fn observe_turn_text(&self, event: &Event) {
        self.agent_turn_text
            .lock()
            .expect("agent turn text mutex poisoned")
            .observe(event);
    }

    /// A claimed terminal must reach the drain even if SDK teardown cancels its caller.
    pub(super) async fn publish_terminal(
        self: &Arc<Self>,
        turn: u64,
        sender: EventSender,
        scope: Option<crate::acp::establishment::NativeRequestContext>,
        reason: String,
        preceding: Vec<Event>,
        result_observed: bool,
    ) -> bool {
        let shared = Arc::clone(self);
        let publication = tokio::spawn(async move {
            let _publication = shared.state.publication.lock().await;
            if shared.terminal_claim.claimed_turn() != Some(turn) {
                return false;
            }
            for event in preceding
                .into_iter()
                .chain(std::iter::once(Event::Stopped { reason }))
            {
                shared.observe_turn_text(&event);
                if !super::command_loop::publish_scoped(
                    &shared,
                    shared.control.is_some(),
                    &scope,
                    &sender,
                    event,
                    !result_observed,
                )
                .await
                {
                    return false;
                }
            }
            #[cfg(test)]
            {
                let gate = shared.state.terminal_before_clear.lock().unwrap().take();
                if let Some((entered, release)) = gate {
                    let _ = entered.send(());
                    release
                        .await
                        .expect("release original terminal publication cleanup");
                }
            }
            shared.terminal_claim.with_current_epoch(turn, || {
                *shared.cancel_deadline.lock().unwrap() = None;
                shared.prompt_in_flight.store(false, Ordering::Release);
                shared.local_prompt_settled.notify_waiters();
            });
            true
        });
        #[cfg(test)]
        let _settle = self.state.terminal_settle_gate.read().await;
        publication.await.expect("terminal publisher completes")
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
            PendingIngress::AdapterFramesShed(count) => {
                self.ingress.skip_adapter_frames(count);
                self.settle_discarded().await;
            }
            PendingIngress::Autonomous { first, second } => {
                for notification in [Some(first), second].into_iter().flatten() {
                    let sender = self
                        .event_tx
                        .with_autonomous(notification.metadata, notification.constructor_scope);
                    let _ = sender.send(notification.event).await;
                }
            }
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
                    let control = Arc::clone(control);
                    let events = self.event_tx.clone();
                    let in_flight = Arc::clone(&self.prompt_in_flight);
                    let state = Arc::clone(&self.state);
                    let ingress = Arc::clone(&self.ingress);
                    let published = tokio::spawn(async move {
                        let publication = if adopted.turn.is_some() {
                            Some(state.publication.clone().lock_owned().await)
                        } else {
                            None
                        };
                        let result = publish_adopted_completion(
                            *adopted,
                            &events,
                            &control,
                            &in_flight,
                            &state.terminal_claim,
                            |evidence| {
                                ingress
                                    .take_original_rate_limit_reset(
                                        evidence,
                                        crate::time::Timestamp::now(),
                                    )
                                    .map(|captured| (captured.window, captured.resets_at))
                            },
                            publication,
                        )
                        .await;
                        if let AdoptedCompletionPublication::Settled(turn) = result {
                            state.terminal_claim.with_current_epoch(turn, || {
                                *state.cancel_deadline.lock().unwrap() = None;
                            });
                        }
                        result
                    })
                    .await
                    .expect("terminal publisher completes");
                    if let AdoptedCompletionPublication::Settled(turn) = published {
                        self.terminal_claim.with_current_epoch(turn, || {
                            self.adopted_turn_active.store(false, Ordering::Release);
                            self.local_prompt_settled.notify_waiters();
                        });
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
        let (update, local_prompt_signals) = if self.control.is_some() {
            let Some(update) = self.prepare_native_notification(update).await else {
                return;
            };
            if !self.ingress.capture_original_rate_limit(&update) {
                return;
            }
            let local_prompt_signals = local_prompt_signals && update.current_scope.is_some();
            (update, local_prompt_signals)
        } else {
            (update, local_prompt_signals)
        };
        self.ingest_notification(update, local_prompt_signals).await;
    }

    async fn prepare_native_notification(
        &self,
        mut update: IngressUpdate,
    ) -> Option<IngressUpdate> {
        // Child traffic is never interpreted by the root contract or ordering gate.
        update.child_route = update.child_route.take().filter(|(_, route)| {
            route.source == update.original_source
                && route.source
                    == update
                        .evidence
                        .physical_source
                        .map(|physical| physical.witness)
                && route
                    .provenance
                    .as_ref()
                    .is_some_and(|proof| Some(proof.origin) == update.original_source)
        });
        if let Some(route) = &update.child_route {
            if !update.suppress_replay {
                if let Some(context) = update
                    .current_scope
                    .as_ref()
                    .filter(|context| context.origin() == route.1.source)
                {
                    if let Ok(_guard) = context.try_effect_guard() {
                        self.record_child_activity(&route.0);
                    }
                }
            }
            let mut metadata = EventMetadata::history();
            metadata.native_origin = update.evidence.physical_source.map(|source| source.witness);
            metadata.reported_session_id = Some(update.notification.session_id.clone());
            metadata.prompt_observation = update.evidence.prompt_observation;
            metadata.child_provenance = route.1.provenance.clone();
            let sender = match update.current_scope.as_ref() {
                Some(context) => {
                    metadata.scope = EventScope::SessionCurrent;
                    self.event_tx
                        .with_scoped_metadata(metadata, Some(Arc::clone(context)))
                }
                None => self.event_tx.with_metadata(metadata),
            };
            match update.notification.update {
                SessionUpdate::SubagentUpdate(child) => {
                    self.handle_subagent_update(
                        &sender,
                        route,
                        update.notification.session_id,
                        child,
                    )
                    .await;
                    return None;
                }
                SessionUpdate::SessionMessage(message) => {
                    self.handle_session_message(
                        &sender,
                        route,
                        update.notification.session_id,
                        message,
                    )
                    .await;
                    return None;
                }
                SessionUpdate::SessionMessageChunk(_) => return None,
                child if route.0 == update.notification.session_id => {
                    self.handle_child_notification(
                        &sender,
                        route,
                        update.notification.session_id,
                        child,
                    )
                    .await;
                    return None;
                }
                other => update.notification.update = other,
            }
        } else if matches!(
            update.notification.update,
            SessionUpdate::SubagentUpdate(_)
                | SessionUpdate::SessionMessage(_)
                | SessionUpdate::SessionMessageChunk(_)
        ) {
            return None;
        }
        Some(update)
    }

    async fn handle_native_notification(&self, update: IngressUpdate) {
        let IngressUpdate {
            mut notification,
            evidence,
            original_source: _,
            child_route,
            current_scope,
            metadata_scope,
            resource_start,
            history,
            suppress_replay,
        } = update;
        // A fork prefix freezes only its original custody before the result.
        // Certify its child identity now, without refreshing any Current scope.
        let history =
            history.filter(|history| history.accepts_session(notification.session_id.0.as_ref()));
        let mut metadata = EventMetadata::history();
        metadata.native_origin = evidence.physical_source.map(|source| source.witness);
        metadata.reported_session_id = Some(notification.session_id.clone());
        metadata.prompt_observation = evidence.prompt_observation;
        metadata.child_provenance = child_route.and_then(|(_, route)| route.provenance);
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
        let sender = if matches!(notification.update, SessionUpdate::UserMessageChunk(_)) {
            historical
        } else if let (true, Some(context)) = (session_metadata, current_scope.as_ref()) {
            metadata.scope = EventScope::SessionCurrent;
            metadata.native_origin = context.origin();
            self.event_tx
                .with_scoped_metadata(metadata, Some(Arc::clone(context)))
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
        // A local prompt's live/late echoes are already journaled. Independent
        // Native user text is retained by this historical sender, never admitted input.
        if matches!(notification.update, SessionUpdate::UserMessageChunk(_))
            && !(history.is_some() && self.seed_user_replay.load(Ordering::Relaxed))
            && (self.prompt_in_flight.load(Ordering::Acquire)
                || self.is_late_echo(&notification.update))
        {
            return;
        }
        let events = map_update_to_events(notification.update, self.profile)
            .into_iter()
            .chain(restored);
        // Late callback bookkeeping remains history, not a new main turn.
        let failed_main_turn =
            !self.prompt_in_flight.load(Ordering::Acquire) && self.terminal_claim.failed();
        for mut event in events {
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
                    if !output_file.is_empty() && !self.profile.supports_subagents {
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
            // A qualified `TaskStop` ends tracking; only the tailer publishes
            // the end after settling the task's agents.
            if let Event::BackgroundTaskFinished { task_id, .. } = &event {
                let _ = super::command_loop::current_effect_wait(
                    current_scope.as_deref(),
                    true,
                    &self.connection_state,
                    || self.bg_tasks.stop(task_id),
                )
                .await;
                continue;
            }
            if let Some(history) = &history {
                update_tool_context_cache(&history.tool_context, &mut event, &mut tool_input);
            }
            if sender.send(event).await.is_err() {
                break;
            }
        }
    }
    /// `local_prompt_signals` is false for a replayed update committed with
    /// the session: it belongs to no local prompt, so it must not reach the
    /// per-prompt watchdog.
    async fn ingest_notification(&self, notification: IngressUpdate, local_prompt_signals: bool) {
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
            if self.control.is_some() {
                self.handle_native_notification(frame.notification).await;
            } else {
                self.handle_adapter_notification(
                    frame.notification.notification,
                    frame.local_prompt_signals,
                    frame.epoch,
                )
                .await;
            }
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
        // Subagent traffic never feeds the root session's dedup or compaction
        // state; it only proves the parent's turn is still live.
        let from_child = self
            .ingress
            .child_route(&notification.session_id)
            .map(|route| (notification.session_id.clone(), route));
        let suppressing = self.suppress_history_replay.load(Ordering::Relaxed);
        // Replayed children died with the old adapter; ingress already routed them.
        if suppressing
            && (from_child.is_some()
                || matches!(
                    notification.update,
                    SessionUpdate::SubagentUpdate(_)
                        | SessionUpdate::SessionMessage(_)
                        | SessionUpdate::SessionMessageChunk(_)
                ))
        {
            tracing::trace!(
                target: "acp.protocol",
                session = %self.session_label,
                "dropping post-load subagent replay frame"
            );
            return;
        }
        match notification.update {
            SessionUpdate::SubagentUpdate(update) => {
                let Some(route) = self
                    .ingress
                    .child_route(&update.session_id)
                    .map(|route| (update.session_id.clone(), route))
                else {
                    return;
                };
                if route.1.parent != notification.session_id {
                    return;
                }
                self.note_child_activity(&route.0, local_prompt_signals, epoch)
                    .await;
                return self
                    .handle_subagent_update(&self.event_tx, &route, notification.session_id, update)
                    .await;
            }
            SessionUpdate::SessionMessage(message) => {
                let child = match (&message.sender_session_id, &message.recipient_session_id) {
                    (MaybeUndefined::Value(sender), MaybeUndefined::Value(recipient))
                        if sender == &notification.session_id =>
                    {
                        recipient
                    }
                    (MaybeUndefined::Value(sender), MaybeUndefined::Value(recipient))
                        if recipient == &notification.session_id =>
                    {
                        sender
                    }
                    _ => return,
                };
                let Some(route) = self
                    .ingress
                    .child_route(child)
                    .map(|route| (child.clone(), route))
                else {
                    return;
                };
                if route.1.parent != notification.session_id {
                    return;
                }
                self.note_child_activity(&route.0, local_prompt_signals, epoch)
                    .await;
                return self
                    .handle_session_message(
                        &self.event_tx,
                        &route,
                        notification.session_id,
                        message,
                    )
                    .await;
            }
            SessionUpdate::SessionMessageChunk(_) if from_child.is_none() => return,
            update if from_child.is_some() => {
                self.note_child_activity(&notification.session_id, local_prompt_signals, epoch)
                    .await;
                return self
                    .handle_child_notification(
                        &self.event_tx,
                        from_child.as_ref().expect("matched child route"),
                        notification.session_id,
                        update,
                    )
                    .await;
            }
            update => notification.update = update,
        }
        self.last_event_at.store(now_ms(), Ordering::Relaxed);
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
        let publication = self.state.publication.lock().await;
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
        drop(publication);
        self.capture_rate_limit(&notification.update);
        let mut tool_input = capture_tool_context_input(&notification.update);
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
            // Only `TaskStop` ends a task here; the tailer publishes the end.
            if let Event::BackgroundTaskFinished { task_id, .. } = &event {
                self.bg_tasks.stop(task_id);
                continue;
            }
            update_tool_context_cache(&self.tool_context_cache, &mut event, &mut tool_input);
            if !suppressing {
                self.observe_turn_text(&event);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::v1::schema::{ContentChunk, TextContent};

    fn echo_shared(
        prompt_in_flight: bool,
    ) -> (Shared, mpsc::Receiver<crate::acp::event_sender::SdkEvent>) {
        let (event_tx, event_rx) = mpsc::channel(8);
        let (lifecycle_tx, _lifecycle_rx) = mpsc::channel(8);
        let shared = Shared::new(
            event_tx.into(),
            "s-echo".to_string(),
            &crate::acp::agent_profiles::CLAUDE,
            lifecycle_tx,
            Arc::new(SessionState::default()),
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
        mut event_rx: mpsc::Receiver<crate::acp::event_sender::SdkEvent>,
        local_prompt_signals: bool,
    ) -> Vec<Event> {
        let chunk = ContentChunk::new(ContentBlock::Text(TextContent::new("hello")));
        shared
            .ingest_notification(
                crate::acp::acp_client::session_identity::SessionIngressNotification::decode_update(
                    serde_json::to_value(SessionNotification::new("native", SessionUpdate::UserMessageChunk(chunk))).unwrap(),
                    false,
                ).unwrap().0,
                local_prompt_signals,
            )
            .await;
        drop(shared);
        let mut events = Vec::new();
        while let Some(event) = event_rx.recv().await {
            let crate::acp::event_sender::SdkEvent::Data(data) = event else {
                panic!("user echo produced a private SDK barrier");
            };
            events.push(Arc::try_unwrap(data.event).unwrap());
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
