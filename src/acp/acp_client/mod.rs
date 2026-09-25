//! ACP client: aoe is the ACP client, the agent adapter the server.
//!
//! `AcpClient` launches or attaches to an agent and runs one long-lived
//! connection task (see `connection`) that owns the session; this module holds
//! the public surface, and each concern lives in a submodule.

mod between_prompt;
mod commands;
mod config_options;
mod connection;
mod control;
mod delete;
mod errors;
mod fs_handlers;
mod handshake;
mod lifecycle;
mod opencode;
mod pending;
mod permission_handlers;
mod plan;
mod rate_limit;
mod raw_input;
mod reset;
mod resolve_command;
mod runner;
mod session_identity;
mod session_sandbox;
mod spawn;
mod steer;
mod terminal_handlers;
#[cfg(test)]
mod test_helpers;
mod tool_context;
mod tool_output;
mod transcript_filter;
mod update_events;
mod watchdog;

pub(crate) use connection::CANCEL_ESCALATION_GRACE;
pub use delete::DeleteSessionOutcome;
pub use errors::{AcpError, IncompatibleAgentError};
pub use reset::ResetSessionOutcome;
pub use resolve_command::{resolve_agent_command, ResolvedAgentCommand};
pub use session_sandbox::SessionSandbox;
pub(crate) use spawn::host_environment_denyreason;
pub use spawn::SpawnConfig;

use crate::acp::agent_compat::ExpectedAgent;
use crate::acp::agent_profiles;
use crate::acp::approvals::{ApprovalDecision, Nonce};
use crate::acp::elicitations::{build_response, summarize_answers, ElicitationResolution};
use crate::acp::event_store::AttachmentBlob;
use crate::acp::fs_handler::{FsPolicy, SandboxPathMap};
use crate::acp::state::{AcpSessionId, Event};
use crate::acp::terminal_handler::TerminalManager;
use crate::daemon::PromptAttachmentKind;
use agent_client_protocol::schema::v1::{
    AudioContent, BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, McpServer, TextContent,
};
use agent_client_protocol::ByteStreams;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::Instrument;

use self::commands::{ClientCmd, ConnectMode};
use self::connection::{run_connection_task, ConnectionParams, RunnerLink};
use self::control::{connect_runner_control_v3, ShutdownControlOnDrop};
use self::delete::ACP_SESSION_DELETE_TIMEOUT;
use self::handshake::wait_for_handshake;
use self::lifecycle::TerminalClaim;
use self::pending::{
    ApprovalResolutionMessage, ElicitationResolutionMessage, PendingResolver, PendingResponders,
};
use self::reset::{SESSION_RESET_IN_TASK_TIMEOUT, SESSION_RESET_TIMEOUT};
use self::runner::spawn_runner_detached;
#[cfg(debug_assertions)]
use self::runner::take_injected_fresh_handshake_failure;
use self::spawn::spawn_subprocess;

pub struct AcpClient {
    pub session_id: AcpSessionId,
    /// Taken by the supervisor's drain task so polling events never holds the
    /// client mutex that `send_prompt` needs.
    inbound: Option<mpsc::Receiver<Event>>,
    cmd_tx: Option<mpsc::Sender<ClientCmd>>,
    pending_responders: PendingResponders,
    /// Kills an in-proc agent when the client drops.
    _child: Option<Arc<Mutex<tokio::process::Child>>>,
    /// The detached runner this client launched, which its lease owns.
    runner_pid: Option<u32>,
    pub(crate) native_store: Option<crate::session::ExecutionBinding>,
}

/// What the connection task needs to serve agent fs/* and terminal/* requests.
#[derive(Clone)]
struct SessionResources {
    fs_policy: Arc<FsPolicy>,
    terminals: TerminalManager,
    cwd: PathBuf,
    label: String,
    sandbox: Option<SessionSandbox>,
}

impl SessionResources {
    /// Allowed fs roots are the cwd plus any additional directories.
    fn new(
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        label: String,
        sandbox: Option<(SessionSandbox, SandboxPathMap)>,
    ) -> Self {
        let mut roots = vec![cwd.clone()];
        roots.extend(additional_dirs);
        let (sandbox, fs_policy) = match sandbox {
            Some((handle, path_map)) => (Some(handle), FsPolicy::with_sandbox_map(roots, path_map)),
            None => (None, FsPolicy::new(roots)),
        };
        Self {
            fs_policy: Arc::new(fs_policy),
            terminals: TerminalManager::new(),
            cwd,
            label,
            sandbox,
        }
    }

    /// Sandboxed agents run in-container, so session requests carry the
    /// container workdir (#2871).
    fn agent_cwd(&self) -> PathBuf {
        session_sandbox::agent_request_cwd(
            self.sandbox.as_ref().map(|s| s.container_workdir.as_path()),
            &self.cwd,
        )
    }
}

/// Everything a launch or attach hands the connection task.
struct Launch {
    session_id: AcpSessionId,
    mode: ConnectMode,
    cwd: PathBuf,
    additional_dirs: Vec<PathBuf>,
    sandbox: Option<(SessionSandbox, SandboxPathMap)>,
    profile: &'static agent_profiles::AgentProfile,
    install_binary: String,
    source_profile: Option<String>,
    default_effort: Option<String>,
    default_mode: Option<String>,
    default_model: Option<String>,
    mcp_servers: Vec<McpServer>,
}

impl Launch {
    /// Spawns the connection task in an `acp_session` span, which the
    /// per-session log tee routes by (#1864), and returns the client half.
    fn start<W, R>(
        self,
        (event_tx, event_rx): (mpsc::Sender<Event>, mpsc::Receiver<Event>),
        transport: ByteStreams<W, R>,
        child: Option<Arc<Mutex<tokio::process::Child>>>,
        runner: Option<RunnerLink>,
    ) -> (AcpClient, oneshot::Receiver<Result<(), AcpError>>)
    where
        W: futures_util::AsyncWrite + Send + 'static,
        R: futures_util::AsyncRead + Send + 'static,
    {
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let (ready_tx, ready_rx) = oneshot::channel();
        let pending_responders: PendingResponders = Arc::new(Mutex::new(HashMap::new()));
        let label = self.session_id.0.clone();
        let params = ConnectionParams {
            event_tx,
            cmd_rx,
            child: child.clone(),
            pending_responders: pending_responders.clone(),
            resources: SessionResources::new(
                self.cwd,
                self.additional_dirs,
                label.clone(),
                self.sandbox,
            ),
            mode: self.mode,
            ready_tx,
            profile: self.profile,
            expected_agent: ExpectedAgent::from_command(&self.install_binary),
            source_profile: self.source_profile,
            default_effort: self.default_effort,
            default_mode: self.default_mode,
            default_model: self.default_model,
            mcp_servers: self.mcp_servers,
            runner,
        };
        let span = tracing::info_span!("acp_session", session = %label);
        tokio::spawn(run_connection_task(transport, params).instrument(span));
        let client = AcpClient {
            session_id: self.session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders,
            _child: child,
            runner_pid: None,
            native_store: None,
        };
        (client, ready_rx)
    }
}

impl AcpClient {
    fn fake(
        session_id: AcpSessionId,
        cmd_tx: Option<mpsc::Sender<ClientCmd>>,
    ) -> (Self, mpsc::Sender<Event>) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let client = Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx,
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            _child: None,
            runner_pid: None,
            native_store: None,
        };
        (client, event_tx)
    }

    /// A client whose command consumer runs `handle` on every command.
    #[cfg(test)]
    fn fake_with_consumer(
        session_id: AcpSessionId,
        mut handle: impl FnMut(ClientCmd) + Send + 'static,
    ) -> (Self, mpsc::Sender<Event>) {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientCmd>(16);
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                handle(cmd);
            }
        });
        Self::fake(session_id, Some(cmd_tx))
    }

    #[cfg(test)]
    pub(crate) async fn test_flush_commands(&self) {
        let (done, flushed) = oneshot::channel();
        self.cmd_tx
            .as_ref()
            .expect("command recorder")
            .send(ClientCmd::FlushForTest(done))
            .await
            .expect("recorder running");
        tokio::time::timeout(std::time::Duration::from_secs(5), flushed)
            .await
            .expect("recorder drain")
            .expect("recorder acknowledgement");
    }

    /// Attached and stdio clients have none.
    pub fn runner_pid(&self) -> Option<u32> {
        self.runner_pid
    }

    #[cfg(test)]
    pub fn with_runner_pid(mut self, pid: u32) -> Self {
        self.runner_pid = Some(pid);
        self
    }

    /// A client that spawns nothing, for structured view state tests.
    pub fn fake_for_test(session_id: AcpSessionId) -> (Self, mpsc::Sender<Event>) {
        Self::fake(session_id, None)
    }

    /// Every command fails with `AgentExited`, as between a connection task
    /// ending and its respawn (#3401).
    #[cfg(test)]
    pub fn fake_for_test_dead_connection(session_id: AcpSessionId) -> Self {
        let (cmd_tx, _) = mpsc::channel::<ClientCmd>(16);
        Self::fake(session_id, Some(cmd_tx)).0
    }

    /// Flags whether `session/delete` was issued, answering it immediately
    /// (#1710).
    #[cfg(test)]
    pub fn fake_for_test_recording(
        session_id: AcpSessionId,
    ) -> (Self, mpsc::Sender<Event>, Arc<AtomicBool>) {
        let saw_delete = Arc::new(AtomicBool::new(false));
        let flag = saw_delete.clone();
        let (client, event_tx) = Self::fake_with_consumer(session_id, move |cmd| {
            if let ClientCmd::DeleteSession { respond_to, .. } = cmd {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = respond_to.send(DeleteSessionOutcome::UnsupportedMethod);
            }
        });
        (client, event_tx, saw_delete)
    }

    /// Records command names in order, answering resets with `"fresh-id"` and
    /// deletes as unsupported (#2979).
    #[cfg(test)]
    pub fn fake_for_test_cmd_recording(
        session_id: AcpSessionId,
    ) -> (
        Self,
        mpsc::Sender<Event>,
        Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) {
        let cmds = Arc::new(std::sync::Mutex::new(Vec::new()));
        let record = cmds.clone();
        let (client, event_tx) = Self::fake_with_consumer(session_id, move |cmd| {
            let name = match cmd {
                ClientCmd::Prompt(_) => "prompt",
                ClientCmd::Cancel => "cancel",
                ClientCmd::ForceStop => "force_stop",
                ClientCmd::SetMode(_) => "set_mode",
                ClientCmd::SetConfigOption { .. } => "set_config_option",
                ClientCmd::ResumeBackgroundTailing(_) => "resume_background_tailing",
                ClientCmd::DeleteSession { respond_to, .. } => {
                    let _ = respond_to.send(DeleteSessionOutcome::UnsupportedMethod);
                    "delete_session"
                }
                ClientCmd::ResetSession { respond_to, .. } => {
                    let _ = respond_to.send(ResetSessionOutcome::Reset {
                        new_acp_session_id: "fresh-id".into(),
                    });
                    "reset_session"
                }
                ClientCmd::FlushForTest(done) => {
                    let _ = done.send(());
                    return;
                }
                ClientCmd::Shutdown => "shutdown",
            };
            record.lock().expect("cmd record mutex").push(name);
        });
        (client, event_tx, cmds)
    }

    /// Fails every driven reset with `message`.
    #[cfg(test)]
    pub fn fake_for_test_reset_failure(
        session_id: AcpSessionId,
        message: impl Into<String>,
    ) -> (Self, mpsc::Sender<Event>) {
        let message = message.into();
        Self::fake_with_consumer(session_id, move |cmd| match cmd {
            ClientCmd::ResetSession { respond_to, .. } => {
                let _ = respond_to.send(ResetSessionOutcome::Failed {
                    message: message.clone(),
                });
            }
            ClientCmd::DeleteSession { respond_to, .. } => {
                let _ = respond_to.send(DeleteSessionOutcome::UnsupportedMethod);
            }
            _ => {}
        })
    }

    /// Launch an agent: a detached runner when `socket_path` is set, else an
    /// in-proc stdio subprocess (tests).
    pub async fn spawn(config: SpawnConfig, session_id: AcpSessionId) -> Result<Self, AcpError> {
        // A missing cwd would ENOENT like a missing binary and misdirect the
        // user to reinstalling the adapter (#1089).
        if !config.cwd.exists() {
            return Err(AcpError::ProjectPathMissing {
                path: config.cwd.clone(),
            });
        }
        let sandbox = match &config.sandbox_info {
            // Resolving the workdir touches git2 and may shell out to docker.
            Some(info) => {
                let (info, cwd) = (info.clone(), config.cwd.clone());
                let profile = config.source_profile.clone();
                let resolved = tokio::task::spawn_blocking(move || {
                    SessionSandbox::from_info(&info, cwd.as_path(), profile)
                })
                .await
                .map_err(|e| AcpError::Spawn(format!("sandbox resolve task panicked: {e}")))??;
                Some(resolved)
            }
            None => None,
        };
        let launch = |sandbox| Launch {
            mode: ConnectMode::Fresh {
                stored_acp_session_id: config.stored_acp_session_id.clone(),
                seed_history_replay: config.seed_history_replay,
                fork_from: config.fork_from.clone(),
            },
            session_id: session_id.clone(),
            cwd: config.cwd.clone(),
            additional_dirs: config.additional_dirs.clone(),
            sandbox,
            profile: agent_profiles::resolve(&config.agent_key),
            install_binary: config.spec.command.clone(),
            source_profile: config.source_profile.clone(),
            default_effort: config.default_effort.clone(),
            default_mode: config.default_mode.clone(),
            default_model: config.default_model.clone(),
            mcp_servers: config.mcp_servers.clone(),
        };

        if let Some(socket_path) = config.socket_path.clone() {
            // A fresh spawn overwrites the registry entry, so reap any prior
            // runner's process group first or its children leak (#1689).
            crate::process::worker_registry::terminate_and_wait(&session_id.0).await;
            let runner_sandbox = sandbox.as_ref().map(|(handle, _)| handle);
            let (runner_pid, native_store) =
                spawn_runner_detached(&config, &socket_path, session_id.0.clone(), runner_sandbox)?;
            let mut client = Self::connect_via_socket(socket_path, launch(sandbox)).await?;
            client.runner_pid = Some(runner_pid);
            client.native_store = native_store;
            return Ok(client);
        }

        let (mut child, native_store) = spawn_subprocess(&config)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::Spawn("no stdin handle".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::Spawn("no stdout handle".into()))?;
        let child = Arc::new(Mutex::new(child));
        let launch = launch(sandbox);
        let (label, install_binary) = (launch.session_id.0.clone(), launch.install_binary.clone());
        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        let events = mpsc::channel(64);
        let (mut client, ready_rx) = launch.start(events, transport, Some(child.clone()), None);
        wait_for_handshake(&label, ready_rx, Some(&child), &install_binary).await?;
        client.native_store = native_store;
        Ok(client)
    }

    /// Dial a runner's control socket, which carries the whole transport
    /// (#2977). The runner owns the agent, so dropping this client leaves the
    /// worker running.
    async fn connect_via_socket(socket_path: PathBuf, launch: Launch) -> Result<Self, AcpError> {
        let control_path = crate::process::worker::control_socket_sibling(&socket_path);
        // Debug-only #1890 hook: fail a fresh handshake after the runner is up.
        #[cfg(debug_assertions)]
        if matches!(launch.mode, ConnectMode::Fresh { .. })
            && take_injected_fresh_handshake_failure()
        {
            return Err(AcpError::Spawn(
                "injected fresh-handshake failure (AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES)".into(),
            ));
        }
        let label = launch.session_id.0.clone();
        let terminal_claim = Arc::new(TerminalClaim::new());
        // The reader may see a cached completion right after Attach, before
        // the handshake marks the adopted turn.
        let prompt_in_flight = Arc::new(AtomicBool::new(matches!(
            &launch.mode,
            ConnectMode::Resume {
                in_flight_turn: true,
                ..
            }
        )));
        let events = mpsc::channel(64);
        let (control, crate_transport) = connect_runner_control_v3(
            &control_path,
            events.0.clone(),
            label.clone(),
            terminal_claim.clone(),
            prompt_in_flight.clone(),
        )
        .await
        .map_err(|error| {
            AcpError::Spawn(format!(
                "runner control attach failed at {}: {error:#}",
                control_path.display()
            ))
        })?;
        let (read_half, write_half) = tokio::io::split(crate_transport);
        let transport = ByteStreams::new(write_half.compat_write(), read_half.compat());
        // Shuts the runner channel down if the handshake fails or is dropped.
        let mut handshake_control = ShutdownControlOnDrop(Some(control.clone()));
        let install_binary = launch.install_binary.clone();
        let runner = RunnerLink {
            control,
            terminal_claim,
            prompt_in_flight,
        };
        let (client, ready_rx) = launch.start(events, transport, None, Some(runner));
        wait_for_handshake(&label, ready_rx, None, &install_binary).await?;
        handshake_control.0.take();
        Ok(client)
    }

    /// Reattach to a live runner on `aoe serve` startup. The agent still holds
    /// the session, so no `session/new` or `session/load` is sent: that would
    /// split context or double-load a busy session. `in_flight_turn` lets the
    /// runner's cached completion finish the adopted turn, with a resume-idle
    /// fallback so the UI cannot stay "thinking".
    #[allow(clippy::too_many_arguments)]
    pub async fn attach(
        socket_path: PathBuf,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        stored_acp_session_id: String,
        in_flight_turn: bool,
        session_id: AcpSessionId,
        sandbox: Option<(SessionSandbox, SandboxPathMap)>,
        agent_key: String,
        source_profile: Option<String>,
    ) -> Result<Self, AcpError> {
        // The binary name keeps the compatibility gate active on reattach; an
        // unknown agent maps to `Other` anyway.
        let install_binary = crate::acp::AgentRegistry::with_defaults()
            .get(&agent_key)
            .map(|spec| spec.command.clone())
            .unwrap_or_default();
        let launch = Launch {
            session_id,
            mode: ConnectMode::Resume {
                acp_session_id: stored_acp_session_id,
                in_flight_turn,
            },
            cwd,
            additional_dirs,
            sandbox,
            profile: agent_profiles::resolve(&agent_key),
            install_binary,
            source_profile,
            // Resume sends no session/new or load, so defaults and MCP servers
            // were applied on first connect.
            default_effort: None,
            default_mode: None,
            default_model: None,
            mcp_servers: Vec::new(),
        };
        Self::connect_via_socket(socket_path, launch).await
    }

    async fn send_cmd(&self, cmd: ClientCmd) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx.send(cmd).await.map_err(|_| AcpError::AgentExited)
    }

    /// Attachments follow the text block. Callers gate kinds on the agent's
    /// prompt capabilities (#1000).
    pub async fn send_prompt(
        &self,
        text: &str,
        attachments: &[AttachmentBlob],
    ) -> Result<(), AcpError> {
        use base64::Engine as _;
        if self.cmd_tx.is_none() {
            return Err(AcpError::NotRunning);
        }
        let mut blocks = Vec::with_capacity(1 + attachments.len());
        blocks.push(ContentBlock::Text(TextContent::new(text)));
        for att in attachments {
            let data = base64::engine::general_purpose::STANDARD.encode(&att.data);
            let mime = att.mime_type.clone();
            blocks.push(match att.kind {
                PromptAttachmentKind::Image => ContentBlock::Image(ImageContent::new(data, mime)),
                PromptAttachmentKind::Audio => ContentBlock::Audio(AudioContent::new(data, mime)),
                PromptAttachmentKind::Resource => {
                    // The bytes never leave the daemon; the uri only names them.
                    let uri = format!("attachment:///{}", att.id);
                    let blob = BlobResourceContents::new(data, uri).mime_type(mime);
                    ContentBlock::Resource(EmbeddedResource::new(
                        EmbeddedResourceResource::BlobResourceContents(blob),
                    ))
                }
            });
        }
        self.send_cmd(ClientCmd::Prompt(blocks)).await
    }

    /// Best-effort `session/cancel`; Ok even with no turn in flight.
    pub async fn cancel_prompt(&self) -> Result<(), AcpError> {
        self.send_cmd(ClientCmd::Cancel).await
    }

    /// Skip the cancel-escalation grace: an in-flight turn ends as
    /// `user_forced`, which kills and respawns the worker (#1727).
    pub async fn force_cancel(&self) -> Result<(), AcpError> {
        self.send_cmd(ClientCmd::ForceStop).await
    }

    /// Config-option mode wins over legacy `session/set_mode` when both exist.
    pub async fn set_mode(&self, mode_id: &str) -> Result<(), AcpError> {
        self.send_cmd(ClientCmd::SetMode(mode_id.to_string())).await
    }

    pub async fn set_config_option(&self, config_id: &str, value: &str) -> Result<(), AcpError> {
        self.send_cmd(ClientCmd::SetConfigOption {
            config_id: config_id.to_string(),
            value: value.to_string(),
        })
        .await
    }

    /// Removes a parked permission responder. A nonce belonging to an
    /// elicitation is unknown here.
    async fn take_approval(
        &self,
        nonce: &Nonce,
    ) -> Result<oneshot::Sender<ApprovalResolutionMessage>, AcpError> {
        let mut map = self.pending_responders.lock().await;
        let PendingResolver::Approval(_) = &map.get(nonce).ok_or(AcpError::UnknownNonce)?.resolver
        else {
            return Err(AcpError::UnknownNonce);
        };
        let PendingResolver::Approval(resolver) = map.remove(nonce).unwrap().resolver else {
            unreachable!("checked above");
        };
        Ok(resolver)
    }

    /// Spawn tailers for sub-agents that survived a daemon restart, from
    /// every unresolved `BackgroundAgentLaunched` for the session. A send
    /// failure just means the connection already died; the caller's next
    /// sweep detaches what is left.
    pub async fn resume_background_tailing(
        &self,
        launches: Vec<crate::acp::event_store::UnresolvedBackgroundAgentLaunch>,
    ) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::ResumeBackgroundTailing(
                launches
                    .into_iter()
                    .map(|l| (l.agent_id, l.output_file))
                    .collect(),
            ))
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// `option_id` picks one of the agent's own options; `None` picks by kind.
    pub async fn resolve_permission(
        &self,
        nonce: Nonce,
        decision: ApprovalDecision,
        option_id: Option<String>,
    ) -> Result<(), AcpError> {
        self.take_approval(&nonce)
            .await?
            .send(ApprovalResolutionMessage::Decision {
                decision,
                option_id,
            })
            .map_err(|_| AcpError::AgentExited)
    }

    pub async fn cancel_permission(&self, nonce: Nonce) -> Result<(), AcpError> {
        self.take_approval(&nonce)
            .await?
            .send(ApprovalResolutionMessage::Cancelled)
            .map_err(|_| AcpError::AgentExited)
    }

    /// The answer is validated before the responder is consumed, so an invalid
    /// answer leaves the elicitation pending for resubmission (#2100).
    pub async fn resolve_elicitation(
        &self,
        nonce: Nonce,
        resolution: ElicitationResolution,
    ) -> Result<(), AcpError> {
        let mut map = self.pending_responders.lock().await;
        let PendingResolver::Elicitation { elicitation, .. } =
            &map.get(&nonce).ok_or(AcpError::UnknownNonce)?.resolver
        else {
            return Err(AcpError::UnknownNonce);
        };
        let outcome = resolution.outcome();
        // Rendered for the transcript before `build_response` consumes it (#2209).
        let answers = match &resolution {
            ElicitationResolution::Accept { answers } => summarize_answers(elicitation, answers),
            ElicitationResolution::Decline | ElicitationResolution::Cancel => Vec::new(),
        };
        let response = build_response(elicitation, resolution)
            .map_err(|e| AcpError::InvalidAnswer(e.to_string()))?;
        let PendingResolver::Elicitation { resolver, .. } = map.remove(&nonce).unwrap().resolver
        else {
            unreachable!("checked above");
        };
        resolver
            .send(ElicitationResolutionMessage {
                response,
                outcome,
                answers,
            })
            .map_err(|_| AcpError::AgentExited)
    }

    /// Best-effort experimental `session/delete` before shutdown, so adapters
    /// can clean up persisted session state (#1404). Never fatal.
    pub async fn delete_session(&self, acp_session_id: String) -> DeleteSessionOutcome {
        let Some(cmd_tx) = self.cmd_tx.as_ref() else {
            return DeleteSessionOutcome::Failed("client not running".into());
        };
        let (tx, rx) = oneshot::channel();
        // The guard covers the send too, so a wedged task cannot stall delete,
        // and outlasts the in-task timeout so the task's classification wins.
        let request = async {
            let cmd = ClientCmd::DeleteSession {
                acp_session_id,
                respond_to: tx,
            };
            if cmd_tx.send(cmd).await.is_err() {
                return DeleteSessionOutcome::Failed("connect task gone".into());
            }
            rx.await
                .unwrap_or_else(|_| DeleteSessionOutcome::Failed("respond channel closed".into()))
        };
        let guard = ACP_SESSION_DELETE_TIMEOUT + std::time::Duration::from_millis(500);
        tokio::time::timeout(guard, request)
            .await
            .unwrap_or(DeleteSessionOutcome::TimedOut)
    }

    /// Drive a conversation reset on the live worker for clear commands whose
    /// adapter cannot return a durable post-reset id (#2979). `text` is the
    /// user's clear invocation, echoed in a mid-turn refusal.
    pub async fn reset_session(&self, text: &str) -> Result<ResetSessionOutcome, AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        let (tx, rx) = oneshot::channel();
        // Created before queueing so queue time counts against the reset.
        let deadline = tokio::time::Instant::now() + SESSION_RESET_IN_TASK_TIMEOUT;
        let failed = |message: &str| ResetSessionOutcome::Failed {
            message: message.into(),
        };
        let request = async {
            let cmd = ClientCmd::ResetSession {
                text: text.to_string(),
                deadline,
                respond_to: tx,
            };
            if cmd_tx.send(cmd).await.is_err() {
                return failed("connect task gone");
            }
            rx.await
                .unwrap_or_else(|_| failed("respond channel closed"))
        };
        Ok(tokio::time::timeout(SESSION_RESET_TIMEOUT, request)
            .await
            .unwrap_or_else(|_| ResetSessionOutcome::Failed {
                message: format!(
                    "agent did not answer session/new within {}s",
                    SESSION_RESET_TIMEOUT.as_secs()
                ),
            }))
    }

    pub async fn shutdown(&self) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        let _ = cmd_tx.send(ClientCmd::Shutdown).await;
        Ok(())
    }

    /// `None` once the receiver was taken or the connection task ended.
    pub async fn next_event(&mut self) -> Option<Event> {
        self.inbound.as_mut()?.recv().await
    }

    pub fn take_inbound(&mut self) -> Option<mpsc::Receiver<Event>> {
        self.inbound.take()
    }
}
