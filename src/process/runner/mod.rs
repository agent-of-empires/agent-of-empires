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
    /// Profile the session was created under. Persisted on the
    /// `WorkerRecord` so reattached `terminal/create` requests re-resolve
    /// sandbox env against the same profile the session originally used.
    /// Defaulted to empty so legacy daemons whose runner predates this
    /// field still load; an absent value resolves to the global default
    /// profile, matching pre-persistence behavior.
    #[arg(long, default_value = "")]
    pub source_profile: String,
    /// Lifecycle generation the daemon minted for this runner. Stamped on
    /// the registry record and compared against the restart marker.
    #[arg(long, default_value_t = 0)]
    pub generation: u64,
    /// Native row generation authorized at physical birth; distinct from runner epoch.
    #[arg(long)]
    pub lifecycle_generation: Option<u64>,
    /// Exact transient unit created by this launch, absent for an unscoped runner.
    #[arg(long)]
    pub systemd_scope: Option<String>,
    #[arg(long)]
    pub prime_runtime: Option<String>,
    /// Agent program + args after `--`.
    #[arg(last = true, required = true)]
    pub agent_argv: Vec<String>,
}

fn native_registration_guard(
    args: &AcpRunnerArgs,
) -> Result<Option<(crate::session::Storage, crate::session::StorageFlock)>> {
    let Some(expected_generation) = args.lifecycle_generation else {
        return Ok(None);
    };
    anyhow::ensure!(
        !args.source_profile.is_empty(),
        "native runner has no source profile"
    );
    let storage = crate::session::Storage::open_unwatched(&args.source_profile)?;
    let lifecycle = storage.acquire_instance_lifecycle_lock(&args.session_id)?;
    let row = storage
        .load()?
        .into_iter()
        .find(|row| row.id == args.session_id)
        .ok_or(crate::session::SessionGone)?;
    row.ensure_startable()?;
    anyhow::ensure!(
        row.lifecycle_generation == expected_generation
            && row.is_structured()
            && row.terminal_handoff.is_none()
            && row.launch_is_finalized()
            && row.status != crate::session::Status::Stopped
            && !row.is_idle_dormant(),
        crate::session::LifecycleReservationError::Superseded
    );
    Ok(Some((storage, lifecycle)))
}

pub async fn run(args: AcpRunnerArgs) -> Result<()> {
    // Paths are derived from the session id; reject traversal before touching the filesystem.
    worker_registry::validate_session_id(&args.session_id).context("invalid --session-id")?;
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

    // Registry admission fences stop requests before binding or publishing.
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
    }

    // Save the record before spawning so a save failure has no agent tree to leak.
    let our_pid = std::process::id();
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
        if args.source_profile.is_empty() {
            None
        } else {
            Some(args.source_profile.clone())
        },
    )
    .with_generation(args.generation);
    record.scope = args
        .systemd_scope
        .as_ref()
        .map(|unit| worker_registry::ScopeOwnership::Owned { unit: unit.clone() })
        .unwrap_or(worker_registry::ScopeOwnership::Unscoped);
    record.prime_runtime = args
        .prime_runtime
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .context("parsing Prime runtime")?;
    let (control_listener, mut agent_child, agent_stdin, agent_stdout, agent_stderr) =
        register_and_spawn_agent(&args, &record)?;
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

    let shared = Arc::new(RunnerShared::new(Some((args.session_id.clone(), our_pid))));

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
        our_pid,
        args.generation,
        Arc::clone(&detached_since),
        session_id.clone(),
        watchdog_tx,
    ));

    let accept_session_id = session_id.clone();
    let accept_shared = Arc::clone(&shared);
    let accept_detached = Arc::clone(&detached_since);
    let accept_stdin = Arc::clone(&agent_stdin);
    let accept_loop = async move {
        loop {
            match control_listener.accept().await {
                Ok((stream, _addr)) => {
                    info!(
                        target: "acp.runner",
                        session = %accept_session_id,
                        "daemon connected (control channel)"
                    );
                    worker_registry::mark_attached(&accept_session_id, our_pid);
                    accept_detached.store(ATTACHED, Ordering::Relaxed);
                    if handle_control_connection(
                        stream,
                        Arc::clone(&accept_shared),
                        Arc::clone(&accept_stdin),
                        accept_session_id.clone(),
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
                    worker_registry::mark_detached(&accept_session_id, our_pid);
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
                self_terminate_agent_tree(reason, &record, &mut agent_child).await;
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
    let cleanup_record = record.clone();
    tokio::task::spawn_blocking(move || worker_registry::cleanup_runtime(&cleanup_record))
        .await??;
    if !preserve_registry {
        worker_registry::delete_if_owned(&session_id, our_pid).ok();
    }
    Ok(())
}

async fn run_watchdog(
    record_path: PathBuf,
    restart_marker: PathBuf,
    own_pid: u32,
    own_generation: u64,
    detached_since: Arc<DetachedSince>,
    session_id: String,
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

        match crate::process::worker::inspect_record_for_runner(&record_path, own_pid, |bytes| {
            serde_json::from_slice::<WorkerRecord>(bytes)
                .ok()
                .map(|rec| rec.pid)
        }) {
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
                    == Some(own_generation)
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
    record: &WorkerRecord,
    agent_child: &mut Child,
) {
    let session_id = &record.session_id;
    let own_pid = record.pid;
    info!(
        target: "acp.runner",
        session = %session_id,
        ?reason,
        "runner abandoned; terminating agent tree"
    );

    if record.prime_runtime.is_some() {
        if let Some(pid) = agent_child.id() {
            crate::process::kill_process_tree(pid);
        }
    }
    #[cfg(unix)]
    if let Some(agent_pid) = agent_child.id() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(agent_pid as i32), Signal::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), agent_child.wait()).await;

    let cleanup_record = record.clone();
    let cleanup_proven = loop {
        let route = cleanup_record.clone();
        match tokio::task::spawn_blocking(move || worker_registry::cleanup_runtime(&route)).await {
            Ok(Ok(())) => break true,
            result => {
                warn!(target: "acp.runner", ?result, "native cleanup unproven; retaining ownership evidence");
                if (cleanup_record.agent_key == "prime-agent"
                    || cleanup_record.prime_runtime.is_some())
                    && cleanup_record
                        .prime_runtime
                        .as_ref()
                        .is_none_or(|runtime| runtime.proof_path.is_none())
                {
                    break false;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    };
    if cleanup_proven
        && !worker_registry::delete_if_owned_by(
            session_id,
            crate::acp::runner_lifecycle::RunnerIdentity::from_record(record),
        )
    {
        warn!(target: "acp.runner", session = %session_id, "settled runner record could not be retired");
    }
    // Kill only the runner's own group after retaining or retiring its evidence.
    if !crate::process::worker::kill_own_process_group_if_leader(own_pid) {
        let _ = agent_child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(2), agent_child.wait()).await;
    }
}

fn register_and_spawn_agent(
    args: &AcpRunnerArgs,
    record: &WorkerRecord,
) -> Result<(
    tokio::net::UnixListener,
    Child,
    tokio::process::ChildStdin,
    tokio::process::ChildStdout,
    Option<tokio::process::ChildStderr>,
)> {
    // Stop commits either before registration or after the complete agent birth.
    let _native_admission = native_registration_guard(args)?;
    let listener =
        worker_registry::register_runner(record).context("registering runner ownership")?;
    match spawn_agent(args) {
        Ok((child, stdin, stdout, stderr)) => Ok((listener, child, stdin, stdout, stderr)),
        Err(error) => {
            worker_registry::delete(&args.session_id).ok();
            Err(error).with_context(|| format!("spawning agent {:?}", args.agent_argv))
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn native_registration_rejects_stopped_and_superseded_births_before_agent_spawn() {
        use crate::session::{Instance, Status, Storage, View};
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let marker = project.path().join("agent-born");
        let mut row = Instance::new("native registration", project.path().to_str().unwrap());
        row.source_profile = "default".into();
        row.view = View::Structured;
        row.status = Status::Running;
        row.lifecycle_generation = 5;
        crate::server::test_support::seed_instances_on_disk_for_test("default", vec![row.clone()]);
        let mut args = AcpRunnerArgs {
            socket: worker_registry::socket_path_for(&row.id).unwrap(),
            session_id: row.id.clone(),
            agent_name: "sh".into(),
            agent_key: "claude".into(),
            cwd: project.path().to_owned(),
            model: None,
            additional_dirs: Vec::new(),
            provider_env_keys: Vec::new(),
            stored_acp_session_id: None,
            source_profile: "default".into(),
            generation: 97,
            lifecycle_generation: Some(5),
            systemd_scope: None,
            prime_runtime: None,
            agent_argv: vec!["sh".into(), "-c".into(), "printf born > agent-born".into()],
        };
        let record = WorkerRecord::new(
            row.id.clone(),
            std::process::id(),
            args.socket.clone(),
            "sh".into(),
            "claude".into(),
            project.path().to_owned(),
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some("default".into()),
        )
        .with_generation(args.generation);
        let storage = Storage::open_unwatched("default").unwrap();
        storage
            .update(|rows, _| {
                rows[0].lifecycle_generation = 6;
                rows[0].status = Status::Stopped;
                rows[0].mark_idle_dormant();
                Ok(())
            })
            .unwrap();
        assert!(register_and_spawn_agent(&args, &record).is_err());
        assert!(!marker.exists());
        assert!(worker_registry::load(&row.id).unwrap().is_none());
        assert!(!crate::process::worker::control_socket_sibling(&args.socket).exists());

        // An explicit later resume cannot admit the original, delayed constructor.
        storage
            .update(|rows, _| {
                rows[0].lifecycle_generation = 7;
                rows[0].status = Status::Idle;
                rows[0].idle_dormant_since = None;
                Ok(())
            })
            .unwrap();
        assert!(register_and_spawn_agent(&args, &record).is_err());
        assert!(!marker.exists());
        args.lifecycle_generation = Some(7);
        let stop_path = worker_registry::workers_dir()
            .unwrap()
            .join(format!("{}.stop", row.id));
        std::fs::write(&stop_path, b"undecodable stop intent").unwrap();
        assert!(register_and_spawn_agent(&args, &record).is_err());
        assert!(!marker.exists());
        std::fs::remove_file(stop_path).unwrap();

        let (listener, mut child, stdin, stdout, stderr) =
            register_and_spawn_agent(&args, &record).unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(std::fs::read(marker).unwrap(), b"born");
        drop((listener, stdin, stdout, stderr));
        worker_registry::retire_selected(&record).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn detached_expiry_retires_only_settled_records_and_sockets_before_self_exit() {
        use std::io::Read as _;
        use std::os::unix::process::ExitStatusExt as _;
        use tokio::io::AsyncWriteExt as _;
        if let Ok(case) = std::env::var("AOE_RUNNER_EXPIRY_CHILD") {
            assert!(unsafe { libc::setsid() } >= 0);
            let id = "expired";
            let socket = worker_registry::socket_path_for(id).unwrap();
            let mut record = WorkerRecord::new(
                id.into(),
                std::process::id(),
                socket,
                "agent".into(),
                "agent".into(),
                std::env::temp_dir(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .with_generation(1);
            if case == "untracked-prime" {
                record.agent_key = "prime-agent".into();
            }
            let _listener = worker_registry::register_runner(&record).unwrap();
            let mut agent = Command::new("cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            agent
                .stdin
                .as_mut()
                .unwrap()
                .write_all(b"agent-ready\n")
                .await
                .unwrap();
            let mut ready = String::new();
            BufReader::new(agent.stdout.take().unwrap())
                .read_line(&mut ready)
                .await
                .unwrap();
            assert_eq!(ready, "agent-ready\n");
            let (tx, rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(run_watchdog(
                worker_registry::record_path(id).unwrap(),
                worker_registry::restart_marker_path(id).unwrap(),
                record.pid,
                record.generation,
                Arc::new(AtomicU64::new(
                    now_secs() - DETACHED_RETENTION.as_secs() - 1,
                )),
                id.into(),
                tx,
            ));
            let reason = tokio::time::timeout(Duration::from_secs(5), rx)
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(reason, WatchdogShutdown::DetachedRetentionExpired));
            task.await.unwrap();
            self_terminate_agent_tree(reason, &record, &mut agent).await;
            panic!("the expired runner must exit its own process group");
        }
        for case in ["settled", "untracked-prime"] {
            let root = tempfile::TempDir::with_prefix_in("aoe-expiry-", "/tmp").unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(root.path());
            let name = format!(
                "{}::detached_expiry_retires_only_settled_records_and_sockets_before_self_exit",
                module_path!().split_once("::").unwrap().1
            );
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &name, "--nocapture"])
                .env("AOE_RUNNER_EXPIRY_CHILD", case)
                .env("AOE_ACP_WATCHDOG_POLL_MS", "1")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("expired runner did not exit");
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            let mut stderr = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            assert_eq!(status.signal(), Some(libc::SIGKILL), "{stderr}");
            let record = worker_registry::load("expired").unwrap();
            let control = crate::process::worker::control_socket_sibling(
                &worker_registry::socket_path_for("expired").unwrap(),
            );
            if case == "settled" {
                assert!(record.is_none());
                assert!(!control.exists());
            } else {
                assert_eq!(record.unwrap().agent_key, "prime-agent");
                assert!(control.exists());
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn watchdog_teardown_preserves_replacement_registry_owner() {
        // Under /tmp, not $TMPDIR: macOS paths exceed the 104-byte sun_path limit.
        let app_root = tempfile::TempDir::with_prefix_in("aoe-wd-", "/tmp").unwrap();
        let _app_dir = crate::session::test_support::isolate_app_dir_at(app_root.path());
        let session_id = "watchdog-replacement";
        let socket = worker_registry::socket_path_for(session_id).unwrap();
        let control_socket = crate::process::worker::control_socket_sibling(&socket);
        let _listener = std::os::unix::net::UnixListener::bind(&control_socket).unwrap();
        worker_registry::save(&WorkerRecord::new(
            session_id.into(),
            222,
            socket,
            "agent".into(),
            "agent".into(),
            PathBuf::from("/repo"),
            None,
            vec![],
            vec![],
            None,
            None,
        ))
        .unwrap();
        let mut child = Command::new("sleep").arg("60").spawn().unwrap();

        let mut original = worker_registry::load(session_id).unwrap().unwrap();
        original.pid = 111;
        self_terminate_agent_tree(WatchdogShutdown::RecordMissing, &original, &mut child).await;

        assert_eq!(worker_registry::load(session_id).unwrap().unwrap().pid, 222);
        assert!(control_socket.exists());
    }
}
