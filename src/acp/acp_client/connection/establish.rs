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
use agent_client_protocol::{Agent, ConnectionTo, JsonRpcRequest};
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
use crate::acp::acp_client::commands::{ClientCmd, ConnectMode};
use crate::acp::acp_client::config_options::{
    apply_config_default, config_option_failure_event, config_options_event, mode_config_id,
    modes_available_event, thought_level_config_id, ConfigOptionDispatchPurpose, SessionChannels,
};
use crate::acp::acp_client::control::{establish_session_v3, DaemonControlClient};
use crate::acp::acp_client::errors::{acp_internal_error, AcpError, IncompatibleAgentError};
use crate::acp::acp_client::handshake::{build_initialize_request, should_fork};
use crate::acp::acp_client::lifecycle::LifecycleEnvelope;
use crate::acp::acp_client::session_identity::ordered_session_request;

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
    pub(super) cmd_rx: mpsc::Receiver<ClientCmd>,
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
    info!(target: "acp.protocol", session = %label, "initializing ACP agent");
    let init: InitializeResponse = match ctx.control.as_ref() {
        Some(control) => {
            let params = serde_json::to_value(build_initialize_request())
                .map_err(|e| acp_internal_error(format!("serialize initialize params: {e}")))?;
            serde_json::from_value(control.initialize(params).await?)
                .map_err(|e| acp_internal_error(format!("deserialize initialize result: {e}")))?
        }
        None => {
            connection
                .send_request(build_initialize_request())
                .block_task()
                .await?
        }
    };

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
        if let Some(tx) = ctx.ready_tx.lock().await.take() {
            let _ = tx.send(Err(AcpError::IncompatibleAgent(Box::new(
                IncompatibleAgentError { detail, message },
            ))));
        }
        return Ok(None);
    }

    let load_session_capable = init.agent_capabilities.load_session;
    // Re-derived on every connect (a respawn may land on another adapter
    // build) and emitted even when false so replay cannot keep a stale true.
    let steering_capable = agent_compat::supports_steering(ctx.expected_agent, &init);
    let prompt_caps = &init.agent_capabilities.prompt_capabilities;
    shared
        .emit(Event::PromptCapabilities {
            image: prompt_caps.image,
            audio: prompt_caps.audio,
            embedded_context: prompt_caps.embedded_context,
            load_session: Some(load_session_capable),
            steering: steering_capable,
        })
        .await;
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
    shared
        .adopted_turn_active
        .store(arm_resume_watchdog, Ordering::Relaxed);
    info!(
        target: "acp.protocol",
        session = %label,
        load_session_capable,
        mode = ?ctx.mode,
        "initialize handshake complete"
    );
    // Ready now: session/load can replay a whole transcript and must stay
    // outside the handshake timeout (#2276).
    if let Some(tx) = ctx.ready_tx.lock().await.take() {
        let _ = tx.send(Ok(()));
    }

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
        session_from_storage: matches!(ctx.mode, ConnectMode::Resume { .. }),
        channels: SessionChannels::default(),
        steering_capable,
        source_profile: ctx.source_profile,
        default_effort: ctx.default_effort,
        default_mode: ctx.default_mode,
        default_model: ctx.default_model,
        agent_cwd: ctx.agent_cwd,
        mcp_servers: mcp_servers.clone(),
        cmd_rx: ctx.cmd_rx,
        lifecycle_rx: ctx.lifecycle_rx,
        pending_prompts: VecDeque::new(),
    };
    session.acp_session_id = match ctx.mode {
        ConnectMode::Resume { acp_session_id, .. } => session.resume(acp_session_id).await?,
        ConnectMode::Fresh {
            stored_acp_session_id,
            seed_history_replay,
            fork_from,
        } => {
            let fork_capable = init.agent_capabilities.session_capabilities.fork.is_some();
            session
                .fresh(
                    stored_acp_session_id,
                    seed_history_replay,
                    fork_from,
                    fork_capable,
                    load_session_capable,
                    mcp_servers,
                )
                .await?
        }
    };
    session.apply_default_model().await;
    session.apply_default_effort().await;
    if arm_resume_watchdog {
        spawn_resume_idle_watchdog(shared);
    }
    Ok(Some(session))
}

/// Fallback when an adopted turn neither completes nor shows activity, so the
/// UI cannot stay "thinking" forever. Once the turn is observable, completion
/// belongs to the between-prompt watchdog.
fn spawn_resume_idle_watchdog(shared: Arc<Shared>) {
    let grace_ms = resume_idle_grace().as_millis() as i64;
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            if shared.terminal_claim.claimed()
                || shared.prompt_sent_since_attach.load(Ordering::Relaxed)
            {
                return;
            }
            if shared.first_event_after_attach.load(Ordering::Relaxed) {
                info!(
                    target: "acp.protocol",
                    session = %shared.session_label,
                    "resume-idle watchdog: disarming, in-flight turn is observable"
                );
                return;
            }
            let idle_ms = now_ms() - shared.last_event_at.load(Ordering::Relaxed);
            if idle_ms >= grace_ms {
                if shared.terminal_claim.claim() {
                    info!(
                        target: "acp.protocol",
                        session = %shared.session_label,
                        idle_ms,
                        "resume-idle watchdog: synthesizing Stopped for orphaned in-flight turn"
                    );
                    shared
                        .emit(Event::Stopped {
                            reason: "reattach_idle".into(),
                        })
                        .await;
                }
                return;
            }
        }
    });
}

impl Session {
    /// A request that mints a new native session: sent by the runner when one
    /// is attached, else over the crate connection. Updates the agent sends
    /// before its reply stay buffered until [`Shared::commit_session`] decides
    /// which id owns them (#3937).
    async fn minting_request<Req>(
        &self,
        method: &str,
        req: Req,
        native_id: fn(&Req::Response) -> SessionId,
    ) -> Result<Req::Response, agent_client_protocol::Error>
    where
        Req: JsonRpcRequest + serde::Serialize,
        Req::Response: serde::de::DeserializeOwned + Send + 'static,
    {
        let ingress = &self.shared.ingress;
        let generation = {
            let _guard = ingress.fence.lock().await;
            ingress.begin()
        };
        match self.control.as_ref() {
            Some(control) => establish_session_v3(control, method, &req).await,
            None => {
                ordered_session_request(&self.connection, ingress, generation, req, native_id).await
            }
        }
    }

    /// The agent still owns the live session, so no load/new is sent. A prior
    /// daemon's reset may commit after our registry snapshot, so a runner's
    /// cached identity wins.
    async fn resume(&self, stored: String) -> Result<SessionId, agent_client_protocol::Error> {
        let stored = match self.control.as_ref() {
            Some(control) => control.resume_session().await?,
            None => stored,
        };
        info!(
            target: "acp.protocol",
            session = %self.shared.session_label,
            stored_id = %stored,
            "resume mode: reusing runner session without agent load/new"
        );
        // Clears a sticky startup error in the UI; same-id is a no-op server-side.
        let id = SessionId::from(stored);
        self.shared.commit_session(id.clone()).await?;
        Ok(id)
    }

    /// A runner sends SessionReady ahead of the replay it is still flushing,
    /// so the load only completes once the barrier behind that replay has
    /// been dispatched (#4016).
    async fn load_request(
        &self,
        req: LoadSessionRequest,
    ) -> Result<LoadSessionResponse, agent_client_protocol::Error> {
        let Some(control) = self.control.as_ref() else {
            return self.connection.send_request(req).block_task().await;
        };
        let result = establish_session_v3(control, "session/load", &req).await;
        if result.is_ok() {
            control.session_replayed().await;
        }
        result
    }

    async fn fresh(
        &mut self,
        stored: Option<String>,
        seed_history_replay: bool,
        fork_from: Option<String>,
        fork_capable: bool,
        load_session_capable: bool,
        mcp_servers: Vec<McpServer>,
    ) -> Result<SessionId, agent_client_protocol::Error> {
        let label = self.shared.session_label.clone();
        let fork_requested = fork_from.as_deref().is_some_and(|s| !s.is_empty());
        // Lost resume context is announced only once a replacement succeeds.
        let mut context_reset_reason =
            (stored.is_some() && !load_session_capable && !fork_requested).then(|| {
                "session/load unavailable; started a new session with empty context".to_string()
            });

        if should_fork(fork_from.as_deref(), fork_capable) {
            // Never falls through to session/new, which would hand the user
            // an empty session they believe is a fork.
            let parent = fork_from.unwrap_or_default();
            info!(target: "acp.protocol", session = %label, parent_acp_id = %parent, "structured fork via session/fork");
            let req = ForkSessionRequest::new(parent.clone(), self.agent_cwd.clone())
                .mcp_servers(mcp_servers);
            return match self
                .minting_request("session/fork", req, |resp: &ForkSessionResponse| {
                    resp.session_id.clone()
                })
                .await
            {
                Ok(resp) => {
                    let new_id = resp.session_id.clone();
                    self.shared.commit_session(new_id.clone()).await?;
                    info!(
                        target: "acp.protocol",
                        session = %label,
                        parent_acp_id = %parent,
                        new_id = %new_id.0,
                        "session/fork succeeded, captured forked acp_session_id"
                    );
                    self.channels =
                        SessionChannels::new(resp.modes.as_ref(), resp.config_options.as_deref());
                    if let Some(modes) = resp.modes.as_ref() {
                        self.shared.emit(modes_available_event(modes)).await;
                    }
                    if let Some(event) = config_options_event(resp.config_options) {
                        self.shared.emit(event).await;
                    }
                    Ok(new_id)
                }
                Err(e) => {
                    warn!(
                        target: "acp.protocol",
                        session = %label,
                        parent_acp_id = %parent,
                        "session/fork failed; failing spawn (no session/new fallback): {e}"
                    );
                    // Clears the one-shot fork marker so reattach does not
                    // retry the failing fork forever.
                    self.shared
                        .emit(Event::SessionContextReset {
                            reason: format!("fork_failed: {e}"),
                        })
                        .await;
                    Err(e)
                }
            };
        }
        if fork_requested {
            warn!(
                target: "acp.protocol",
                session = %label,
                "fork requested but agent does not advertise fork; falling back to session/new"
            );
            self.shared
                .emit(Event::SessionContextReset {
                    reason: "fork_unsupported_by_agent".to_string(),
                })
                .await;
        }

        if let (true, Some(stored)) = (load_session_capable, stored) {
            info!(target: "acp.protocol", session = %label, stored_id = %stored, "resuming session via session/load");
            // Set before sending: the adapter replays history during the load.
            // An import (#2276) has an empty store and wants the replay.
            if !seed_history_replay {
                self.shared
                    .suppress_history_replay
                    .store(true, Ordering::Relaxed);
            }
            // The replay arrives under the stored id, so identity commits
            // before the request rather than on its reply.
            {
                let ingress = &self.shared.ingress;
                let _guard = ingress.fence.lock().await;
                ingress.finish(Some(SessionId::from(stored.clone())))?;
            }
            let req = LoadSessionRequest::new(stored.clone(), self.agent_cwd.clone())
                .mcp_servers(mcp_servers.clone());
            match self.load_request(req).await {
                Ok(resp) => {
                    self.session_from_storage = true;
                    info!(
                        target: "acp.protocol",
                        session = %label,
                        stored_id = %stored,
                        "session/load succeeded; suppressing post-load history replay"
                    );
                    self.channels =
                        SessionChannels::new(resp.modes.as_ref(), resp.config_options.as_deref());
                    self.shared
                        .emit(Event::AcpSessionAssigned {
                            acp_session_id: stored.clone(),
                        })
                        .await;
                    if let Some(event) = config_options_event(resp.config_options) {
                        self.shared.emit(event).await;
                    }
                    return Ok(SessionId::from(stored));
                }
                // A partial replay may already be in the empty store, so a
                // failed import must not continue on a fresh session.
                Err(e) if seed_history_replay => {
                    warn!(
                        target: "acp.protocol",
                        session = %label,
                        stored_id = %stored,
                        "session/load failed for imported session; failing import (no session/new fallback): {e}"
                    );
                    return Err(e);
                }
                Err(e) => {
                    warn!(
                        target: "acp.protocol",
                        session = %label,
                        stored_id = %stored,
                        "session/load failed, falling back to session/new: {e}"
                    );
                    {
                        let ingress = &self.shared.ingress;
                        let _guard = ingress.fence.lock().await;
                        ingress.finish(None)?;
                    }
                    self.shared
                        .suppress_history_replay
                        .store(false, Ordering::Relaxed);
                    if !fork_requested {
                        context_reset_reason = Some(format!("session/load failed: {e}"));
                    }
                }
            }
        }
        self.new_session(mcp_servers, context_reset_reason).await
    }

    async fn new_session(
        &mut self,
        mcp_servers: Vec<McpServer>,
        context_reset_reason: Option<String>,
    ) -> Result<SessionId, agent_client_protocol::Error> {
        let label = self.shared.session_label.clone();
        info!(target: "acp.protocol", session = %label, "creating fresh session via session/new");
        let req = NewSessionRequest::new(self.agent_cwd.clone()).mcp_servers(mcp_servers);
        let new_session = self
            .minting_request("session/new", req, |resp: &NewSessionResponse| {
                resp.session_id.clone()
            })
            .await?;
        let id = new_session.session_id.clone();
        if let Some(reason) = context_reset_reason {
            self.shared
                .emit(Event::SessionContextReset { reason })
                .await;
        }
        self.shared.commit_session(id.clone()).await?;
        info!(
            target: "acp.protocol",
            session = %label,
            new_id = %id.0,
            "session/new succeeded, captured acp_session_id"
        );
        self.channels = SessionChannels::new(
            new_session.modes.as_ref(),
            new_session.config_options.as_deref(),
        );
        if let Some(modes) = &new_session.modes {
            self.shared.emit(modes_available_event(modes)).await;
        }
        if let Some(event) = config_options_event(new_session.config_options.clone()) {
            self.shared.emit(event).await;
        }
        // Only a live `mode` config option is driven from defaults (#2631).
        if let (Some(mode), Some(options)) = (
            self.default_mode.as_deref(),
            new_session.config_options.as_deref(),
        ) {
            match mode_config_id(options) {
                Some(config_id) => {
                    let _ = apply_config_default(
                        &self.connection,
                        &self.shared.event_tx,
                        id.clone(),
                        config_id,
                        mode,
                        &label,
                    )
                    .await;
                }
                None => debug!(
                    target: "acp.protocol",
                    session = %label,
                    "default structured view mode skipped; no mode option"
                ),
            }
        }
        Ok(id)
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
        let result = apply_config_default(
            &self.connection,
            &self.shared.event_tx,
            self.acp_session_id.clone(),
            SessionConfigId::new(option.id.clone()),
            model,
            label,
        )
        .await;
        match result {
            Ok(options) => {
                self.channels.thought_level_config_option_id =
                    thought_level_config_id(&options).map(|id| id.0.to_string());
            }
            Err(reason) => {
                let event = config_option_failure_event(
                    option.id.clone(),
                    model.to_string(),
                    reason,
                    ConfigOptionDispatchPurpose::Generic,
                );
                self.shared.emit(event).await;
            }
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
                    &self.shared.event_tx,
                    self.acp_session_id.clone(),
                    SessionConfigId::new(config_id.to_string()),
                    effort,
                    &self.shared.session_label,
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
