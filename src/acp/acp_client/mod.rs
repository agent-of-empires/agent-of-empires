//! ACP client: aoe is the ACP client, the agent adapter the server.
//!
//! `AcpClient` launches or attaches to an agent and runs one long-lived
//! connection task (see `connection`) that owns the session; this module holds
//! the public surface, and each concern lives in a submodule.

mod between_prompt;
mod close;
mod commands;
mod config_options;
mod connection;
mod control;
mod delete;
mod errors;
mod fs_handlers;
mod handshake;
mod lifecycle;
mod list;
mod opencode;
pub(crate) mod pending;
mod permission_handlers;
mod plan;
mod rate_limit;
mod raw_input;
pub(crate) use raw_input::background_resource_from_value as native_resource_start_cue;
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
#[cfg(test)]
pub(crate) use test_helpers::{
    SdkTaskObservation, SdkTestGate, SdkTestOptions, TrackingObservation,
};
mod tool_context;
mod tool_output;
mod transcript_filter;
mod update_events;
pub(crate) use update_events::native_compaction_cue;
mod watchdog;

pub use close::CloseSessionOutcome;
pub(crate) use connection::CANCEL_ESCALATION_GRACE;
pub use delete::DeleteSessionOutcome;
pub use errors::{AcpError, IncompatibleAgentError};
pub(crate) use list::probe_option_catalog;
pub use list::{list_native_sessions, ListSessionsError};
pub use reset::ResetSessionOutcome;
pub use resolve_command::{
    is_bare_command, on_agent_path, resolve_agent_command, ResolvedAgentCommand, Unresolved,
};
pub(crate) use runner::wrap_in_scope;
pub use session_sandbox::SessionSandbox;
pub(crate) use spawn::host_environment_denyreason;
pub use spawn::SpawnConfig;

use crate::acp::agent_compat::ExpectedAgent;
use crate::acp::agent_profiles;
use crate::acp::approvals::{ApprovalDecision, Nonce};
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::event_store::AttachmentBlob;
use crate::acp::fs_handler::{FsPolicy, SandboxPathMap};
use crate::acp::state::AcpSessionId;
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

use self::commands::{ClientCmd, ConnectMode, QueuedClientCmd};
pub(crate) use self::connection::ConnectionState;
use self::connection::{run_connection_task, ConnectionParams, RunnerLink};
use self::control::{connect_runner_control, wait_runner_registration, ShutdownControlOnDrop};
use self::delete::ACP_SESSION_DELETE_TIMEOUT;
use self::handshake::wait_for_handshake;
use self::lifecycle::TerminalClaim;
use self::pending::{ApprovalResolutionMessage, PendingResponders};
pub(crate) use self::reset::{SESSION_RESET_IN_TASK_TIMEOUT, SESSION_RESET_TIMEOUT};
use self::runner::spawn_runner_detached;
#[cfg(debug_assertions)]
use self::runner::take_injected_fresh_handshake_failure;
use self::spawn::spawn_subprocess;
use crate::acp::event_sender::{EventSender, SdkEvent};
const CONNECTION_SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

struct ShutdownTask<'a> {
    task: Option<tokio::task::JoinHandle<()>>,
    slot: &'a std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for ShutdownTask<'_> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            if !task.is_finished() {
                let mut slot = self.slot.lock().expect("connection task mutex poisoned");
                if slot.is_none() {
                    *slot = Some(task);
                }
            }
        }
    }
}

pub struct AcpClient {
    pub session_id: AcpSessionId,
    /// Taken by the supervisor's drain task so polling events never holds the
    /// client mutex that `send_prompt` needs.
    inbound: Option<mpsc::Receiver<SdkEvent>>,
    cmd_tx: Option<mpsc::Sender<QueuedClientCmd>>,
    pending_responders: PendingResponders,
    connection_state: Arc<ConnectionState>,
    establishment_projection: Option<Arc<crate::acp::event_sender::ProjectionState>>,
    /// Kills an in-proc agent when the client drops.
    _child: Option<Arc<Mutex<tokio::process::Child>>>,
    /// The detached runner this client launched, which its lease owns.
    runner_pid: Option<u32>,
    connected_runner: Option<crate::acp::runner_lifecycle::RunnerIdentity>,
    pub(crate) native_store: Option<crate::session::ExecutionBinding>,
    connection_shutdown: tokio_util::sync::CancellationToken,
    shutdown_gate: Mutex<()>,
    connection_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    _prime_runtime: Option<crate::process::prime::PrimeCleanupGuard>,
}

impl Drop for AcpClient {
    fn drop(&mut self) {
        self.connection_shutdown.cancel();
        if let Some(task) = self
            .connection_task
            .get_mut()
            .expect("connection task mutex poisoned")
            .take()
        {
            task.abort();
        }
    }
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

#[cfg(test)]
pub(crate) static OWNED_REGISTRATION_GATE: std::sync::Mutex<
    Option<(
        oneshot::Sender<crate::acp::runner_lifecycle::RunnerIdentity>,
        oneshot::Receiver<()>,
    )>,
> = std::sync::Mutex::new(None);

/// Everything a launch or attach hands the connection task.
struct Launch {
    session_id: AcpSessionId,
    mode: ConnectMode,
    projection: Arc<crate::acp::event_sender::ProjectionState>,
    native_scope: Option<crate::acp::state::NativeObservationScope>,
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
    claude_home: Option<PathBuf>,
    cancellation: tokio_util::sync::CancellationToken,
}

impl Launch {
    /// Spawns the connection task in an `acp_session` span, which the
    /// per-session log tee routes by (#1864), and returns the client half.
    fn start<W, R>(
        self,
        (event_tx, event_rx): (mpsc::Sender<SdkEvent>, mpsc::Receiver<SdkEvent>),
        transport: ByteStreams<W, R>,
        child: Option<Arc<Mutex<tokio::process::Child>>>,
        runner: Option<RunnerLink>,
    ) -> (AcpClient, oneshot::Receiver<Result<(), AcpError>>)
    where
        W: futures_util::AsyncWrite + Send + 'static,
        R: futures_util::AsyncRead + Send + 'static,
    {
        #[cfg(test)]
        let mut test_options = test_helpers::SDK_TEST_OPTIONS
            .try_with(|options| options.lock().unwrap().take())
            .ok()
            .flatten()
            .unwrap_or_default();
        #[cfg(test)]
        let command_capacity = test_options.command_capacity.unwrap_or(16);
        #[cfg(not(test))]
        let command_capacity = 16;
        let (cmd_tx, cmd_rx) = mpsc::channel::<QueuedClientCmd>(command_capacity);
        let (ready_tx, ready_rx) = oneshot::channel();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let pending_responders: PendingResponders = Arc::new(Mutex::new(HashMap::new()));
        let connection_state = runner
            .as_ref()
            .map(|runner| runner.control.connection_state.clone())
            .unwrap_or_else(|| Arc::new(ConnectionState::default()));
        let label = self.session_id.0.clone();
        let connected_runner = runner.as_ref().map(|runner| runner.control.peer_identity());
        let projection = self.projection;
        let _ = connection_state.projection.set(Arc::clone(&projection));
        let event_tx = EventSender::tracked(event_tx, Arc::clone(&projection), connected_runner);
        #[cfg(test)]
        {
            let _ = connection_state.test_queue.set(event_tx.downgrade());
            if let Some(observer) = test_options.queue.take() {
                let _ = observer.send(event_tx.downgrade());
            }
            *connection_state.test_callback.lock().unwrap() = test_options.callback.take();
            *connection_state.test_reverse_send.lock().unwrap() = test_options.reverse_send.take();
            *connection_state.test_default_publish.lock().unwrap() =
                test_options.default_publish.take();
            *connection_state
                .test_initialize_response_application
                .lock()
                .unwrap() = test_options.initialize_response_application.take();
            *connection_state
                .test_fresh_response_application
                .lock()
                .unwrap() = test_options.fresh_response_application.take();
            *connection_state.test_establishment_usable.lock().unwrap() =
                test_options.establishment_usable.take();
            *connection_state.test_fresh_write_pending.lock().unwrap() =
                test_options.fresh_write_pending.take();
            *connection_state.test_prompt_completed.lock().unwrap() =
                test_options.prompt_completed.take();
            *connection_state.test_control_output.lock().unwrap() =
                test_options.control_output.take();
            *connection_state.test_native_session_seen.lock().unwrap() =
                test_options.native_session_seen.take();
            *connection_state.test_update.lock().unwrap() = test_options.update.take();
        }
        let params = ConnectionParams {
            event_tx,
            connection_state: connection_state.clone(),
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
            claude_home: self.claude_home,
            runner,
            cancellation: self.cancellation.clone(),
            shutdown: shutdown.clone(),
            #[cfg(test)]
            tracking_observer: test_options.tracking.take(),
        };
        #[cfg(test)]
        let bootstrap_fence = test_options.bootstrap_fence.take();
        #[cfg(test)]
        let test_ingress = params
            .runner
            .as_ref()
            .map(|runner| Arc::clone(&runner.control.ingress));
        let span = tracing::info_span!("acp_session", session = %label);
        let task = tokio::spawn(
            async move {
                let run = async move {
                    #[cfg(test)]
                    if let Some(probe) = test_options.select_probe {
                        return connection::TEST_SELECT_PROBE
                            .scope(probe, run_connection_task(transport, params))
                            .await;
                    }
                    run_connection_task(transport, params).await;
                };
                #[cfg(test)]
                if let Some((entered, release)) = bootstrap_fence {
                    let ingress =
                        test_ingress.expect("bootstrap fence requires an actual runner connection");
                    let guard = ingress.fence.write().await;
                    let _ = entered.send(());
                    tokio::join!(run, async move {
                        let _ = release.await;
                        drop(guard);
                    });
                    return;
                }
                run.await;
            }
            .instrument(span),
        );
        let client = AcpClient {
            session_id: self.session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders,
            connection_state,
            establishment_projection: Some(projection),
            _child: child,
            runner_pid: None,
            connected_runner,
            native_store: None,
            connection_shutdown: shutdown,
            shutdown_gate: Mutex::new(()),
            connection_task: std::sync::Mutex::new(Some(task)),
            _prime_runtime: None,
        };
        (client, ready_rx)
    }
}

/// The original connection phase, with no strong lifecycle lease.
pub(crate) struct PendingRunnerConnection {
    socket_path: PathBuf,
    launch: Option<Launch>,
    owner: crate::acp::runner_lifecycle::RunnerIdentity,
    native_store: Option<crate::session::ExecutionBinding>,
    newborn: bool,
    client: Option<AcpClient>,
    handshake:
        Option<std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), AcpError>> + Send>>>,
    control: Option<ShutdownControlOnDrop>,
}

impl PendingRunnerConnection {
    fn new(
        socket_path: PathBuf,
        launch: Launch,
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
        native_store: Option<crate::session::ExecutionBinding>,
    ) -> Result<Self, AcpError> {
        let newborn = matches!(launch.mode, ConnectMode::Fresh);
        #[cfg(debug_assertions)]
        if newborn && take_injected_fresh_handshake_failure() {
            return Err(AcpError::Spawn(
                "injected fresh-handshake failure (AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES)".into(),
            ));
        }
        Ok(Self {
            socket_path,
            launch: Some(launch),
            owner,
            native_store,
            newborn,
            client: None,
            handshake: None,
            control: None,
        })
    }

    pub(crate) async fn advance(
        &mut self,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<(), AcpError> {
        let owned = self.newborn;
        if let Some(launch) = self.launch.as_ref() {
            let label = launch.session_id.0.clone();
            let terminal_claim = Arc::new(TerminalClaim::new());
            if matches!(
                &launch.mode,
                ConnectMode::Resume {
                    unsuccessful_main: true,
                    in_flight_turn: false,
                    ..
                }
            ) {
                terminal_claim.claim_failed();
            }
            let prompt_in_flight = Arc::new(AtomicBool::new(matches!(
                &launch.mode,
                ConnectMode::Resume {
                    in_flight_turn: true,
                    ..
                }
            )));
            let control_path = crate::process::worker::control_socket_sibling(&self.socket_path);
            let connected = tokio::select! {
                biased;
                _ = launch.cancellation.cancelled() =>
                    return Err(AcpError::Spawn("runner construction cancelled".into())),
                result = async {
                    wait_runner_registration(&label, self.owner, self.newborn, deadline).await?;
                    let connect = connect_runner_control(&control_path, label.clone(),
                        terminal_claim.clone(), prompt_in_flight.clone(), self.owner,
                        Arc::clone(&launch.projection));
                    match deadline {
                        Some(end) => tokio::time::timeout_at(end, connect).await
                            .map_err(|_| if owned { AcpError::RegistryBusy } else { AcpError::AttachTimedOut })?,
                        None => connect.await,
                    }
                } => result,
            }?;
            let (control, transport) = connected;
            control
                .set_establishment_context(self.native_store.clone(), launch.native_scope.clone());
            let (read_half, write_half) = tokio::io::split(transport);
            let transport = ByteStreams::new(write_half.compat_write(), read_half.compat());
            let launch = self.launch.take().expect("one original connection");
            let install_binary = launch.install_binary.clone();
            let (mut client, ready) = launch.start(
                mpsc::channel(64),
                transport,
                None,
                Some(RunnerLink {
                    control: control.clone(),
                    terminal_claim,
                    prompt_in_flight,
                }),
            );
            if self.newborn {
                client.runner_pid = Some(self.owner.pid);
                client.native_store = self.native_store.take();
            }
            self.client = Some(client);
            self.control = Some(ShutdownControlOnDrop(Some(control)));
            self.handshake = Some(Box::pin(async move {
                wait_for_handshake(&label, ready, None, &install_binary, None).await
            }));
        }
        let handshake = self
            .handshake
            .as_mut()
            .expect("retained original handshake");
        let result = match deadline {
            Some(end) => tokio::time::timeout_at(end, handshake).await.map_err(|_| {
                if owned {
                    AcpError::RegistryBusy
                } else {
                    AcpError::AttachTimedOut
                }
            })?,
            None => handshake.await,
        };
        self.handshake.take();
        if let Err(error) = result {
            let client = self.client.as_ref().expect("original SDK actor");
            if !matches!(&error, AcpError::ConnectionUnavailable(_))
                && (client.connection_unavailable().is_none()
                    || client.connection_state.driver_finished())
            {
                return Err(error);
            }
        }
        self.control
            .as_mut()
            .expect("original socket guard")
            .0
            .take();
        Ok(())
    }

    pub(crate) fn attest_owner(
        &mut self,
        owner: crate::acp::runner_lifecycle::RunnerIdentity,
    ) -> Result<(), AcpError> {
        if owner.pid != self.owner.pid
            || owner.generation != self.owner.generation
            || owner.birth_fingerprint.is_none()
            || self
                .owner
                .birth_fingerprint
                .is_some_and(|birth| Some(birth) != owner.birth_fingerprint)
        {
            return Err(AcpError::RetainedRuntimeUnavailable(Box::new(
                crate::process::worker_registry::ControlAdmissionRefusal::unconfirmed(Some(
                    self.owner,
                )),
            )));
        }
        self.owner = owner;
        Ok(())
    }

    pub(crate) fn take_client(&mut self) -> AcpClient {
        self.client.take().expect("completed original connection")
    }

    pub(crate) fn take_retiring_client(&mut self) -> Option<AcpClient> {
        if self.client.is_some() {
            if let Some(control) = self.control.as_mut() {
                control.0.take();
            }
        }
        self.client.take()
    }
}

impl AcpClient {
    fn fake(
        session_id: AcpSessionId,
        cmd_tx: Option<mpsc::Sender<QueuedClientCmd>>,
    ) -> (Self, EventSender) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let client = Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx,
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            connection_state: Arc::new(ConnectionState::default()),
            establishment_projection: None,
            _child: None,
            runner_pid: None,
            connected_runner: None,
            native_store: None,
            connection_shutdown: tokio_util::sync::CancellationToken::new(),
            shutdown_gate: Mutex::new(()),
            connection_task: std::sync::Mutex::new(None),
            _prime_runtime: None,
        };
        (client, EventSender::from(event_tx))
    }

    /// A client whose command consumer runs `handle` on every command.
    #[cfg(test)]
    fn fake_with_consumer(
        session_id: AcpSessionId,
        mut handle: impl FnMut(ClientCmd) + Send + 'static,
    ) -> (Self, EventSender) {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<QueuedClientCmd>(16);
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                handle(cmd.command);
            }
        });
        Self::fake(session_id, Some(cmd_tx))
    }

    #[cfg(test)]
    pub(crate) fn fake_for_test_held_close(
        session_id: AcpSessionId,
    ) -> (
        Self,
        oneshot::Receiver<oneshot::Sender<CloseSessionOutcome>>,
        oneshot::Receiver<String>,
    ) {
        let (entered, close) = oneshot::channel();
        let (deleted, deletion) = oneshot::channel();
        let mut entered = Some(entered);
        let mut deleted = Some(deleted);
        let (client, _) = Self::fake_with_consumer(session_id, move |cmd| match cmd {
            ClientCmd::CloseSession(response) => {
                if let Some(entered) = entered.take() {
                    let _ = entered.send(response);
                }
            }
            ClientCmd::DeleteSession {
                acp_session_id,
                respond_to,
            } => {
                if let Some(deleted) = deleted.take() {
                    let _ = deleted.send(acp_session_id);
                }
                let _ = respond_to.send(DeleteSessionOutcome::UnsupportedMethod);
            }
            _ => {}
        });
        (client, close, deletion)
    }

    #[cfg(test)]
    pub(crate) async fn test_flush_commands(&self) {
        let (done, flushed) = oneshot::channel();
        self.cmd_tx
            .as_ref()
            .expect("command recorder")
            .send(QueuedClientCmd {
                command: ClientCmd::FlushForTest(done),
                scope: None,
                correlation: None,
            })
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
    pub fn fake_for_test(session_id: AcpSessionId) -> (Self, EventSender) {
        Self::fake(session_id, None)
    }

    /// Model the SDK task's ownership of a fixture's sole event producer.
    /// Shutdown joins this task; queued events remain available to the drainer.
    #[cfg(test)]
    pub(crate) fn with_test_owned_event_producer(mut self, producer: EventSender) -> Self {
        let shutdown = self.connection_shutdown.clone();
        let task = self
            .connection_task
            .get_mut()
            .expect("connection task mutex poisoned");
        assert!(
            task.is_none(),
            "fake client must not already own a producer"
        );
        *task = Some(tokio::spawn(async move {
            shutdown.cancelled().await;
            drop(producer);
        }));
        self
    }

    #[cfg(test)]
    pub(crate) fn test_observe_native_ready(&self) {
        self.connection_state.established();
        self.connection_state.record_usable();
    }

    #[cfg(test)]
    pub(crate) fn test_observe_native_loss(&self) {
        self.connection_state.observe(
            crate::acp::state::AcpConnectionSource::Native,
            crate::acp::state::AcpConnectionCause::StdoutEof,
        );
    }

    /// Every command fails with `AgentExited`, as between a connection task
    /// ending and its respawn (#3401).
    #[cfg(test)]
    pub fn fake_for_test_dead_connection(session_id: AcpSessionId) -> Self {
        let (cmd_tx, _) = mpsc::channel::<QueuedClientCmd>(16);
        Self::fake(session_id, Some(cmd_tx)).0
    }

    /// Flags whether `session/delete` was issued, answering it immediately
    /// (#1710).
    #[cfg(test)]
    pub fn fake_for_test_recording(
        session_id: AcpSessionId,
    ) -> (Self, EventSender, Arc<AtomicBool>) {
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
    ) -> (Self, EventSender, Arc<std::sync::Mutex<Vec<&'static str>>>) {
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
                ClientCmd::ResumeTaskTailing(_) => "resume_task_tailing",
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
                ClientCmd::CloseSession(respond_to) => {
                    let _ = respond_to.send(CloseSessionOutcome::Unsupported);
                    "close_session"
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
    ) -> (Self, EventSender) {
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

    #[cfg(test)]
    pub(crate) async fn spawn_with_test_runner(
        config: SpawnConfig,
        session_id: AcpSessionId,
        executable: PathBuf,
    ) -> Result<Self, AcpError> {
        runner::TEST_RUNNER_EXECUTABLE
            .scope(executable, Self::spawn(config, session_id))
            .await
    }

    /// Launch an agent: a detached runner when `socket_path` is set, else an
    /// in-proc stdio subprocess (tests).
    pub async fn spawn(config: SpawnConfig, session_id: AcpSessionId) -> Result<Self, AcpError> {
        let constructor = config
            .launch_lease
            .as_ref()
            .and_then(|lease| lease.retain_original_constructor());
        if config
            .launch_lease
            .as_ref()
            .is_some_and(|lease| lease.cancellation().is_cancelled())
        {
            return Err(AcpError::Spawn(
                "launch cancelled before preparation".into(),
            ));
        }
        let establishment = config
            .launch_lease
            .as_ref()
            .and_then(|lease| lease.admission())
            .and_then(|admission| admission.establishment)
            .ok_or_else(|| {
                AcpError::Spawn(
                    "native establishment requires an exact durable Pending attempt".into(),
                )
            })?;
        let projection = Arc::new(crate::acp::event_sender::ProjectionState::new(Some(
            Arc::clone(&establishment),
        )));
        config
            .launch_lease
            .as_ref()
            .expect("validated original lease")
            .retirement()
            .retain_establishment_projection(Arc::clone(&projection));
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
        let cancellation = config
            .launch_lease
            .as_ref()
            .map_or_else(tokio_util::sync::CancellationToken::new, |lease| {
                lease.cancellation().child_token()
            });
        if cancellation.is_cancelled() {
            return Err(AcpError::Spawn("launch cancelled before spawning".into()));
        }
        let launch = |sandbox, native_store: &Option<crate::session::ExecutionBinding>| Launch {
            projection: Arc::clone(&projection),
            native_scope: config
                .launch_lease
                .as_ref()
                .and_then(|lease| lease.native_scope().ok().flatten()),
            mode: ConnectMode::Fresh,
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
            claude_home: native_store
                .as_ref()
                .filter(|binding| binding.agent == "claude" && binding.filesystem == "host")
                .and_then(|binding| binding.stores.first())
                .cloned(),
            cancellation: cancellation.clone(),
        };

        if let Some(socket_path) = config.socket_path.clone() {
            if let Some(record) = crate::process::worker_registry::control_admission(&session_id.0)
                .map_err(|refusal| AcpError::RetainedRuntimeUnavailable(Box::new(refusal)))?
                .filter(crate::process::worker_registry::retained_process_may_be_alive)
            {
                return Err(AcpError::RetainedRuntimeUnavailable(Box::new(
                    crate::process::worker_registry::ControlAdmissionRefusal::unconfirmed(Some(
                        crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record),
                    )),
                )));
            }
            // A fresh spawn overwrites the registry entry, so reap any prior
            // runner's process group first or its children leak (#1689).
            crate::process::worker_registry::terminate_and_wait(&session_id.0)
                .await
                .map_err(|e| AcpError::Spawn(format!("previous runtime cleanup: {e:#}")))?;
            if cancellation.is_cancelled() {
                return Err(AcpError::Spawn("launch cancelled before spawning".into()));
            }
            let runner_sandbox = sandbox.as_ref().map(|(handle, _)| handle);
            let (runner_identity, native_store) =
                spawn_runner_detached(&config, &socket_path, session_id.0.clone(), runner_sandbox)
                    .await?;
            #[cfg(test)]
            let gate = { OWNED_REGISTRATION_GATE.lock().unwrap().take() };
            #[cfg(test)]
            if let Some((entered, release)) = gate {
                let _ = entered.send(runner_identity);
                let _ = release.await;
            }
            let mut pending = PendingRunnerConnection::new(
                socket_path,
                launch(sandbox, &native_store),
                runner_identity,
                native_store,
            )?;
            let advanced = pending.advance(None).await;
            if let (Some(lease), Some(projection)) = (
                config.launch_lease.as_ref(),
                pending
                    .client
                    .as_ref()
                    .and_then(AcpClient::establishment_projection),
            ) {
                lease
                    .retirement()
                    .retain_establishment_projection(projection);
            }
            if let Err(error) = advanced {
                if matches!(
                    &error,
                    AcpError::RegistryBusy | AcpError::RetainedRuntimeUnavailable(_)
                ) {
                    if let Some(lease) = config.launch_lease.as_ref() {
                        lease.retirement().defer_registration(pending, constructor);
                        return Err(error);
                    }
                }
                if let Some(client) = pending.client.as_ref() {
                    let _ = client.shutdown().await;
                }
                return Err(error);
            }
            return Ok(pending.take_client());
        }

        let (mut child, native_store, prime_runtime) = spawn_subprocess(&config)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::Spawn("no stdin handle".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::Spawn("no stdout handle".into()))?;
        let child = Arc::new(Mutex::new(child));
        let launch = launch(sandbox, &native_store);
        let (label, install_binary) = (launch.session_id.0.clone(), launch.install_binary.clone());
        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        let events = mpsc::channel(64);
        let (mut client, ready_rx) = launch.start(events, transport, Some(child.clone()), None);
        client._prime_runtime = prime_runtime.map(|mut guard| {
            guard.1 = Some(child.clone());
            guard
        });
        if let Err(error) =
            wait_for_handshake(&label, ready_rx, Some(&child), &install_binary, None).await
        {
            if !matches!(&error, AcpError::ConnectionUnavailable(_))
                && (client.connection_unavailable().is_none()
                    || client.connection_state.driver_finished())
            {
                let _ = client.shutdown().await;
                return Err(error);
            }
        }
        client.native_store = native_store;
        Ok(client)
    }

    /// Dial a runner's control socket, which carries the whole transport
    /// (#2977). The runner owns the agent, so dropping this client leaves the
    /// worker running.
    async fn connect_via_socket(
        socket_path: PathBuf,
        launch: Launch,
        deadline: Option<tokio::time::Instant>,
        expected_owner: crate::acp::runner_lifecycle::RunnerIdentity,
        native_store: Option<crate::session::ExecutionBinding>,
    ) -> Result<Self, AcpError> {
        let mut pending =
            PendingRunnerConnection::new(socket_path, launch, expected_owner, native_store)?;
        if let Err(error) = pending.advance(deadline).await {
            if let Some(client) = pending.client.as_ref() {
                let _ = client.shutdown().await;
            }
            return Err(error);
        }
        Ok(pending.take_client())
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
        unsuccessful_main: bool,
        session_id: AcpSessionId,
        sandbox: Option<(SessionSandbox, SandboxPathMap)>,
        agent_key: String,
        source_profile: Option<String>,
        deadline: Option<tokio::time::Instant>,
        launch_lease: Option<crate::acp::runner_lifecycle::Lease>,
        claude_home: Option<PathBuf>,
        native_store: Option<crate::session::ExecutionBinding>,
    ) -> Result<Self, AcpError> {
        let expected_owner = match launch_lease
            .as_ref()
            .and_then(|lease| lease.spawned_runner())
        {
            Some(identity) => identity,
            None => {
                let record =
                    crate::process::worker_registry::control_admission(&session_id.0)
                        .map_err(|refusal| AcpError::RetainedRuntimeUnavailable(Box::new(refusal)))?
                        .ok_or_else(|| {
                            AcpError::RetainedRuntimeUnavailable(Box::new(
                        crate::process::worker_registry::ControlAdmissionRefusal::unconfirmed(None),
                    ))
                        })?;
                crate::acp::runner_lifecycle::RunnerIdentity::from_record(&record)
            }
        };
        // The binary name keeps the compatibility gate active on reattach; an
        // unknown agent maps to `Other` anyway.
        let install_binary = crate::acp::AgentRegistry::with_defaults()
            .get(&agent_key)
            .map(|spec| spec.command.clone())
            .unwrap_or_default();
        let original = launch_lease
            .as_ref()
            .and_then(|lease| lease.admission())
            .and_then(|admission| admission.establishment);
        let projection = Arc::new(crate::acp::event_sender::ProjectionState::read_only(
            original,
        ));
        if let Some(lease) = &launch_lease {
            lease
                .retirement()
                .retain_establishment_projection(Arc::clone(&projection));
        }
        let launch = Launch {
            session_id,
            projection,
            native_scope: launch_lease
                .as_ref()
                .and_then(|lease| lease.native_scope().ok().flatten()),
            mode: ConnectMode::Resume {
                acp_session_id: stored_acp_session_id,
                in_flight_turn,
                unsuccessful_main,
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
            claude_home,
            cancellation: launch_lease
                .as_ref()
                .map_or_else(tokio_util::sync::CancellationToken::new, |lease| {
                    lease.cancellation().child_token()
                }),
        };
        Self::connect_via_socket(socket_path, launch, deadline, expected_owner, native_store).await
    }

    pub(crate) fn establishment_projection(
        &self,
    ) -> Option<Arc<crate::acp::event_sender::ProjectionState>> {
        self.establishment_projection
            .clone()
            .or_else(|| self.connection_state.projection.get().cloned())
    }

    pub fn connection_unavailable(&self) -> Option<crate::acp::state::AcpConnectionUnavailability> {
        self.connection_state.loss()
    }

    pub(crate) fn owns_connection_loss(
        &self,
        state: &Arc<ConnectionState>,
        observation: crate::acp::state::AcpConnectionUnavailability,
    ) -> bool {
        Arc::ptr_eq(&self.connection_state, state)
            && self.connection_state.loss() == Some(observation)
    }

    pub(crate) fn original_request_context(
        &self,
    ) -> Option<crate::acp::establishment::NativeRequestContext> {
        use crate::acp::establishment::{NativeRequestContext, NativeRequestScope};
        let projection = self.establishment_projection()?;
        let receipt = projection.receipt_snapshot()?;
        let attempt = projection.attempt()?;
        let owner = self.connected_runner?;
        if !attempt.same_source(&receipt.attempt) || receipt.owner != owner {
            return None;
        }
        NativeRequestContext::new(
            NativeRequestScope::Original {
                attempt,
                owner,
                store: self.native_store.clone().map(Arc::new),
                operation: None,
            },
            None,
        )
        .with_original_evidence(receipt)
        .ok()
    }

    pub(crate) fn connection_ready(&self) -> bool {
        self.connection_state.ready()
            && self
                .establishment_projection
                .as_ref()
                .is_none_or(|projection| projection.applied().is_some())
    }

    pub(crate) async fn wait_until_usable(&self) -> Result<(), AcpError> {
        self.connection_state.wait_until_usable().await
    }

    pub(crate) fn connected_runner(&self) -> Option<crate::acp::runner_lifecycle::RunnerIdentity> {
        self.connected_runner
    }

    pub(crate) fn ensure_available(&self) -> Result<(), AcpError> {
        self.connection_state.admit(|| ())
    }

    pub(crate) fn ensure_established(&self) -> Result<(), AcpError> {
        self.ensure_available()?;
        // Native work cannot overtake a queued Fresh writer. Direct human
        // resolutions use ensure_available and release their existing callback.
        let _duration = self
            .connection_state
            .callback_duration
            .try_read()
            .map_err(|_| {
                AcpError::ResetFailed(
                    "conversation reset is in progress; request was not issued".into(),
                )
            })?;
        if self.establishment_projection.is_some() && !self.connection_ready() {
            return Err(crate::acp::establishment::EstablishmentRefusal::Unresolved.into());
        }
        Ok(())
    }

    /// Capture a new operation from this client's certified original projection.
    /// The value is frozen before the command encounters its first queue await.
    pub(crate) async fn capture_operation_context(
        &self,
        correlation: crate::acp::establishment::NativeOperationCorrelation,
        deadline: tokio::time::Instant,
        expected: Option<&crate::acp::establishment::EstablishmentAttempt>,
    ) -> Result<Option<crate::acp::establishment::NativeRequestContext>, AcpError> {
        use crate::acp::establishment::{EstablishmentRefusal, NativeOperationPermission};
        self.ensure_established()?;
        let Some(projection) = self.establishment_projection() else {
            // Unprojected local clients have no native authority to transport.
            return if self.connected_runner.is_none() && expected.is_none() {
                Ok(None)
            } else {
                Err(EstablishmentRefusal::Unconfirmed.into())
            };
        };
        let applied = projection
            .applied()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let receipt = projection
            .receipt_snapshot()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let owner = self
            .connected_runner
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        if applied.attempt_id != receipt.attempt.id
            || expected.is_some_and(|expected| !receipt.attempt.same_source(expected))
        {
            return Err(EstablishmentRefusal::Superseded.into());
        }
        let permission = tokio::select! {
            biased;
            _ = self.connection_state.wait_callback_end() => {
                self.ensure_established()?;
                return Err(AcpError::NotRunning);
            }
            _ = self.connection_shutdown.cancelled() => {
                self.ensure_established()?;
                return Err(AcpError::NotRunning);
            }
            captured = tokio::time::timeout_at(deadline,
                NativeOperationPermission::capture_owned(&receipt, &owner, correlation, deadline),
            ) => captured.map_err(|_| AcpError::RegistryBusy)?
                .map_err(|error| {
                    if error.is::<crate::process::worker_registry::RegistryBusy>() {
                        AcpError::RegistryBusy
                    } else {
                        EstablishmentRefusal::Superseded.into()
                    }
                })?,
        };
        self.ensure_established()?;
        if !projection
            .applied()
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &applied))
            || !projection
                .receipt_snapshot()
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &receipt))
        {
            return Err(EstablishmentRefusal::Superseded.into());
        }
        Ok(Some(crate::acp::establishment::NativeRequestContext::new(
            crate::acp::establishment::NativeRequestScope::Operation {
                permission: Arc::new(permission),
            },
            Some(receipt),
        )))
    }

    async fn queue_command(
        &self,
        command: ClientCmd,
        correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
    ) -> Result<QueuedClientCmd, AcpError> {
        use crate::acp::establishment::EstablishmentRefusal;
        if command.requires_operation() != correlation.is_some() {
            return Err(EstablishmentRefusal::Unconfirmed.into());
        }
        let scope = match &command {
            ClientCmd::Shutdown => None,
            #[cfg(test)]
            ClientCmd::FlushForTest(_) => None,
            ClientCmd::ResumeBackgroundTailing(_) | ClientCmd::ResumeTaskTailing(_) => None,
            _ => {
                let deadline = match &command {
                    ClientCmd::ResetSession { deadline, .. } => *deadline,
                    _ => tokio::time::Instant::now() + crate::acp::supervisor::WORKER_READY_TIMEOUT,
                };
                self.capture_operation_context(
                    correlation.ok_or(EstablishmentRefusal::Unconfirmed)?,
                    deadline,
                    None,
                )
                .await?
            }
        };
        Ok(QueuedClientCmd {
            command,
            scope,
            correlation,
        })
    }

    async fn send_cmd(
        &self,
        cmd: ClientCmd,
        request_nonce: Option<uuid::Uuid>,
    ) -> Result<crate::acp::establishment::NativeOperationCorrelation, AcpError> {
        let correlation = crate::acp::establishment::NativeOperationCorrelation::new(request_nonce);
        let queued = self.queue_command(cmd, Some(correlation)).await?;
        self.send_queued(queued).await?;
        Ok(correlation)
    }

    async fn send_queued(&self, queued: QueuedClientCmd) -> Result<(), AcpError> {
        self.ensure_established()?;
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        let permit = tokio::select! {
            biased;
            observation = self.connection_state.wait_unavailable() =>
                return Err(AcpError::ConnectionUnavailable(observation)),
            permit = cmd_tx.reserve() => permit.map_err(|_| {
                self.connection_unavailable().map(AcpError::ConnectionUnavailable)
                    .unwrap_or(AcpError::NotRunning)
            })?,
        };
        self.ensure_established()?;
        self.connection_state.admit(|| {
            permit.send(queued);
        })?;
        Ok(())
    }

    /// Attachments follow the text block. Callers gate kinds on the agent's
    /// prompt capabilities (#1000).
    pub async fn send_prompt(
        &self,
        text: &str,
        attachments: &[AttachmentBlob],
    ) -> Result<(), AcpError> {
        self.ensure_established()?;
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
        self.send_cmd(ClientCmd::Prompt(blocks), None)
            .await
            .map(|_| ())
    }

    /// Best-effort `session/cancel`; Ok even with no turn in flight.
    pub async fn cancel_prompt(&self) -> Result<(), AcpError> {
        self.send_cmd(ClientCmd::Cancel, None).await.map(|_| ())
    }

    /// Skip the cancel-escalation grace: an in-flight turn ends as
    /// `user_forced`, which kills and respawns the worker (#1727).
    pub async fn force_cancel(&self) -> Result<(), AcpError> {
        self.send_cmd(ClientCmd::ForceStop, None).await.map(|_| ())
    }

    /// Config-option mode wins over legacy `session/set_mode` when both exist.
    pub async fn set_mode(
        &self,
        mode_id: &str,
        request_nonce: Option<uuid::Uuid>,
    ) -> Result<crate::acp::establishment::NativeOperationCorrelation, AcpError> {
        self.send_cmd(ClientCmd::SetMode(mode_id.to_string()), request_nonce)
            .await
    }

    pub(crate) async fn set_mode_for_establishment(
        &self,
        mode_id: &str,
        expected: Option<&Arc<crate::acp::establishment::EstablishmentAttempt>>,
        deadline: tokio::time::Instant,
    ) -> Result<crate::acp::establishment::NativeOperationCorrelation, AcpError> {
        if expected.is_none() && self.establishment_projection.is_some() {
            return Err(crate::acp::establishment::EstablishmentRefusal::Unconfirmed.into());
        }
        let correlation = crate::acp::establishment::NativeOperationCorrelation::new(None);
        let scope = self
            .capture_operation_context(correlation, deadline, expected.map(Arc::as_ref))
            .await?;
        let queued = QueuedClientCmd {
            command: ClientCmd::SetMode(mode_id.to_string()),
            scope,
            correlation: Some(correlation),
        };
        tokio::time::timeout_at(deadline, self.send_queued(queued))
            .await
            .map_err(|_| AcpError::RegistryBusy)??;
        Ok(correlation)
    }

    pub async fn set_config_option(
        &self,
        config_id: &str,
        value: &str,
        request_nonce: Option<uuid::Uuid>,
    ) -> Result<crate::acp::establishment::NativeOperationCorrelation, AcpError> {
        self.send_cmd(
            ClientCmd::SetConfigOption {
                config_id: config_id.to_string(),
                value: value.to_string(),
            },
            request_nonce,
        )
        .await
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
            .send(
                self.queue_command(ClientCmd::ResumeBackgroundTailing(launches), None)
                    .await?,
            )
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Spawn transcript tailers for background tasks that survived a daemon
    /// restart. Errors like [`Self::resume_background_tailing`].
    pub async fn resume_task_tailing(
        &self,
        tasks: Vec<crate::acp::state::BackgroundTaskLaunch>,
    ) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(
                self.queue_command(ClientCmd::ResumeTaskTailing(tasks), None)
                    .await?,
            )
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
        self.ensure_available()?;
        pending::resolve_approval(
            &self.pending_responders,
            &nonce,
            ApprovalResolutionMessage::Decision {
                decision,
                option_id,
            },
        )
        .await
    }

    pub async fn cancel_permission(&self, nonce: Nonce) -> Result<(), AcpError> {
        self.ensure_available()?;
        pending::resolve_approval(
            &self.pending_responders,
            &nonce,
            ApprovalResolutionMessage::Cancelled,
        )
        .await
    }

    /// The answer is validated before the responder is consumed, so an invalid
    /// answer leaves the elicitation pending for resubmission (#2100).
    pub async fn resolve_elicitation(
        &self,
        nonce: Nonce,
        resolution: ElicitationResolution,
    ) -> Result<(), AcpError> {
        self.ensure_available()?;
        pending::resolve_elicitation(&self.pending_responders, &nonce, resolution).await
    }

    /// Best-effort experimental `session/delete` before shutdown, so adapters
    /// can clean up persisted session state (#1404). Never fatal.
    pub async fn delete_session(&self, acp_session_id: String) -> DeleteSessionOutcome {
        if let Err(error) = self.ensure_available() {
            return DeleteSessionOutcome::Failed(error.to_string());
        }
        let (tx, rx) = oneshot::channel();
        // The guard covers the send too, so a wedged task cannot stall delete,
        // and outlasts the in-task timeout so the task's classification wins.
        let request = async {
            let cmd = ClientCmd::DeleteSession {
                acp_session_id,
                respond_to: tx,
            };
            if let Err(error) = self.send_cmd(cmd, None).await {
                return DeleteSessionOutcome::Failed(error.to_string());
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
    pub(crate) async fn reset_session(
        &self,
        text: &str,
        proposal: Arc<crate::acp::supervisor::LiveFreshProposal>,
        deadline: tokio::time::Instant,
        outer_deadline: tokio::time::Instant,
    ) -> Result<ResetSessionOutcome, AcpError> {
        self.ensure_established()?;
        let (tx, rx) = oneshot::channel();
        let failed = |message: &str| ResetSessionOutcome::Failed {
            message: message.into(),
        };
        let request = async {
            let cmd = ClientCmd::ResetSession {
                text: text.to_string(),
                proposal,
                deadline,
                respond_to: tx,
            };
            self.send_cmd(cmd, None).await?;
            Ok(rx
                .await
                .unwrap_or_else(|_| failed("respond channel closed")))
        };
        match tokio::time::timeout_at(outer_deadline, request).await {
            Ok(result) => result,
            Err(_) => Ok(ResetSessionOutcome::Failed {
                message: format!(
                    "agent did not answer session/new within {}s",
                    SESSION_RESET_TIMEOUT.as_secs()
                ),
            }),
        }
    }

    pub async fn close_session(&self) -> CloseSessionOutcome {
        if let Err(error) = self.ensure_available() {
            return CloseSessionOutcome::Failed(error.to_string());
        }
        let (respond_to, response) = oneshot::channel();
        let request = async {
            if let Err(error) = self
                .send_cmd(ClientCmd::CloseSession(respond_to), None)
                .await
            {
                return CloseSessionOutcome::Failed(error.to_string());
            }
            response
                .await
                .unwrap_or_else(|_| CloseSessionOutcome::Failed("respond channel closed".into()))
        };
        tokio::time::timeout(
            close::CLOSE_TIMEOUT + std::time::Duration::from_millis(500),
            request,
        )
        .await
        .unwrap_or(CloseSessionOutcome::TimedOut)
    }

    pub async fn shutdown(&self) -> Result<(), AcpError> {
        let _shutdown = self.shutdown_gate.lock().await;
        let native = self
            ._prime_runtime
            .as_ref()
            .and_then(|guard| guard.0.as_ref());
        if native.is_some() {
            let _ = self.close_session().await;
        }
        self.connection_state.mark_stopping();
        self.connection_shutdown.cancel();
        let task = self
            .connection_task
            .lock()
            .expect("connection task mutex poisoned")
            .take();
        let connection_result = if let Some(task) = task {
            let mut owned = ShutdownTask {
                task: Some(task),
                slot: &self.connection_task,
            };
            let task = owned.task.as_mut().expect("taken connection task");
            match tokio::time::timeout(CONNECTION_SHUTDOWN_GRACE, &mut *task).await {
                Ok(result) => {
                    owned.task.take();
                    result.map_err(|error| {
                        AcpError::Spawn(format!("connection task failed: {error}"))
                    })
                }
                Err(_) => {
                    task.abort();
                    if tokio::time::timeout(CONNECTION_SHUTDOWN_GRACE, &mut *task)
                        .await
                        .is_ok()
                    {
                        owned.task.take();
                    }
                    Err(AcpError::Spawn("connection shutdown timed out".into()))
                }
            }
        } else {
            if let Some(cmd_tx) = &self.cmd_tx {
                let _ = tokio::time::timeout(
                    CONNECTION_SHUTDOWN_GRACE,
                    cmd_tx.send(QueuedClientCmd {
                        command: ClientCmd::Shutdown,
                        scope: None,
                        correlation: None,
                    }),
                )
                .await;
            }
            Ok(())
        };
        if let Some(runtime) = native {
            if let Some(child) = &self._child {
                let mut child = child.lock().await;
                if let Some(pid) = child.id() {
                    crate::process::kill_process_tree(pid);
                }
                child.start_kill().ok();
            }
            let runtime = runtime.clone();
            tokio::task::spawn_blocking(move || runtime.cleanup())
                .await
                .map_err(|error| AcpError::Transport(error.to_string()))?
                .map_err(|error| AcpError::Transport(error.to_string()))?;
        }
        connection_result?;
        Ok(())
    }

    /// `None` once the receiver was taken or the connection task ended.
    pub async fn next_event(&mut self) -> Option<SdkEvent> {
        self.inbound.as_mut()?.recv().await
    }

    pub fn take_inbound(&mut self) -> Option<mpsc::Receiver<SdkEvent>> {
        self.inbound.take()
    }
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use crate::acp::state::Event;

    struct OnDrop(Option<oneshot::Sender<()>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            if let Some(tx) = self.0.take() {
                let _ = tx.send(());
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_bounds_a_full_event_queue_and_abort_on_waiter_drop() {
        for cancel_waiter in [false, true] {
            let (client, events) = AcpClient::fake(AcpSessionId("blocked".into()), None);
            for _ in 0..64 {
                events
                    .send(Event::Stopped {
                        reason: "filled".into(),
                    })
                    .await
                    .unwrap();
            }
            let (entered, blocked) = oneshot::channel();
            let (dropped, observed_drop) = oneshot::channel();
            let task = tokio::spawn(async move {
                let _drop = OnDrop(Some(dropped));
                entered.send(()).unwrap();
                events
                    .send(Event::Stopped {
                        reason: "blocked".into(),
                    })
                    .await
                    .unwrap();
            });
            *client.connection_task.lock().unwrap() = Some(task);
            blocked.await.unwrap();
            let mut shutdown = Box::pin(client.shutdown());
            assert!(futures_util::poll!(&mut shutdown).is_pending());
            let mut concurrent = Box::pin(client.shutdown());
            assert!(
                futures_util::poll!(&mut concurrent).is_pending(),
                "another shutdown cannot infer completion while the first owns the task"
            );
            if cancel_waiter {
                drop(shutdown);
            } else {
                tokio::time::advance(CONNECTION_SHUTDOWN_GRACE).await;
                assert!(shutdown.await.is_err());
            }
            observed_drop.await.unwrap();
            let result = concurrent.await;
            if cancel_waiter {
                assert!(
                    result.is_err(),
                    "the next owner observes the original aborted task"
                );
                assert!(
                    client.shutdown().await.is_ok(),
                    "a joined error leaves no task debt"
                );
            } else {
                assert!(
                    result.is_ok(),
                    "the next owner follows the actual completed join"
                );
            }
        }
    }
}
