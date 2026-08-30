//! ACP client wrapper.
//!
//! aoe is the *client* in ACP terms; the agent (claude-code, aoe-agent,
//! gemini, etc.) is the *server*. The client sends `initialize`,
//! `session/new`, `session/prompt` and handles incoming `session/update`
//! notifications and `session/request_permission` requests.
//!
//! Architecture: spawn the agent subprocess, build a `ByteStreams`
//! transport over its stdio, run `Client.builder().connect_with(...)` on
//! a background tokio task. The task drives a long-lived loop:
//! initialize once, create one ACP session, then pump commands from an
//! mpsc channel into ACP requests until shutdown.
//!
//! `AcpClient` and its public surface live here; each concern of the client
//! lives in a submodule.

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
use crate::acp::state::{AcpSessionId, Event, PromptAttachmentKind};
use crate::acp::terminal_handler::TerminalManager;
use agent_client_protocol::schema::v1::{
    AudioContent, BlobResourceContents, ContentBlock, EmbeddedResource, EmbeddedResourceResource,
    ImageContent, McpServer, TextContent,
};
use agent_client_protocol::ByteStreams;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::Instrument;

use self::commands::{ClientCmd, ConnectMode};
use self::connection::run_connection_task;
use self::control::{connect_runner_control_v2, ShutdownControlOnDrop};
use self::delete::ACP_SESSION_DELETE_TIMEOUT;
use self::handshake::wait_for_handshake;
use self::lifecycle::TerminalClaim;
use self::pending::{
    ApprovalResolutionMessage, ElicitationResolutionMessage, PendingResolver, PendingResponders,
};
use self::reset::{SESSION_RESET_IN_TASK_TIMEOUT, SESSION_RESET_TIMEOUT};
use self::runner::{
    runner_socket_deadline, spawn_runner_detached, take_injected_fresh_handshake_failure,
    wait_for_socket,
};
use self::spawn::spawn_subprocess;

/// Top-level ACP client. Owns the subprocess lifetime and pumps events
/// from the connection task.
pub struct AcpClient {
    pub session_id: AcpSessionId,
    /// Inbound event receiver. Optional so the supervisor can `take()` it
    /// for the drain task, decoupling event polling from the client mutex
    /// (otherwise next_event().await would hold the mutex forever and
    /// deadlock send_prompt).
    inbound: Option<mpsc::Receiver<Event>>,
    cmd_tx: Option<mpsc::Sender<ClientCmd>>,
    pending_responders: PendingResponders,
    /// Hold the subprocess so it gets killed when the client is dropped.
    _child: Option<Arc<Mutex<tokio::process::Child>>>,
}

/// Per-session resources the connection task uses to handle ACP fs/* and
/// terminal/* requests delegated by the agent.
#[derive(Clone)]
struct SessionResources {
    fs_policy: Arc<FsPolicy>,
    terminals: TerminalManager,
    cwd: PathBuf,
    label: String,
    sandbox: Option<SessionSandbox>,
}

impl AcpClient {
    /// Construct a client that does not actually spawn anything. Useful
    /// for unit tests of structured view state without a real agent.
    pub fn fake_for_test(session_id: AcpSessionId) -> (Self, mpsc::Sender<Event>) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let client = Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: None,
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            _child: None,
        };
        (client, event_tx)
    }

    /// Like `fake_for_test`, but with a live `cmd_tx` whose consumer is
    /// already gone, reproducing a worker between its connection task
    /// ending (force-stop teardown) and the respawn installing a fresh
    /// client: every `ClientCmd` send fails immediately with
    /// `AgentExited`. See #3401.
    #[cfg(test)]
    pub fn fake_for_test_dead_connection(session_id: AcpSessionId) -> Self {
        let (_event_tx, event_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
        drop(cmd_rx);
        Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            _child: None,
        }
    }

    /// Like `fake_for_test`, but wires a live `cmd_tx` whose consumer
    /// records whether a `session/delete` RPC was issued. The returned
    /// `AtomicBool` flips to `true` the moment a
    /// `ClientCmd::DeleteSession` is received, and the consumer answers
    /// it immediately so the caller's `delete_session` returns without
    /// waiting on the timeout. Used to assert that reversible teardown
    /// does NOT delete the agent transcript while permanent removal
    /// does (#1710).
    #[cfg(test)]
    pub fn fake_for_test_recording(
        session_id: AcpSessionId,
    ) -> (
        Self,
        mpsc::Sender<Event>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let saw_delete = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_delete_task = saw_delete.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if let ClientCmd::DeleteSession { respond_to, .. } = cmd {
                    saw_delete_task.store(true, std::sync::atomic::Ordering::SeqCst);
                    let _ = respond_to.send(DeleteSessionOutcome::UnsupportedMethod);
                }
            }
        });
        let client = Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            _child: None,
        };
        (client, event_tx, saw_delete)
    }

    /// Like `fake_for_test`, but wires a live `cmd_tx` whose consumer
    /// records the name of every command received (in order) and answers
    /// the request/response-shaped ones so callers don't park on their
    /// oneshot: `ResetSession` gets a successful `Reset` outcome carrying
    /// `"fresh-id"`, `DeleteSession` an `UnsupportedMethod`. Used to
    /// assert supervisor-level command routing (#2979).
    #[cfg(test)]
    pub fn fake_for_test_cmd_recording(
        session_id: AcpSessionId,
    ) -> (
        Self,
        mpsc::Sender<Event>,
        std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let cmds = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let cmds_task = cmds.clone();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                let name = match cmd {
                    ClientCmd::Prompt(_) => "prompt",
                    ClientCmd::Cancel => "cancel",
                    ClientCmd::ForceStop => "force_stop",
                    ClientCmd::SetMode(_) => "set_mode",
                    ClientCmd::SetConfigOption { .. } => "set_config_option",
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
                    ClientCmd::Shutdown => "shutdown",
                };
                cmds_task.lock().expect("cmd record mutex").push(name);
            }
        });
        let client = Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            _child: None,
        };
        (client, event_tx, cmds)
    }

    /// Like `fake_for_test_cmd_recording`, but answers a driven reset
    /// with a deterministic failure. Used by supervisor tests to assert
    /// that a busy or otherwise rejected reset never publishes the
    /// successful `SessionCleared` boundary.
    #[cfg(test)]
    pub fn fake_for_test_reset_failure(
        session_id: AcpSessionId,
        message: impl Into<String>,
    ) -> (Self, mpsc::Sender<Event>) {
        let (event_tx, event_rx) = mpsc::channel(64);
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let message = message.into();
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    ClientCmd::ResetSession { respond_to, .. } => {
                        let _ = respond_to.send(ResetSessionOutcome::Failed {
                            message: message.clone(),
                        });
                    }
                    ClientCmd::DeleteSession { respond_to, .. } => {
                        let _ = respond_to.send(DeleteSessionOutcome::UnsupportedMethod);
                    }
                    _ => {}
                }
            }
        });
        let client = Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders: Arc::new(Mutex::new(HashMap::new())),
            _child: None,
        };
        (client, event_tx)
    }

    /// Spawn an ACP agent subprocess, run the handshake + create a
    /// session, and start pumping notifications into the inbound channel.
    pub async fn spawn(config: SpawnConfig, session_id: AcpSessionId) -> Result<Self, AcpError> {
        // Pre-flight: if the session's project_path was renamed or moved
        // externally (e.g. `git worktree move` or a plain `mv`), the
        // agent process's `current_dir` will ENOENT at exec time. POSIX
        // surfaces that as the same `os error 2` as a missing binary,
        // so without the pre-flight the UI lands on the wrong "install
        // the adapter" remediation. Fail fast with a typed variant so
        // the supervisor can route to a targeted banner. See #1089.
        if !config.cwd.exists() {
            return Err(AcpError::ProjectPathMissing {
                path: config.cwd.clone(),
            });
        }
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let (event_tx, event_rx) = mpsc::channel::<Event>(64);
        let pending_responders: PendingResponders = Arc::new(Mutex::new(HashMap::new()));

        // Two transports:
        //  - Socket (runner-mediated): for every structured view session in
        //    production. Spawn `aoe __acp-runner` detached via
        //    `setsid`; the runner binds the unix socket, spawns the
        //    agent over stdio, and survives `aoe serve --stop`. The
        //    daemon then dials the socket and runs the ACP handshake.
        //  - Stdio (in-proc): the legacy direct-spawn path. Retained for
        //    tests where we don't want to depend on `current_exe()` being
        //    a real `aoe` binary, and as a safety valve.
        let mode = ConnectMode::Fresh {
            stored_acp_session_id: config.stored_acp_session_id.clone(),
            seed_history_replay: config.seed_history_replay,
            fork_from: config.fork_from.clone(),
        };
        let sandbox_pair = if let Some(info) = &config.sandbox_info {
            // `from_info` resolves the container workdir, which touches git2 and
            // (for a legacy session with no pinned workdir) shells out to
            // `docker inspect`. Run it off the async executor.
            let info = info.clone();
            let cwd = config.cwd.clone();
            let profile = config.source_profile.clone();
            Some(
                tokio::task::spawn_blocking(move || {
                    SessionSandbox::from_info(&info, cwd.as_path(), profile)
                })
                .await
                .map_err(|e| AcpError::Spawn(format!("sandbox resolve task panicked: {e}")))??,
            )
        } else {
            None
        };
        let runner_sandbox = sandbox_pair.as_ref().map(|(handle, _)| handle);
        let profile = agent_profiles::resolve(&config.agent_key);
        let install_binary = config.spec.command.clone();
        let source_profile_for_task = config.source_profile.clone();
        let default_effort = config.default_effort.clone();
        let default_mode = config.default_mode.clone();
        let mcp_servers = config.mcp_servers.clone();
        if let Some(socket_path) = config.socket_path.clone() {
            // Supersede guard: a fresh spawn overwrites this session's
            // registry entry, so any runner already registered for it would
            // be orphaned (its agent's node/SDK children reparent to PID 1
            // and leak, accumulating across restarts). Reap the prior
            // runner's whole process group and clear its stale entry/socket
            // before binding the replacement. No-op when there is no live
            // prior runner. See #1689.
            crate::process::worker_registry::terminate(&session_id.0);
            spawn_runner_detached(&config, &socket_path, session_id.0.clone(), runner_sandbox)?;
            return Self::connect_via_socket(
                socket_path,
                config.cwd,
                config.additional_dirs,
                mode,
                session_id,
                pending_responders,
                cmd_tx,
                cmd_rx,
                event_tx,
                event_rx,
                sandbox_pair,
                profile,
                install_binary,
                source_profile_for_task,
                default_effort.clone(),
                default_mode.clone(),
                mcp_servers,
            )
            .await;
        }

        let child = spawn_subprocess(&config)?;
        let child = Arc::new(Mutex::new(child));
        Self::start_with_stdio(
            config.cwd,
            config.additional_dirs,
            mode,
            session_id,
            child,
            pending_responders,
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            sandbox_pair,
            profile,
            install_binary,
            source_profile_for_task,
            default_effort,
            default_mode,
            mcp_servers,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_with_stdio(
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        mode: ConnectMode,
        session_id: AcpSessionId,
        child: Arc<Mutex<tokio::process::Child>>,
        pending_responders: PendingResponders,
        cmd_tx: mpsc::Sender<ClientCmd>,
        cmd_rx: mpsc::Receiver<ClientCmd>,
        event_tx: mpsc::Sender<Event>,
        event_rx: mpsc::Receiver<Event>,
        sandbox: Option<(SessionSandbox, SandboxPathMap)>,
        profile: &'static agent_profiles::AgentProfile,
        install_binary: String,
        source_profile: Option<String>,
        default_effort: Option<String>,
        default_mode: Option<String>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<Self, AcpError> {
        let (stdin, stdout) = {
            let mut guard = child.lock().await;
            let stdin = guard
                .stdin
                .take()
                .ok_or_else(|| AcpError::Spawn("no stdin handle".into()))?;
            let stdout = guard
                .stdout
                .take()
                .ok_or_else(|| AcpError::Spawn("no stdout handle".into()))?;
            (stdin, stdout)
        };

        let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
        let session_label = session_id.0.clone();
        let child_for_task = child.clone();
        let pending_for_task = pending_responders.clone();
        let expected_agent = ExpectedAgent::from_command(&install_binary);

        // Allowed fs roots: cwd + any explicit additional directories.
        let mut roots = vec![cwd.clone()];
        roots.extend(additional_dirs);
        let (sandbox_handle, fs_policy) = match sandbox {
            Some((handle, path_map)) => (
                Some(handle),
                Arc::new(FsPolicy::with_sandbox_map(roots, path_map)),
            ),
            None => (None, Arc::new(FsPolicy::new(roots))),
        };
        let resources = SessionResources {
            fs_policy,
            terminals: TerminalManager::new(),
            cwd: cwd.clone(),
            label: session_label.clone(),
            sandbox: sandbox_handle,
        };

        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();

        // Wrap the per-session connection task in a span carrying the
        // session id so every nested event inherits it; the daemon's
        // per-session log tee routes by that field (#1864). The span name
        // must match `crate::acp::session_tee::SESSION_SPAN`.
        let conn_span = tracing::info_span!("acp_session", session = %session_label);
        tokio::spawn(
            run_connection_task(
                transport,
                event_tx,
                cmd_rx,
                cwd,
                session_label.clone(),
                Some(child_for_task),
                pending_for_task,
                resources,
                None,
                mode,
                Some(ready_tx),
                profile,
                expected_agent,
                source_profile,
                default_effort,
                default_mode,
                mcp_servers,
                // Direct stdio agents have no runner and thus no control
                // channel; the task owns its own terminal claim and
                // prompt-in-flight flag and speaks the full protocol over
                // stdio.
                None,
                None,
                None,
            )
            .instrument(conn_span),
        );

        wait_for_handshake(&session_label, ready_rx, Some(&child), &install_binary).await?;

        Ok(Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders,
            _child: Some(child),
        })
    }

    /// Connect to a per-session runner over its unix socket. Used by the
    /// post-spawn "wait for runner to bind, then dial" path AND by the
    /// `Self::attach` reattach path on `aoe serve` startup. The runner
    /// owns the agent subprocess so this constructor returns an
    /// `AcpClient` with `_child = None`; dropping the client does not
    /// terminate the worker.
    #[allow(clippy::too_many_arguments)]
    async fn connect_via_socket(
        socket_path: PathBuf,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        mode: ConnectMode,
        session_id: AcpSessionId,
        pending_responders: PendingResponders,
        cmd_tx: mpsc::Sender<ClientCmd>,
        cmd_rx: mpsc::Receiver<ClientCmd>,
        event_tx: mpsc::Sender<Event>,
        event_rx: mpsc::Receiver<Event>,
        sandbox: Option<(SessionSandbox, SandboxPathMap)>,
        profile: &'static agent_profiles::AgentProfile,
        install_binary: String,
        source_profile: Option<String>,
        default_effort: Option<String>,
        default_mode: Option<String>,
        mcp_servers: Vec<McpServer>,
    ) -> Result<Self, AcpError> {
        // Poll for the runner to finish binding the socket. The runner
        // binds before it spawns the agent so this is usually fast (a
        // few ms) but bound the wait so a wedged runner returns a typed
        // error instead of parking the supervisor.
        let stream = wait_for_socket(&socket_path, runner_socket_deadline()).await?;
        // #1890 regression hook (debug-only): simulate a fresh-spawn whose
        // daemon-side handshake fails after the runner is already up and
        // registered. Dropping the socket closes the daemon's end cleanly; the
        // runner keeps its agent alive and its registry entry, leaving the
        // orphaned-but-live-runner state the readopt pass must recover from.
        // Gated on `Fresh` so the recovery reattach/respawn is never failed,
        // and budgeted by the env var so only the first spawn trips.
        #[cfg(debug_assertions)]
        if matches!(mode, ConnectMode::Fresh { .. }) && take_injected_fresh_handshake_failure() {
            drop(stream);
            return Err(AcpError::Spawn(
                "injected fresh-handshake failure (AOE_ACP_TEST_FAIL_FIRST_HANDSHAKES)".into(),
            ));
        }
        let (read_half, write_half) = stream.into_split();
        let transport = ByteStreams::new(write_half.compat_write(), read_half.compat());

        let mut roots = vec![cwd.clone()];
        roots.extend(additional_dirs);
        let (sandbox_handle, fs_policy) = match sandbox {
            Some((handle, path_map)) => (
                Some(handle),
                Arc::new(FsPolicy::with_sandbox_map(roots, path_map)),
            ),
            None => (None, Arc::new(FsPolicy::new(roots))),
        };
        let resources = SessionResources {
            fs_policy,
            terminals: TerminalManager::new(),
            cwd: cwd.clone(),
            label: session_id.0.clone(),
            sandbox: sandbox_handle,
        };

        let session_label = session_id.0.clone();
        let pending_for_task = pending_responders.clone();
        let expected_agent = ExpectedAgent::from_command(&install_binary);

        // #2976 Phase B: dial the runner's sibling control socket. A v2
        // runner returns a control client the connection task uses to drive
        // the handshake and every turn (the runner owns the ACP client side
        // now). The shared terminal guard lets the client's reader deliver
        // an adopted turn's completion on a mid-flight resume, so the
        // resume-idle / between-prompt watchdogs stand down. An absent or
        // older (v1) runner returns None: the task falls back to the
        // byte-relay handshake and, for a mid-flight resume, the resume-idle
        // watchdog (guard left None so it still fires).
        let guard = Arc::new(TerminalClaim::new());
        // Shared with the connection task so the control reader can hand idle
        // ownership back when it surfaces a waiterless completion (#3190).
        let prompt_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let control_client = connect_runner_control_v2(
            &socket_path,
            event_tx.clone(),
            session_label.clone(),
            guard.clone(),
            prompt_in_flight.clone(),
        )
        .await;
        let external_terminal_guard = control_client.as_ref().map(|_| guard);
        let external_prompt_in_flight = control_client.as_ref().map(|_| prompt_in_flight);
        let mut handshake_control = ShutdownControlOnDrop(control_client.clone());

        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), AcpError>>();

        // See the sibling spawn in `spawn`: the connection task runs inside
        // an `acp_session` span so per-session log teeing (#1864) catches
        // events that do not set the `session` field explicitly.
        let conn_span = tracing::info_span!("acp_session", session = %session_label);
        tokio::spawn(
            run_connection_task(
                transport,
                event_tx,
                cmd_rx,
                cwd,
                session_label.clone(),
                None,
                pending_for_task,
                resources,
                None,
                mode,
                Some(ready_tx),
                profile,
                expected_agent,
                source_profile,
                default_effort,
                default_mode,
                mcp_servers,
                external_terminal_guard,
                external_prompt_in_flight,
                control_client,
            )
            .instrument(conn_span),
        );
        wait_for_handshake(&session_label, ready_rx, None, &install_binary).await?;
        handshake_control.0.take();

        Ok(Self {
            session_id,
            inbound: Some(event_rx),
            cmd_tx: Some(cmd_tx),
            pending_responders,
            _child: None,
        })
    }

    /// Reattach to an already-running structured view worker over its unix
    /// socket. Used by `aoe serve` startup when a registry entry has a
    /// live PID and an existing socket file; we connect, send only the
    /// (idempotent) ACP `initialize` request, and reuse the existing
    /// `stored_acp_session_id` directly. We deliberately do NOT issue
    /// `session/new` or `session/load`: the agent process is still
    /// running (the runner kept it alive across `aoe serve --stop`) and
    /// the session is already loaded in its memory, so re-sending those
    /// requests would either split context onto a new session id (when
    /// the agent doesn't advertise `loadSession`) or double-load against
    /// a busy session.
    ///
    /// `in_flight_turn = true` tells the connection task that the
    /// session was mid-prompt when the previous daemon detached. The
    /// task arms a watchdog that emits a synthetic
    /// `Event::Stopped { reason: "reattach_idle" }` after
    /// `RESUME_IDLE_GRACE` of inbound silence, because the agent's
    /// eventual response to the orphaned `session/prompt` carries a
    /// request id this client never issued and is dropped silently by
    /// the underlying transport, leaving the UI otherwise stuck on
    /// "thinking".
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
        let (cmd_tx, cmd_rx) = mpsc::channel::<ClientCmd>(16);
        let (event_tx, event_rx) = mpsc::channel::<Event>(64);
        let pending_responders: PendingResponders = Arc::new(Mutex::new(HashMap::new()));
        let mode = ConnectMode::Resume {
            acp_session_id: stored_acp_session_id,
            in_flight_turn,
        };
        let profile = agent_profiles::resolve(&agent_key);
        // Resolve the binary name from the registry so the resume path
        // still routes through the per-adapter compatibility gate
        // (`agent_compat::ExpectedAgent::from_command`). Reattaching to
        // a stale claude-agent-acp@0.32.0 worker that survived an aoe
        // serve restart should re-trigger the >=0.37.0 check, not
        // silently skip it just because the resume path has no install
        // hint to surface. Empty fallback only when the agent key is
        // not in the registry (an unknown user-configured agent);
        // policy maps that to Other anyway.
        let install_binary = crate::acp::AgentRegistry::with_defaults()
            .get(&agent_key)
            .map(|spec| spec.command.clone())
            .unwrap_or_default();
        Self::connect_via_socket(
            socket_path,
            cwd,
            additional_dirs,
            mode,
            session_id,
            pending_responders,
            cmd_tx,
            cmd_rx,
            event_tx,
            event_rx,
            sandbox,
            profile,
            install_binary,
            source_profile,
            None,
            // Reattach uses ConnectMode::Resume, which reuses the stored ACP
            // session id without sending session/new or session/load, so
            // neither default effort/mode nor MCP servers are forwarded here
            // (they were applied on first connect).
            None,
            Vec::new(),
        )
        .await
    }

    /// Send a user message to the agent (ACP `session/prompt`). The
    /// `attachments` are mapped to the matching ACP `ContentBlock`
    /// (`Image` / `Audio` / `Resource`) and appended after the text
    /// block. Callers are responsible for gating attachment kinds on
    /// the agent's advertised `prompt_capabilities`; this method does
    /// not re-check them. See #1000 / #965.
    pub async fn send_prompt(
        &self,
        text: &str,
        attachments: &[AttachmentBlob],
    ) -> Result<(), AcpError> {
        use base64::Engine as _;
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        let mut blocks: Vec<ContentBlock> = Vec::with_capacity(1 + attachments.len());
        blocks.push(ContentBlock::Text(TextContent::new(text)));
        for att in attachments {
            let data_b64 = base64::engine::general_purpose::STANDARD.encode(&att.data);
            let block = match att.kind {
                PromptAttachmentKind::Image => {
                    ContentBlock::Image(ImageContent::new(data_b64, att.mime_type.clone()))
                }
                PromptAttachmentKind::Audio => {
                    ContentBlock::Audio(AudioContent::new(data_b64, att.mime_type.clone()))
                }
                PromptAttachmentKind::Resource => {
                    // Embedded binary resource. ACP requires a uri; the
                    // bytes never leave the daemon so a synthetic
                    // `attachment://` uri is enough for the agent to
                    // refer to it.
                    let uri = format!("attachment:///{}", att.id);
                    let blob =
                        BlobResourceContents::new(data_b64, uri).mime_type(att.mime_type.clone());
                    ContentBlock::Resource(EmbeddedResource::new(
                        EmbeddedResourceResource::BlobResourceContents(blob),
                    ))
                }
            };
            blocks.push(block);
        }
        cmd_tx
            .send(ClientCmd::Prompt(blocks))
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Cancel the agent's currently-running turn (ACP `session/cancel`
    /// notification). Best-effort: returns Ok even if no turn is in
    /// flight, since the UI can race the agent finishing on its own.
    pub async fn cancel_prompt(&self) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::Cancel)
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Force-stop the in-flight turn immediately, bypassing the 10s
    /// cancel-escalation grace. If a prompt is in flight the connection
    /// task ends the turn with `Stopped { reason: "user_forced" }`, which
    /// the drain task treats like `agent_unresponsive`: it kills the
    /// worker's process group and respawns with `session/load`. This is
    /// the only lever that reliably stops a tool the agent runs
    /// internally (a monitor/until loop) and ignores `session/cancel` on.
    /// Best-effort: returns Ok even if no turn is in flight. See #1727.
    pub async fn force_cancel(&self) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::ForceStop)
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Switch the active session mode through the mode channel advertised
    /// by the adapter. Config-option mode takes precedence over the legacy
    /// `session/set_mode` channel when both are present.
    pub async fn set_mode(&self, mode_id: &str) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::SetMode(mode_id.to_string()))
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Set a per-session selector (model, reasoning effort, etc.) via
    /// ACP `session/set_config_option`. The structured view treats every
    /// adapter-advertised category through this one path; specific
    /// helpers per category would just duplicate the wiring. See
    /// #1403.
    pub async fn set_config_option(&self, config_id: &str, value: &str) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        cmd_tx
            .send(ClientCmd::SetConfigOption {
                config_id: config_id.to_string(),
                value: value.to_string(),
            })
            .await
            .map_err(|_| AcpError::AgentExited)
    }

    /// Resolve a pending permission request. Looks up the parked
    /// responder by nonce and unblocks the `on_receive_request` callback.
    pub async fn resolve_permission(
        &self,
        nonce: Nonce,
        decision: ApprovalDecision,
    ) -> Result<(), AcpError> {
        let mut map = self.pending_responders.lock().await;
        // Only consume the entry if it is actually a permission; a nonce
        // that belongs to an elicitation is "unknown" to this endpoint.
        let PendingResolver::Approval(_) = &map.get(&nonce).ok_or(AcpError::UnknownNonce)?.resolver
        else {
            return Err(AcpError::UnknownNonce);
        };
        let PendingResolver::Approval(resolver) = map.remove(&nonce).unwrap().resolver else {
            unreachable!("checked above");
        };
        resolver
            .send(ApprovalResolutionMessage::Decision { decision })
            .map_err(|_| AcpError::AgentExited)
    }

    /// Cancel a pending permission request. Marks it as cancelled so
    /// the agent receives a structured cancellation outcome.
    pub async fn cancel_permission(&self, nonce: Nonce) -> Result<(), AcpError> {
        let mut map = self.pending_responders.lock().await;
        let PendingResolver::Approval(_) = &map.get(&nonce).ok_or(AcpError::UnknownNonce)?.resolver
        else {
            return Err(AcpError::UnknownNonce);
        };
        let PendingResolver::Approval(resolver) = map.remove(&nonce).unwrap().resolver else {
            unreachable!("checked above");
        };
        resolver
            .send(ApprovalResolutionMessage::Cancelled)
            .map_err(|_| AcpError::AgentExited)
    }

    /// Resolve a pending elicitation by nonce, unblocking the parked
    /// `elicitation/create` callback with the user's accept/decline/cancel
    /// answer. A nonce belonging to a permission (or already resolved) is
    /// reported as unknown.
    ///
    /// The submitted answer is validated (`build_response`) BEFORE the
    /// parked resolver is consumed. An invalid answer returns
    /// `InvalidAnswer` and leaves the elicitation pending, so the client
    /// can correct it and resubmit instead of the question aborting on a
    /// client/server validation mismatch (#2100). Only a valid answer
    /// removes the nonce and forwards the built response to the agent.
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
        // Validate against the parked form while it is still borrowed; on
        // failure the nonce stays in the map untouched.
        let outcome = resolution.outcome();
        // Render the submitted answers for the transcript before
        // `build_response` consumes `resolution`. The parked form supplies
        // question titles; selects carry the clean label. See #2209.
        let answers = match &resolution {
            ElicitationResolution::Accept { answers } => summarize_answers(elicitation, answers),
            ElicitationResolution::Decline | ElicitationResolution::Cancel => Vec::new(),
        };
        let response = build_response(elicitation, resolution)
            .map_err(|e| AcpError::InvalidAnswer(e.to_string()))?;
        // Valid: now consume the responder and forward the built response.
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

    /// Best-effort experimental `session/delete` RPC. Sent before
    /// `shutdown` during structured view session deletion so adapters that
    /// persist session-side state (claude-agent-acp clears the on-disk
    /// Claude session record) get a chance to clean up before SIGTERM.
    ///
    /// All outcomes are non-fatal. Adapters that don't implement the
    /// method return `-32601 method_not_found` and surface as
    /// `UnsupportedMethod`; the supervisor proceeds to the existing
    /// kill path either way. Bounded by `ACP_SESSION_DELETE_TIMEOUT`
    /// so a wedged adapter cannot stall delete. See #1404.
    pub async fn delete_session(&self, acp_session_id: String) -> DeleteSessionOutcome {
        let Some(cmd_tx) = self.cmd_tx.as_ref() else {
            return DeleteSessionOutcome::Failed("client not running".into());
        };
        let (tx, rx) = oneshot::channel();
        // Outer guard wraps BOTH the cmd_tx send AND the response wait.
        // The mpsc send is `await`-able and can block if the connect
        // task is wedged or the channel is saturated; without the
        // guard a stalled worker would freeze the delete path
        // indefinitely while the supervisor holds the per-instance
        // lock at `sessions.rs:1361`. Wait slightly longer than the
        // in-task `ACP_SESSION_DELETE_TIMEOUT` so the inner
        // classification (Deleted/UnsupportedMethod/Failed) wins when
        // the task is healthy.
        let request = async {
            if cmd_tx
                .send(ClientCmd::DeleteSession {
                    acp_session_id,
                    respond_to: tx,
                })
                .await
                .is_err()
            {
                return DeleteSessionOutcome::Failed("connect task gone".into());
            }
            match rx.await {
                Ok(outcome) => outcome,
                Err(_) => DeleteSessionOutcome::Failed("respond channel closed".into()),
            }
        };
        match tokio::time::timeout(
            ACP_SESSION_DELETE_TIMEOUT + std::time::Duration::from_millis(500),
            request,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(_) => DeleteSessionOutcome::TimedOut,
        }
    }

    /// Drive a real conversation reset on the live worker: the connection
    /// task issues a fresh `session/new`, swaps its ACP session id, and
    /// emits `SessionCleared` + `SessionContextReset` +
    /// `AcpSessionAssigned` + a terminal `Stopped`. Used for clear
    /// commands whose adapter cannot hand AoE a durable post-reset session
    /// id (codex `/new`, #2979; claude `/clear`, upstream #906).
    /// `text` is the user's clear invocation, surfaced in the mid-turn
    /// refusal's `PromptRejected`. Bounded so a wedged adapter cannot
    /// stall the prompt path; a `session/new` timeout surfaces as a
    /// `Failed` outcome, while post-reset config re-application remains
    /// best-effort.
    pub async fn reset_session(&self, text: &str) -> Result<ResetSessionOutcome, AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        let (tx, rx) = oneshot::channel();
        let deadline = tokio::time::Instant::now() + SESSION_RESET_IN_TASK_TIMEOUT;
        // Mirror `delete_session`: the guard wraps BOTH the cmd_tx send
        // and the response wait so a wedged connection task cannot park
        // the caller indefinitely. The connection task receives the
        // caller-created inner deadline so queueing time counts too.
        let request = async {
            if cmd_tx
                .send(ClientCmd::ResetSession {
                    text: text.to_string(),
                    deadline,
                    respond_to: tx,
                })
                .await
                .is_err()
            {
                return ResetSessionOutcome::Failed {
                    message: "connect task gone".into(),
                };
            }
            match rx.await {
                Ok(outcome) => outcome,
                Err(_) => ResetSessionOutcome::Failed {
                    message: "respond channel closed".into(),
                },
            }
        };
        match tokio::time::timeout(SESSION_RESET_TIMEOUT, request).await {
            Ok(outcome) => Ok(outcome),
            Err(_) => Ok(ResetSessionOutcome::Failed {
                message: format!(
                    "agent did not answer session/new within {}s",
                    SESSION_RESET_TIMEOUT.as_secs()
                ),
            }),
        }
    }

    /// Shutdown the connection task and kill the subprocess.
    pub async fn shutdown(&self) -> Result<(), AcpError> {
        let cmd_tx = self.cmd_tx.as_ref().ok_or(AcpError::NotRunning)?;
        let _ = cmd_tx.send(ClientCmd::Shutdown).await;
        Ok(())
    }

    /// Drain the next event the agent emitted. Returns None once the
    /// receiver has been moved out via `take_inbound` (the supervisor
    /// path) or the connection task has dropped its sender.
    pub async fn next_event(&mut self) -> Option<Event> {
        match self.inbound.as_mut() {
            Some(rx) => rx.recv().await,
            None => None,
        }
    }

    /// Take ownership of the inbound event receiver. The supervisor uses
    /// this so the drain task can poll events without holding the client
    /// mutex (which would deadlock send_prompt).
    pub fn take_inbound(&mut self) -> Option<mpsc::Receiver<Event>> {
        self.inbound.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_client_round_trips_events() {
        let (mut client, tx) = AcpClient::fake_for_test(AcpSessionId("s-1".into()));
        tx.send(Event::ThinkingStarted).await.unwrap();
        let event = client.next_event().await.expect("event delivered");
        assert!(matches!(event, Event::ThinkingStarted));
    }
}
