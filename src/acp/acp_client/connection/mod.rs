//! The long-lived connection task: initialize once, establish one session,
//! then pump commands into ACP requests until shutdown.
//!
//! [`notifications::Shared`] is the turn state the notification handler and
//! the command loop both touch; [`establish`] runs the handshake;
//! [`command_loop::Session`] owns the established session, running each
//! prompt through [`prompt`].

mod command_loop;
mod establish;
mod notifications;
mod prompt;
mod subagents;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use prompt::{SelectProbe as TestSelectProbe, SELECT_PROBE as TEST_SELECT_PROBE};

use crate::acp::agent_compat::ExpectedAgent;
use crate::acp::agent_profiles::AgentProfile;
use crate::acp::event_sender::EventSender;
use crate::acp::state::{
    AcpConnectionCause, AcpConnectionPhase, AcpConnectionSource, AcpConnectionUnavailability, Event,
};
use agent_client_protocol::v1::schema::{
    CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, ElicitationScope, KillTerminalRequest, KillTerminalResponse, McpServer,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::v1::{ConnectionTo, Responder};
use agent_client_protocol::{Agent, ByteStreams, Client};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{error, info, warn};

use super::commands::{ConnectMode, QueuedClientCmd};
use super::control::DaemonControlClient;
use super::errors::{acp_internal_error, AcpError};
use super::fs_handlers::{handle_read_text_file, handle_write_text_file};
use super::pending::PendingResponders;
use super::permission_handlers::{handle_elicitation_request, handle_permission_request};
use super::rate_limit::{captured_rate_limit_reset, classify_rate_limit_error};
use super::session_identity::{
    spawn_native_callback, PendingIngress, RequestPolicy, SessionIngress,
    SessionIngressNotification,
};
use super::terminal_handlers::{
    handle_create_terminal, handle_kill_terminal, handle_release_terminal, handle_terminal_output,
    handle_wait_for_terminal_exit,
};
use super::{SessionResources, SessionState};
use notifications::Shared;

/// After a cancel, declare the adapter unresponsive if no prompt response
/// arrives. A transport-wedge defense even for adapters that cancel promptly.
pub(crate) const CANCEL_ESCALATION_GRACE: Duration = Duration::from_secs(10);

/// A detached runner's control channel, which owns the ACP handshake and the
/// turn; everything else is relayed through the crate transport.
/// Taken once, by whichever of the handshake or the failure path answers first.
pub(super) type ReadyTx = Mutex<Option<oneshot::Sender<Result<(), AcpError>>>>;

#[derive(Default)]
pub(crate) struct ConnectionState {
    phase: AtomicU8,
    unavailable: OnceLock<AcpConnectionUnavailability>,
    reported: AtomicBool,
    usable: AtomicBool,
    bootstrap_finished: AtomicBool,
    ordered_loss: AtomicBool,
    intentional_stop: AtomicBool,
    foreground_finished: AtomicBool,
    establishment_declined: AtomicBool,
    pub(super) original_establishment_rejection:
        std::sync::Mutex<Option<(crate::acp::establishment::AttemptId, Arc<serde_json::Value>)>>,
    pub(super) admission: std::sync::Mutex<()>,
    pub(super) initial_prompt: std::sync::Mutex<Option<super::commands::InitialPromptState>>,
    sdk_closing: AtomicBool,
    unavailable_wake: tokio::sync::Notify,
    ordered_loss_wake: tokio::sync::Notify,
    completed_outcomes: std::sync::atomic::AtomicUsize,
    pub(super) callback_duration: Arc<tokio::sync::RwLock<()>>,
    native_durations: std::sync::Mutex<NativeDurations>,
    completed_outcomes_wake: tokio::sync::Notify,
    pub(super) projection: OnceLock<Arc<crate::acp::event_sender::ProjectionState>>,
    pub(super) control: OnceLock<std::sync::Weak<DaemonControlClient>>,
    #[cfg(test)]
    pub(super) test_callback: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_reverse_send: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_default_publish: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_initialize_response_application: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_fresh_response_application: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_establishment_usable: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_fresh_write_pending: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    #[cfg(test)]
    pub(super) test_prompt_admission: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_prompt_admission_busy: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    #[cfg(test)]
    pub(super) test_queue: OnceLock<crate::acp::event_sender::WeakEventSender>,
    #[cfg(test)]
    pub(super) test_shim: OnceLock<Arc<Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>>>,
    #[cfg(test)]
    pub(super) test_relay_observer:
        std::sync::Mutex<Option<mpsc::UnboundedSender<super::control::TestRelayObservation>>>,
    #[cfg(test)]
    pub(super) test_sdk_output: std::sync::Mutex<Option<mpsc::UnboundedSender<serde_json::Value>>>,
    #[cfg(test)]
    pub(super) test_prompt_completed: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_control_output:
        std::sync::Mutex<Option<mpsc::UnboundedSender<crate::acp::control_protocol::ControlBody>>>,
    #[cfg(test)]
    pub(super) test_native_session_seen: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    #[cfg(test)]
    pub(super) test_update: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(crate) test_connection_loss_published: std::sync::Mutex<Option<super::SdkTestGate>>,
    #[cfg(test)]
    pub(super) test_session_update: std::sync::Mutex<
        Option<(
            agent_client_protocol::v1::schema::SessionId,
            super::SdkTestGate,
        )>,
    >,
    #[cfg(test)]
    pub(super) test_terminal: OnceLock<Arc<super::lifecycle::TerminalClaim>>,
    #[cfg(test)]
    pub(super) test_in_flight: OnceLock<Arc<AtomicBool>>,
}

#[derive(Default)]
struct NativeDurations {
    guard: std::sync::Weak<tokio::sync::OwnedRwLockReadGuard<()>>,
    operations: usize,
}

pub(super) struct NativeOperationDuration {
    state: Arc<ConnectionState>,
    _duration: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
}

impl Drop for NativeOperationDuration {
    fn drop(&mut self) {
        self.state.native_durations.lock().unwrap().operations -= 1;
    }
}

pub(super) enum CallbackDuration {
    Direct {
        _duration: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Inherited {
        _duration: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    },
}

impl ConnectionState {
    pub(super) fn initial_prompt_cancellation(
        &self,
        correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
    ) -> Option<tokio_util::sync::CancellationToken> {
        let correlation = correlation?;
        let slot = self.initial_prompt.lock().unwrap();
        slot.as_ref()
            .filter(|pending| pending.correlation == Some(correlation))
            .map(|pending| pending.cancellation.clone())
    }

    pub(super) fn begin_initial_prompt_write(
        &self,
        correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
    ) -> bool {
        let mut slot = self.initial_prompt.lock().unwrap();
        if let Some(pending) = slot
            .as_mut()
            .filter(|pending| correlation.is_some() && pending.correlation == correlation)
        {
            if pending.cancellation.is_cancelled() {
                return false;
            }
            pending.issued = true;
        }
        true
    }

    pub(super) fn initial_prompt_issued(
        &self,
        correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
    ) -> bool {
        correlation.is_some()
            && self
                .initial_prompt
                .lock()
                .unwrap()
                .as_ref()
                .is_some_and(|pending| pending.correlation == correlation && pending.issued)
    }

    pub(super) fn initial_prompt_matches(
        &self,
        id: uuid::Uuid,
        correlation: crate::acp::establishment::NativeOperationCorrelation,
    ) -> bool {
        self.initial_prompt
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|pending| {
                pending.id == id
                    && pending.correlation == Some(correlation)
                    && pending.issued
                    && !pending.cancellation.is_cancelled()
            })
    }

    pub(super) fn finish_initial_prompt_operation(
        &self,
        correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
    ) {
        let mut slot = self.initial_prompt.lock().unwrap();
        if correlation.is_some()
            && slot
                .as_ref()
                .is_some_and(|pending| pending.correlation == correlation)
        {
            slot.take();
        }
    }

    pub(super) fn reserve_native_duration(
        self: &Arc<Self>,
    ) -> Result<NativeOperationDuration, AcpError> {
        let guard = Arc::clone(&self.callback_duration)
            .try_read_owned()
            .map_err(|_| AcpError::ResetFailed("conversation reset is in progress".into()))?;
        let mut durations = self.native_durations.lock().unwrap();
        let guard = durations.guard.upgrade().unwrap_or_else(|| Arc::new(guard));
        durations.guard = Arc::downgrade(&guard);
        durations.operations += 1;
        Ok(NativeOperationDuration {
            state: Arc::clone(self),
            _duration: guard,
        })
    }

    pub(super) fn callback_publication(&self) -> Option<CallbackDuration> {
        if let Ok(guard) = Arc::clone(&self.callback_duration).try_read_owned() {
            return Some(CallbackDuration::Direct { _duration: guard });
        }
        let durations = self.native_durations.lock().unwrap();
        // Fresh cannot arm while this operation's guard survives. Its callbacks
        // must finish without acquiring another fair read behind Fresh.
        (durations.operations != 0)
            .then(|| durations.guard.upgrade())
            .flatten()
            .map(|guard| CallbackDuration::Inherited { _duration: guard })
    }

    pub(crate) fn observed_native_origin(&self) -> Option<crate::acp::state::NativeEventOrigin> {
        self.control
            .get()?
            .upgrade()?
            .ingress
            .observed_native_origin()
    }

    pub(super) fn begin_completed_outcome(&self) {
        self.completed_outcomes.fetch_add(1, Ordering::AcqRel);
    }

    pub(super) fn finish_completed_outcome(&self) {
        if self.completed_outcomes.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.completed_outcomes_wake.notify_waiters();
        }
    }

    pub(super) async fn wait_completed_outcomes(&self) {
        loop {
            let wake = self.completed_outcomes_wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.is_stopping() {
                if let Some(projection) = self.projection.get() {
                    projection.original_delivered().await;
                }
                return;
            }
            if self.completed_outcomes.load(Ordering::Acquire) == 0 {
                return;
            }
            wake.await;
        }
    }

    pub(super) fn initialized(&self) {
        self.phase.store(1, Ordering::Release);
    }

    pub(super) fn established(&self) {
        self.phase.store(2, Ordering::Release);
    }

    pub(super) fn loss(&self) -> Option<AcpConnectionUnavailability> {
        self.unavailable.get().copied()
    }

    pub(super) fn ready(&self) -> bool {
        self.usable.load(Ordering::Acquire)
            && self.loss().is_none()
            && !self.is_stopping()
            && !self.driver_finished()
    }

    pub(super) fn initial_prompt_intent_available(&self) -> bool {
        self.phase.load(Ordering::Acquire) == 1
            && !self.usable.load(Ordering::Acquire)
            && self.loss().is_none()
            && !self.is_stopping()
            && !self.driver_finished()
    }

    pub(super) fn driver_finished(&self) -> bool {
        self.foreground_finished.load(Ordering::Acquire)
    }

    fn finish_foreground(&self) {
        self.foreground_finished.store(true, Ordering::Release);
        self.unavailable_wake.notify_waiters();
    }

    pub(super) fn decline_establishment(&self) {
        let _admission = self
            .admission
            .lock()
            .expect("connection admission poisoned");
        self.establishment_declined.store(true, Ordering::Release);
        self.unavailable_wake.notify_waiters();
    }

    pub(super) fn record_usable(&self) {
        self.usable.store(true, Ordering::Release);
        self.unavailable_wake.notify_waiters();
    }

    pub(super) fn finish_bootstrap(&self) {
        self.bootstrap_finished.store(true, Ordering::Release);
        self.unavailable_wake.notify_waiters();
    }

    pub(super) async fn wait_until_usable(&self) -> Result<(), AcpError> {
        loop {
            let changed = self.unavailable_wake.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.admit(|| self.ready() && self.bootstrap_finished.load(Ordering::Acquire))? {
                return Ok(());
            }
            changed.await;
        }
    }

    /// Intentional shutdown wins; otherwise a known loss survives foreground completion.
    /// Refusal never executes the operation or downgrades a retained transport cause.
    pub(super) fn admit<T>(&self, operation: impl FnOnce() -> T) -> Result<T, AcpError> {
        let _admission = self
            .admission
            .lock()
            .expect("connection admission poisoned");
        if self.intentional_stop.load(Ordering::Acquire) {
            return Err(AcpError::NotRunning);
        }
        if let Some(observation) = self.loss() {
            return Err(AcpError::ConnectionUnavailable(observation));
        }
        if self.driver_finished() {
            return Err(AcpError::NotRunning);
        }
        if self.establishment_declined.load(Ordering::Acquire) {
            return Err(crate::acp::establishment::EstablishmentRefusal::Unresolved.into());
        }
        Ok(operation())
    }

    pub(super) fn sdk_is_closing(&self) -> bool {
        self.sdk_closing.load(Ordering::Acquire)
    }

    pub(super) async fn wait_unavailable(&self) -> AcpConnectionUnavailability {
        loop {
            let unavailable = self.unavailable_wake.notified();
            tokio::pin!(unavailable);
            unavailable.as_mut().enable();
            if let Some(observation) = self.loss() {
                return observation;
            }
            unavailable.await;
        }
    }
    pub(super) async fn wait_callback_end(&self) {
        loop {
            let changed = self.unavailable_wake.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.establishment_declined.load(Ordering::Acquire)
                || self.is_stopping()
                || self.driver_finished()
                || self.loss().is_some()
            {
                return;
            }
            changed.await;
        }
    }

    pub(super) fn mark_ordered_loss(&self) {
        if self.loss().is_some() {
            self.ordered_loss.store(true, Ordering::Release);
            self.ordered_loss_wake.notify_waiters();
        }
    }

    pub(super) async fn wait_ordered_unavailable(&self) -> AcpConnectionUnavailability {
        loop {
            let ordered = self.ordered_loss_wake.notified();
            tokio::pin!(ordered);
            ordered.as_mut().enable();
            if self.ordered_loss.load(Ordering::Acquire) {
                return self.loss().expect("ordered connection observation");
            }
            ordered.await;
        }
    }

    pub(super) fn mark_stopping(&self) {
        self.intentional_stop.store(true, Ordering::Release);
        self.completed_outcomes_wake.notify_waiters();
        self.unavailable_wake.notify_waiters();
    }

    pub(super) fn is_stopping(&self) -> bool {
        self.intentional_stop.load(Ordering::Acquire)
    }

    pub(super) fn observe(&self, source: AcpConnectionSource, cause: AcpConnectionCause) {
        let _admission = self
            .admission
            .lock()
            .expect("connection admission poisoned");
        self.record_loss(source, cause);
    }

    /// Caller holds `admission`, including synchronous I/O-failure observation.
    pub(super) fn record_loss(&self, source: AcpConnectionSource, cause: AcpConnectionCause) {
        if source == AcpConnectionSource::Sdk
            && self.driver_finished()
            && self.establishment_declined.load(Ordering::Acquire)
        {
            return;
        }
        // A bridge ending because foreground already returned is teardown,
        // not an independent observation that a peer transport failed.
        if cause == AcpConnectionCause::ForwardPumpEnded && self.driver_finished() {
            return;
        }
        let phase = match self.phase.load(Ordering::Acquire) {
            0 => AcpConnectionPhase::Initializing,
            1 => AcpConnectionPhase::Establishing,
            _ => AcpConnectionPhase::Established,
        };
        if self
            .unavailable
            .set(AcpConnectionUnavailability {
                source,
                cause,
                phase,
            })
            .is_ok()
        {
            self.unavailable_wake.notify_waiters();
        }
    }

    fn take_unreported_loss(&self) -> Option<AcpConnectionUnavailability> {
        let observation = self.loss()?;
        if self.reported.swap(true, Ordering::AcqRel) {
            return None;
        }
        Some(observation)
    }
}
pub(super) struct RunnerLink {
    pub(super) control: Arc<DaemonControlClient>,
    pub(super) state: Arc<SessionState>,
    pub(super) prompt_in_flight: Arc<AtomicBool>,
}

pub(super) struct ConnectionParams {
    pub(super) event_tx: EventSender,
    pub(super) connection_state: Arc<ConnectionState>,
    pub(super) cmd_rx: mpsc::Receiver<QueuedClientCmd>,
    /// Killed when the task ends. `None` for a runner, which owns its agent
    /// and outlives this connection.
    pub(super) child: Option<Arc<Mutex<tokio::process::Child>>>,
    pub(super) pending_responders: PendingResponders,
    pub(super) resources: SessionResources,
    pub(super) mode: ConnectMode,
    pub(super) ready_tx: oneshot::Sender<Result<(), AcpError>>,
    pub(super) profile: &'static AgentProfile,
    pub(super) expected_agent: ExpectedAgent,
    pub(super) source_profile: Option<String>,
    pub(super) default_effort: Option<String>,
    pub(super) default_model_config: std::collections::BTreeMap<String, String>,
    pub(super) default_mode: Option<String>,
    pub(super) default_model: Option<String>,
    pub(super) mcp_servers: Vec<McpServer>,
    pub(super) claude_home: Option<PathBuf>,
    pub(super) runner: Option<RunnerLink>,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    pub(super) shutdown: tokio_util::sync::CancellationToken,
    #[cfg(test)]
    pub(super) tracking_observer: Option<oneshot::Sender<super::test_helpers::TrackingObservation>>,
}

#[cfg(test)]
pub(super) async fn test_local_silent_orphan_cancel_recovery() {
    use crate::acp::acp_client::commands::{ClientCmd, QueuedClientCmd};
    use crate::acp::event_sender::SdkEvent;
    use agent_client_protocol::v1::schema::{ContentBlock, TextContent};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

    struct LocalActor {
        peer: tokio::io::DuplexStream,
        commands: mpsc::Sender<QueuedClientCmd>,
        events: mpsc::Receiver<SdkEvent>,
        task: tokio::task::JoinHandle<agent_client_protocol::Result<()>>,
        state: Arc<ConnectionState>,
        in_flight: Arc<AtomicBool>,
    }

    async fn local_actor(
        retained: Arc<SessionState>,
        in_flight: bool,
        adopted: bool,
    ) -> LocalActor {
        let (daemon, peer) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(daemon);
        let (event_tx, events) = mpsc::channel(32);
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(32);
        let (commands, cmd_rx) = mpsc::channel(8);
        let state = Arc::new(ConnectionState::default());
        let in_flight = Arc::new(AtomicBool::new(in_flight));
        let id = SessionId::from("local-orphan");
        let ingress = Arc::new(SessionIngress::new(Some(id.clone())));
        let shared = Arc::new(Shared::new(
            event_tx.into(),
            "local-orphan".into(),
            &crate::acp::agent_profiles::GEMINI,
            lifecycle_tx,
            retained,
            Arc::clone(&in_flight),
            None,
            ingress,
            None,
            Arc::clone(&state),
            None,
            Arc::default(),
        ));
        shared.adopted_turn_active.store(adopted, Ordering::Release);
        let notifications = Arc::clone(&shared);
        let closed = Arc::clone(&state);
        let (entered, connected) = oneshot::channel();
        let task = tokio::spawn(async move {
            Client
                .v1()
                .name("local-orphan-actor-test")
                .on_close(move |_connection| {
                    let closed = Arc::clone(&closed);
                    async move {
                        closed.observe(AcpConnectionSource::Sdk, AcpConnectionCause::IncomingEnded);
                        closed.mark_ordered_loss();
                        Err(acp_internal_error(
                            "local test peer closed its actual duplex".into(),
                        ))
                    }
                })
                .on_receive_notification(
                    move |notification: SessionIngressNotification, _cx| {
                        let shared = Arc::clone(&notifications);
                        async move {
                            let SessionIngressNotification::Update(params) = notification else {
                                return Ok(());
                            };
                            let (notification, bytes) =
                                SessionIngressNotification::decode_update(params, false)?;
                            let notification =
                                shared.ingress.capture_notification(notification, false);
                            if let Some((notification, _guard)) =
                                shared.ingress.notification(notification, bytes).await?
                            {
                                shared.handle_notification(notification, true).await;
                            }
                            Ok(())
                        }
                    },
                    agent_client_protocol::on_receive_notification!(),
                )
                .connect_with(
                    ByteStreams::new(write.compat_write(), read.compat()),
                    move |connection: ConnectionTo<Agent>| async move {
                        entered.send(()).expect("local actor starts");
                        command_loop::Session {
                            connection,
                            shared,
                            control: None,
                            acp_session_id: id,
                            channels: super::config_options::SessionChannels::default(),
                            steering_capable: false,
                            close_capable: false,
                            source_profile: None,
                            default_effort: None,
                            default_model_config: Default::default(),
                            default_mode: None,
                            default_model: None,
                            agent_cwd: std::env::temp_dir(),
                            mcp_servers: Vec::new(),
                            cmd_rx,
                            lifecycle_rx,
                            pending_prompts: Default::default(),
                            pending_reset: None,
                        }
                        .run()
                        .await
                    },
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), connected)
            .await
            .unwrap()
            .unwrap();
        LocalActor {
            peer,
            commands,
            events,
            task,
            state,
            in_flight,
        }
    }

    async fn data(events: &mut mpsc::Receiver<SdkEvent>) -> Event {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .unwrap()
            .expect("local actor publishes");
        let SdkEvent::Data(data) = event else {
            panic!("a local actor cannot manufacture a private Native barrier");
        };
        assert_eq!(data.metadata.native_origin, None);
        assert_eq!(
            data.metadata
                .operation
                .and_then(|operation| operation.outcome),
            None
        );
        (*data.event).clone()
    }

    let retained = Arc::new(SessionState::default());
    let send_gate = retained.cancel_send_gate.write().await;
    let mut first = local_actor(Arc::clone(&retained), false, false).await;
    first
        .commands
        .send(QueuedClientCmd {
            command: ClientCmd::Prompt(
                vec![ContentBlock::Text(TextContent::new("local orphan"))].into(),
            ),
            scope: None,
            correlation: None,
        })
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        Duration::from_secs(5),
        BufReader::new(&mut first.peer).read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let prompt: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(prompt["method"], "session/prompt");
    assert_eq!(prompt["params"]["sessionId"], "local-orphan");
    let progress = serde_json::json!({
        "jsonrpc":"2.0","method":"session/update","params":{
            "sessionId":"local-orphan","update":{
                "sessionUpdate":"agent_message_chunk",
                "content":{"type":"text","text":"actual local progress"}
            }
        }
    });
    first
        .peer
        .write_all(format!("{progress}\n").as_bytes())
        .await
        .unwrap();
    loop {
        if matches!(data(&mut first.events).await, Event::AgentMessageChunk { text, .. } if text == "actual local progress")
        {
            break;
        }
    }
    let grace = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(grace) = retained.cancel_deadline.lock().unwrap().clone() {
                break grace;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual local orphan watchdog arms before blocked cancel send");
    assert!(
        grace.scope.is_none(),
        "only this local path has no Native request context"
    );
    assert_eq!(grace.turn, retained.terminal_claim.current_turn());
    assert!(!retained.terminal_claim.claimed());
    let captured_active = first.in_flight.load(Ordering::Acquire);
    assert!(captured_active);
    let mut byte = [0_u8; 1];
    assert!(
        tokio::time::timeout(Duration::from_millis(20), first.peer.read(&mut byte))
            .await
            .is_err(),
        "the held local cancel cannot reach its peer"
    );
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(6)).await;
    tokio::time::resume();
    first.peer.shutdown().await.unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), first.task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        result.is_err(),
        "actual local peer EOF terminates its original SDK"
    );
    assert!(first.state.loss().is_some());
    assert!(
        !retained.terminal_claim.claimed(),
        "EOF is not a successful local response"
    );
    while let Ok(event) = first.events.try_recv() {
        let SdkEvent::Data(data) = event else {
            panic!("unexpected Native barrier")
        };
        assert!(!matches!(
            data.event.as_ref(),
            Event::Stopped { .. } | Event::AgentStartupError { .. }
        ));
    }
    drop(send_gate);
    let mut second = local_actor(Arc::clone(&retained), captured_active, true).await;
    let recovered = retained.cancel_deadline.lock().unwrap().clone().unwrap();
    assert_eq!(recovered.deadline, grace.deadline);
    assert_eq!(recovered.turn, grace.turn);
    assert!(recovered.scope.is_none());
    assert!(
        tokio::time::timeout_at(
            grace.deadline - Duration::from_millis(1),
            second.events.recv()
        )
        .await
        .is_err(),
        "local recovery preserves only the original remaining grace"
    );
    let stopped = data(&mut second.events).await;
    assert!(matches!(stopped, Event::Stopped { reason } if reason == "agent_unresponsive"));
    let result = tokio::time::timeout(Duration::from_secs(5), second.task)
        .await
        .unwrap()
        .unwrap();
    result.expect("recovered local command actor exits normally after its one escalation");
    assert!(retained.terminal_claim.claimed());
    assert_eq!(retained.terminal_claim.current_turn(), grace.turn);
    assert!(retained.cancel_deadline.lock().unwrap().is_none());
    assert!(
        tokio::time::timeout(Duration::from_secs(2), second.events.recv())
            .await
            .unwrap()
            .is_none()
    );
}

/// Handlers compute an answer without knowing whether the request arrived over
/// direct stdio or the runner's control relay.
fn reply<T: agent_client_protocol::JsonRpcResponse>(
    responder: Responder<T>,
    outcome: Result<T, agent_client_protocol::Error>,
) -> agent_client_protocol::Result<()> {
    match outcome {
        Ok(value) => responder.respond(value),
        Err(e) => responder.respond_with_error(e),
    }
}

/// Register request handlers that each answer from a `SessionResources` clone.
/// The builder is typestate, so each registration shadows the last.
macro_rules! resource_requests {
    ($builder:expr, $resources:expr, $ingress:expr, $state:expr, $control:expr, $(($req:ty, $resp:ty, $handler:path)),+ $(,)?) => {{
        let builder = $builder;
        $(
            let builder = {
                let res = $resources.clone();
                let ingress = $ingress.clone();
                let state = $state.clone();
                let control = $control.clone();
                builder.on_receive_request(
                    move |request: $req, responder: Responder<$resp>, connection: ConnectionTo<Agent>| {
                        let (res, ingress, state) = (res.clone(), ingress.clone(), state.clone());
                        let control = control.clone();
                        let request_id = responder.id().clone();
                        let callback = control.as_ref().and_then(|control| {
                            match control.admit_reverse_callback(&request_id) {
                                super::control::ReverseCallbackAdmission::Admitted(callback) => Some(callback),
                                _ => None,
                            }
                        });
                        let scope = match control.as_ref() {
                            Some(_) => callback.as_ref().map(|callback| callback.ingress.clone()),
                            None => Some(ingress.capture_request()),
                        };
                        let context = callback.and_then(|callback| callback.context.clone());
                        let id = request.session_id.clone();
                        // Owned-resource handlers capture their entry synchronously here.
                        let work = $handler(request, res, context, Arc::clone(&state));
                        async move {
                            spawn_native_callback(&connection, &state, control.as_ref(), &request_id, move |admission| async move {
                                if let Err(error) = admission {
                                    return reply(responder, Err(error));
                                }
                                let Some(scope) = scope else {
                                    return reply(responder, Err(super::session_identity::SessionIngress::request_refused()));
                                };
                                let outcome = ingress.run_request(scope, &id, RequestPolicy::ParentOnly, work).await;
                                reply(responder, outcome)
                            })
                        }
                    },
                    agent_client_protocol::on_receive_request!(),
                )
            };
        )+
        builder
    }};
}

pub(super) async fn run_connection_task<W, R>(
    transport: ByteStreams<W, R>,
    params: ConnectionParams,
) where
    W: futures_util::AsyncWrite + Send + 'static,
    R: futures_util::AsyncRead + Send + 'static,
{
    let ConnectionParams {
        event_tx,
        connection_state,
        cmd_rx,
        child,
        pending_responders,
        resources,
        mode,
        ready_tx,
        profile,
        expected_agent,
        source_profile,
        default_effort,
        default_model_config,
        default_mode,
        default_model,
        mcp_servers,
        claude_home,
        runner,
        cancellation,
        shutdown,
        #[cfg(test)]
        tracking_observer,
    } = params;
    let label = resources.label.clone();
    let (lifecycle_tx, lifecycle_rx) = mpsc::channel(128);
    let (control, state, prompt_in_flight) = match runner {
        Some(r) => (Some(r.control), r.state, r.prompt_in_flight),
        None => (None, Arc::default(), Arc::new(AtomicBool::new(false))),
    };
    let _control_shutdown = super::control::ShutdownControlOnDrop(control.clone());
    // A runner owns identity across daemons, so its ingress is the one that
    // already holds the adopted session.
    let ingress = control
        .as_ref()
        .map(|control| control.ingress.clone())
        .unwrap_or_else(|| {
            let initial = match &mode {
                ConnectMode::Resume { acp_session_id, .. } => {
                    Some(SessionId::from(acp_session_id.clone()))
                }
                ConnectMode::Fresh => None,
            };
            Arc::new(SessionIngress::new(initial))
        });
    if let Some(projection) = event_tx.projection() {
        ingress.observe_projection(projection);
    }
    ingress.begin();
    let original_ingress = match control.as_ref() {
        Some(control) => control.install_original_ingress(),
        None => Ok(()),
    };
    let shared = Arc::new(Shared::new(
        event_tx.clone(),
        label.clone(),
        profile,
        lifecycle_tx,
        state,
        prompt_in_flight,
        resources.sandbox.as_ref(),
        ingress.clone(),
        claude_home,
        connection_state.clone(),
        control.clone(),
        pending_responders.clone(),
    ));
    #[cfg(test)]
    if let Some(observer) = tracking_observer {
        let _ = observer.send(super::test_helpers::TrackingObservation {
            tasks: Arc::clone(&shared.bg_tasks),
            agents: Arc::clone(&shared.between_prompt.bg_agents),
        });
    }
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

    let builder = Client
        .v1()
        .name("aoe-acp")
        .on_close({
            let control = control.clone();
            let shared = shared.clone();
            move |_connection| async move {
                shared
                    .connection_state
                    .sdk_closing
                    .store(true, Ordering::Release);
                if let Some(control) = control {
                    control.close_sdk_route(
                        AcpConnectionSource::Sdk,
                        AcpConnectionCause::IncomingEnded,
                    );
                } else {
                    shared
                        .connection_state
                        .observe(AcpConnectionSource::Sdk, AcpConnectionCause::IncomingEnded);
                    shared.connection_state.mark_ordered_loss();
                }
                // Preserve an earlier correlated response's diagnostic and
                // terminal before this actual SDK close cancels foreground.
                // The foreground may be committing buffered bootstrap replay.
                // Its existing write fence protects that ordered transfer from
                // cancellation by this SDK close; otherwise outer cleanup owns it.
                drop(shared.ingress.fence.read().await);
                shared.wait_local_prompt_settled().await;
                shared.connection_state.wait_completed_outcomes().await;
                Err(acp_internal_error("agent transport closed".into()))
            }
        })
        .on_receive_notification(
            {
                let shared = shared.clone();
                let control = control.clone();
                // Only the relay tags updates with their original frame size.
                let control_notifications = control.is_some();
                move |notification: SessionIngressNotification, _cx| {
                    let (shared, control) = (shared.clone(), control.clone());
                    async move {
                        let params = match notification {
                            SessionIngressNotification::Replayed(marker) => {
                                shared
                                    .handle_ingress_marker(PendingIngress::Replayed(marker))
                                    .await?;
                                return Ok(());
                            }
                            SessionIngressNotification::PromptCompleted(marker) => {
                                if let Some(control) = control.as_ref() {
                                    if let Some(adopted) = marker.adopted {
                                        shared
                                            .handle_ingress_marker(PendingIngress::PromptCompleted(
                                                adopted,
                                            ))
                                            .await?;
                                    } else {
                                        control.deliver_prompt_completion(marker);
                                    }
                                }
                                return Ok(());
                            }
                            SessionIngressNotification::NativeLost(_) => {
                                if control.is_some() {
                                    shared
                                        .handle_ingress_marker(PendingIngress::NativeLost)
                                        .await?;
                                }
                                return Ok(());
                            }
                            // No session id to fence on, and a refused session
                            // still reports the account it was refused for.
                            SessionIngressNotification::AuthStatus(status) => {
                                let (mut auth, _) = SessionIngressNotification::decode_auth(
                                    status, control_notifications,
                                )?;
                                auth.current_scope = shared.ingress.notification_scope(auth.evidence);
                                shared
                                    .handle_ingress_marker(PendingIngress::Auth(Box::new(auth)))
                                    .await?;
                                return Ok(());
                            }
                            SessionIngressNotification::AutonomousSnapshot(params) => {
                                if !control_notifications { return Ok(()); }
                                let envelope: crate::acp::control_protocol::NativeAutonomousSnapshotEnvelope =
                                    serde_json::from_value(params)
                                        .map_err(|_| agent_client_protocol::Error::invalid_params())?;
                                envelope.snapshot.validate()
                                    .map_err(|_| agent_client_protocol::Error::invalid_params())?;
                                let snapshot = envelope.snapshot;
                                let Some(first) = shared.ingress.capture_autonomous(
                                    crate::acp::control_protocol::NativeAutonomousPhase::Started,
                                    snapshot.started.notification.clone(), snapshot.started.evidence,
                                    Some(envelope.wire_bytes),
                                ) else { return Ok(()); };
                                let second = if let Some(ended) = &snapshot.ended {
                                    let Some(second) = shared.ingress.capture_autonomous(
                                        crate::acp::control_protocol::NativeAutonomousPhase::Ended,
                                        ended.notification.clone(), ended.evidence, Some(0),
                                    ) else { return Ok(()); };
                                    Some(Box::new(second))
                                } else { None };
                                shared.handle_ingress_marker(PendingIngress::Autonomous {
                                    first: Box::new(first), second,
                                }).await?;
                                return Ok(());
                            }
                            SessionIngressNotification::Autonomous(phase, params) => {
                                let (notification, evidence, bytes) =
                                    SessionIngressNotification::decode_autonomous(
                                        phase, params, control_notifications,
                                    )?;
                                if let Some(notification) = shared.ingress.capture_autonomous(
                                    phase, notification, evidence, bytes,
                                ) {
                                    shared.handle_ingress_marker(
                                        PendingIngress::Autonomous { first: Box::new(notification), second: None },
                                    ).await?;
                                }
                                return Ok(());
                            }
                            SessionIngressNotification::Update(params) => params,
                        };
                        let (notification, wire_bytes) = SessionIngressNotification::decode_update(
                            params,
                            control_notifications,
                        )?;
                        let notification = shared.ingress.capture_notification(
                            notification, shared.suppress_history_replay.load(Ordering::Relaxed),
                        );
                        #[cfg(test)]
                        {
                            let gate = {
                                let mut scoped = shared.connection_state.test_session_update.lock().unwrap();
                                if scoped.as_ref().is_some_and(|(id, _)| {
                                    id == &notification.notification.session_id
                                }) {
                                    scoped.take().map(|(_, gate)| gate)
                                } else {
                                    None
                                }
                            }.or_else(|| shared.connection_state.test_update.lock().unwrap().take());
                            if let Some((entered, release)) = gate {
                                let _ = entered.send(());
                                tokio::select! {
                                    biased;
                                    _ = shared.connection_state.wait_callback_end() => return Ok(()),
                                    released = release => if released.is_err() { return Ok(()); },
                                }
                            }
                        }
                        let admitted = shared
                            .ingress
                            .notification(notification, wire_bytes)
                            .await?;
                        match admitted {
                            Some((notification, _guard)) => {
                                shared.handle_notification(notification, true).await;
                            }
                            None => shared.settle_discarded().await,
                        }
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let shared = shared.clone();
                let pending = pending_responders.clone();
                let control = control.clone();
                move |request: RequestPermissionRequest,
                      responder: Responder<RequestPermissionResponse>,
                      conn: ConnectionTo<Agent>| {
                    let (shared, pending) = (shared.clone(), pending.clone());
                    let control = control.clone();
                    let request_id = responder.id().clone();
                    let scope = shared.ingress.capture_request();
                    async move {
                        let state = shared.connection_state.clone();

                        spawn_native_callback(
                            &conn,
                            &state,
                            control.as_ref(),
                            &request_id,
                            move |admission| async move {
                                let captured = match admission {
                                    Ok(captured) => captured,
                                    Err(error) => return reply(responder, Err(error)),
                                };
                                let id = request.session_id.clone();
                                if let Some(callback) = &captured {
                                    if let Err(error) = shared.ingress.request(
                                        &callback.ingress, &id, RequestPolicy::ParentOrChild,
                                    ).await {
                                        return reply(responder, Err(error));
                                    }
                                }
                                let cache = match &captured {
                                    Some(callback) => callback.history.as_ref()
                                        .map(|history| Arc::clone(&history.tool_context)),
                                    None => Some(Arc::clone(&shared.tool_context_cache)),
                                };
                                let sender = match &captured {
                                    Some(callback) => shared.event_tx.with_context(callback.context.clone()),
                                    None => shared.event_tx.clone(),
                                };
                                let native = captured.is_some();
                                let handled = handle_permission_request(
                                    request, sender, pending, profile, cache, captured,
                                );
                                let outcome = if native {
                                    handled.await
                                } else {
                                    shared.ingress.run_request(scope, &id, RequestPolicy::ParentOrChild, handled).await
                                };
                                reply(responder, outcome)
                            },
                        )?;
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let shared = shared.clone();
                let pending = pending_responders.clone();
                let control = control.clone();
                move |request: CreateElicitationRequest,
                      responder: Responder<CreateElicitationResponse>,
                      conn: ConnectionTo<Agent>| {
                    let (shared, pending) = (shared.clone(), pending.clone());
                    let control = control.clone();
                    let request_id = responder.id().clone();
                    let scope = shared.ingress.capture_request();
                    async move {
                        let state = shared.connection_state.clone();

                        spawn_native_callback(
                            &conn,
                            &state,
                            control.as_ref(),
                            &request_id,
                            move |admission| async move {
                                let captured = match admission {
                                    Ok(captured) => captured,
                                    Err(error) => return reply(responder, Err(error)),
                                };
                                let sender = match &captured {
                                    Some(callback) => shared.event_tx.with_context(callback.context.clone()),
                                    None => shared.event_tx.clone(),
                                };
                                let id = match request.scope() {
                                    ElicitationScope::Session(scope) => Some(scope.session_id.clone()),
                                    _ => None,
                                };
                                if let (Some(callback), Some(id)) = (&captured, &id) {
                                    if let Err(error) = shared.ingress.request(
                                        &callback.ingress, id, RequestPolicy::ParentOrChild,
                                    ).await {
                                        return reply(responder, Err(error));
                                    }
                                }
                                let native = captured.is_some();
                                let work = handle_elicitation_request(request, sender, pending, captured);
                                let outcome = if native {
                                    work.await
                                } else if let Some(id) = id {
                                    shared.ingress.run_request(scope, &id, RequestPolicy::ParentOrChild, work).await
                                } else {
                                    work.await
                                };
                                reply(responder, outcome)
                            },
                        )?;
                        Ok(())
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        );
    let builder = resource_requests!(
        builder,
        resources,
        ingress,
        connection_state,
        control,
        (
            ReadTextFileRequest,
            ReadTextFileResponse,
            handle_read_text_file
        ),
        (
            WriteTextFileRequest,
            WriteTextFileResponse,
            handle_write_text_file
        ),
        (
            CreateTerminalRequest,
            CreateTerminalResponse,
            handle_create_terminal
        ),
        (
            TerminalOutputRequest,
            TerminalOutputResponse,
            handle_terminal_output
        ),
        (
            WaitForTerminalExitRequest,
            WaitForTerminalExitResponse,
            handle_wait_for_terminal_exit
        ),
        (
            KillTerminalRequest,
            KillTerminalResponse,
            handle_kill_terminal
        ),
        (
            ReleaseTerminalRequest,
            ReleaseTerminalResponse,
            handle_release_terminal
        ),
    );

    let establish_ctx = establish::EstablishCtx {
        shared: shared.clone(),
        control: control.clone(),
        ready_tx: ready_tx.clone(),
        mode,
        expected_agent,
        mcp_servers,
        default_effort,
        default_model_config,
        default_mode,
        default_model,
        source_profile,
        agent_cwd: resources.agent_cwd(),
        cmd_rx,
        lifecycle_rx,
    };
    let foreground_shutdown = shutdown.clone();
    let foreground_cancellation = cancellation.clone();
    let foreground_state = connection_state.clone();
    let foreground_shared = shared.clone();
    let foreground_ready = ready_tx.clone();
    let connection = builder.connect_with(
        transport,
        move |connection: ConnectionTo<Agent>| async move {
            let foreground = async move {
                original_ingress?;
                let session = establish::establish(connection, establish_ctx).await?;
                match session {
                    Some(session) => session.run().await,
                    None => Ok(()),
                }
            };
            tokio::pin!(foreground);
            let mut result = tokio::select! {
                biased;
                _ = foreground_shutdown.cancelled() => {
                    if let Some(projection) = foreground_state.projection.get()
                        .filter(|projection| projection.has_completed_original())
                    {
                        tokio::select! {
                            result = &mut foreground => result,
                            _ = projection.original_delivered() => Ok(()),
                        }
                    } else {
                        Ok(())
                    }
                },
                _ = foreground_cancellation.cancelled() => {
                    if let Some(projection) = foreground_state.projection.get()
                        .filter(|projection| projection.has_completed_original())
                    {
                        tokio::select! {
                            result = &mut foreground => result,
                            _ = projection.original_delivered() => Ok(()),
                        }
                    } else {
                        Ok(())
                    }
                },
                error = ingress.failed() => {
                    foreground_state.observe(
                        AcpConnectionSource::Sdk,
                        AcpConnectionCause::ProtocolEnded,
                    );
                    // Preserve already received correlated outcomes before
                    // protocol loss drops their foreground application. This
                    // never waits for an unresolved native request.
                    if let Some(projection) = foreground_state.projection.get()
                        .filter(|projection| projection.has_completed_original())
                    {
                        tokio::select! {
                            _ = projection.original_delivered() => {},
                            _ = &mut foreground => {},
                        }
                    } else if foreground_state.completed_outcomes.load(Ordering::Acquire) != 0 {
                        tokio::select! {
                            biased;
                            _ = foreground_shutdown.cancelled() => {},
                            _ = foreground_cancellation.cancelled() => {},
                            _ = foreground_state.wait_completed_outcomes() => {},
                            _ = &mut foreground => {},
                        }
                    }
                    foreground_state.mark_ordered_loss();
                    Err(error)
                },
                result = &mut foreground => result,
            };
            // The foreground can observe the same overflow after the failure
            // future was polled, then win this select before its wake is polled.
            // Preserve that positive SDK failure regardless of which branch won.
            if result.is_err() && ingress.has_failed() {
                foreground_state
                    .observe(AcpConnectionSource::Sdk, AcpConnectionCause::ProtocolEnded);
                foreground_state.mark_ordered_loss();
            }
            if result.is_err()
                && !foreground_shutdown.is_cancelled()
                && !foreground_cancellation.is_cancelled()
            {
                if foreground_state
                    .establishment_declined
                    .load(Ordering::Acquire)
                    && foreground_state.loss().is_none()
                {
                    if let Err(error) = &result {
                        report_connection_error(error, &foreground_shared, &foreground_ready).await;
                    }
                    tokio::select! {
                        _ = foreground_shutdown.cancelled() => { result = Ok(()); }
                        _ = foreground_cancellation.cancelled() => { result = Ok(()); }
                        _ = foreground_state.wait_unavailable() => {}
                    }
                }
                if let Some(observation) = foreground_state.loss() {
                    if let Some(tx) = foreground_ready.lock().await.take() {
                        let _ = tx.send(Err(AcpError::ConnectionUnavailable(observation)));
                    }
                    // Earlier responses and buffered markers get their
                    // ordered boundary before the failed establish unwinds.
                    foreground_state.wait_ordered_unavailable().await;
                    foreground_shared.finish_unavailable_ingress().await;
                    foreground_shared.publish_connection_unavailable().await;
                    if observation.source == AcpConnectionSource::Native
                        && !foreground_state.sdk_is_closing()
                    {
                        // Native I/O is unavailable, not this management/
                        // SDK ownership. No fictitious Session is installed.
                        tokio::select! {
                            _ = foreground_shutdown.cancelled() => {}
                            _ = foreground_cancellation.cancelled() => {}
                        }
                        result = Ok(());
                    }
                }
            }
            if result.is_ok() {
                foreground_state.mark_stopping();
            }
            foreground_state.finish_foreground();
            result
        },
    );
    tokio::pin!(connection);
    let result = tokio::select! {
        biased;
        result = &mut connection => result,
        observation = connection_state.wait_unavailable() => {
            if let Some(tx) = ready_tx.lock().await.take() {
                let _ = tx.send(Err(AcpError::ConnectionUnavailable(observation)));
            }
            // Install owned-unavailable state even if foreground event/replay
            // backpressure prevented its early handshake reply.
            connection.await
        }
    };

    pending_responders.lock().await.clear();

    if let Err(e) = &result {
        if !shutdown.is_cancelled() && !cancellation.is_cancelled() {
            report_connection_error(e, &shared, &ready_tx).await;
        }
    } else {
        info!(target: "acp.protocol", session = %label, "ACP connection task ended cleanly");
    }
    // SDK return also covers its background error dropping the foreground future.
    // Classify/report the SDK result first; retained Native ownership is a separate lifetime.
    connection_state.finish_foreground();
    if (connection_state.loss().is_some()
        || connection_state
            .establishment_declined
            .load(Ordering::Acquire))
        && !connection_state
            .loss()
            .is_some_and(|loss| loss.source == AcpConnectionSource::Control)
        && !connection_state.is_stopping()
        && !shutdown.is_cancelled()
        && !cancellation.is_cancelled()
        && (child.is_some() || control.is_some())
    {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {}
            _ = cancellation.cancelled() => {}
            _ = async {
                if let Some(child) = child.as_ref() {
                    let _ = child.lock().await.wait().await;
                } else if let Some(control) = control.as_ref() {
                    control.wait_peer_exit().await;
                }
            } => {}
        }
    }
    if let Some(child) = child.as_ref() {
        let mut guard = child.lock().await;
        match guard.try_wait() {
            Ok(Some(status)) => info!(
                target: "acp.protocol",
                session = %label,
                "agent process already exited: status={status}"
            ),
            Ok(None) => info!(
                target: "acp.protocol",
                session = %label,
                "killing agent process after connection task end"
            ),
            Err(e) => warn!(
                target: "acp.protocol",
                session = %label,
                "try_wait failed before kill: {e}"
            ),
        }
        let _ = guard.kill().await;
    }
}

async fn report_connection_error(
    e: &agent_client_protocol::Error,
    shared: &Shared,
    ready_tx: &ReadyTx,
) {
    let label = &shared.session_label;
    let event_tx = &shared.event_tx;
    error!(
        target: "acp.protocol",
        session = %label,
        "ACP connection task ended with error: {:?}", e
    );
    let now = crate::time::Timestamp::now();
    let captured = captured_rate_limit_reset(&shared.rate_limit_rejections, now);
    let rate_limit = classify_rate_limit_error(e, captured.as_ref(), now);
    let established = shared.connection_state.phase.load(Ordering::Acquire) >= 2;
    // Before a session exists, an explicit account rejection is not transport loss.
    if (established || rate_limit.is_none())
        && shared.connection_state.loss().is_none()
        && !shared
            .connection_state
            .establishment_declined
            .load(Ordering::Acquire)
        && (!shared
            .connection_state
            .foreground_finished
            .load(Ordering::Acquire)
            || established)
    {
        shared
            .connection_state
            .observe(AcpConnectionSource::Sdk, AcpConnectionCause::DispatchFailed);
    }
    if let Some(observation) = shared.connection_state.loss() {
        shared.connection_state.mark_ordered_loss();
        if let Some(tx) = ready_tx.lock().await.take() {
            let _ = tx.send(Err(AcpError::ConnectionUnavailable(observation)));
        }
        shared.finish_unavailable_ingress().await;
        shared.publish_connection_unavailable().await;
        return;
    }
    if let Some(tx) = ready_tx.lock().await.take() {
        // A handshake-time limit fails the spawn as rate-limited so the
        // supervisor parks instead of burning respawn budget (#3514).
        let err = match rate_limit {
            Some(info) => {
                info!(
                    target: "acp.protocol",
                    session = %label,
                    resets_at = ?info.resets_at,
                    "handshake failed on rate_limit; failing spawn as rate-limited"
                );
                AcpError::RateLimited(Box::new(info))
            }
            None => AcpError::Spawn(format!(
                "ACP connection failed: {}",
                super::errors::public_error_message(e)
            )),
        };
        let _ = tx.send(Err(err));
    } else if let Some(info) = rate_limit {
        info!(
            target: "acp.protocol",
            session = %label,
            "connection task ended with rate_limit; emitting RateLimit + Stopped"
        );
        let _ = event_tx.send(Event::RateLimit { info }).await;
        let _ = event_tx
            .send(Event::Stopped {
                reason: "rate_limited".into(),
            })
            .await;
    } else {
        let _ = event_tx
            .send(Event::startup_error(format!(
                "ACP connection failed: {}",
                super::errors::public_error_message(e)
            )))
            .await;
    }
}
