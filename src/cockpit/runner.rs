//! `aoe __cockpit-runner` — the per-worker shim that owns the agent
//! subprocess and outlives `aoe serve`.
//!
//! Invoked by `Supervisor::spawn` as a detached child via `setsid` so its
//! process group is independent of the daemon's. The runner:
//!
//! 1. Writes a registry entry at
//!    `<app_dir>/cockpit-workers/<session_id>.json` with its PID, socket
//!    path, and agent metadata.
//! 2. Spawns the configured ACP agent as a child over stdio.
//! 3. Binds a Unix listener at `<app_dir>/cockpit-workers/<session_id>.sock`
//!    and accepts connections in a loop, proxying bytes between the
//!    currently-connected aoe daemon and the agent's stdio.
//! 4. Buffers agent → daemon traffic (line-oriented ndjson) in a ring
//!    buffer while no daemon is attached, so the next reattach replays
//!    the gap.
//! 5. On agent exit or SIGTERM/SIGINT: deletes the registry file and
//!    socket, then exits.
//!
//! The daemon disconnects the unix socket on `detach_all` without
//! signalling the runner; the runner just sees a closed connection and
//! goes back to accepting.
//!
//! Logging: the runner appends to
//! `<app_dir>/cockpit-workers/<session_id>.log` so `aoe cockpit logs
//! --session <id> --follow` can tail it independently of the shared
//! `debug.log` that all aoe processes append to.
//!
//! ## Why a shim and not "let the agent bind the socket"
//!
//! Issue #1037's Proposal A suggested patching ACP agents to listen on
//! a unix socket directly, with the daemon connecting in. That works
//! for cooperating agents (`aoe-agent` already honors `AOE_ACP_SOCKET`)
//! but the third-party agents we proxy (`claude-agent-acp`, etc.)
//! only speak stdio today. This shim bridges stdio-only agents into
//! the socket-mode lifecycle without requiring upstream changes.
//!
//! Treat the shim as a deprecation path, not a permanent layer:
//! agents that gain native socket-mode transport in the future can
//! bypass `aoe __cockpit-runner` entirely and have the daemon connect
//! to them directly. The wire protocol is just newline-delimited
//! JSON-RPC (ACP), no shim-specific framing, so collapsing this
//! process is purely an agent-side change.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::worker_registry::{self, WorkerRecord};

/// Cap on agent → daemon notification lines stored while detached.
/// Each entry is at most one ndjson line (a few KB). Past this, oldest
/// entries are dropped — the daemon-side event_store still has them.
const NOTIFICATION_BUFFER_LINES: usize = 256;

/// Pipe-read buffer for the agent's stdout. 64KB matches the default
/// pipe size on macOS/Linux.
const STDOUT_READ_BUF: usize = 64 * 1024;

#[derive(Args, Debug, Clone)]
pub struct CockpitRunnerArgs {
    #[arg(long)]
    pub socket: PathBuf,
    #[arg(long)]
    pub session_id: String,
    #[arg(long)]
    pub agent_name: String,
    /// Registry key for the agent (e.g. `claude`, `codex`,
    /// `opencode`). Persisted on the WorkerRecord so the daemon's
    /// attach path resolves the right `AgentProfile` after a restart;
    /// `agent_name` carries the binary command and is not a valid
    /// profile key. Defaulted to empty so legacy daemons rolling out
    /// the new field don't immediately break runners already in flight.
    #[arg(long, default_value = "")]
    pub agent_key: String,
    #[arg(long)]
    pub cwd: PathBuf,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, value_delimiter = ',')]
    pub additional_dirs: Vec<PathBuf>,
    /// Comma-separated keys of provider_env passed through at spawn.
    /// Recorded in the registry so `aoe cockpit ps` can show what
    /// auth-shape the session uses without re-reading the daemon.
    #[arg(long, value_delimiter = ',', default_value = "")]
    pub provider_env_keys: Vec<String>,
    /// Cached ACP session id, written by the daemon and read on
    /// reattach. The runner doesn't itself use this field; it surfaces
    /// in the registry for the daemon's restart path.
    #[arg(long)]
    pub stored_acp_session_id: Option<String>,
    /// Profile the session was created under. Persisted on the
    /// `WorkerRecord` so reattached `terminal/create` requests re-resolve
    /// sandbox env against the same profile the session originally used.
    /// Defaulted to empty so legacy daemons whose runner predates this
    /// field still load; an absent value resolves to the global default
    /// profile, matching pre-persistence behavior.
    #[arg(long, default_value = "")]
    pub source_profile: String,
    /// Agent program + args after `--`.
    #[arg(last = true, required = true)]
    pub agent_argv: Vec<String>,
}

/// Entry point dispatched from `main.rs`.
pub async fn run(args: CockpitRunnerArgs) -> Result<()> {
    // `aoe __cockpit-runner` is a hidden subcommand, but a curious
    // user can still invoke it directly. The session_id flows into
    // path construction for the registry/socket/log files; validate
    // it up front so a malicious `--session-id "../../foo"` can't
    // write files outside the workers dir. Production callers pass
    // UUIDs which pass trivially. This is a defensive check, not the
    // only one: `worker_registry::{record_path, socket_path_for,
    // log_path_for, restart_marker_path}` all re-validate.
    worker_registry::validate_session_id(&args.session_id).context("invalid --session-id")?;
    init_runner_logging(&args.session_id)?;

    // Watch the shared runtime_filter file so `aoe log-level` from the
    // daemon propagates to this runner subprocess without restart. The
    // FileWatchService primitive is process-local to this subprocess; each
    // entry path constructs its own Arc.
    if let Ok(app_dir) = crate::session::get_app_dir() {
        match crate::file_watch::FileWatchService::new() {
            Ok(svc) => {
                tokio::spawn(crate::logging::watch_runtime_filter(svc, app_dir));
            }
            Err(e) => {
                tracing::warn!(
                    target: "cockpit.runner",
                    error = %e,
                    "FileWatchService init failed; runtime filter live propagation disabled"
                );
            }
        }
    }

    info!(
        target: "cockpit.runner",
        session = %args.session_id,
        socket = %args.socket.display(),
        agent = %args.agent_name,
        "cockpit runner starting"
    );

    // Bind the socket BEFORE spawning the agent so the daemon's
    // post-spawn connect doesn't race the listener creation.
    if args.socket.exists() {
        let _ = std::fs::remove_file(&args.socket);
    }
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
    }
    let listener = UnixListener::bind(&args.socket)
        .with_context(|| format!("bind {}", args.socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o600));
    }

    let (mut agent_child, agent_stdin, agent_stdout, agent_stderr) =
        spawn_agent(&args).with_context(|| format!("spawning agent {:?}", args.agent_argv))?;

    let our_pid = std::process::id();
    let record = WorkerRecord::new(
        args.session_id.clone(),
        our_pid,
        args.socket.clone(),
        args.agent_name.clone(),
        args.agent_key.clone(),
        args.cwd.clone(),
        args.model.clone(),
        args.additional_dirs.clone(),
        args.provider_env_keys.clone(),
        args.stored_acp_session_id.clone(),
        if args.source_profile.is_empty() {
            None
        } else {
            Some(args.source_profile.clone())
        },
    );
    worker_registry::save(&record).context("writing registry record")?;

    // Drain agent stderr into the per-session log file. Without this the
    // child blocks once the stderr pipe fills (~64KB on Linux), looking
    // like a wedged handshake. The same lines also land on the daemon
    // debug.log via tracing so they appear in the unified timeline; the
    // direct file write is what gives `aoe cockpit logs --session <id>`
    // and `GET /api/sessions/:id/cockpit/worker-log` something to read
    // (init_runner_logging routes tracing to debug.log, not the
    // per-session file). See #1449.
    if let Some(stderr) = agent_stderr {
        let label = args.session_id.clone();
        let per_session_log = worker_registry::log_path_for(&args.session_id).ok();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!(target: "cockpit.runner.agent.stderr", session = %label, "{line}");
                if let Some(path) = per_session_log.as_ref() {
                    append_agent_stderr_line(path, &line);
                }
            }
        });
    }

    let shared = Arc::new(RunnerShared::new());

    // Fan-out task: reads agent stdout and either forwards to the
    // currently-attached daemon or buffers in the ring. Single owner of
    // the read half of the agent's stdout pipe.
    let agent_stdout_task = tokio::spawn(fanout_agent_stdout(
        agent_stdout,
        Arc::clone(&shared),
        args.session_id.clone(),
    ));

    // Wrap agent stdin in a tokio Mutex so the accept loop can hand it
    // to one connection at a time. Wrapping (not splitting) keeps stdin
    // alive across reconnects — closing it would cause aoe-agent to
    // `process.exit(0)`.
    let agent_stdin = Arc::new(Mutex::new(agent_stdin));

    // Signal handling: SIGTERM/SIGINT → kill agent, cleanup, exit.
    let shutdown_signal = wait_for_shutdown();

    let session_id = args.session_id.clone();
    let accept_session_id = session_id.clone();
    let accept_shared = Arc::clone(&shared);
    let accept_loop = async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    info!(
                        target: "cockpit.runner",
                        session = %accept_session_id,
                        "daemon connected"
                    );
                    worker_registry::mark_attached(&accept_session_id);
                    handle_connection(
                        stream,
                        Arc::clone(&accept_shared),
                        Arc::clone(&agent_stdin),
                        accept_session_id.clone(),
                    )
                    .await;
                    info!(
                        target: "cockpit.runner",
                        session = %accept_session_id,
                        "daemon disconnected; runner stays alive"
                    );
                    worker_registry::mark_detached(&accept_session_id);
                }
                Err(e) => {
                    warn!(target: "cockpit.runner", "accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    };

    // Wait for: agent exit, signal, or accept loop death (latter is
    // unreachable but kept for symmetry).
    tokio::select! {
        status = agent_child.wait() => {
            match status {
                Ok(s) => info!(
                    target: "cockpit.runner",
                    session = %session_id,
                    status = ?s,
                    "agent exited; runner shutting down"
                ),
                Err(e) => warn!(
                    target: "cockpit.runner",
                    session = %session_id,
                    "agent wait error: {e}"
                ),
            }
        }
        _ = shutdown_signal => {
            info!(
                target: "cockpit.runner",
                session = %session_id,
                "shutdown signal received; terminating agent"
            );
            let _ = agent_child.start_kill();
            let _ = agent_child.wait().await;
        }
        _ = accept_loop => {
            warn!(target: "cockpit.runner", session = %session_id, "accept loop exited unexpectedly");
        }
    }

    agent_stdout_task.abort();
    worker_registry::delete(&session_id).ok();
    Ok(())
}

/// State the accept loop and the agent-stdout fanout share. The active
/// connection is the daemon's write-half of the socket; only one daemon
/// is attached at a time.
struct RunnerShared {
    /// The currently-attached daemon's send-side of the unix socket. The
    /// fanout task writes agent → daemon notifications here when set.
    active_outbound: Mutex<Option<tokio::net::unix::OwnedWriteHalf>>,
    /// Ring of agent → daemon ndjson lines that arrived while no daemon
    /// was attached. Drained into the next attached daemon's outbound.
    pending: Mutex<VecDeque<Vec<u8>>>,
    /// JSON-RPC request ids the agent issued to the daemon that have
    /// not yet seen a response. Populated from agent → daemon traffic
    /// (`method` + numeric `id`) and cleared on response (`id` only).
    /// On daemon disconnect the runner synthesizes a cancellation
    /// response for every outstanding `session/request_permission` so
    /// the agent doesn't park forever on a request the new daemon
    /// can't answer (the responder oneshot died with the old daemon's
    /// `pending_responders` map). See #1099.
    outstanding_requests: Mutex<HashMap<i64, String>>,
}

/// JSON-RPC peek for outstanding-request tracking. Pulls only the
/// fields needed; anything else (params, result, error) is ignored.
/// `serde(default)` so notification lines (no id, no method) and
/// responses (id without method) deserialise without complaint.
#[derive(Deserialize)]
struct JsonRpcPeek {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
}

/// Method name we synthesize cancellations for. Other agent → daemon
/// requests (fs/* etc.) can park too in principle, but their typed
/// response shapes vary and synthesizing them safely would need
/// per-method work, which is out of scope for the headline approval fix.
const PERMISSION_METHOD: &str = "session/request_permission";

/// Soft cap on `outstanding_requests`. Hit only if the daemon stops
/// answering non-permission requests (which a healthy ACP daemon
/// always does); a misbehaving daemon shouldn't be able to grow the
/// map without bound across reconnects. When the cap trips we drop
/// every non-permission entry so the permission-cancellation path
/// stays accurate (those are the only ids we ever synthesize for) and
/// log once at warn so the leak is visible.
const MAX_OUTSTANDING_REQUESTS: usize = 1024;

impl RunnerShared {
    fn new() -> Self {
        Self {
            active_outbound: Mutex::new(None),
            pending: Mutex::new(VecDeque::with_capacity(NOTIFICATION_BUFFER_LINES)),
            outstanding_requests: Mutex::new(HashMap::new()),
        }
    }

    /// Forward a line to the daemon if attached; else buffer. Returns
    /// whether forwarding happened (false → buffered).
    async fn deliver_line(&self, line: &[u8]) -> bool {
        // Peek-parse outgoing agent → daemon traffic to track outstanding
        // requests. A line with both a numeric `id` and a `method` is a
        // request the agent is making to the daemon; record it so we can
        // synthesize a cancellation response if the daemon disconnects
        // before answering. Notifications (no id) and responses (id but
        // no method) are not requests; ignore them here.
        if let Some((id, method)) = parse_request(line) {
            let mut map = self.outstanding_requests.lock().await;
            if map.len() >= MAX_OUTSTANDING_REQUESTS {
                let before = map.len();
                map.retain(|_, m| m.as_str() == PERMISSION_METHOD);
                warn!(
                    target: "cockpit.runner",
                    before,
                    after = map.len(),
                    "outstanding_requests soft cap reached; evicted non-permission ids"
                );
            }
            map.insert(id, method);
        }

        let mut guard = self.active_outbound.lock().await;
        if let Some(out) = guard.as_mut() {
            if out.write_all(line).await.is_ok() && out.flush().await.is_ok() {
                return true;
            }
            // Write failure: daemon side closed. Drop the writer and
            // buffer this line for the next attach.
            *guard = None;
        }
        drop(guard);
        let mut pending = self.pending.lock().await;
        while pending.len() >= NOTIFICATION_BUFFER_LINES {
            pending.pop_front();
        }
        pending.push_back(line.to_vec());
        false
    }

    /// Peek-parse a daemon → agent line: if it's a response (id without
    /// method) clear the matching outstanding request.
    async fn note_daemon_response(&self, line: &[u8]) {
        if let Some(id) = parse_response_id(line) {
            self.outstanding_requests.lock().await.remove(&id);
        }
    }

    /// On daemon disconnect, synthesize a cancellation response for
    /// every outstanding `session/request_permission` request so the
    /// agent's blocked stdio loop unblocks instead of waiting on a
    /// responder that died with the previous daemon. Other methods are
    /// left tracked; their responses have method-specific schemas and
    /// synthesizing them generically would risk corrupting the agent's
    /// state machine.
    async fn cancel_outstanding_permission_requests(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        session_id: &str,
    ) {
        let drained: Vec<(i64, String)> = {
            let mut map = self.outstanding_requests.lock().await;
            let keep: Vec<(i64, String)> = map
                .iter()
                .filter(|(_, m)| m.as_str() != PERMISSION_METHOD)
                .map(|(id, m)| (*id, m.clone()))
                .collect();
            let cancellable: Vec<(i64, String)> = map
                .iter()
                .filter(|(_, m)| m.as_str() == PERMISSION_METHOD)
                .map(|(id, m)| (*id, m.clone()))
                .collect();
            map.clear();
            for (id, method) in keep {
                map.insert(id, method);
            }
            cancellable
        };

        if drained.is_empty() {
            return;
        }
        info!(
            target: "cockpit.runner",
            session = %session_id,
            count = drained.len(),
            "synthesising cancellation responses for outstanding permission requests"
        );
        let mut stdin = agent_stdin.lock().await;
        for (id, _method) in drained {
            // ACP `RequestPermissionResponse` with the `cancelled`
            // outcome. The agent SDK unblocks its parked stdio loop on
            // receipt and either retries on the next user prompt or
            // surfaces a cancelled-tool-call event upstream.
            let response = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "outcome": { "outcome": "cancelled" }
                }
            });
            let mut bytes = match serde_json::to_vec(&response) {
                Ok(b) => b,
                Err(e) => {
                    warn!(
                        target: "cockpit.runner",
                        session = %session_id,
                        "failed to serialise cancellation for id {id}: {e}"
                    );
                    continue;
                }
            };
            bytes.push(b'\n');
            if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                warn!(
                    target: "cockpit.runner",
                    session = %session_id,
                    "agent stdin write failed during cancellation synthesis"
                );
                break;
            }
        }
    }

    /// Install the daemon's outbound write half. First drains the
    /// pending ring into it so the reattaching daemon sees the gap's
    /// notifications.
    async fn install_outbound(
        &self,
        mut out: tokio::net::unix::OwnedWriteHalf,
    ) -> Option<tokio::net::unix::OwnedWriteHalf> {
        let mut pending = self.pending.lock().await;
        while let Some(line) = pending.pop_front() {
            if out.write_all(&line).await.is_err() || out.flush().await.is_err() {
                // Drain failed mid-way — push the remaining lines back
                // and surface the write half as unusable.
                pending.push_front(line);
                return None;
            }
        }
        drop(pending);
        let mut guard = self.active_outbound.lock().await;
        let prev = guard.take();
        *guard = Some(out);
        prev
    }

    async fn clear_outbound(&self) {
        let mut guard = self.active_outbound.lock().await;
        *guard = None;
    }
}

/// Extract `(id, method)` from a JSON-RPC request line. Returns None
/// for malformed lines, notifications (no id), responses (no method),
/// and lines whose id is non-numeric (we only track i64 ids; ACP
/// agents in practice always use numbers, and a fast peek doesn't
/// have to model the entire JSON-RPC spec).
fn parse_request(line: &[u8]) -> Option<(i64, String)> {
    let peek: JsonRpcPeek = serde_json::from_slice(line).ok()?;
    let id = peek.id?.as_i64()?;
    let method = peek.method?;
    Some((id, method))
}

/// Extract the response id from a JSON-RPC response line, i.e. a line
/// with an `id` field but no `method`. Notifications and requests
/// return None.
fn parse_response_id(line: &[u8]) -> Option<i64> {
    let peek: JsonRpcPeek = serde_json::from_slice(line).ok()?;
    if peek.method.is_some() {
        return None;
    }
    peek.id?.as_i64()
}

/// Read agent stdout line-by-line (ndjson) and either forward to the
/// daemon or buffer.
async fn fanout_agent_stdout(
    stdout: tokio::process::ChildStdout,
    shared: Arc<RunnerShared>,
    session_id: String,
) {
    let mut reader = BufReader::with_capacity(STDOUT_READ_BUF, stdout);
    let mut line = Vec::with_capacity(4096);
    loop {
        line.clear();
        // read_until preserves the trailing newline, which ndjson
        // consumers (the daemon's ACP transport) need.
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => {
                debug!(target: "cockpit.runner", session = %session_id, "agent stdout EOF");
                break;
            }
            Ok(_) => {
                shared.deliver_line(&line).await;
            }
            Err(e) => {
                warn!(target: "cockpit.runner", session = %session_id, "stdout read error: {e}");
                break;
            }
        }
    }
}

/// Handle one daemon connection: install its write half, then pump
/// inbound lines (daemon → agent stdin) until the socket closes. Reads
/// line-by-line so the runner can peek-parse responses and clear the
/// outstanding-requests map; without that, the cancellation-on-detach
/// sweep wouldn't know which ids the daemon has already answered.
async fn handle_connection(
    stream: UnixStream,
    shared: Arc<RunnerShared>,
    agent_stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    session_id: String,
) {
    let (read_half, write_half) = stream.into_split();
    let prev = shared.install_outbound(write_half).await;
    if prev.is_some() {
        debug!(
            target: "cockpit.runner",
            session = %session_id,
            "evicting prior daemon outbound (concurrent attach)"
        );
    }

    let mut reader = BufReader::with_capacity(STDOUT_READ_BUF, read_half);
    let mut line = Vec::with_capacity(4096);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => break, // EOF: daemon closed the connection.
            Ok(_) => {
                shared.note_daemon_response(&line).await;
                let mut stdin = agent_stdin.lock().await;
                if stdin.write_all(&line).await.is_err() || stdin.flush().await.is_err() {
                    warn!(
                        target: "cockpit.runner",
                        session = %session_id,
                        "agent stdin write failed; agent likely exited"
                    );
                    break;
                }
            }
            Err(e) => {
                warn!(target: "cockpit.runner", session = %session_id, "daemon read error: {e}");
                break;
            }
        }
    }
    // Daemon disconnected. Synthesize cancellation responses for any
    // outstanding `session/request_permission` requests so the agent's
    // stdio loop unblocks instead of waiting forever on a responder
    // that died with the previous daemon.
    shared
        .cancel_outstanding_permission_requests(&agent_stdin, &session_id)
        .await;
    shared.clear_outbound().await;
}

fn spawn_agent(
    args: &CockpitRunnerArgs,
) -> Result<(
    Child,
    tokio::process::ChildStdin,
    tokio::process::ChildStdout,
    Option<tokio::process::ChildStderr>,
)> {
    let mut argv = args.agent_argv.iter();
    let program = argv
        .next()
        .ok_or_else(|| anyhow!("agent_argv empty; expected `-- <command> [args...]`"))?;
    let mut cmd = Command::new(program);
    for a in argv {
        cmd.arg(a);
    }
    cmd.current_dir(&args.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Inherit env from the runner's launching daemon (env is already
    // filtered at the daemon-side spawn site in acp_client.rs).
    let mut child = cmd.spawn().with_context(|| format!("spawning {program}"))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("agent has no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("agent has no stdout"))?;
    let stderr = child.stderr.take();
    Ok((child, stdin, stdout, stderr))
}

#[cfg(unix)]
async fn wait_for_shutdown() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigterm = signal(SignalKind::terminate()).ok();
    let mut sigint = signal(SignalKind::interrupt()).ok();
    tokio::select! {
        _ = async {
            match sigterm.as_mut() {
                Some(s) => { s.recv().await; }
                None => std::future::pending().await,
            }
        } => {}
        _ = async {
            match sigint.as_mut() {
                Some(s) => { s.recv().await; }
                None => std::future::pending().await,
            }
        } => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_runner_logging(session_id: &str) -> Result<()> {
    // Keep the per-session log file path created so `aoe cockpit logs
    // --session <id>` and any external tail works. The actual tracing
    // output goes to the shared `debug.log` so daemon + every runner
    // appear in one timeline; runner spans add `session_id` for filtering.
    // The agent stderr drainer at run() writes lines here directly so
    // the per-session file is the cockpit's "what did the adapter say"
    // surface (used by GET /cockpit/worker-log). See #1449.
    let per_session = worker_registry::log_path_for(session_id)?;
    open_log_file(&per_session)?;
    write_runner_startup_marker(&per_session, session_id);

    // Same precedence as main.rs: env > [logging] in config.toml > info
    // baseline. The notify watcher on runtime_filter still takes over
    // for live swaps once the daemon writes one.
    let filter = crate::logging::LogConfig::from_env()
        .filter_string()
        .or_else(crate::logging::load_persisted_filter)
        .unwrap_or_else(crate::logging::serve_default_filter);

    let app_dir = crate::session::get_app_dir()?;
    let log_cfg = crate::session::load_config()
        .ok()
        .flatten()
        .map(|c| c.logging)
        .unwrap_or_default();
    let resolution =
        crate::logging::resolve_sink(&log_cfg, &app_dir, crate::logging::ProcessContext::Runner);

    let init =
        crate::logging::init_subscriber_with_options(resolution.target, filter, log_cfg.show_spans);
    if let Some(c) = init.controller {
        crate::logging::install_controller(c);
    }
    if let Some(w) = resolution.warning {
        tracing::warn!(target: "log.runtime", "{}", w);
    }
    Ok(())
}

/// Write a one-line marker to the per-session log so the file is never
/// empty after the runner has started. Best-effort.
fn write_runner_startup_marker(path: &Path, session_id: &str) {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let _ = writeln!(
        f,
        "[{ts}] runner.startup: cockpit runner up session={session_id}"
    );
}

/// Append one line of agent stderr to the per-session log file with a
/// timestamp prefix. Best-effort: a write failure is ignored so the
/// runner does not crash when disk fills, lost permissions, etc.
fn append_agent_stderr_line(path: &Path, line: &str) {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let _ = writeln!(f, "[{ts}] agent.stderr: {line}");
}

fn open_log_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening runner log {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = f.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_request_extracts_id_and_method() {
        let line =
            br#"{"jsonrpc":"2.0","id":42,"method":"session/request_permission","params":{}}"#;
        let parsed = parse_request(line);
        assert_eq!(parsed, Some((42, "session/request_permission".into())));
    }

    #[test]
    fn parse_request_returns_none_for_notifications() {
        let line = br#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#;
        assert_eq!(parse_request(line), None);
    }

    #[test]
    fn parse_request_returns_none_for_responses() {
        let line = br#"{"jsonrpc":"2.0","id":7,"result":{}}"#;
        assert_eq!(parse_request(line), None);
    }

    #[test]
    fn parse_request_skips_non_numeric_ids() {
        // String ids exist in the JSON-RPC spec but ACP agents emit
        // numeric ids in practice. The peek skips strings rather than
        // misclassifying them.
        let line = br#"{"jsonrpc":"2.0","id":"abc","method":"foo","params":{}}"#;
        assert_eq!(parse_request(line), None);
    }

    #[test]
    fn parse_response_id_extracts_numeric_id() {
        let line = br#"{"jsonrpc":"2.0","id":42,"result":{"outcome":{"outcome":"cancelled"}}}"#;
        assert_eq!(parse_response_id(line), Some(42));
    }

    #[test]
    fn parse_response_id_ignores_requests() {
        let line = br#"{"jsonrpc":"2.0","id":42,"method":"foo"}"#;
        assert_eq!(parse_response_id(line), None);
    }

    #[test]
    fn parse_response_id_handles_error_envelope() {
        let line = br#"{"jsonrpc":"2.0","id":5,"error":{"code":-32000,"message":"oops"}}"#;
        assert_eq!(parse_response_id(line), Some(5));
    }

    #[test]
    fn parse_helpers_tolerate_malformed_json() {
        assert_eq!(parse_request(b"not json"), None);
        assert_eq!(parse_response_id(b"not json"), None);
    }

    /// `deliver_line` populates the outstanding-requests map on the
    /// agent → daemon request path; `note_daemon_response` removes it
    /// on the daemon → agent reply path. The map is the source of
    /// truth for `cancel_outstanding_permission_requests`, so this
    /// covers the bookkeeping invariant directly.
    #[tokio::test]
    async fn outstanding_requests_tracked_and_cleared() {
        let shared = RunnerShared::new();
        let req = br#"{"jsonrpc":"2.0","id":1,"method":"session/request_permission","params":{}}
"#;
        // No active outbound: line just gets buffered, but the peek
        // path still runs.
        shared.deliver_line(req).await;
        assert_eq!(
            shared.outstanding_requests.lock().await.get(&1),
            Some(&"session/request_permission".to_string())
        );

        let resp = br#"{"jsonrpc":"2.0","id":1,"result":{"outcome":{"outcome":"selected","optionId":"allow"}}}
"#;
        shared.note_daemon_response(resp).await;
        assert!(shared.outstanding_requests.lock().await.is_empty());
    }

    /// Soft-cap protection against an unanswered-non-permission flood.
    /// Permission ids must survive the eviction; everything else is
    /// fair game so the permission-cancellation path stays accurate.
    #[tokio::test]
    async fn outstanding_requests_evicts_non_permission_at_soft_cap() {
        let shared = RunnerShared::new();
        // One permission request that must survive.
        let perm =
            br#"{"jsonrpc":"2.0","id":9999,"method":"session/request_permission","params":{}}
"#;
        shared.deliver_line(perm).await;
        // Pre-fill the map up to the cap with non-permission requests.
        for id in 0..(MAX_OUTSTANDING_REQUESTS as i64 - 1) {
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"fs/read_text_file\",\"params\":{{}}}}\n"
            );
            shared.deliver_line(line.as_bytes()).await;
        }
        assert_eq!(
            shared.outstanding_requests.lock().await.len(),
            MAX_OUTSTANDING_REQUESTS
        );
        // One more push trips the eviction; only the permission entry
        // and the just-inserted line remain.
        let extra = br#"{"jsonrpc":"2.0","id":424242,"method":"fs/read_text_file","params":{}}
"#;
        shared.deliver_line(extra).await;
        let map = shared.outstanding_requests.lock().await;
        assert_eq!(
            map.get(&9999),
            Some(&"session/request_permission".to_string()),
            "permission id must survive eviction"
        );
        assert_eq!(
            map.get(&424242),
            Some(&"fs/read_text_file".to_string()),
            "the request that tripped the cap is inserted after the sweep"
        );
        assert!(
            map.len() <= MAX_OUTSTANDING_REQUESTS,
            "map stays within the cap after eviction"
        );
    }
}
