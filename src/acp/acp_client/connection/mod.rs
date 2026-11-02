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
#[cfg(test)]
mod tests;

use crate::acp::agent_compat::ExpectedAgent;
use crate::acp::agent_profiles::AgentProfile;
use crate::acp::control_protocol::PromptCompletedMarker;
use crate::acp::state::{
    AcpConnectionCause, AcpConnectionPhase, AcpConnectionSource, AcpConnectionUnavailability, Event,
};
use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, ElicitationScope, KillTerminalRequest, KillTerminalResponse, McpServer,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Responder};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{error, info, warn};

use super::commands::{ClientCmd, ConnectMode};
use super::control::DaemonControlClient;
use super::errors::{acp_internal_error, AcpError};
use super::fs_handlers::{handle_read_text_file, handle_write_text_file};
use super::lifecycle::TerminalClaim;
use super::pending::PendingResponders;
use super::permission_handlers::{handle_elicitation_request, handle_permission_request};
use super::rate_limit::captured_rate_limit_resets_at;
use super::session_identity::{
    spawn_native_callback, PendingIngress, SessionIngress, SessionIngressNotification,
};
use super::terminal_handlers::{
    handle_create_terminal, handle_kill_terminal, handle_release_terminal, handle_terminal_output,
    handle_wait_for_terminal_exit,
};
use super::SessionResources;
use notifications::Shared;

/// After a cancel, declare the adapter unresponsive if no prompt response
/// arrives. A transport-wedge defense even for adapters that cancel promptly.
pub(crate) const CANCEL_ESCALATION_GRACE: Duration = Duration::from_secs(10);

/// A detached runner's control channel, which owns the ACP handshake and the
/// turn; everything else is relayed through the crate transport.
/// Taken once, by whichever of the handshake or the failure path answers first.
pub(super) type ReadyTx = Mutex<Option<oneshot::Sender<Result<(), AcpError>>>>;

#[derive(Default)]
pub(super) struct ConnectionState {
    phase: AtomicU8,
    unavailable: OnceLock<AcpConnectionUnavailability>,
    reported: AtomicBool,
    usable: AtomicBool,
    ordered_loss: AtomicBool,
    intentional_stop: AtomicBool,
    foreground_finished: AtomicBool,
    pub(super) admission: std::sync::Mutex<()>,
    sdk_closing: AtomicBool,
    unavailable_wake: tokio::sync::Notify,
    ordered_loss_wake: tokio::sync::Notify,
    completed_outcomes: std::sync::atomic::AtomicUsize,
    completed_outcomes_wake: tokio::sync::Notify,
}

impl ConnectionState {
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
            if self.is_stopping() || self.completed_outcomes.load(Ordering::Acquire) == 0 {
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

    pub(super) fn driver_finished(&self) -> bool {
        self.foreground_finished.load(Ordering::Acquire)
    }

    pub(super) fn record_usable(&self) {
        self.usable.store(true, Ordering::Release);
    }

    pub(super) fn admit<T>(&self, operation: impl FnOnce() -> T) -> Result<T, AcpError> {
        let _admission = self
            .admission
            .lock()
            .expect("connection admission poisoned");
        if self.intentional_stop.load(Ordering::Acquire) || self.driver_finished() {
            return Err(AcpError::NotRunning);
        }
        if let Some(observation) = self.loss() {
            return Err(AcpError::ConnectionUnavailable(observation));
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
    /// Shared with the control reader, which claims an adopted turn's
    /// completion so the fallback watchdogs stand down.
    pub(super) terminal_claim: Arc<TerminalClaim>,
    pub(super) prompt_in_flight: Arc<AtomicBool>,
}

pub(super) struct ConnectionParams {
    pub(super) event_tx: mpsc::Sender<Event>,
    pub(super) connection_state: Arc<ConnectionState>,
    pub(super) cmd_rx: mpsc::Receiver<ClientCmd>,
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
    pub(super) default_mode: Option<String>,
    pub(super) default_model: Option<String>,
    pub(super) mcp_servers: Vec<McpServer>,
    pub(super) claude_home: Option<PathBuf>,
    pub(super) runner: Option<RunnerLink>,
    pub(super) cancellation: tokio_util::sync::CancellationToken,
    pub(super) shutdown: tokio_util::sync::CancellationToken,
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
    ($builder:expr, $resources:expr, $ingress:expr, $state:expr, $(($req:ty, $resp:ty, $handler:path)),+ $(,)?) => {{
        let builder = $builder;
        $(
            let builder = {
                let res = $resources.clone();
                let ingress = $ingress.clone();
                let state = $state.clone();
                builder.on_receive_request(
                    move |request: $req, responder: Responder<$resp>, connection: ConnectionTo<Agent>| {
                        let (res, ingress, state) = (res.clone(), ingress.clone(), state.clone());
                        async move {
                            spawn_native_callback(&connection, &state, async move {
                                let _guard = match ingress.request(&request.session_id).await {
                                    Ok(guard) => guard,
                                    Err(error) => return reply(responder, Err(error)),
                                };
                                reply(responder, $handler(request, res).await)
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
        default_mode,
        default_model,
        mcp_servers,
        claude_home,
        runner,
        cancellation,
        shutdown,
    } = params;
    let label = resources.label.clone();
    let (lifecycle_tx, lifecycle_rx) = mpsc::channel(128);
    let (control, terminal_claim, prompt_in_flight) = match runner {
        Some(r) => (Some(r.control), r.terminal_claim, r.prompt_in_flight),
        None => (
            None,
            Arc::new(TerminalClaim::new()),
            Arc::new(AtomicBool::new(false)),
        ),
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
                ConnectMode::Fresh { .. } => None,
            };
            Arc::new(SessionIngress::new(initial))
        });
    ingress.begin();
    let shared = Arc::new(Shared::new(
        event_tx.clone(),
        label.clone(),
        profile,
        lifecycle_tx,
        terminal_claim,
        prompt_in_flight,
        resources.sandbox.as_ref(),
        ingress.clone(),
        claude_home,
        connection_state.clone(),
        control.clone(),
        pending_responders.clone(),
    ));
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

    let builder = Client
        .builder()
        .name("aoe-acp")
        .on_close({
            let control = control.clone();
            let shared = shared.clone();
            move |_connection| async move {
                shared
                    .connection_state
                    .sdk_closing
                    .store(true, Ordering::Release);
                shared
                    .connection_state
                    .observe(AcpConnectionSource::Sdk, AcpConnectionCause::IncomingEnded);
                shared.connection_state.mark_ordered_loss();
                if let Some(control) = control {
                    control.shutdown();
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
                                if let Some(control) = control.as_ref() {
                                    control.mark_session_replayed(marker);
                                }
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
                                        control.deliver_prompt_completion(
                                            PromptCompletedMarker::default(),
                                        );
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
                                shared
                                    .handle_ingress_marker(PendingIngress::Auth(status))
                                    .await?;
                                return Ok(());
                            }
                            SessionIngressNotification::Update(params) => params,
                        };
                        let (notification, wire_bytes) = SessionIngressNotification::decode_update(
                            params,
                            control_notifications,
                        )?;
                        let admitted = shared
                            .ingress
                            .notification(notification, wire_bytes)
                            .await?;
                        if let Some((notification, _guard)) = admitted {
                            shared.handle_notification(notification, true).await;
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
                move |request: RequestPermissionRequest,
                      responder: Responder<RequestPermissionResponse>,
                      connection: ConnectionTo<Agent>| {
                    let (shared, pending) = (shared.clone(), pending.clone());
                    async move {
                        let state = shared.connection_state.clone();
                        spawn_native_callback(&connection, &state, async move {
                            let _guard = match shared.ingress.request(&request.session_id).await {
                                Ok(guard) => guard,
                                Err(error) => return reply(responder, Err(error)),
                            };
                            let outcome = handle_permission_request(
                                request,
                                shared.event_tx.clone(),
                                pending,
                                profile,
                                shared.tool_context_cache.clone(),
                            )
                            .await;
                            reply(responder, outcome)
                        })
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let shared = shared.clone();
                let pending = pending_responders.clone();
                move |request: CreateElicitationRequest,
                      responder: Responder<CreateElicitationResponse>,
                      connection: ConnectionTo<Agent>| {
                    let (shared, pending) = (shared.clone(), pending.clone());
                    async move {
                        let state = shared.connection_state.clone();
                        spawn_native_callback(&connection, &state, async move {
                            // Only a session-scoped elicitation carries an identity
                            // to fence on.
                            let _guard = if let ElicitationScope::Session(scope) = request.scope() {
                                match shared.ingress.request(&scope.session_id).await {
                                    Ok(guard) => Some(guard),
                                    Err(error) => return reply(responder, Err(error)),
                                }
                            } else {
                                None
                            };
                            let outcome = handle_elicitation_request(
                                request,
                                shared.event_tx.clone(),
                                pending,
                            )
                            .await;
                            reply(responder, outcome)
                        })
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
                let session = establish::establish(connection, establish_ctx).await?;
                match session {
                    Some(session) => session.run().await,
                    None => Ok(()),
                }
            };
            tokio::pin!(foreground);
            let mut result = tokio::select! {
                biased;
                _ = foreground_shutdown.cancelled() => Ok(()),
                _ = foreground_cancellation.cancelled() => Ok(()),
                error = ingress.failed() => {
                    foreground_state.observe(
                        AcpConnectionSource::Sdk,
                        AcpConnectionCause::ProtocolEnded,
                    );
                    // Preserve already received correlated outcomes before
                    // protocol loss drops their foreground application. This
                    // never waits for an unresolved native request.
                    if foreground_state.completed_outcomes.load(Ordering::Acquire) != 0 {
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
            if result.is_err()
                && !foreground_shutdown.is_cancelled()
                && !foreground_cancellation.is_cancelled()
            {
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
            foreground_state
                .foreground_finished
                .store(true, Ordering::Release);
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
    if connection_state.loss().is_some()
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
    if shared.connection_state.loss().is_none()
        && (!shared
            .connection_state
            .foreground_finished
            .load(Ordering::Acquire)
            || shared.connection_state.phase.load(Ordering::Acquire) >= 2)
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
    let resets_at =
        captured_rate_limit_resets_at(&shared.rate_limit_rejections, chrono::Utc::now());
    let rate_limit = super::rate_limit::classify_rate_limit_error(e, resets_at).or_else(|| {
        super::rate_limit::classify_rate_limit_from_message(&e.message, resets_at).map(
            |mut info| {
                info.status = super::errors::public_error_message(e);
                info
            },
        )
    });
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
            .send(Event::AgentStartupError {
                message: format!(
                    "ACP connection failed: {}",
                    super::errors::public_error_message(e)
                ),
                native_subject: None,
            })
            .await;
    }
}
