//! `aoe __acp-runner`: the per-worker shim that owns a stdio ACP agent and outlives `aoe serve`.

mod connection;
mod jsonrpc;
mod shared;

use self::connection::{fanout_agent_stdout, handle_control_connection};
use self::shared::RunnerShared;
use crate::process::worker::RunnerRecordState;
use crate::process::worker_registry::{self, WorkerRecord};
use crate::util::now_secs;
use anyhow::{anyhow, Context, Result};
use clap::Args;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(10);

fn watchdog_poll_interval() -> Duration {
    std::env::var("AOE_ACP_WATCHDOG_POLL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(WATCHDOG_POLL_INTERVAL)
}

/// Consecutive `Missing` polls before the record counts as gone, so a supersede or rename
/// window cannot trigger a false self-destruct.
const WATCHDOG_MISSING_THRESHOLD: u32 = 2;

/// A detached runner self-terminates after this long without a daemon reattaching.
const DETACHED_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);

const ATTACHED: u64 = 0;

type DetachedSince = AtomicU64;

#[derive(Debug, Clone, Copy)]
enum WatchdogShutdown {
    RecordMissing,
    Superseded,
    DetachedRetentionExpired,
    Requested,
}

/// Keep in step with `runner_socket_deadline()` in `acp/acp_client/runner.rs`.
const FAST_EXIT_THRESHOLD: Duration = Duration::from_secs(10);

/// Carries `Config.environment` (JSON `[[key, value], ...]`) to the adapter without
/// applying it to the runner itself.
pub(crate) const ACP_AGENT_ENV: &str = "AOE_ACP_AGENT_ENV";

const STDOUT_READ_BUF: usize = 64 * 1024;

#[derive(Args, Debug, Clone)]
pub struct AcpRunnerArgs {
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
    /// Recorded in the registry so `aoe acp ps` can show what
    /// auth-shape the session uses without re-reading the daemon.
    #[arg(long, value_delimiter = ',', default_value = "")]
    pub provider_env_keys: Vec<String>,
    /// ACP session id supplied when the runner starts. The runner durably
    /// replaces this registry field before exposing a newly established id.
    #[arg(long)]
    pub stored_acp_session_id: Option<String>,

    /// Lifecycle generation the daemon minted for this runner. Stamped on
    /// the registry record and compared against the restart marker.
    #[arg(long, default_value_t = 0)]
    pub generation: u64,
    #[arg(long)]
    pub managed_profile: String,
    #[arg(long)]
    pub launch_nonce: uuid::Uuid,
    /// Agent program + args after `--`.
    #[arg(last = true, required = true)]
    pub agent_argv: Vec<String>,
}

pub async fn run(args: AcpRunnerArgs) -> Result<()> {
    // Paths are derived from the session id; reject traversal before touching the filesystem.
    worker_registry::validate_session_id(&args.session_id).context("invalid --session-id")?;
    let (owner_storage, born_identity) = crate::session::runner_journal::accept_authorization(
        &args.managed_profile,
        &args.session_id,
        args.launch_nonce,
        args.generation,
    )?;
    init_runner_logging(&args.session_id)?;

    if let Ok(app_dir) = crate::session::get_app_dir() {
        match crate::file_watch::FileWatchService::new() {
            Ok(svc) => {
                tokio::spawn(crate::logging::watch_runtime_filter(svc, app_dir));
            }
            Err(e) => {
                tracing::warn!(
                    target: "acp.runner",
                    error = %e,
                    "FileWatchService init failed; runtime filter live propagation disabled"
                );
            }
        }
    }

    info!(
        target: "acp.runner",
        session = %args.session_id,
        socket = %args.socket.display(),
        agent = %args.agent_name,
        "structured view runner starting"
    );

    let control_socket = crate::process::worker::control_socket_sibling(&args.socket);
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
    }

    let our_pid = std::process::id();
    let stop_socket = crate::session::runner_journal::stop_socket(&args.session_id, our_pid)?;
    let mut stop_endpoint = worker_registry::BoundEndpoint::bind(&args.session_id, &stop_socket)?;
    let stop_listener = stop_endpoint.listener();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stop_socket, std::fs::Permissions::from_mode(0o600))?;
    }
    let endpoint_identity = stop_endpoint.identity();
    anyhow::ensure!(
        endpoint_identity.is_durable(),
        "native stop endpoint birth time is unavailable; fresh publication is unproven"
    );
    let endpoint_owner = owner_storage.clone();
    let endpoint_id = args.session_id.clone();
    let endpoint_nonce = args.launch_nonce;
    let endpoint_generation = args.generation;
    tokio::task::spawn_blocking(move || {
        crate::session::runner_journal::record_stop_endpoint(
            &endpoint_owner,
            &endpoint_id,
            endpoint_nonce,
            endpoint_generation,
            endpoint_identity,
        )
    })
    .await
    .context("publishing owned stop endpoint task")??;
    let mut record = WorkerRecord::new(
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
        Some(args.managed_profile.clone()),
    )
    .with_generation(args.generation);
    record.launch_nonce = born_identity.launch_nonce;
    record.boot = born_identity.boot;
    record.incarnation = born_identity.incarnation;
    record.profile_identity = born_identity.profile_identity;
    let (record, control_listener, _control_endpoint, owner_storage) =
        tokio::task::spawn_blocking(move || {
            let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
            let _identity = crate::session::acquire_session_identity_lock()?;
            owner_storage.verify_profile_identity()?;
            let (listener, endpoint) =
                crate::session::runner_journal::publish_registry_under_locks(
                    &owner_storage,
                    &mut record,
                    |record| {
                        let endpoint =
                            worker_registry::publish_control_listener(record, &control_socket)
                                .context(
                                    "publishing runner control listener and registry record",
                                )?;
                        Ok((endpoint.listener(), endpoint))
                    },
                )?;
            anyhow::Ok((record, listener, endpoint, owner_storage))
        })
        .await
        .context("publishing born runner control endpoint task")??;
    let owner = Arc::new(shared::RegistryOwner {
        storage: owner_storage,
        record: std::sync::Mutex::new(record),
    });

    let (mut agent_child, agent_stdin, agent_stdout, agent_stderr) = match spawn_agent(&args) {
        Ok(handles) => handles,
        Err(e) => {
            let _ = owner.retire().await;
            return Err(e).with_context(|| format!("spawning agent {:?}", args.agent_argv));
        }
    };
    let agent_started_at = std::time::Instant::now();

    // Drain stderr so the child cannot block on a full pipe.
    if let Some(stderr) = agent_stderr {
        let label = args.session_id.clone();
        let per_session_log = worker_registry::log_path_for(&args.session_id).ok();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!(target: "acp.runner.agent.stderr", session = %label, "{line}");
                if let Some(path) = per_session_log.as_ref() {
                    append_log_line(path, &format!("agent.stderr: {line}"));
                }
            }
        });
    }

    let shared = Arc::new(RunnerShared::new(Some(owner.clone())));

    // Shared, never split: closing stdin makes aoe-agent exit.
    let agent_stdin = Arc::new(Mutex::new(agent_stdin));

    let agent_stdout_task = tokio::spawn(fanout_agent_stdout(
        agent_stdout,
        Arc::clone(&shared),
        Arc::clone(&agent_stdin),
        args.session_id.clone(),
    ));

    let shutdown_signal = wait_for_shutdown();
    let session_id = args.session_id.clone();
    let detached_since: Arc<DetachedSince> = Arc::new(AtomicU64::new(now_secs()));
    let (watchdog_tx, mut watchdog_rx) = tokio::sync::oneshot::channel::<WatchdogShutdown>();
    let watchdog_handle = tokio::spawn(run_watchdog(
        worker_registry::record_path(&args.session_id)?,
        worker_registry::restart_marker_path(&args.session_id)?,
        (session_id.clone(), born_identity),
        Arc::clone(&detached_since),
        watchdog_tx,
    ));

    let accept_session_id = session_id.clone();
    let accept_shared = Arc::clone(&shared);
    let accept_detached = Arc::clone(&detached_since);
    let accept_stdin = Arc::clone(&agent_stdin);
    let accept_owner = owner.clone();
    let accept_loop = async move {
        loop {
            match control_listener.accept().await {
                Ok((stream, _addr)) => {
                    info!(
                        target: "acp.runner",
                        session = %accept_session_id,
                        "daemon connected (control channel)"
                    );
                    if let Err(error) = accept_owner.update(worker_registry::mark_attached).await {
                        warn!(target: "acp.runner", session = %accept_session_id, %error, "native record attachment lost its original file custody");
                        return true;
                    }
                    accept_detached.store(ATTACHED, Ordering::Relaxed);
                    if handle_control_connection(
                        stream,
                        Arc::clone(&accept_shared),
                        Arc::clone(&accept_stdin),
                        accept_session_id.clone(),
                        args.launch_nonce,
                    )
                    .await
                    {
                        return true;
                    }
                    info!(
                        target: "acp.runner",
                        session = %accept_session_id,
                        "daemon disconnected (control channel); runner stays alive"
                    );
                    if let Err(error) = accept_owner.update(worker_registry::mark_detached).await {
                        warn!(target: "acp.runner", session = %accept_session_id, %error, "native record detach lost its original file custody");
                        return true;
                    }
                    accept_detached.store(now_secs(), Ordering::Relaxed);
                }
                Err(e) => {
                    warn!(target: "acp.runner", "control accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    };

    let mut preserve_registry = false;

    tokio::select! {
        requested = crate::session::runner_journal::wait_for_stop(stop_listener, args.launch_nonce, |idle| shared.admit_stop(idle)) => {
            stop_endpoint.cleanup();
            match requested {
                Ok(true) => {
                    let _ = owner.retire().await;
                    if !crate::process::worker::kill_own_process_group_if_leader(our_pid) {
                        let _ = agent_child.start_kill();
                        let _ = agent_child.wait().await;
                    }
                }
                Ok(false) => self_terminate_agent_tree(WatchdogShutdown::Requested, &session_id, &owner, &mut agent_child).await,
                Err(error) => {
                    warn!(target: "acp.runner", session = %session_id, "stop endpoint failed: {error:#}");
                    self_terminate_agent_tree(WatchdogShutdown::Requested, &session_id, &owner, &mut agent_child).await;
                }
            }
        }
        status = agent_child.wait() => {
            let elapsed = agent_started_at.elapsed();
            match status {
                Ok(s) if elapsed < FAST_EXIT_THRESHOLD => warn!(
                    target: "acp.runner",
                    session = %session_id,
                    status = ?s,
                    elapsed_ms = elapsed.as_millis(),
                    "agent exited within {}s of startup (likely a broken spawn); runner shutting down",
                    FAST_EXIT_THRESHOLD.as_secs()
                ),
                Ok(s) => info!(
                    target: "acp.runner",
                    session = %session_id,
                    status = ?s,
                    "agent exited; runner shutting down"
                ),
                Err(e) => warn!(
                    target: "acp.runner",
                    session = %session_id,
                    "agent wait error: {e}"
                ),
            }
        }
        _ = shutdown_signal => {
            info!(
                target: "acp.runner",
                session = %session_id,
                "shutdown signal received; terminating agent"
            );
            let _ = agent_child.start_kill();
            let _ = agent_child.wait().await;
        }
        _ = shared.fatal_triggered() => {
            warn!(
                target: "acp.runner",
                session = %session_id,
                "fatal runner state detected; terminating agent"
            );
            let _ = agent_child.start_kill();
            let _ = agent_child.wait().await;
        }
        reason = &mut watchdog_rx => {
            if let Ok(reason) = reason {
                // The replacement runner owns the registry and socket now.
                if matches!(reason, WatchdogShutdown::Superseded) {
                    preserve_registry = true;
                }
                stop_endpoint.cleanup();
                self_terminate_agent_tree(reason, &session_id, &owner, &mut agent_child).await;
            }
        }
        terminate_runner = accept_loop => {
            debug_assert!(terminate_runner);
            // The daemon may already have spawned a replacement; never unlink it.
            preserve_registry = true;
            let _ = agent_child.start_kill();
            let _ = agent_child.wait().await;
        }
    }

    watchdog_handle.abort();
    agent_stdout_task.abort();
    if !preserve_registry {
        let _ = owner.retire().await;
    }
    stop_endpoint.cleanup();
    if !crate::process::worker::kill_own_process_group_if_leader(our_pid) {
        let _ = agent_child.start_kill();
        let _ = agent_child.wait().await;
    }
    Ok(())
}

async fn run_watchdog(
    record_path: PathBuf,
    restart_marker: PathBuf,
    (session_id, identity): (String, crate::acp::runner_lifecycle::RunnerIdentity),
    detached_since: Arc<DetachedSince>,
    tx: tokio::sync::oneshot::Sender<WatchdogShutdown>,
) {
    let mut missing = 0u32;
    let poll_interval = watchdog_poll_interval();
    loop {
        // The initial delay doubles as startup grace for the boot-time record write.
        tokio::time::sleep(poll_interval).await;

        let since = detached_since.load(Ordering::Relaxed);
        if since != ATTACHED && now_secs().saturating_sub(since) >= DETACHED_RETENTION.as_secs() {
            warn!(
                target: "acp.runner",
                session = %session_id,
                "detached past retention with no daemon; self-terminating"
            );
            let _ = tx.send(WatchdogShutdown::DetachedRetentionExpired);
            return;
        }

        match crate::process::worker::inspect_record_for_runner(
            &record_path,
            identity.pid,
            |bytes| {
                let record = serde_json::from_slice::<WorkerRecord>(bytes).ok()?;
                if identity.matches_record(&record) {
                    Some(identity.pid)
                } else if identity.proves_different_record(&record) {
                    Some(0)
                } else {
                    None
                }
            },
        ) {
            RunnerRecordState::Matches | RunnerRecordState::Unreadable => missing = 0,
            RunnerRecordState::Superseded => {
                warn!(
                    target: "acp.runner",
                    session = %session_id,
                    "registry record now owned by a different pid; superseded, self-terminating"
                );
                let _ = tx.send(WatchdogShutdown::Superseded);
                return;
            }
            RunnerRecordState::Missing => {
                // `aoe acp restart` deletes the record right before it SIGTERMs us.
                if crate::process::worker::read_restart_marker(&restart_marker)
                    == Some(identity.generation)
                {
                    missing = 0;
                    continue;
                }
                missing += 1;
                if missing >= WATCHDOG_MISSING_THRESHOLD {
                    warn!(
                        target: "acp.runner",
                        session = %session_id,
                        "registry record gone; abandoned, self-terminating"
                    );
                    let _ = tx.send(WatchdogShutdown::RecordMissing);
                    return;
                }
            }
        }
    }
}

async fn self_terminate_agent_tree(
    reason: WatchdogShutdown,
    session_id: &str,
    owner: &Arc<shared::RegistryOwner>,
    agent_child: &mut Child,
) {
    info!(
        target: "acp.runner",
        session = %session_id,
        ?reason,
        "runner abandoned; terminating agent tree"
    );

    let _ = owner.retire().await;

    #[cfg(unix)]
    if let Some(agent_pid) = agent_child.id() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(agent_pid as i32), Signal::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), agent_child.wait()).await;

    // As group leader this SIGKILLs the whole agent tree and the runner itself.
    if !crate::process::worker::kill_own_process_group_if_leader(std::process::id()) {
        let _ = agent_child.start_kill();
        let _ = agent_child.wait().await;
    }
}

fn spawn_agent(
    args: &AcpRunnerArgs,
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
    // Env is already filtered by the daemon. `Config.environment` arrives via the carrier so
    // HOME/PATH entries apply to the adapter, not the runner, and outrank inherited values.
    cmd.env_remove(ACP_AGENT_ENV);
    if let Ok(encoded) = std::env::var(ACP_AGENT_ENV) {
        match serde_json::from_str::<Vec<(String, String)>>(&encoded) {
            Ok(pairs) => {
                let mut applied: Vec<String> = Vec::new();
                for (key, value) in pairs {
                    if let Some(reason) = crate::acp::acp_client::host_environment_denyreason(&key)
                    {
                        warn!(
                            target: "acp.runner",
                            key = %key,
                            reason,
                            "rejecting configured host environment key"
                        );
                        continue;
                    }
                    cmd.env(&key, value);
                    applied.push(key);
                }
                info!(
                    target: "acp.runner",
                    host_environment = ?applied,
                    "applied configured host environment to agent"
                );
            }
            Err(e) => {
                warn!(
                    target: "acp.runner",
                    error = %e,
                    "ignoring malformed configured host environment carrier"
                );
            }
        }
    }
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
    async fn recv(signal: Option<&mut tokio::signal::unix::Signal>) {
        match signal {
            Some(signal) => {
                signal.recv().await;
            }
            None => std::future::pending().await,
        }
    }
    tokio::select! {
        _ = recv(sigterm.as_mut()) => {}
        _ = recv(sigint.as_mut()) => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}

fn init_runner_logging(session_id: &str) -> Result<()> {
    let per_session = worker_registry::log_path_for(session_id)?;
    open_log_file(&per_session)?;
    append_log_line(
        &per_session,
        &format!("runner.startup: structured view runner up session={session_id}"),
    );

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

    let init = crate::logging::init_subscriber_with_options(
        resolution.target,
        filter,
        log_cfg.show_spans,
        None,
    );
    if let Some(c) = init.controller {
        crate::logging::install_controller(c);
    }
    if let Some(w) = resolution.warning {
        tracing::warn!(target: "log.runtime", "{}", w);
    }
    Ok(())
}

fn append_log_line(path: &Path, line: &str) {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
    let _ = writeln!(f, "[{ts}] {line}");
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
