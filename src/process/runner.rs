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

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Args;
use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use super::worker_registry::{self, WorkerRecord};
use crate::acp::control_protocol::{self, ControlBody};
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

    // Bind the sibling control socket BEFORE the main relay socket, and
    // both before spawning the agent. The daemon waits for the main
    // socket to appear, then dials the control socket; binding control
    // first guarantees it is connectable by the time the main socket is,
    // so no capability handshake or record field is needed to advertise
    // it. Phase A of #1054.
    let control_socket = crate::process::worker::control_socket_sibling(&args.socket);
    if let Some(parent) = args.socket.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating socket dir {}", parent.display()))?;
    }
    if control_socket.exists() {
        let _ = std::fs::remove_file(&control_socket);
    }
    let control_listener = UnixListener::bind(&control_socket)
        .with_context(|| format!("bind {}", control_socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&control_socket, std::fs::Permissions::from_mode(0o600));
    }

    // Bind the main relay socket. The runner binds before it spawns the
    // agent so the daemon's post-spawn connect doesn't race the listener
    // creation.
    if args.socket.exists() {
        let _ = std::fs::remove_file(&args.socket);
    }
    let listener = UnixListener::bind(&args.socket)
        .with_context(|| format!("bind {}", args.socket.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o600));
    }

    // Persist the registry record BEFORE spawning the agent. The record is
    // built entirely from `args`, our pid, and the socket bound above, so it
    // needs no agent handle; saving first means a save failure has no agent
    // process (nor any node/`claude` descendants the adapter might spawn) to
    // leak, only the socket to remove.
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
    if let Err(e) = worker_registry::save(&record).context("writing registry record") {
        let _ = std::fs::remove_file(&args.socket);
        return Err(e);
    }

    let (mut agent_child, agent_stdin, agent_stdout, agent_stderr) = match spawn_agent(&args) {
        Ok(handles) => handles,
        Err(e) => {
            // Roll back the record and socket we just wrote so a failed spawn
            // leaves nothing for the daemon to dial or later sweep.
            worker_registry::delete(&args.session_id).ok();
            return Err(e).with_context(|| format!("spawning agent {:?}", args.agent_argv));
        }
    };
    // Anchor for the fast-exit warn below: an agent that dies within
    // FAST_EXIT_THRESHOLD is almost always a broken spawn (missing adapter,
    // bad command, immediate handshake failure) and is what drove the silent
    // reconciler respawn loop. Measure from agent spawn, not run() entry, so
    // logging/socket/registry setup time isn't counted. See #1945.
    let agent_started_at = std::time::Instant::now();

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
    // Fan-out task: reads agent stdout and either forwards to the
    // currently-attached daemon or buffers in the ring. Single owner of
    // the read half of the agent's stdout pipe.
    let agent_stdout_task = tokio::spawn(fanout_agent_stdout(
        agent_stdout,
        Arc::clone(&shared),
        args.session_id.clone(),
    ));

    // Control-channel accept loop (#1054 Phase A). Serves the sibling
    // `<id>.control.sock`, over which the runner reports native
    // turn-complete signals. Independent of the main byte-relay accept
    // loop; a daemon attaches to both. Detached like the stderr drainer:
    // the process teardown drops it.
    let control_shared = Arc::clone(&shared);
    let control_session = args.session_id.clone();
    let control_accept_task = tokio::spawn(async move {
        loop {
            match control_listener.accept().await {
                Ok((stream, _addr)) => {
                    info!(
                        target: "acp.runner",
                        session = %control_session,
                        "daemon connected (control channel)"
                    );
                    handle_control_connection(
                        stream,
                        Arc::clone(&control_shared),
                        control_session.clone(),
                    )
                    .await;
                    info!(
                        target: "acp.runner",
                        session = %control_session,
                        "daemon disconnected (control channel)"
                    );
                }
                Err(e) => {
                    warn!(target: "acp.runner", "control accept error: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });

    // Wrap agent stdin in a tokio Mutex so the accept loop can hand it
    // to one connection at a time. Wrapping (not splitting) keeps stdin
    // alive across reconnects; closing it would cause aoe-agent to
    // `process.exit(0)`.
    let agent_stdin = Arc::new(Mutex::new(agent_stdin));

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
    control_accept_task.abort();
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
async fn self_terminate_agent_tree(
    reason: WatchdogShutdown,
    session_id: &str,
    own_pid: u32,
    agent_child: &mut Child,
) {
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
    #[cfg(unix)]
    if let Some(agent_pid) = agent_child.id() {
        use nix::sys::signal::{kill, Signal};
        use nix::unistd::Pid;
        let _ = kill(Pid::from_raw(agent_pid as i32), Signal::SIGTERM);
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), agent_child.wait()).await;

    // Final hammer. When the runner is its own process-group leader (via
    // setsid), SIGKILLing the group reaps the node wrapper and its `claude`
    // grandchild together, and the runner itself, which is exactly the
    // intent: nothing is left to clean up. The platform-specific
    // group-leader check and kill live in `process::worker`. If we are not
    // the leader (setsid failed) or the platform is non-unix, fall back to
    // killing just the direct child and exit normally.
    if !crate::process::worker::kill_own_process_group_if_leader(own_pid) {
        let _ = agent_child.start_kill();
        let _ = agent_child.wait().await;
    }
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
    /// On daemon disconnect the runner synthesizes a response for every
    /// outstanding request so the agent doesn't park forever on one the
    /// new daemon can't answer (the responder oneshot died with the old
    /// daemon's `pending_responders` map): a `cancelled` outcome for
    /// `session/request_permission`, a JSON-RPC error for other methods.
    /// See #1099.
    outstanding_requests: Mutex<HashMap<i64, String>>,
    /// JSON-RPC ids of daemon-issued `session/prompt` requests awaiting a
    /// response. Populated from daemon to agent traffic; drained when the
    /// matching response is seen on the agent to daemon path, which fires
    /// a `PromptCompleted` control event. Lives on `RunnerShared` (not a
    /// connection) so a prompt issued by one daemon still reports
    /// completion to whichever daemon is attached when the agent
    /// responds. Phase A of #1054.
    prompt_requests: Mutex<HashSet<i64>>,
    /// Control-channel outbound and the single completion buffered across
    /// a no-daemon gap, under one lock so emit and attach are mutually
    /// exclusive (no drain-then-set TOCTOU).
    control: Mutex<ControlChannel>,
    /// Mirror of `active_outbound.is_some()`, read lock-free by
    /// `emit_control` to decide whether a completion is owned by a live
    /// main-relay daemon (drop it) or occurred during a genuine no-daemon
    /// gap (buffer it for the next control attach). Set/cleared alongside
    /// `active_outbound`.
    main_attached: std::sync::atomic::AtomicBool,
}

/// Control-channel state for the sibling `<id>.control.sock`. A single
/// mutex covers both fields so `emit_control` (check outbound, else
/// buffer) and `install_control_outbound` (drain buffer, set outbound)
/// cannot interleave and strand a completion.
#[derive(Default)]
struct ControlChannel {
    /// Write half to the attached daemon, or None when detached.
    outbound: Option<tokio::net::unix::OwnedWriteHalf>,
    /// The one completion produced during a no-daemon gap, awaiting the
    /// next control attach. ACP is serial per session, so at most one turn
    /// is ever legitimately pending; a newer completion supersedes.
    pending: Option<ControlBody>,
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

/// Method that gets a semantic `cancelled` outcome on disconnect. Every
/// other outstanding method is answered with a generic JSON-RPC error
/// (see `cancel_outstanding_requests`), so no request parks; only this
/// one needs a typed result because its `cancelled` outcome is a normal,
/// non-error control-flow signal the agent expects.
const PERMISSION_METHOD: &str = "session/request_permission";

/// The daemon-issued request whose response marks a turn complete. The
/// runner tracks its id (seen on the daemon to agent path) and surfaces
/// a native `PromptCompleted` when the matching response comes back on
/// the agent to daemon path. Phase A of #1054.
const PROMPT_METHOD: &str = "session/prompt";

/// Deadline for a single control-channel frame write. `emit_control`
/// holds the `control` mutex across the write and runs on the sole
/// stdout-relay task, so an unbounded write to a stalled control peer (a
/// slow reader or a full socket buffer) would freeze the mutex and the
/// whole session's relay. Capping it bounds that blast radius; a timeout
/// is treated as a write failure so the existing drop/buffer cleanup
/// runs. Phase A of #1054.
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Write a control frame with a bounded deadline. Returns `true` on a
/// successful write, `false` on a write error or timeout; callers treat
/// `false` as a dead/stalled socket and run their drop/buffer cleanup.
async fn write_control_frame(
    out: &mut tokio::net::unix::OwnedWriteHalf,
    body: &ControlBody,
) -> bool {
    matches!(
        tokio::time::timeout(
            CONTROL_WRITE_TIMEOUT,
            control_protocol::write_frame(out, body)
        )
        .await,
        Ok(Ok(()))
    )
}

/// Soft cap on `outstanding_requests`. Hit only if the daemon stops
/// answering non-permission requests (which a healthy ACP daemon
/// always does); a misbehaving daemon shouldn't be able to grow the
/// map without bound across reconnects. When the cap trips we shed the
/// non-permission entries first (permission cancellations are the
/// semantically important ones to preserve for the disconnect sweep)
/// and log once at warn so the leak is visible.
const MAX_OUTSTANDING_REQUESTS: usize = 1024;

impl RunnerShared {
    fn new() -> Self {
        Self {
            active_outbound: Mutex::new(None),
            pending: Mutex::new(VecDeque::with_capacity(NOTIFICATION_BUFFER_LINES)),
            outstanding_requests: Mutex::new(HashMap::new()),
            prompt_requests: Mutex::new(HashSet::new()),
            control: Mutex::new(ControlChannel::default()),
            main_attached: std::sync::atomic::AtomicBool::new(false),
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

        // Phase A #1054: if this is the agent's response to a tracked
        // `session/prompt`, surface a native turn-complete over the
        // control channel. Gated on actually tracking a prompt so a busy
        // session does not JSON-parse every agent line twice (this on top
        // of `note_daemon_response`). Independent of the byte-relay
        // outbound below, since the control channel is a separate socket.
        if !self.prompt_requests.lock().await.is_empty() {
            if let Some((id, stop_reason)) = parse_response(line) {
                if self.prompt_requests.lock().await.remove(&id) {
                    self.emit_control(ControlBody::PromptCompleted {
                        prompt_req_id: id,
                        stop_reason,
                    })
                    .await;
                }
            }
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
        // Buffer while STILL holding `active_outbound`. Dropping it before
        // locking `pending` opens a TOCTOU window: a reattaching
        // `install_outbound` (which locks `active_outbound` then `pending`
        // in the same order) could drain `pending` and install its writer in
        // the gap, stranding this line until the next reattach. Holding the
        // lock makes the "no live writer, so buffer" step atomic. Lock order
        // is `active_outbound` then `pending` everywhere, so no deadlock.
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

    /// On daemon disconnect, unblock every outstanding agent → daemon
    /// request so the agent's stdio loop never parks on a responder that
    /// died with the previous daemon. `session/request_permission` gets a
    /// semantic `cancelled` outcome (the agent retries on the next prompt);
    /// every other method gets a method-agnostic JSON-RPC error, which the
    /// agent's RPC layer resolves by id without needing the method's typed
    /// result shape, so no per-method synthesis (and its state-corruption
    /// risk) is required.
    async fn cancel_outstanding_requests(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        session_id: &str,
    ) {
        let drained: Vec<(i64, String)> = {
            let mut map = self.outstanding_requests.lock().await;
            let drained: Vec<(i64, String)> = map.iter().map(|(id, m)| (*id, m.clone())).collect();
            map.clear();
            drained
        };

        if drained.is_empty() {
            return;
        }
        info!(
            target: "acp.runner",
            session = %session_id,
            count = drained.len(),
            "synthesising responses for outstanding requests on daemon disconnect"
        );
        // Drop these now-answered requests from the pending replay ring so a
        // reattaching daemon is not handed a request the agent already saw
        // resolved (which it would answer a second time). Notifications and
        // any requests made after this sweep stay buffered and replay
        // normally. A `deliver_line` racing between its outstanding-insert
        // and its pending-push could still slip one request past this purge,
        // but that is harmless: the agent's transport already resolved the
        // id, so the duplicate response the new daemon sends is ignored.
        let cancelled_ids: HashSet<i64> = drained.iter().map(|(id, _)| *id).collect();
        {
            let mut pending = self.pending.lock().await;
            pending.retain(|line| match parse_request(line) {
                Some((id, _)) => !cancelled_ids.contains(&id),
                None => true,
            });
        }
        let mut stdin = agent_stdin.lock().await;
        for (id, method) in drained {
            let response = if method == PERMISSION_METHOD {
                // ACP `RequestPermissionResponse` with the `cancelled`
                // outcome. The agent SDK unblocks its parked stdio loop on
                // receipt and either retries on the next user prompt or
                // surfaces a cancelled-tool-call event upstream.
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": { "outcome": { "outcome": "cancelled" } }
                })
            } else {
                // Method-agnostic JSON-RPC error. The agent's transport
                // rejects the pending request by id; no typed result shape
                // is guessed, so this is safe for fs/*, terminal/*, and any
                // future method. Code -32001 is a server-defined error
                // signalling the daemon went away.
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32001,
                        "message": "daemon disconnected; request cancelled"
                    }
                })
            };
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
        // Hold `active_outbound` across the whole drain + install so a
        // concurrent `deliver_line` (which locks `active_outbound` first,
        // sees None, then buffers into `pending`) cannot slip a line into
        // `pending` after we have drained it but before the writer is
        // installed. Lock order is `active_outbound` then `pending`
        // everywhere, so this cannot deadlock.
        let mut guard = self.active_outbound.lock().await;
        let prev = guard.take();
        let mut pending = self.pending.lock().await;
        while let Some(line) = pending.pop_front() {
            if out.write_all(&line).await.is_err() || out.flush().await.is_err() {
                // Drain failed mid-way, so push the remaining lines back
                // and surface the write half as unusable. `active_outbound`
                // stays None (via the earlier take), matching the old
                // behavior of leaving no live writer on a failed attach.
                pending.push_front(line);
                return None;
            }
        }
        drop(pending);
        *guard = Some(out);
        self.main_attached
            .store(true, std::sync::atomic::Ordering::Relaxed);
        prev
    }

    async fn clear_outbound(&self) {
        *self.active_outbound.lock().await = None;
        self.main_attached
            .store(false, std::sync::atomic::Ordering::Relaxed);
        // A main-relay disconnect starts a no-daemon gap. Drop any
        // completion left un-drained from a prior gap so only the current
        // gap's completion is ever replayed to the next resuming daemon.
        self.control.lock().await.pending = None;
    }

    /// Peek a daemon to agent line: if it is a `session/prompt` request,
    /// record its id so the matching response (agent to daemon) reports a
    /// native turn-complete. Phase A of #1054.
    async fn note_prompt_request(&self, line: &[u8]) {
        if let Some((id, method)) = parse_request(line) {
            if method == PROMPT_METHOD {
                self.prompt_requests.lock().await.insert(id);
            }
        }
    }

    /// Deliver a control frame to the attached daemon, else buffer it for
    /// the next control attach. Phase A delivers exactly one completion
    /// per control attach (the adopted turn), so a successful live write
    /// tears the outbound down: no later frame is written to a socket the
    /// daemon has stopped reading. A completion is only buffered during a
    /// genuine no-daemon gap; while a main-relay daemon is attached it
    /// already owns that turn's completion via its own prompt future, so
    /// buffering it would replay a stale completion onto a future adopted
    /// turn. One lock over both fields keeps this mutually exclusive with
    /// `install_control_outbound`.
    async fn emit_control(&self, body: ControlBody) {
        let mut ch = self.control.lock().await;
        if let Some(out) = ch.outbound.as_mut() {
            let ok = write_control_frame(out, &body).await;
            ch.outbound = None;
            if ok {
                return;
            }
            // Dead or stalled control socket. A control daemon had dialed
            // in (this is the resume/adopted-turn path), so this completion
            // is real and was not received: buffer it unconditionally for
            // the next control attach to replay. Do NOT fall through to the
            // main_attached gate, which would drop it (the write timing out
            // is exactly when the fast path matters most). See PR #2975.
            ch.pending = Some(body);
            return;
        }
        // No control daemon was ever attached on this channel. Only buffer
        // during a genuine no-daemon gap; while a main-relay daemon is
        // attached it owns this turn's completion via its own prompt
        // future, so buffering would replay a stale completion onto a
        // future adopted turn.
        if self
            .main_attached
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        ch.pending = Some(body);
    }

    /// Install a control-channel write half: greet with `Hello`, then under
    /// the same lock either hand off the one buffered completion (and leave
    /// the outbound unset, since that is the adopted turn's single frame)
    /// or store the write half to deliver the completion live later.
    async fn install_control_outbound(
        &self,
        mut out: tokio::net::unix::OwnedWriteHalf,
        session_id: &str,
    ) {
        let hello = ControlBody::Hello {
            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            session_id: session_id.to_string(),
        };
        if !write_control_frame(&mut out, &hello).await {
            return;
        }
        let mut ch = self.control.lock().await;
        if let Some(body) = ch.pending.take() {
            if !write_control_frame(&mut out, &body).await {
                ch.pending = Some(body);
            }
            // Delivered (or failed on a dead/stalled socket); one completion
            // per attach, so do not retain the outbound.
            return;
        }
        ch.outbound = Some(out);
    }

    async fn clear_control_outbound(&self) {
        self.control.lock().await.outbound = None;
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

/// Hard cap on a single NDJSON frame (agent stdout or daemon inbound).
/// A buggy or hostile peer that never sends a newline would otherwise
/// grow the line buffer until the runner OOMs; the per-line ring bounds
/// line *count*, not bytes. 64 MiB sits far above any legitimate ACP
/// frame (large tool outputs, file contents, diffs) while still bounding
/// memory.
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Read one newline-terminated NDJSON frame into `buf`, bounded to
/// `MAX_FRAME_BYTES`. Returns `Ok(0)` at EOF, `Ok(n)` for an `n`-byte
/// frame (trailing newline preserved, as ndjson consumers need), or an
/// `InvalidData` error once the frame exceeds the cap. Mirrors
/// `AsyncBufReadExt::read_until(b'\n', ..)` but refuses to buffer an
/// unbounded line, so an unterminated or enormous frame terminates the
/// connection instead of exhausting memory.
async fn read_frame_bounded<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    buf: &mut Vec<u8>,
) -> std::io::Result<usize> {
    buf.clear();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(buf.len()); // EOF (buf holds any final unterminated bytes)
        }
        let newline = available.iter().position(|&b| b == b'\n');
        let take = newline.map_or(available.len(), |pos| pos + 1);
        buf.extend_from_slice(&available[..take]);
        reader.consume(take);
        if buf.len() > MAX_FRAME_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "ndjson frame exceeds MAX_FRAME_BYTES",
            ));
        }
        if newline.is_some() {
            return Ok(buf.len());
        }
    }
}

/// Peek fields of a JSON-RPC response line for turn-complete detection:
/// the `result.stopReason` when the response succeeded.
#[derive(Deserialize)]
struct JsonRpcResponsePeek {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
}

/// Parse a JSON-RPC response line into `(id, stop_reason)`. Returns None
/// for requests (a `method` is present), notifications (no `id`),
/// non-numeric ids, and malformed lines. An error-envelope response (no
/// `result`) still counts as a completion, with `stop_reason` None; the
/// turn ended either way, so the UI should stop showing "thinking".
fn parse_response(line: &[u8]) -> Option<(i64, Option<String>)> {
    let peek: JsonRpcResponsePeek = serde_json::from_slice(line).ok()?;
    if peek.method.is_some() {
        return None;
    }
    let id = peek.id?.as_i64()?;
    let stop_reason = peek
        .result
        .as_ref()
        .and_then(|r| r.get("stopReason"))
        .and_then(|s| s.as_str())
        .map(|s| s.to_string());
    Some((id, stop_reason))
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
        // read_frame_bounded preserves the trailing newline, which ndjson
        // consumers (the daemon's ACP transport) need, and caps frame size.
        match read_frame_bounded(&mut reader, &mut line).await {
            Ok(0) => {
                debug!(target: "acp.runner", session = %session_id, "agent stdout EOF");
                break;
            }
            Ok(_) => {
                shared.deliver_line(&line).await;
            }
            Err(e) => {
                warn!(target: "acp.runner", session = %session_id, "stdout read error: {e}");
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
            target: "acp.runner",
            session = %session_id,
            "evicting prior daemon outbound (concurrent attach)"
        );
    }

    let mut reader = BufReader::with_capacity(STDOUT_READ_BUF, read_half);
    let mut line = Vec::with_capacity(4096);
    loop {
        match read_frame_bounded(&mut reader, &mut line).await {
            Ok(0) => break, // EOF: daemon closed the connection.
            Ok(_) => {
                shared.note_daemon_response(&line).await;
                shared.note_prompt_request(&line).await;
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
    // Daemon disconnected. Synthesize responses for any outstanding
    // agent → daemon requests so the agent's stdio loop unblocks instead
    // of waiting forever on a responder that died with the previous
    // daemon (permission requests get a `cancelled` outcome, everything
    // else a JSON-RPC error).
    shared
        .cancel_outstanding_requests(&agent_stdin, &session_id)
        .await;
    shared.clear_outbound().await;
}

/// Handle one control-channel connection: install its write half
/// (greeting with `Hello` and draining buffered completion events), then
/// read daemon to runner frames until EOF so a disconnect is detected.
/// Phase A of #1054 has no daemon to runner frames that require action
/// (the daemon's `Attach` just confirms the version), so the read loop
/// exists only to observe the socket closing.
async fn handle_control_connection(
    stream: UnixStream,
    shared: Arc<RunnerShared>,
    session_id: String,
) {
    let (mut read_half, write_half) = stream.into_split();
    shared
        .install_control_outbound(write_half, &session_id)
        .await;
    loop {
        match control_protocol::read_frame(&mut read_half).await {
            Ok(Some(_body)) => {}
            Ok(None) => break, // clean EOF: daemon closed the control socket.
            Err(e) => {
                warn!(
                    target: "acp.runner",
                    session = %session_id,
                    "control read error: {e}"
                );
                break;
            }
        }
    }
    shared.clear_control_outbound().await;
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
        assert_eq!(parse_response(b"not json"), None);
    }

    #[test]
    fn parse_response_extracts_stop_reason() {
        let line = br#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#;
        assert_eq!(parse_response(line), Some((3, Some("end_turn".into()))));
    }

    #[test]
    fn parse_response_treats_error_envelope_as_completion() {
        // An error response still ends the turn; detected as a completion
        // with no stopReason.
        let line = br#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"boom"}}"#;
        assert_eq!(parse_response(line), Some((4, None)));
    }

    #[test]
    fn parse_response_ignores_requests_and_notifications() {
        assert_eq!(
            parse_response(br#"{"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{}}"#),
            None
        );
        assert_eq!(
            parse_response(br#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#),
            None
        );
    }

    #[test]
    fn parse_request_detects_prompt() {
        let line = br#"{"jsonrpc":"2.0","id":11,"method":"session/prompt","params":{}}"#;
        assert_eq!(parse_request(line), Some((11, PROMPT_METHOD.into())));
    }

    /// The core Phase A invariant: a tracked `session/prompt` request id,
    /// seen on the daemon to agent path, produces a `PromptCompleted`
    /// control event when the matching response arrives on the agent to
    /// daemon path. With no control daemon attached (and no main daemon
    /// attached, i.e. a genuine gap) the event is buffered in
    /// `control.pending`, which is exactly the mid-restart case the change
    /// exists to cover.
    #[tokio::test]
    async fn prompt_response_emits_completed_control_event() {
        let shared = RunnerShared::new();
        let prompt = br#"{"jsonrpc":"2.0","id":5,"method":"session/prompt","params":{}}
"#;
        shared.note_prompt_request(prompt).await;
        assert!(shared.prompt_requests.lock().await.contains(&5));

        let resp = br#"{"jsonrpc":"2.0","id":5,"result":{"stopReason":"end_turn"}}
"#;
        shared.deliver_line(resp).await;

        // The prompt id is drained and a completion event is buffered.
        assert!(shared.prompt_requests.lock().await.is_empty());
        assert_eq!(
            shared.control.lock().await.pending,
            Some(ControlBody::PromptCompleted {
                prompt_req_id: 5,
                stop_reason: Some("end_turn".into()),
            })
        );
    }

    /// The gate: a completion produced while a main-relay daemon is
    /// attached is owned by that daemon's own prompt future, so it must
    /// NOT be buffered (buffering would replay it onto a future adopted
    /// turn, prematurely stopping it). See PR #2975 review.
    #[tokio::test]
    async fn completion_not_buffered_while_main_daemon_attached() {
        let shared = RunnerShared::new();
        shared
            .main_attached
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let prompt = br#"{"jsonrpc":"2.0","id":8,"method":"session/prompt","params":{}}
"#;
        shared.note_prompt_request(prompt).await;
        let resp = br#"{"jsonrpc":"2.0","id":8,"result":{"stopReason":"end_turn"}}
"#;
        shared.deliver_line(resp).await;
        assert!(
            shared.control.lock().await.pending.is_none(),
            "a live main-relay daemon owns the completion; nothing should buffer"
        );
    }

    /// Regression for PR #2975: when a live control write fails (dead or
    /// stalled socket), the completion must be buffered for the next attach
    /// rather than dropped through the `main_attached` gate. A control
    /// daemon had dialed in, so the completion is real and unreceived.
    #[tokio::test]
    async fn emit_control_buffers_on_write_failure_even_when_main_attached() {
        use std::sync::atomic::Ordering;

        let shared = RunnerShared::new();
        // A control outbound whose peer is gone: writes to it fail. For an
        // AF_UNIX stream, a write after the peer closes returns an error
        // rather than buffering, so this is deterministic.
        let (peer, ours) = tokio::net::UnixStream::pair().unwrap();
        drop(peer);
        let (_r, w) = ours.into_split();
        shared.control.lock().await.outbound = Some(w);
        // Main relay attached: the pre-fix code would drop here.
        shared.main_attached.store(true, Ordering::Relaxed);

        let body = ControlBody::PromptCompleted {
            prompt_req_id: 9,
            stop_reason: Some("end_turn".into()),
        };
        shared.emit_control(body.clone()).await;

        let ch = shared.control.lock().await;
        assert!(ch.outbound.is_none(), "a failed write clears the outbound");
        assert_eq!(
            ch.pending,
            Some(body),
            "completion buffered despite main_attached, not dropped through the gate"
        );
    }

    /// A response id the runner never tracked as a prompt (e.g. a reply to
    /// an fs/terminal request) must not produce a completion event.
    #[tokio::test]
    async fn untracked_response_emits_nothing() {
        let shared = RunnerShared::new();
        let resp = br#"{"jsonrpc":"2.0","id":77,"result":{}}
"#;
        shared.deliver_line(resp).await;
        assert!(shared.control.lock().await.pending.is_none());
    }

    /// `deliver_line` populates the outstanding-requests map on the
    /// agent → daemon request path; `note_daemon_response` removes it
    /// on the daemon → agent reply path. The map is the source of
    /// truth for `cancel_outstanding_requests`, so this covers the
    /// bookkeeping invariant directly.
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
