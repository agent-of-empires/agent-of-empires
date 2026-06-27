//! `aoe __acp-runner`: the per-worker shim that owns the agent
//! subprocess and outlives `aoe serve`.
//!
//! Invoked by `Supervisor::spawn` as a detached child via `setsid` so its
//! process group is independent of the daemon's. The runner:
//!
//! 1. Writes a registry entry at
//!    `<app_dir>/acp-workers/<session_id>.json` with its PID, socket
//!    path, and agent metadata.
//! 2. Spawns the configured ACP agent as a child over stdio.
//! 3. Binds a Unix listener at `<app_dir>/acp-workers/<session_id>.sock`
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
//! `<app_dir>/acp-workers/<session_id>.log` so `aoe acp logs
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
//! bypass `aoe __acp-runner` entirely and have the daemon connect
//! to them directly. The wire protocol is just newline-delimited
//! JSON-RPC (ACP), no shim-specific framing, so collapsing this
//! process is purely an agent-side change.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Args;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::worker_registry::{self, WorkerRecord};
use crate::process::worker::RunnerRecordState;

/// How often the abandonment watchdog inspects its own registry record.
const WATCHDOG_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Resolve the watchdog poll interval. Tests shrink it via
/// `AOE_ACP_WATCHDOG_POLL_MS` so an orphan dies in well under a second
/// instead of tens of seconds; production always uses
/// [`WATCHDOG_POLL_INTERVAL`]. Mirrors the
/// `AOE_ACP_RUNNER_SOCKET_TIMEOUT_MS` test knob.
fn watchdog_poll_interval() -> Duration {
    std::env::var("AOE_ACP_WATCHDOG_POLL_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .unwrap_or(WATCHDOG_POLL_INTERVAL)
}

/// Consecutive `Missing` polls before the watchdog treats the record as
/// gone for good. Debounced so a daemon-side delete+respawn (supersede) or
/// an atomic-rename window can't trigger a false self-destruct on a single
/// observation. The first poll only fires after `WATCHDOG_POLL_INTERVAL`,
/// which doubles as a startup grace so the initial record write isn't
/// raced.
const WATCHDOG_MISSING_THRESHOLD: u32 = 2;

/// Bounded retention for a detached runner. While no daemon is attached,
/// the runner keeps the agent alive so a fresh `aoe serve` can reattach
/// mid-turn (this is the whole point of the shim outliving the daemon).
/// But a daemon that crashes/SIGKILLs in a persistent `$HOME` and never
/// restarts would otherwise leave the runner + agent alive forever, with
/// no daemon left to reap them. After this long with no attachment, the
/// runner self-terminates. Generous enough to cover an overnight or
/// weekend daemon stop; the clock resets on every reattach. See #1921.
const DETACHED_RETENTION: Duration = Duration::from_secs(48 * 60 * 60);

/// Sentinel in [`DetachedSince`] meaning "a daemon is currently attached",
/// so the detached-retention clock is not running.
const ATTACHED: u64 = 0;

/// Shared unix-epoch-seconds marker for when the runner last went
/// detached, or [`ATTACHED`] while a daemon is connected. Written by the
/// accept loop on connect/disconnect, read by the watchdog.
type DetachedSince = AtomicU64;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Why the runner is tearing down. Drives whether teardown deletes the
/// registry entry: a superseded runner must NOT delete, since the files
/// now belong to the fresh runner that replaced it.
#[derive(Debug, Clone, Copy)]
enum WatchdogShutdown {
    /// Our registry record vanished (HOME deleted, or daemon `delete`d it).
    RecordMissing,
    /// A fresh runner superseded us; the on-disk files are now theirs.
    Superseded,
    /// Detached past [`DETACHED_RETENTION`] with no daemon reattaching.
    DetachedRetentionExpired,
}

/// Cap on agent → daemon notification lines stored while detached.
/// Each entry is at most one ndjson line (a few KB). Past this, oldest
/// entries are dropped; the daemon-side event_store still has them.
const NOTIFICATION_BUFFER_LINES: usize = 256;

/// An agent that exits within this window of being spawned is treated as a
/// broken spawn and logged at warn (not info), so a crash loop is visible in
/// debug.log without grepping for the absence of success. Intentionally
/// mirrors `runner_socket_deadline()` in `acp/acp_client.rs` (the
/// daemon's 10s wait for this runner's socket to appear); update both if
/// the handshake window changes. See #1945.
const FAST_EXIT_THRESHOLD: Duration = Duration::from_secs(10);

/// Pipe-read buffer for the agent's stdout. 64KB matches the default
/// pipe size on macOS/Linux.
const STDOUT_READ_BUF: usize = 64 * 1024;

/// Stdout-silence keepalive (see #2455). `claude-agent-acp` (<= 0.52.0)
/// wraps `process.stdout.write` in a Web Streams writable with no
/// backpressure handling, so a mid-turn stdout write can stall until a
/// stdin `data` event wakes the Node event loop and lets libuv flush. The
/// runner writes a harmless `\n` to the agent's stdin once stdout goes
/// silent, which wakes the adapter and flushes the stalled output; the
/// adapter's ndjson decoder skips blank lines, so the nudge is never
/// parsed as a message or a user prompt. Temporary mitigation; remove once
/// the adapter handles stdout backpressure (upstream fix tracked
/// separately). Fast burst clears a transient stall quickly; the slow
/// unbounded tail guarantees a stall that begins after a long legitimate
/// silence (e.g. a multi-minute model think with zero output) is still
/// eventually flushed.
const STDOUT_NUDGE_FAST_INTERVAL: Duration = Duration::from_secs(2);
const STDOUT_NUDGE_FAST_BURST: u32 = 3;
const STDOUT_NUDGE_SLOW_INTERVAL: Duration = Duration::from_secs(30);
/// Cap on a single nudge write so a wedged adapter that stopped reading
/// stdin cannot park the keepalive while it holds the shared stdin mutex
/// (which would block real daemon -> agent traffic).
const STDOUT_NUDGE_WRITE_TIMEOUT: Duration = Duration::from_millis(250);

/// Resolved keepalive timing for a runner. `None` (from
/// [`stdout_nudge_config`]) disables the keepalive entirely.
struct StdoutNudgeConfig {
    fast_interval: Duration,
    fast_burst: u32,
    slow_interval: Duration,
    write_timeout: Duration,
}

/// Decide whether and how to run the stdout-silence keepalive for this
/// agent. Enabled by default only for `claude-agent-acp` (the adapter the
/// `\n`-is-safely-skipped behavior was verified against); other agents
/// might treat a blank line as a protocol error, so they stay off unless
/// `AOE_ACP_STDOUT_NUDGE=1` forces it. `AOE_ACP_STDOUT_NUDGE=0` disables it
/// even for claude. `AOE_ACP_STDOUT_NUDGE_MS` / `_SLOW_MS` shrink the
/// intervals for tests, mirroring `AOE_ACP_WATCHDOG_POLL_MS`.
fn stdout_nudge_config(args: &AcpRunnerArgs) -> Option<StdoutNudgeConfig> {
    let enabled = match std::env::var("AOE_ACP_STDOUT_NUDGE").ok().as_deref() {
        Some("0") | Some("false") | Some("off") => return None,
        Some("1") | Some("true") | Some("on") => true,
        _ => stdout_nudge_default_for_agent(args),
    };
    if !enabled {
        return None;
    }
    let fast_interval =
        env_duration_ms("AOE_ACP_STDOUT_NUDGE_MS").unwrap_or(STDOUT_NUDGE_FAST_INTERVAL);
    // A zero fast interval is an explicit kill switch.
    if fast_interval.is_zero() {
        return None;
    }
    let slow_interval = env_duration_ms("AOE_ACP_STDOUT_NUDGE_SLOW_MS")
        .unwrap_or(STDOUT_NUDGE_SLOW_INTERVAL)
        // Never tighter than the fast cadence (also rules out zero).
        .max(fast_interval);
    Some(StdoutNudgeConfig {
        fast_interval,
        fast_burst: STDOUT_NUDGE_FAST_BURST,
        slow_interval,
        write_timeout: STDOUT_NUDGE_WRITE_TIMEOUT,
    })
}

fn stdout_nudge_default_for_agent(args: &AcpRunnerArgs) -> bool {
    args.agent_name.contains("claude-agent-acp")
        || args
            .agent_argv
            .first()
            .map(|s| s.contains("claude-agent-acp"))
            .unwrap_or(false)
}

fn env_duration_ms(key: &str) -> Option<Duration> {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_millis)
}

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
pub async fn run(args: AcpRunnerArgs) -> Result<()> {
    // `aoe __acp-runner` is a hidden subcommand, but a curious
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
    // Anchor for the fast-exit warn below: an agent that dies within
    // FAST_EXIT_THRESHOLD is almost always a broken spawn (missing adapter,
    // bad command, immediate handshake failure) and is what drove the silent
    // reconciler respawn loop. Measure from agent spawn, not run() entry, so
    // logging/socket/registry setup time isn't counted. See #1945.
    let agent_started_at = std::time::Instant::now();

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
    // direct file write is what gives `aoe acp logs --session <id>`
    // and `GET /api/sessions/:id/acp/worker-log` something to read
    // (init_runner_logging routes tracing to debug.log, not the
    // per-session file). See #1449.
    if let Some(stderr) = agent_stderr {
        let label = args.session_id.clone();
        let per_session_log = worker_registry::log_path_for(&args.session_id).ok();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!(target: "acp.runner.agent.stderr", session = %label, "{line}");
                if let Some(path) = per_session_log.as_ref() {
                    append_agent_stderr_line(path, &line);
                }
            }
        });
    }

    let shared = Arc::new(RunnerShared::new());

    // Last time (epoch millis) the agent wrote to stdout; drives the
    // stdout-silence keepalive (#2455).
    let stdout_activity = Arc::new(AtomicU64::new(now_millis()));

    // Fan-out task: reads agent stdout and either forwards to the
    // currently-attached daemon or buffers in the ring. Single owner of
    // the read half of the agent's stdout pipe.
    let agent_stdout_task = tokio::spawn(fanout_agent_stdout(
        agent_stdout,
        Arc::clone(&shared),
        args.session_id.clone(),
        Arc::clone(&stdout_activity),
    ));

    // Wrap agent stdin in a tokio Mutex so the accept loop can hand it
    // to one connection at a time. Wrapping (not splitting) keeps stdin
    // alive across reconnects; closing it would cause aoe-agent to
    // `process.exit(0)`.
    let agent_stdin = Arc::new(Mutex::new(agent_stdin));

    // Stdout-silence keepalive (#2455): nudge the agent's stdin with a
    // harmless `\n` when its stdout stalls mid-turn, so a buffering bug in
    // the upstream adapter can't freeze the session. Disabled for agents
    // it isn't verified safe for (see `stdout_nudge_config`).
    let keepalive_task = stdout_nudge_config(&args).map(|cfg| {
        tokio::spawn(stdout_silence_nudge(
            cfg,
            Arc::clone(&stdout_activity),
            Arc::clone(&agent_stdin),
            args.session_id.clone(),
        ))
    });

    // Signal handling: SIGTERM/SIGINT → kill agent, cleanup, exit.
    let shutdown_signal = wait_for_shutdown();

    let session_id = args.session_id.clone();

    // Abandonment watchdog: a daemon that dies without explicitly killing
    // its runners (crash, SIGKILL, or an ephemeral test `$HOME` that gets
    // deleted) would otherwise leave this runner + agent + grandchildren
    // alive forever, since every other reaper runs inside a live daemon in
    // the same `$HOME`. The watchdog gives the runner a self-destruct path.
    // It polls the registry record via a non-creating read of a path
    // captured now (while the dir exists), so it never resurrects a deleted
    // `$HOME`. `detached_since` starts "detached" (no daemon yet) and is
    // flipped by the accept loop. See #1921.
    let detached_since: Arc<DetachedSince> = Arc::new(AtomicU64::new(now_secs()));
    let watchdog_task = {
        let record_path = worker_registry::record_path(&args.session_id)?;
        let restart_marker = worker_registry::restart_marker_path(&args.session_id)?;
        let (watchdog_tx, watchdog_rx) = tokio::sync::oneshot::channel::<WatchdogShutdown>();
        let handle = tokio::spawn(run_watchdog(
            record_path,
            restart_marker,
            our_pid,
            Arc::clone(&detached_since),
            session_id.clone(),
            watchdog_tx,
        ));
        (handle, watchdog_rx)
    };
    let (watchdog_handle, mut watchdog_rx) = watchdog_task;

    let accept_session_id = session_id.clone();
    let accept_shared = Arc::clone(&shared);
    let accept_detached = Arc::clone(&detached_since);
    let accept_loop = async move {
        loop {
            match listener.accept().await {
                Ok((stream, _addr)) => {
                    info!(
                        target: "acp.runner",
                        session = %accept_session_id,
                        "daemon connected"
                    );
                    worker_registry::mark_attached(&accept_session_id);
                    accept_detached.store(ATTACHED, Ordering::Relaxed);
                    handle_connection(
                        stream,
                        Arc::clone(&accept_shared),
                        Arc::clone(&agent_stdin),
                        accept_session_id.clone(),
                    )
                    .await;
                    info!(
                        target: "acp.runner",
                        session = %accept_session_id,
                        "daemon disconnected; runner stays alive"
                    );
                    worker_registry::mark_detached(&accept_session_id);
                    accept_detached.store(now_secs(), Ordering::Relaxed);
                }
                Err(e) => {
                    warn!(target: "acp.runner", "accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    };

    // Set when teardown must leave the registry/socket in place because a
    // newer runner now owns them (the superseded case).
    let mut preserve_registry = false;

    // Wait for: agent exit, signal, watchdog self-destruct, or accept loop
    // death (last is unreachable but kept for symmetry).
    tokio::select! {
        status = agent_child.wait() => {
            let elapsed = agent_started_at.elapsed();
            match status {
                // A clean (status 0) but near-instant exit is still a broken
                // worker; warn regardless of exit code so a `grep -E
                // 'error|warn'` over debug.log surfaces the crash loop that
                // INFO-level logging used to hide. See #1945.
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
        reason = &mut watchdog_rx => {
            if let Ok(reason) = reason {
                // A superseded runner must not delete the registry/socket:
                // they belong to the fresh runner that replaced it. The
                // group-leader teardown SIGKILLs itself and never returns
                // here, but the non-leader fallback (and the non-unix path)
                // do return, so guard the post-loop delete below too.
                if matches!(reason, WatchdogShutdown::Superseded) {
                    preserve_registry = true;
                }
                self_terminate_agent_tree(reason, &session_id, our_pid, &mut agent_child).await;
            }
        }
        _ = accept_loop => {
            warn!(target: "acp.runner", session = %session_id, "accept loop exited unexpectedly");
        }
    }

    watchdog_handle.abort();
    agent_stdout_task.abort();
    if let Some(task) = keepalive_task {
        task.abort();
    }
    if !preserve_registry {
        worker_registry::delete(&session_id).ok();
    }
    Ok(())
}

/// Poll this runner's own registry record and signal the main loop to
/// self-destruct when it observes that the runner has been abandoned.
/// Sends at most one [`WatchdogShutdown`] and returns; the main `select!`
/// owns the actual teardown so there is exactly one killer (no double-fire
/// with the signal/agent-exit paths, which simply cancel this task). See
/// #1921.
async fn run_watchdog(
    record_path: PathBuf,
    restart_marker: PathBuf,
    own_pid: u32,
    detached_since: Arc<DetachedSince>,
    session_id: String,
    tx: tokio::sync::oneshot::Sender<WatchdogShutdown>,
) {
    let mut missing = 0u32;
    let poll_interval = watchdog_poll_interval();
    loop {
        // Sleep first: the initial delay doubles as a startup grace so the
        // record write at boot isn't raced.
        tokio::time::sleep(poll_interval).await;

        // Detached-retention backstop for the persistent-`$HOME`
        // crash-no-restart case, where the record survives but no daemon
        // is left to reap us.
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

        // Parse the pid from our own record format here so `process::worker`
        // stays payload-agnostic; a parse failure maps to `Unreadable`,
        // preserving the "malformed record is non-fatal" watchdog semantics.
        match crate::process::worker::inspect_record_for_runner(&record_path, own_pid, |bytes| {
            serde_json::from_slice::<WorkerRecord>(bytes)
                .ok()
                .map(|rec| rec.pid)
        }) {
            // Still ours, or a transient read hiccup we shouldn't act on.
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
                // `aoe acp restart` deletes the record right before it
                // SIGTERMs us; the marker tells us not to race that to a
                // hard self-destruct.
                if restart_marker.exists() {
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

/// Tear down the agent process tree after the watchdog flags abandonment.
/// Politely SIGTERMs the agent, waits briefly, then SIGKILLs the whole
/// process group (runner + node wrapper + `claude` grandchild) so nothing
/// is left orphaned under PID 1.
#[cfg(unix)]
async fn self_terminate_agent_tree(
    reason: WatchdogShutdown,
    session_id: &str,
    own_pid: u32,
    agent_child: &mut Child,
) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::{getpgrp, getpid, Pid};

    info!(
        target: "acp.runner",
        session = %session_id,
        ?reason,
        "runner abandoned; terminating agent tree"
    );

    // A superseded runner must NOT delete the registry/socket: those files
    // now belong to the fresh runner that replaced us, and deleting them
    // would make the new runner's own watchdog see "missing" and cascade.
    // Every other reason means we still own them (or they're already gone),
    // so cleanup is safe and clears a stale socket that would confuse
    // attach.
    if !matches!(reason, WatchdogShutdown::Superseded) {
        worker_registry::delete(session_id).ok();
    }

    // Polite SIGTERM to the agent (node) so a cooperative adapter can
    // flush; the group SIGKILL below is the guarantee.
    if let Some(agent_pid) = agent_child.id() {
        let _ = kill(Pid::from_raw(agent_pid as i32), Signal::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), agent_child.wait()).await;

    // Final hammer. The runner is its own process-group leader via setsid,
    // so SIGKILLing the group reaps the node wrapper and its `claude`
    // grandchild together. It also kills the runner itself, which is
    // exactly the intent: nothing is left to clean up after this. Guard on
    // actually being the group leader so a failed setsid can't make us
    // SIGKILL the daemon's inherited group.
    if getpgrp() == getpid() {
        crate::process::worker::kill_process_group(own_pid);
    } else {
        // setsid failed; we share another group. Never group-kill it. Kill
        // just the direct child and fall through to a normal runner exit.
        let _ = agent_child.start_kill();
        let _ = agent_child.wait().await;
    }
}

#[cfg(not(unix))]
async fn self_terminate_agent_tree(
    reason: WatchdogShutdown,
    session_id: &str,
    _own_pid: u32,
    agent_child: &mut Child,
) {
    info!(
        target: "acp.runner",
        session = %session_id,
        ?reason,
        "runner abandoned; terminating agent"
    );
    if !matches!(reason, WatchdogShutdown::Superseded) {
        worker_registry::delete(session_id).ok();
    }
    let _ = agent_child.start_kill();
    let _ = agent_child.wait().await;
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
                    target: "acp.runner",
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
            target: "acp.runner",
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
                        target: "acp.runner",
                        session = %session_id,
                        "failed to serialise cancellation for id {id}: {e}"
                    );
                    continue;
                }
            };
            bytes.push(b'\n');
            if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                warn!(
                    target: "acp.runner",
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
                // Drain failed mid-way, so push the remaining lines back
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
    stdout_activity: Arc<AtomicU64>,
) {
    let mut reader = BufReader::with_capacity(STDOUT_READ_BUF, stdout);
    let mut line = Vec::with_capacity(4096);
    loop {
        line.clear();
        // read_until preserves the trailing newline, which ndjson
        // consumers (the daemon's ACP transport) need.
        match reader.read_until(b'\n', &mut line).await {
            Ok(0) => {
                debug!(target: "acp.runner", session = %session_id, "agent stdout EOF");
                break;
            }
            Ok(_) => {
                // Mark progress for the stdout-silence keepalive (#2455).
                stdout_activity.store(now_millis(), Ordering::Relaxed);
                shared.deliver_line(&line).await;
            }
            Err(e) => {
                warn!(target: "acp.runner", session = %session_id, "stdout read error: {e}");
                break;
            }
        }
    }
}

/// Stdout-silence keepalive loop (#2455). Watches `stdout_activity`; when
/// the agent has produced no stdout for `fast_interval`, writes a harmless
/// `\n` to its stdin to wake a stalled upstream adapter. Nudges in a fast
/// burst, then a slow unbounded tail, until stdout resumes (which re-arms
/// the burst). Runs regardless of daemon attachment: a stall while
/// detached would otherwise leave the runner's replay ring missing the
/// gap. Ends when the agent's stdin closes (agent gone); the runner's
/// teardown also aborts it.
async fn stdout_silence_nudge(
    cfg: StdoutNudgeConfig,
    stdout_activity: Arc<AtomicU64>,
    agent_stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    session_id: String,
) {
    loop {
        let mut last = stdout_activity.load(Ordering::Relaxed);
        tokio::time::sleep(cfg.fast_interval).await;
        if stdout_activity.load(Ordering::Relaxed) != last {
            continue; // stdout flowed within the interval; not silent
        }
        // Silent past the fast interval. Nudge, then keep nudging at the
        // fast cadence for the burst and the slow cadence after, until
        // stdout activity resumes.
        let mut sent: u32 = 0;
        loop {
            if !write_stdin_nudge(&agent_stdin, cfg.write_timeout, &session_id, sent).await {
                return; // stdin write failed: agent gone
            }
            sent += 1;
            let wait = if sent < cfg.fast_burst {
                cfg.fast_interval
            } else {
                cfg.slow_interval
            };
            last = stdout_activity.load(Ordering::Relaxed);
            tokio::time::sleep(wait).await;
            if stdout_activity.load(Ordering::Relaxed) != last {
                break; // a nudge flushed output (or output resumed); re-arm
            }
        }
    }
}

/// Write a single `\n` keepalive to the agent's stdin under the shared
/// mutex (so it never interleaves a daemon-originated JSON-RPC line),
/// bounded by `write_timeout`. Returns false only on a write error (the
/// pipe is broken: the agent exited), which ends the keepalive loop. A
/// timeout returns true so the loop keeps trying at the slow cadence;
/// process lifecycle is owned by the main `select!`, not here.
async fn write_stdin_nudge(
    agent_stdin: &Mutex<tokio::process::ChildStdin>,
    write_timeout: Duration,
    session_id: &str,
    sent: u32,
) -> bool {
    let mut stdin = agent_stdin.lock().await;
    match tokio::time::timeout(write_timeout, async {
        stdin.write_all(b"\n").await?;
        stdin.flush().await
    })
    .await
    {
        Ok(Ok(())) => {
            debug!(
                target: "acp.runner",
                session = %session_id,
                nudge = sent + 1,
                "stdout-silence keepalive nudge"
            );
            true
        }
        Ok(Err(e)) => {
            warn!(
                target: "acp.runner",
                session = %session_id,
                "keepalive nudge write failed; agent likely exited: {e}"
            );
            false
        }
        Err(_) => {
            warn!(
                target: "acp.runner",
                session = %session_id,
                write_timeout_ms = write_timeout.as_millis(),
                "keepalive nudge write timed out; agent not reading stdin"
            );
            true
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
            target: "acp.runner",
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
                        target: "acp.runner",
                        session = %session_id,
                        "agent stdin write failed; agent likely exited"
                    );
                    break;
                }
            }
            Err(e) => {
                warn!(target: "acp.runner", session = %session_id, "daemon read error: {e}");
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
    // Keep the per-session log file path created so `aoe acp logs
    // --session <id>` and any external tail works. The actual tracing
    // output goes to the shared `debug.log` so daemon + every runner
    // appear in one timeline; runner spans add `session_id` for filtering.
    // The agent stderr drainer at run() writes lines here directly so
    // the per-session file is the structured view's "what did the adapter say"
    // surface (used by GET /acp/worker-log). See #1449.
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

    // The runner is single-session; its tracing still flows to the shared
    // debug.log. The per-session tee runs only in the daemon (#1864), so
    // no tee layer is installed here.
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
        "[{ts}] runner.startup: structured view runner up session={session_id}"
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

    fn args_with_agent(agent_key: &str, agent_name: &str, argv0: &str) -> AcpRunnerArgs {
        AcpRunnerArgs {
            socket: PathBuf::from("/tmp/x.sock"),
            session_id: "s".into(),
            agent_name: agent_name.into(),
            agent_key: agent_key.into(),
            cwd: PathBuf::from("/tmp"),
            model: None,
            additional_dirs: vec![],
            provider_env_keys: vec![],
            stored_acp_session_id: None,
            source_profile: String::new(),
            agent_argv: vec![argv0.into()],
        }
    }

    /// The keepalive defaults on only for claude-agent-acp, the adapter the
    /// blank-line-safe behavior was verified against. Other agents stay off
    /// so a stray `\n` can't corrupt a transport that rejects blank lines.
    #[test]
    fn stdout_nudge_default_scoped_to_claude_adapter() {
        assert!(stdout_nudge_default_for_agent(&args_with_agent(
            "claude",
            "claude-agent-acp",
            "claude-agent-acp"
        )));
        // Matched by agent_name / argv even when the key differs.
        assert!(stdout_nudge_default_for_agent(&args_with_agent(
            "",
            "claude-agent-acp",
            "/usr/bin/claude-agent-acp"
        )));
        // A bare `claude` key without the claude-agent-acp binary is not
        // enough: the keepalive defaults on only for the verified adapter.
        assert!(!stdout_nudge_default_for_agent(&args_with_agent(
            "claude", "claude", "claude"
        )));
        // Other adapters are off by default.
        assert!(!stdout_nudge_default_for_agent(&args_with_agent(
            "codex",
            "codex-acp",
            "codex-acp"
        )));
        assert!(!stdout_nudge_default_for_agent(&args_with_agent(
            "opencode", "opencode", "opencode"
        )));
    }

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
