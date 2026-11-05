//! The handshake: `initialize`, then resume, fork, load, or create the
//! session and apply configured defaults.

use crate::acp::agent_compat::{self, ExpectedAgent};
use crate::acp::mcp_config;
use crate::acp::state::{Event, StartupErrorDetail};
use agent_client_protocol::schema::v1::{
    ForkSessionRequest, ForkSessionResponse, InitializeResponse, LoadSessionRequest,
    LoadSessionResponse, McpServer, NewSessionRequest, NewSessionResponse, SessionConfigId,
    SessionId,
};
use agent_client_protocol::{Agent, ConnectionTo};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::command_loop::Session;
use super::notifications::{now_ms, Shared};
use super::ReadyTx;
use crate::acp::acp_client::commands::{ConnectMode, QueuedClientCmd};
use crate::acp::acp_client::config_options::{
    apply_config_default, config_option_failure_event, config_options_event, modes_available_event,
    thought_level_config_id, ConfigOptionDispatchPurpose, SessionChannels,
};
use crate::acp::acp_client::control::{
    establish_session, DaemonControlClient, EstablishmentApplication,
};
use crate::acp::acp_client::errors::{acp_internal_error, AcpError, IncompatibleAgentError};
use crate::acp::acp_client::handshake::build_initialize_request;
use crate::acp::acp_client::lifecycle::LifecycleEnvelope;
use crate::acp::acp_client::session_identity::ordered_native_request;

/// Fully silent grace after reattaching to an in-flight turn.
const RESUME_IDLE_GRACE_DEFAULT: Duration = Duration::from_secs(30);

/// Debug builds honor `AOE_RESUME_IDLE_GRACE_MS`, clamped to at least 100ms.
fn resume_idle_grace() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("AOE_RESUME_IDLE_GRACE_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        return Duration::from_millis(ms.max(100));
    }
    RESUME_IDLE_GRACE_DEFAULT
}

/// Debug builds honor `AOE_RESUME_IDLE_CHECK_INTERVAL_MS`, clamped to at least 10ms.
fn resume_idle_check_interval() -> Duration {
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("AOE_RESUME_IDLE_CHECK_INTERVAL_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
    {
        return Duration::from_millis(ms.max(10));
    }
    Duration::from_millis(500)
}

pub(super) struct EstablishCtx {
    pub(super) shared: Arc<Shared>,
    pub(super) control: Option<Arc<DaemonControlClient>>,
    pub(super) ready_tx: Arc<ReadyTx>,
    pub(super) mode: ConnectMode,
    pub(super) expected_agent: ExpectedAgent,
    pub(super) mcp_servers: Vec<McpServer>,
    pub(super) default_effort: Option<String>,
    pub(super) default_mode: Option<String>,
    pub(super) default_model: Option<String>,
    pub(super) source_profile: Option<String>,
    pub(super) agent_cwd: PathBuf,
    pub(super) cmd_rx: mpsc::Receiver<QueuedClientCmd>,
    pub(super) lifecycle_rx: mpsc::Receiver<LifecycleEnvelope>,
}

/// `Ok(None)` when the agent failed the compatibility check; the typed error
/// already went out on `ready_tx`.
pub(super) async fn establish(
    connection: ConnectionTo<Agent>,
    ctx: EstablishCtx,
) -> Result<Option<Session>, agent_client_protocol::Error> {
    let shared = ctx.shared.clone();
    let label = shared.session_label.clone();
    let _application = ctx
        .control
        .as_ref()
        .map(|control| EstablishmentApplication::new(Arc::clone(control)))
        .transpose()?;
    info!(target: "acp.protocol", session = %label, "initializing ACP agent");
    // A new adapter process may never report, so an earlier process's
    // identity must not carry over. Cleared before `initialize`, so nothing
    // the new process sends can precede it; a reattach keeps its report.
    if matches!(ctx.mode, ConnectMode::Fresh) {
        shared.emit(Event::AuthStatusUpdated { status: None }).await;
    }
    let initialized = async {
        let init: InitializeResponse = match ctx.control.as_ref() {
            Some(control) => {
                let params = serde_json::to_value(build_initialize_request())
                    .map_err(|e| acp_internal_error(format!("serialize initialize params: {e}")))?;
                serde_json::from_value(control.initialize(params).await?).map_err(|e| {
                    acp_internal_error(format!("deserialize initialize result: {e}"))
                })?
            }
            None => ordered_native_request(
                &connection,
                &shared.connection_state,
                None,
                build_initialize_request(),
            )
            .map_err(|_| acp_internal_error("ACP connection unavailable".into()))?
            .wait()
            .await
            .into_result()?,
        };
        Ok::<_, agent_client_protocol::Error>(init)
    }
    .await;
    let init = match initialized {
        Ok(init) => init,
        Err(error) => {
            if matches!(ctx.mode, ConnectMode::Fresh) {
                shared
                    .event_tx
                    .close_establishment(Some(
                        crate::acp::establishment::EstablishmentBlockReason::NotIssued,
                    ))
                    .await;
            }
            return Err(error);
        }
    };
    shared.connection_state.initialized();

    // The supervisor mirrors the typed error into events; a clean return
    // keeps the outer cleanup from adding a generic startup error.
    if let Err(err) = agent_compat::validate(ctx.expected_agent, &init) {
        let message = err.user_message();
        warn!(
            target: "acp.protocol",
            session = %label,
            kind = err.kind(),
            message = %message,
            "agent compatibility check failed; refusing to enter session"
        );
        let detail = StartupErrorDetail::from(&err);
        if matches!(ctx.mode, ConnectMode::Fresh) {
            shared
                .event_tx
                .close_establishment(Some(
                    crate::acp::establishment::EstablishmentBlockReason::NotIssued,
                ))
                .await;
        }
        if let Some(tx) = ctx.ready_tx.lock().await.take() {
            let _ = tx.send(Err(AcpError::IncompatibleAgent(Box::new(
                IncompatibleAgentError { detail, message },
            ))));
        }
        return Ok(None);
    }

    #[cfg(test)]
    {
        let gate = shared
            .connection_state
            .test_initialize_response_application
            .lock()
            .unwrap()
            .take();
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            if release.await.is_err() {
                return Ok(None);
            }
        }
    }

    // Return initialized ownership before any bootstrap replay can fill the
    // public queue. Session establishment still owns its own readiness gate.
    if let Some(tx) = ctx.ready_tx.lock().await.take() {
        let _admission = shared
            .connection_state
            .admission
            .lock()
            .expect("connection admission lock");
        let readiness = match shared.connection_state.loss() {
            Some(observation) => Err(AcpError::ConnectionUnavailable(observation)),
            None => Ok(()),
        };
        let _ = tx.send(readiness);
    }
    if matches!(&ctx.mode, ConnectMode::Fresh) {
        let _guard = shared.ingress.fence.write().await;
        let replay = shared.ingress.finish(None)?;
        shared.apply_replay(replay).await;
    }

    let load_session_capable = init.agent_capabilities.load_session;
    // Re-derived on every connect (a respawn may land on another adapter
    // build) and emitted even when false so replay cannot keep a stale true.
    let steering_capable = agent_compat::supports_steering(ctx.expected_agent, &init);
    let prompt_caps = &init.agent_capabilities.prompt_capabilities;
    if steering_capable {
        info!(
            target: "acp.protocol",
            session = %label,
            "agent supports _session/steering; mid-turn prompts will be injected into the running turn"
        );
    }
    let arm_resume_watchdog = matches!(
        &ctx.mode,
        ConnectMode::Resume {
            in_flight_turn: true,
            ..
        }
    );
    shared.adopted_turn_active.store(
        arm_resume_watchdog && shared.connection_state.loss().is_none(),
        Ordering::Relaxed,
    );
    info!(
        target: "acp.protocol",
        session = %label,
        load_session_capable,
        mode = ?ctx.mode,
        "initialize handshake complete"
    );

    let mcp_servers = mcp_config::filter_for_capabilities(
        ctx.mcp_servers,
        &init.agent_capabilities.mcp_capabilities,
        &label,
    );
    let mut session = Session {
        connection,
        shared: shared.clone(),
        control: ctx.control,
        acp_session_id: SessionId::new(""),
        channels: SessionChannels::default(),
        steering_capable,
        close_capable: init.agent_capabilities.session_capabilities.close.is_some(),
        source_profile: ctx.source_profile,
        default_effort: ctx.default_effort,
        default_mode: ctx.default_mode,
        default_model: ctx.default_model,
        agent_cwd: ctx.agent_cwd,
        mcp_servers: mcp_servers.clone(),
        cmd_rx: ctx.cmd_rx,
        lifecycle_rx: ctx.lifecycle_rx,
        pending_prompts: VecDeque::new(),
        pending_reset: None,
    };
    session.acp_session_id = match ctx.mode {
        ConnectMode::Resume { acp_session_id, .. } => session.resume(acp_session_id).await?,
        ConnectMode::Fresh => {
            session
                .fresh(
                    init.agent_capabilities.session_capabilities.fork.is_some(),
                    load_session_capable,
                    mcp_servers,
                )
                .await?
        }
    };
    // Initialize describes this physical peer, but only a completed
    // establishment supplies custody for publishing its Current capabilities.
    shared
        .emit(Event::PromptCapabilities {
            image: prompt_caps.image,
            audio: prompt_caps.audio,
            embedded_context: prompt_caps.embedded_context,
            load_session: Some(load_session_capable),
            steering: steering_capable,
        })
        .await;
    session.apply_default_model().await;
    session.apply_default_effort().await;
    if session.control.is_none() && arm_resume_watchdog && shared.connection_state.loss().is_none()
    {
        spawn_resume_idle_watchdog(shared, &session.connection);
    }
    Ok(Some(session))
}

/// Fallback when an adopted turn neither completes nor shows activity, so the
/// UI cannot stay "thinking" forever. Once the turn is observable, completion
/// belongs to the between-prompt watchdog.
fn spawn_resume_idle_watchdog(shared: Arc<Shared>, connection: &ConnectionTo<Agent>) {
    let grace_ms = resume_idle_grace().as_millis() as i64;
    let interval = resume_idle_check_interval();
    let adopted_turn = shared.terminal_claim.current_turn();
    let state = shared.connection_state.clone();
    let _ = state.admit(|| connection.spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(interval) => {}
                _ = shared.connection_state.wait_unavailable() => return Ok(()),
            }
            if shared.terminal_claim.claimed()
                || shared.prompt_sent_since_attach.load(Ordering::Relaxed)
                || shared.first_event_after_attach.load(Ordering::Relaxed)
            {
                return Ok(());
            }
            let idle_ms = now_ms() - shared.last_event_at.load(Ordering::Relaxed);
            if idle_ms >= grace_ms {
                let Ok(permit) = shared.event_tx.reserve().await else {
                    return Ok(());
                };
                let _ = shared.connection_state.admit(|| shared.terminal_claim.claim_and_publish(
                    adopted_turn,
                    || {
                        !shared.prompt_sent_since_attach.load(Ordering::Relaxed)
                            && shared.adopted_turn_active.load(Ordering::Relaxed)
                            && !shared.first_event_after_attach.load(Ordering::Relaxed)
                    },
                    || {
                        info!(target: "acp.protocol", session = %shared.session_label, idle_ms,
                            "resume-idle watchdog: synthesizing Stopped for orphaned in-flight turn");
                        permit.send(Event::Stopped { reason: "reattach_idle".into() });
                    },
                ));
                return Ok(());
            }
        }
    }));
}

impl Session {
    async fn resume(&self, stored: String) -> Result<SessionId, agent_client_protocol::Error> {
        let control = self.control.as_ref().ok_or_else(|| {
            acp_internal_error("native adoption requires the owned control protocol".into())
        })?;
        let result = control.resume_session().await.map(SessionId::from);
        if let Ok(id) = &result {
            if !stored.is_empty() && stored != id.0.as_ref() {
                control.projection.lost();
                return Err(acp_internal_error(
                    "retained native identity differs from the selected conversation".into(),
                ));
            }
        }
        self.finish_establishment(result, None).await
    }

    async fn fresh(
        &mut self,
        fork_capable: bool,
        load_capable: bool,
        mcp_servers: Vec<McpServer>,
    ) -> Result<SessionId, agent_client_protocol::Error> {
        use crate::acp::establishment::{EstablishmentBlockReason, EstablishmentTarget};
        let control = self.control.as_ref().ok_or_else(|| {
            acp_internal_error("native establishment requires the owned control protocol".into())
        })?;
        let attempt = control
            .projection
            .attempt()
            .ok_or_else(|| acp_internal_error("no durable native establishment attempt".into()))?;
        let mut failure = None;
        let constructor = self.shared.ingress.constructor_scope();
        let response = match &attempt.target {
            EstablishmentTarget::Fork { .. } if !fork_capable => {
                failure = Some(EstablishmentBlockReason::NotIssued);
                Err(acp_internal_error(
                    "the selected agent does not support session/fork".into(),
                ))
            }
            EstablishmentTarget::Load { .. } if !load_capable => {
                failure = Some(EstablishmentBlockReason::NotIssued);
                Err(acp_internal_error(
                    "the selected agent does not support session/load".into(),
                ))
            }
            EstablishmentTarget::Fork { session_id } => {
                let req = ForkSessionRequest::new(session_id.clone(), self.agent_cwd.clone())
                    .mcp_servers(mcp_servers);
                establish_session::<ForkSessionResponse>(control, "session/fork", &req)
                    .await
                    .map(|response: ForkSessionResponse| {
                        (response.session_id, response.modes, response.config_options)
                    })
            }
            EstablishmentTarget::Load { session_id, import } => {
                self.shared
                    .suppress_history_replay
                    .store(!import, Ordering::Relaxed);
                // The original load's known identity admits reverse callbacks
                // without waiting for the result those callbacks may complete.
                self.shared
                    .activate_session(SessionId::from(session_id.clone()))
                    .await?;
                let req = LoadSessionRequest::new(session_id.clone(), self.agent_cwd.clone())
                    .mcp_servers(mcp_servers);
                establish_session::<LoadSessionResponse>(control, "session/load", &req)
                    .await
                    .map(|response| {
                        (
                            SessionId::from(session_id.clone()),
                            response.modes,
                            response.config_options,
                        )
                    })
            }
            EstablishmentTarget::New => {
                let req = NewSessionRequest::new(self.agent_cwd.clone()).mcp_servers(mcp_servers);
                establish_session::<NewSessionResponse>(control, "session/new", &req)
                    .await
                    .map(|response: NewSessionResponse| {
                        (response.session_id, response.modes, response.config_options)
                    })
            }
        };
        #[cfg(test)]
        if response.is_ok() && matches!(attempt.target, EstablishmentTarget::New) {
            let gate = self
                .shared
                .connection_state
                .test_fresh_response_application
                .lock()
                .unwrap()
                .take();
            if let Some((entered, release)) = gate {
                let writer = self.shared.ingress.fence.write().await;
                let _ = entered.send(());
                let _ = release.await;
                // Activation takes this same writer; only the test application pause owns it here.
                drop(writer);
            }
        }
        let result = match response {
            Ok((id, modes, options)) => match self.shared.activate_session(id.clone()).await {
                Ok(()) => {
                    self.channels = SessionChannels::new(modes.as_ref(), options.as_deref());
                    if let Some(modes) = modes.as_ref() {
                        self.shared
                            .emit_with_constructor(
                                modes_available_event(modes),
                                constructor.clone(),
                            )
                            .await;
                    }
                    if let Some(event) = config_options_event(options) {
                        self.shared
                            .emit_with_constructor(event, constructor.clone())
                            .await;
                    }
                    Ok(id)
                }
                Err(error) => {
                    self.control
                        .as_ref()
                        .expect("owned establishment")
                        .projection
                        .lost();
                    Err(error)
                }
            },
            Err(error) => Err(error),
        };
        let id = self.finish_establishment(result, failure).await?;
        if matches!(attempt.target, EstablishmentTarget::New) {
            if let (Some(mode), Some(option)) = (
                self.default_mode.as_deref(),
                self.channels.mode_config_option_id.as_ref(),
            ) {
                let _ = apply_config_default(
                    &self.connection,
                    &self.shared.connection_state,
                    &self.shared.event_tx,
                    agent_client_protocol::schema::v1::SetSessionConfigOptionRequest::new(
                        id.clone(),
                        SessionConfigId::new(option.clone()),
                        agent_client_protocol::schema::v1::SessionConfigValueId::new(mode),
                    ),
                    &self.shared.session_label,
                    self.control
                        .as_ref()
                        .and_then(|control| control.original_request_context()),
                )
                .await;
            }
        }
        Ok(id)
    }

    async fn finish_establishment(
        &self,
        result: Result<SessionId, agent_client_protocol::Error>,
        failure: Option<crate::acp::establishment::EstablishmentBlockReason>,
    ) -> Result<SessionId, agent_client_protocol::Error> {
        let control = self.control.as_ref().ok_or_else(|| {
            acp_internal_error("native establishment has no original control receipt".into())
        })?;
        Self::settle_establishment(&self.shared, control, result, failure, true).await
    }

    pub(super) async fn settle_establishment(
        shared: &Arc<Shared>,
        control: &Arc<crate::acp::acp_client::control::DaemonControlClient>,
        mut result: Result<SessionId, agent_client_protocol::Error>,
        failure: Option<crate::acp::establishment::EstablishmentBlockReason>,
        publish_assignment: bool,
    ) -> Result<SessionId, agent_client_protocol::Error> {
        use crate::acp::establishment::EstablishmentBlockReason;
        use crate::acp::event_sender::EstablishmentResolution;
        let (expected_id, expected_context) = control.applying_attempt().ok_or_else(|| {
            acp_internal_error("original application custody is unavailable".into())
        })?;
        let receipt = control
            .projection
            .receipt_snapshot()
            .filter(|receipt| receipt.attempt.id == expected_id);
        let native_id = receipt.as_ref().and_then(|receipt| receipt.session_id());
        if let Some(id) = result
            .as_ref()
            .ok()
            .cloned()
            .or_else(|| native_id.map(|id| SessionId::from(id.to_owned())))
        {
            if let Err(error) = shared.activate_session(id).await {
                control.projection.lost();
                if result.is_ok() {
                    result = Err(error);
                }
            }
        } else if !shared
            .ingress
            .current()
            .as_ref()
            .is_some_and(|id| shared.ingress.is_current_stable(id))
        {
            let _guard = shared.ingress.fence.write().await;
            match shared.ingress.finish(shared.ingress.current()) {
                Ok(replay) => shared.apply_replay(replay).await,
                Err(error) => {
                    control.projection.lost();
                    if result.is_ok() {
                        result = Err(error);
                    }
                }
            }
        }
        control.session_replayed().await;
        let failure = if native_id.is_some() {
            None
        } else {
            failure.or_else(|| {
                result.as_ref().err().map(|_| {
                    control
                        .projection
                        .failure_reason()
                        .unwrap_or(EstablishmentBlockReason::OutcomeUnknown)
                })
            })
        };
        let delivery = shared
            .event_tx
            .close_establishment(failure)
            .await
            .ok_or_else(|| {
                acp_internal_error("original establishment delivery is unavailable".into())
            })?;
        let resolution = delivery.settled().await;
        let id = match result {
            Ok(id) => id,
            Err(error) => {
                shared.ingress.revoke_requests();
                shared.connection_state.decline_establishment();
                return Err(error);
            }
        };
        match resolution {
            EstablishmentResolution::Applied(postimage)
                if delivery.attempt.id == expected_id
                    && postimage.attempt_id == expected_id
                    && postimage.context == expected_context
                    && postimage.choice.acp_session_id.as_deref() == Some(id.0.as_ref()) =>
            {
                #[cfg(test)]
                let gate = shared
                    .connection_state
                    .test_establishment_usable
                    .lock()
                    .unwrap()
                    .take();
                #[cfg(test)]
                if let Some((entered, release)) = gate {
                    let _ = entered.send(());
                    let _ = release.await;
                }
                let ready = control.native_session_ready()
                    && control.projection.applied().is_some()
                    && !shared.connection_state.is_stopping();
                if publish_assignment {
                    if ready {
                        shared.connection_state.record_usable();
                    }
                    shared
                        .emit(Event::AcpSessionAssigned {
                            acp_session_id: id.0.to_string(),
                            connection_ready: ready,
                            native_subject: None,
                        })
                        .await;
                } else if !ready {
                    return Err(acp_internal_error(
                        "new native binding is applied but its channel is unavailable".into(),
                    ));
                }
                Ok(id)
            }
            _ => {
                shared.ingress.revoke_requests();
                shared.connection_state.decline_establishment();
                Err(acp_internal_error(
                    "native establishment or original replay remains unconfirmed".into(),
                ))
            }
        }
    }

    /// The persisted model pick, re-asserted after any establish path because
    /// claude-agent-acp ignores `AOE_AGENT_MODEL` and re-applies its settings
    /// pin inside `session/load`. Runs before the effort: a model switch
    /// rebuilds the option set, so the effort's option id is re-read.
    async fn apply_default_model(&mut self) {
        let Some(model) = self.default_model.as_deref() else {
            return;
        };
        let label = &self.shared.session_label;
        let Some(option) = &self.channels.model_option else {
            debug!(
                target: "acp.protocol",
                session = %label,
                "structured view model skipped; no model option"
            );
            return;
        };
        if option.is_current(model) {
            debug!(
                target: "acp.protocol",
                session = %label,
                model,
                "structured view model already current"
            );
            return;
        }
        let scope = self
            .control
            .as_ref()
            .and_then(|control| control.original_request_context());
        let result = apply_config_default(
            &self.connection,
            &self.shared.connection_state,
            &self.shared.event_tx,
            agent_client_protocol::schema::v1::SetSessionConfigOptionRequest::new(
                self.acp_session_id.clone(),
                SessionConfigId::new(option.id.clone()),
                agent_client_protocol::schema::v1::SessionConfigValueId::new(model),
            ),
            label,
            scope.clone(),
        )
        .await;
        match result {
            Ok(options) => {
                self.channels.thought_level_config_option_id =
                    thought_level_config_id(&options).map(|id| id.0.to_string());
            }
            Err(reason) if self.shared.connection_state.loss().is_none() => {
                let event = config_option_failure_event(
                    option.id.clone(),
                    model.to_string(),
                    reason,
                    ConfigOptionDispatchPurpose::Generic,
                );
                let sender = if self.control.is_some() {
                    self.shared.event_tx.with_context(scope.map(Arc::new))
                } else {
                    self.shared.event_tx.clone()
                };
                let _ = sender.send(event).await;
            }
            Err(_) => {}
        }
    }

    /// Effort is a pin carried across respawns, which resume via load or fork,
    /// so it applies after any establish path. Resume captures no option id.
    async fn apply_default_effort(&self) {
        let Some(effort) = self.default_effort.as_deref() else {
            return;
        };
        match self.channels.thought_level_config_option_id.as_deref() {
            Some(config_id) => {
                let _ = apply_config_default(
                    &self.connection,
                    &self.shared.connection_state,
                    &self.shared.event_tx,
                    agent_client_protocol::schema::v1::SetSessionConfigOptionRequest::new(
                        self.acp_session_id.clone(),
                        SessionConfigId::new(config_id.to_string()),
                        agent_client_protocol::schema::v1::SessionConfigValueId::new(effort),
                    ),
                    &self.shared.session_label,
                    self.control
                        .as_ref()
                        .and_then(|control| control.original_request_context()),
                )
                .await;
            }
            None => debug!(
                target: "acp.protocol",
                session = %self.shared.session_label,
                "structured view effort skipped; no thought_level option"
            ),
        }
    }
}
