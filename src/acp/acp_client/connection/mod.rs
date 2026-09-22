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
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, CreateTerminalRequest,
    CreateTerminalResponse, ElicitationScope, KillTerminalRequest, KillTerminalResponse, McpServer,
    ReadTextFileRequest, ReadTextFileResponse, ReleaseTerminalRequest, ReleaseTerminalResponse,
    RequestPermissionRequest, RequestPermissionResponse, SessionId, TerminalOutputRequest,
    TerminalOutputResponse, WaitForTerminalExitRequest, WaitForTerminalExitResponse,
    WriteTextFileRequest, WriteTextFileResponse,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo, Responder};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
use super::rate_limit::{captured_rate_limit_resets_at, classify_rate_limit_from_message};
use super::session_identity::{SessionIngress, SessionIngressNotification};
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

pub(super) struct RunnerLink {
    pub(super) control: Arc<DaemonControlClient>,
    /// Shared with the control reader, which claims an adopted turn's
    /// completion so the fallback watchdogs stand down.
    pub(super) terminal_claim: Arc<TerminalClaim>,
    pub(super) prompt_in_flight: Arc<AtomicBool>,
}

pub(super) struct ConnectionParams {
    pub(super) event_tx: mpsc::Sender<Event>,
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
    pub(super) runner: Option<RunnerLink>,
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
    ($builder:expr, $resources:expr, $ingress:expr, $(($req:ty, $resp:ty, $handler:path)),+ $(,)?) => {{
        let builder = $builder;
        $(
            let builder = {
                let res = $resources.clone();
                let ingress = $ingress.clone();
                builder.on_receive_request(
                    move |request: $req, responder: Responder<$resp>, _conn| {
                        let (res, ingress) = (res.clone(), ingress.clone());
                        async move {
                            // A foreign session's callback performs no effect.
                            let _guard = match ingress.request(&request.session_id).await {
                                Ok(guard) => guard,
                                Err(error) => return reply(responder, Err(error)),
                            };
                            reply(responder, $handler(request, res).await)
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
        runner,
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
    if control.is_some() && matches!(&mode, ConnectMode::Resume { .. }) {
        ingress.begin();
    }
    let shared = Arc::new(Shared::new(
        event_tx.clone(),
        label.clone(),
        profile,
        lifecycle_tx,
        terminal_claim,
        prompt_in_flight,
        resources.sandbox.as_ref(),
        ingress.clone(),
    ));
    let ready_tx = Arc::new(Mutex::new(Some(ready_tx)));

    let builder = Client
        .builder()
        .name("aoe-acp")
        .on_close({
            let control = control.clone();
            move |_connection| async move {
                if let Some(control) = control {
                    control.shutdown();
                }
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
                      _conn| {
                    let (shared, pending) = (shared.clone(), pending.clone());
                    async move {
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
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let event_tx = event_tx.clone();
                let ingress = ingress.clone();
                move |request: CreateElicitationRequest,
                      responder: Responder<CreateElicitationResponse>,
                      _conn| {
                    let (event_tx, pending) = (event_tx.clone(), pending_responders.clone());
                    let ingress = ingress.clone();
                    async move {
                        // Only a session-scoped elicitation carries an identity
                        // to fence on.
                        let _guard = if let ElicitationScope::Session(scope) = request.scope() {
                            match ingress.request(&scope.session_id).await {
                                Ok(guard) => Some(guard),
                                Err(error) => return reply(responder, Err(error)),
                            }
                        } else {
                            None
                        };
                        let outcome = handle_elicitation_request(request, event_tx, pending).await;
                        reply(responder, outcome)
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        );
    let builder = resource_requests!(
        builder,
        resources,
        ingress,
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
    let result = builder
        .connect_with(
            transport,
            move |connection: ConnectionTo<Agent>| async move {
                tokio::select! {
                    error = ingress.failed() => Err(error),
                    result = async move {
                        match establish::establish(connection, establish_ctx).await? {
                            Some(session) => session.run().await,
                            None => Ok(()),
                        }
                    } => result,
                }
            },
        )
        .await;

    if let Err(e) = &result {
        report_connection_error(e, &shared, &ready_tx).await;
    } else {
        info!(target: "acp.protocol", session = %label, "ACP connection task ended cleanly");
    }
    // Bridge tasks hold socket clones, so a runner's accept loop is only
    // released by shutting the control socket down explicitly.
    if let Some(control) = control.as_ref() {
        control.shutdown();
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
    let message = format!("ACP connection failed: {e}");
    let rate_limit = classify_rate_limit_from_message(
        &message,
        captured_rate_limit_resets_at(&shared.rate_limit_rejections, chrono::Utc::now()),
    );
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
            None => AcpError::Spawn(message),
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
    } else if shared.context_reset_emitted.load(Ordering::Relaxed) {
        // The reset already told the user why. A distinct reason keeps the
        // drain task from confusing this with a driven `session_reset`.
        let _ = event_tx
            .send(Event::Stopped {
                reason: "stored_session_rejected".into(),
            })
            .await;
    } else {
        let _ = event_tx.send(Event::AgentStartupError { message }).await;
    }
}
