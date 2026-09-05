//! `aoe __acp-runner`: the per-worker shim that owns the agent
//! subprocess and outlives `aoe serve`.
//!
//! The detached runner writes its registry record, spawns a stdio-only ACP
//! agent, and exposes the framed v3 protocol on `<session_id>.control.sock`.
//! It owns the ACP handshake, forwards both RPC directions, buffers outbound
//! control frames while detached, and accepts the next daemon connection
//! without restarting an established agent session.
//!
//! On agent exit, signal, abandonment, or an incomplete handshake disconnect,
//! the runner terminates the agent process tree and removes only registry files
//! that still belong to its PID. Per-runner logs remain under `acp-workers`
//! so `aoe acp logs --session <id> --follow` can tail them independently.
//!
//! This shim lets third-party agents that only speak ACP over stdio participate
//! in the detached worker lifecycle without requiring agent-side socket support.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::Args;
use serde::Deserialize;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch, Mutex};
use tracing::{debug, info, warn};

use super::worker_registry::{self, WorkerRecord};
use crate::acp::control_protocol::{self, ControlBody, PromptOutcome};
use crate::process::worker::RunnerRecordState;
use crate::util::now_secs;

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

/// An agent that exits within this window of being spawned is treated as a
/// broken spawn and logged at warn (not info), so a crash loop is visible in
/// debug.log without grepping for the absence of success. Intentionally
/// mirrors `runner_socket_deadline()` in `acp/acp_client/runner.rs` (the
/// daemon's 10s wait for this runner's socket to appear); update both if
/// the handshake window changes. See #1945.
const FAST_EXIT_THRESHOLD: Duration = Duration::from_secs(10);

/// Reserved daemon-to-runner carrier for the trusted configured environment
/// (`Config.environment`) that applies to the ACP adapter but must not apply
/// to the runner infrastructure itself. Holds JSON `[[key, value], ...]`;
/// written by `acp_client::spawn_runner_detached`, consumed and removed by
/// `spawn_agent` before the adapter starts.
pub(crate) const ACP_AGENT_ENV: &str = "AOE_ACP_AGENT_ENV";

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

    // Bind the control socket before spawning the agent, so the daemon's
    // post-spawn connect cannot race the listener. This is the only live socket;
    // the legacy `<id>.sock` path is retained only as a derivation base,
    // and the daemon's readiness probe and liveness check both key on the
    // control socket instead. `--socket` stays the derivation base for the
    // sibling path (and the registry record's `socket_path`), so runner and
    // daemon agree without a new field.
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
        let _ = std::fs::remove_file(&control_socket);
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

    let shared = Arc::new(RunnerShared::new(Some((args.session_id.clone(), our_pid))));

    // Wrap agent stdin in a tokio Mutex so the control loop and the stdout
    // fanout can each write to it. Wrapping (not splitting) keeps stdin
    // alive across reconnects; closing it would cause aoe-agent to
    // `process.exit(0)`. The fanout also answers agent calls on the daemon's
    // behalf during capacity refusal and disconnect cleanup.
    let agent_stdin = Arc::new(Mutex::new(agent_stdin));

    // Fan-out task: reads agent stdout, classifies each line, and appends
    // the daemon-facing frame to the control queue. Single owner of the read
    // half of the agent's stdout pipe.
    let agent_stdout_task = tokio::spawn(fanout_agent_stdout(
        agent_stdout,
        Arc::clone(&shared),
        Arc::clone(&agent_stdin),
        args.session_id.clone(),
    ));

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
    let accept_stdin = Arc::clone(&agent_stdin);
    // The control-channel accept loop owns attach and detach bookkeeping:
    // `mark_attached` / `mark_detached` on the registry record, and the
    // `detached_since` clock the abandonment watchdog reads. Missing that
    // move would leave the runner permanently "detached" and have the 48h
    // retention watchdog eventually reap a session a daemon is actively
    // using.
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
        terminate_runner = accept_loop => {
            debug_assert!(terminate_runner);
            // The daemon owns path cleanup after cancelling an incomplete
            // handshake and may already have spawned a replacement. Never
            // unlink that replacement here.
            preserve_registry = true;
            let _ = agent_child.start_kill();
            let _ = agent_child.wait().await;
        }
    }

    watchdog_handle.abort();
    agent_stdout_task.abort();
    if !preserve_registry {
        worker_registry::delete_if_owned(&session_id, our_pid).ok();
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

/// State the control accept loop and the agent-stdout fanout share. One
/// daemon is attached at a time; the runner outlives all of them.
struct RunnerShared {
    /// JSON-RPC ids of daemon-issued prompts awaiting a response.
    prompt_requests: Mutex<HashSet<i64>>,
    /// The ordered runner-to-daemon queue and active attachment identity.
    control: Mutex<ControlChannel>,
    /// Wakes the writer when frames arrive or queue space is released.
    control_wake: tokio::sync::Notify,
    /// Wakes producers waiting for the byte-bounded queue to make space.
    control_space: tokio::sync::Notify,
    /// Monotonic identity for daemon attachments. Connection-bound frames and
    /// correlations are purged by this id on disconnect.
    next_attachment_id: AtomicU64,
    /// Runner-owned ACP handshake state, cached independently of any attach.
    handshake: Mutex<RunnerHandshake>,
    /// Serializes handshake round trips without blocking stdout fanout.
    handshake_gate: Mutex<()>,
    /// Sole JSON-RPC id allocator for requests sent to the agent.
    next_req_id: AtomicI64,
    /// Inline handshake request responders.
    pending_client_responses: Mutex<HashMap<i64, tokio::sync::oneshot::Sender<serde_json::Value>>>,
    /// Agent-to-daemon calls awaiting an answer from their originating attach.
    pending_server_calls: Mutex<HashMap<u64, PendingServerCall>>,
    /// Monotonic allocator for reverse-lane call ids.
    next_call_id: AtomicU64,
    /// Agent-bound requests issued for a specific daemon attachment.
    pending_agent_calls: Mutex<HashMap<i64, PendingAgentCall>>,
    /// Registry ownership used to commit session identity before success.
    registry_owner: Option<(String, u32)>,
    /// A durability failure makes further runner state unsafe to expose.
    fatal: AtomicBool,
    fatal_wake: tokio::sync::Notify,
}

/// A reverse-lane call awaiting a daemon answer.
struct PendingServerCall {
    agent_id: serde_json::Value,
    method: String,
    attachment_id: u64,
}

/// A forward-lane call awaiting the agent answer for one attachment.
struct PendingAgentCall {
    call_id: u64,
    attachment_id: u64,
    method: String,
}

/// Runner-owned ACP handshake state.
#[derive(Default)]
struct RunnerHandshake {
    /// Cached raw `initialize` result once the runner has run it.
    initialized: Option<serde_json::Value>,
    /// Cached `(acp_session_id, raw session response result)` once the
    /// session is established.
    session: Option<(String, serde_json::Value)>,
}

/// Whether a queued frame may survive the daemon attachment that produced it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryScope {
    Persistent,
    Attachment(u64),
}

/// Metadata needed for shedding and connection-scoped cleanup. The encoded
/// bytes are the sole payload copy retained by the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum QueuedKind {
    Notify,
    PromptCompleted,
    Handshake,
    ServerCall,
    AgentReply,
}

struct QueuedControl {
    id: u64,
    scope: DeliveryScope,
    kind: QueuedKind,
    wire: Arc<[u8]>,
}

#[derive(Default)]
struct ControlChannel {
    queue: VecDeque<QueuedControl>,
    queued_bytes: usize,
    active_attachment: Option<u64>,
    next_entry_id: u64,
    in_flight: Option<u64>,
    /// Last flushed prompt completion, replayed until a newer prompt starts.
    last_prompt_completion: Option<Arc<[u8]>>,
}

/// Bound both queue dimensions. One maximum-sized ACP frame must fit, while a
/// detached runner must never retain an unbounded number of large JSON values.
const MAX_CONTROL_QUEUE: usize = 4096;
const MAX_CONTROL_QUEUE_BYTES: usize = 128 * 1024 * 1024;

impl ControlChannel {
    fn remove_at(&mut self, index: usize) -> QueuedControl {
        let frame = self.queue.remove(index).expect("queue index exists");
        self.queued_bytes = self.queued_bytes.saturating_sub(frame.wire.len());
        frame
    }

    fn make_room(&mut self, incoming_bytes: usize) -> bool {
        while self.queue.len() >= MAX_CONTROL_QUEUE
            || self.queued_bytes.saturating_add(incoming_bytes) > MAX_CONTROL_QUEUE_BYTES
        {
            let Some(index) = self.queue.iter().position(|frame| {
                frame.kind == QueuedKind::Notify && self.in_flight != Some(frame.id)
            }) else {
                return false;
            };
            self.remove_at(index);
        }
        true
    }

    fn push(&mut self, scope: DeliveryScope, kind: QueuedKind, wire: Arc<[u8]>) {
        let id = self.next_entry_id;
        self.next_entry_id = self.next_entry_id.wrapping_add(1);
        self.queued_bytes += wire.len();
        self.queue.push_back(QueuedControl {
            id,
            scope,
            kind,
            wire,
        });
    }

    fn purge_attachment(&mut self, attachment_id: u64) {
        let mut index = 0;
        while index < self.queue.len() {
            if self.queue[index].scope == DeliveryScope::Attachment(attachment_id) {
                self.remove_at(index);
            } else {
                index += 1;
            }
        }
        if self.active_attachment == Some(attachment_id) {
            self.active_attachment = None;
        }
        self.in_flight = None;
    }
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
/// (see `disconnect_control`), so no request parks; only this
/// one needs a typed result because its `cancelled` outcome is a normal,
/// non-error control-flow signal the agent expects.
const PERMISSION_METHOD: &str = "session/request_permission";

fn disconnected_server_call_outcome(
    method: &str,
) -> Result<serde_json::Value, control_protocol::JsonRpcError> {
    if method == PERMISSION_METHOD {
        Ok(serde_json::json!({ "outcome": { "outcome": "cancelled" } }))
    } else {
        Err(control_protocol::JsonRpcError::new(
            control_protocol::DAEMON_GONE,
            "daemon disconnected; request cancelled",
        ))
    }
}

/// The daemon-issued request whose response marks a turn complete. The
/// runner tracks its id (seen on the daemon to agent path) and surfaces
/// a native `PromptCompleted` when the matching response comes back on
/// the agent to daemon path.
const PROMPT_METHOD: &str = "session/prompt";

/// Seed for the runner's own agent-bound JSON-RPC ids. Since #2977 retired
/// the relay, the runner allocates EVERY id on the agent's stdin (handshake,
/// turn, and the forward-lane methods the daemon requests), so this is the
/// only id space there and collisions are impossible by construction. The
/// high seed is a leftover guard from when the daemon also put ids on that
/// wire; harmless to keep, and it makes runner-issued ids obvious in a log.
const RUNNER_REQUEST_ID_BASE: i64 = 1 << 48;

/// Deadline for a single control-channel frame write. Since #2977 the write
/// happens on a dedicated writer task rather than under the queue lock, so a
/// stalled peer no longer freezes the session; the cap is what stops that
/// writer parking forever on a peer that accepted the connection and then
/// stopped reading. A timeout is treated as a write failure, so the frame is
/// requeued for the next attach.
const CONTROL_WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Maximum time a peer may hold the sole accept slot without proving it
/// speaks the current control protocol.
const CONTROL_ATTACH_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Cap on reverse calls outstanding at once. Hit only if the daemon stops
/// answering (a healthy one always does), so it exists to stop a wedged or
/// misbehaving daemon growing `pending_server_calls` without bound across
/// reconnects.
///
/// At the cap the runner REFUSES the new request with an error rather than
/// evicting an existing entry. Evicting would drop the bookkeeping for a
/// request the agent is already parked on, so nothing would ever answer it;
/// refusing lets the agent's RPC layer resolve the id and move on.
const MAX_OUTSTANDING_REQUESTS: usize = 1024;

impl RunnerShared {
    fn new(registry_owner: Option<(String, u32)>) -> Self {
        Self {
            prompt_requests: Mutex::new(HashSet::new()),
            control: Mutex::new(ControlChannel::default()),
            control_wake: tokio::sync::Notify::new(),
            control_space: tokio::sync::Notify::new(),
            next_attachment_id: AtomicU64::new(1),
            handshake: Mutex::new(RunnerHandshake::default()),
            handshake_gate: Mutex::new(()),
            next_req_id: AtomicI64::new(RUNNER_REQUEST_ID_BASE),
            pending_client_responses: Mutex::new(HashMap::new()),
            pending_server_calls: Mutex::new(HashMap::new()),
            next_call_id: AtomicU64::new(1),
            pending_agent_calls: Mutex::new(HashMap::new()),
            registry_owner,
            fatal: AtomicBool::new(false),
            fatal_wake: tokio::sync::Notify::new(),
        }
    }

    fn persist_acp_session_id(
        &self,
        acp_session_id: &str,
    ) -> std::result::Result<(), control_protocol::JsonRpcError> {
        let Some((session_id, owner_pid)) = self.registry_owner.as_ref() else {
            return Ok(());
        };
        if let Err(error) =
            worker_registry::update_stored_acp_session_id(session_id, *owner_pid, acp_session_id)
        {
            warn!(
                target: "acp.runner",
                session = %session_id,
                %error,
                "failed to persist ACP session identity; terminating runner"
            );
            self.fatal.store(true, Ordering::Release);
            self.fatal_wake.notify_one();
            return Err(control_protocol::JsonRpcError::new(
                control_protocol::INTERNAL_ERROR,
                format!("failed to persist ACP session identity: {error}"),
            ));
        }
        Ok(())
    }

    async fn fatal_triggered(&self) {
        loop {
            let notified = self.fatal_wake.notified();
            if self.fatal.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
    /// Classify one agent stdout line and enqueue whatever the daemon needs.
    async fn deliver_line(&self, line: &[u8], agent_stdin: &Mutex<tokio::process::ChildStdin>) {
        if let Some(id) = parse_response_id(line) {
            let responder = self.pending_client_responses.lock().await.remove(&id);
            if let Some(tx) = responder {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) {
                    let _ = tx.send(value);
                }
                return;
            }
        }

        if !self.prompt_requests.lock().await.is_empty() {
            if let Some((id, outcome)) = parse_response(line) {
                if self.prompt_requests.lock().await.remove(&id) {
                    self.enqueue(
                        DeliveryScope::Persistent,
                        QueuedKind::PromptCompleted,
                        ControlBody::PromptCompleted {
                            prompt_req_id: id,
                            outcome,
                        },
                    )
                    .await;
                    return;
                }
            }
        }

        if let Some(id) = parse_response_id(line) {
            let pending = self.pending_agent_calls.lock().await.remove(&id);
            if let Some(pending) = pending {
                let frame = match parse_agent_call_outcome(line) {
                    Ok(result) => match self
                        .refresh_session_from_reset(&pending.method, &result)
                        .await
                    {
                        Ok(()) => ControlBody::AgentResult {
                            call_id: pending.call_id,
                            result,
                        },
                        Err(error) => ControlBody::AgentError {
                            call_id: pending.call_id,
                            error,
                        },
                    },
                    Err(error) => ControlBody::AgentError {
                        call_id: pending.call_id,
                        error,
                    },
                };
                self.enqueue(
                    DeliveryScope::Attachment(pending.attachment_id),
                    QueuedKind::AgentReply,
                    frame,
                )
                .await;
                return;
            }
        }

        if let Some((agent_id, method)) = parse_request_value_id(line) {
            self.forward_server_call(agent_id, method, line, agent_stdin)
                .await;
            return;
        }

        if let Some((method, params)) = parse_notification(line) {
            self.enqueue(
                DeliveryScope::Persistent,
                QueuedKind::Notify,
                ControlBody::Notify { method, params },
            )
            .await;
        }
    }

    /// Forward an agent-issued request only to the daemon attachment that was
    /// active when it arrived. Capacity refusal is answered at the agent.
    async fn forward_server_call(
        &self,
        agent_id: serde_json::Value,
        method: String,
        line: &[u8],
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
    ) {
        let params = serde_json::from_slice::<serde_json::Value>(line)
            .ok()
            .and_then(|mut v| v.get_mut("params").map(std::mem::take))
            .unwrap_or(serde_json::Value::Null);
        let call_id = self.next_call_id.fetch_add(1, Ordering::Relaxed);
        let body = ControlBody::ServerCall {
            call_id,
            method: method.clone(),
            params,
        };
        let wire: Arc<[u8]> = match control_protocol::encode_frame(&body) {
            Ok(frame) => frame.into(),
            Err(error) => {
                warn!(target: "acp.runner", %error, "reverse call exceeds control frame limit");
                self.answer_agent(
                    agent_stdin,
                    &agent_id,
                    Err(control_protocol::JsonRpcError::new(
                        control_protocol::DAEMON_GONE,
                        "runner reverse call exceeds transport capacity",
                    )),
                )
                .await;
                return;
            }
        };

        let mut channel = self.control.lock().await;
        let Some(attachment_id) = channel.active_attachment else {
            drop(channel);
            self.answer_agent(
                agent_stdin,
                &agent_id,
                disconnected_server_call_outcome(&method),
            )
            .await;
            return;
        };
        let mut pending = self.pending_server_calls.lock().await;
        if pending.len() >= MAX_OUTSTANDING_REQUESTS || !channel.make_room(wire.len()) {
            warn!(
                target: "acp.runner",
                method = %method,
                outstanding = pending.len(),
                "reverse-call capacity reached; refusing the request"
            );
            drop(pending);
            drop(channel);
            self.answer_agent(
                agent_stdin,
                &agent_id,
                Err(control_protocol::JsonRpcError::new(
                    control_protocol::DAEMON_GONE,
                    "runner reverse-call capacity exceeded",
                )),
            )
            .await;
            return;
        }

        pending.insert(
            call_id,
            PendingServerCall {
                agent_id,
                method,
                attachment_id,
            },
        );
        channel.push(
            DeliveryScope::Attachment(attachment_id),
            QueuedKind::ServerCall,
            wire,
        );
        drop(pending);
        drop(channel);
        self.control_wake.notify_one();
    }

    async fn answer_agent(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        agent_id: &serde_json::Value,
        outcome: Result<serde_json::Value, control_protocol::JsonRpcError>,
    ) -> bool {
        let response = match outcome {
            Ok(result) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": agent_id,
                "result": result,
            }),
            Err(error) => serde_json::json!({
                "jsonrpc": "2.0",
                "id": agent_id,
                "error": error,
            }),
        };
        self.write_agent_line(agent_stdin, &response).await
    }

    async fn resolve_server_call(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        call_id: u64,
        outcome: Result<serde_json::Value, control_protocol::JsonRpcError>,
        session_id: &str,
    ) {
        let pending = {
            let mut pending = self.pending_server_calls.lock().await;
            match pending.get(&call_id) {
                Some(call) if call.attachment_id == attachment_id => pending.remove(&call_id),
                _ => None,
            }
        };
        let Some(pending) = pending else {
            debug!(
                target: "acp.runner",
                session = %session_id,
                call_id,
                "ignoring answer for an unknown or detached reverse call"
            );
            return;
        };
        if !self
            .answer_agent(agent_stdin, &pending.agent_id, outcome)
            .await
        {
            warn!(
                target: "acp.runner",
                session = %session_id,
                call_id,
                method = %pending.method,
                "agent stdin write failed answering a reverse call"
            );
        }
    }

    /// Purge every correlation and queued frame owned by one dead attachment.
    async fn disconnect_control(
        &self,
        attachment_id: u64,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        session_id: &str,
    ) {
        let drained = {
            let mut channel = self.control.lock().await;
            let mut pending = self.pending_server_calls.lock().await;
            let ids: Vec<u64> = pending
                .iter()
                .filter_map(|(id, call)| (call.attachment_id == attachment_id).then_some(*id))
                .collect();
            let drained: Vec<_> = ids
                .into_iter()
                .filter_map(|id| pending.remove(&id).map(|call| (id, call)))
                .collect();
            self.pending_agent_calls
                .lock()
                .await
                .retain(|_, call| call.attachment_id != attachment_id);
            channel.purge_attachment(attachment_id);
            drained
        };
        self.control_space.notify_waiters();
        if !drained.is_empty() {
            info!(
                target: "acp.runner",
                session = %session_id,
                count = drained.len(),
                "synthesising responses for reverse calls on control disconnect"
            );
        }
        for (_, pending) in drained {
            if !self
                .answer_agent(
                    agent_stdin,
                    &pending.agent_id,
                    disconnected_server_call_outcome(&pending.method),
                )
                .await
            {
                warn!(target: "acp.runner", session = %session_id, "agent stdin write failed during disconnect cleanup");
                break;
            }
        }
    }

    async fn abort_agent_calls(&self, session_id: &str) {
        let drained: Vec<PendingAgentCall> = {
            let mut map = self.pending_agent_calls.lock().await;
            map.drain().map(|(_, pending)| pending).collect()
        };
        if drained.is_empty() {
            return;
        }
        info!(
            target: "acp.runner",
            session = %session_id,
            count = drained.len(),
            "failing in-flight forward calls; agent is gone"
        );
        for pending in drained {
            self.enqueue(
                DeliveryScope::Attachment(pending.attachment_id),
                QueuedKind::AgentReply,
                ControlBody::AgentError {
                    call_id: pending.call_id,
                    error: control_protocol::JsonRpcError::new(
                        control_protocol::DAEMON_GONE,
                        "agent exited before answering",
                    ),
                },
            )
            .await;
        }
    }

    /// Encode once and admit by exact wire bytes. Notifications may be shed;
    /// correlation and completion frames apply backpressure instead.
    async fn enqueue(&self, scope: DeliveryScope, kind: QueuedKind, body: ControlBody) -> bool {
        let wire: Arc<[u8]> = match control_protocol::encode_frame(&body) {
            Ok(frame) => frame.into(),
            Err(error) => {
                warn!(target: "acp.runner", %error, "dropping unrepresentable control frame");
                return false;
            }
        };
        loop {
            let space = self.control_space.notified();
            {
                let mut channel = self.control.lock().await;
                if let DeliveryScope::Attachment(attachment_id) = scope {
                    if channel.active_attachment != Some(attachment_id) {
                        return false;
                    }
                }
                if channel.make_room(wire.len()) {
                    channel.push(scope, kind, Arc::clone(&wire));
                    drop(channel);
                    self.control_wake.notify_one();
                    return true;
                }
                if kind == QueuedKind::Notify {
                    return false;
                }
            }
            space.await;
        }
    }

    async fn begin_attachment(&self) -> u64 {
        let attachment_id = self.next_attachment_id.fetch_add(1, Ordering::Relaxed);
        self.control.lock().await.active_attachment = Some(attachment_id);
        self.control_wake.notify_one();
        attachment_id
    }
    async fn cached_prompt_completion(&self) -> Option<Arc<[u8]>> {
        self.control.lock().await.last_prompt_completion.clone()
    }

    async fn clear_prompt_completion(&self) {
        self.control.lock().await.last_prompt_completion = None;
    }

    async fn install_control(
        &self,
        out: &mut Option<tokio::net::unix::OwnedWriteHalf>,
        session_id: &str,
    ) -> bool {
        let hello = ControlBody::Hello {
            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
            session_id: session_id.to_string(),
        };
        write_control_frame(out.as_mut().expect("write half present"), &hello).await
    }

    async fn next_outbound(&self, attachment_id: u64) -> Option<(u64, Arc<[u8]>)> {
        let mut channel = self.control.lock().await;
        if channel.active_attachment != Some(attachment_id) || channel.in_flight.is_some() {
            return None;
        }
        let (id, wire) = {
            let frame = channel.queue.front()?;
            (frame.id, Arc::clone(&frame.wire))
        };
        channel.in_flight = Some(id);
        Some((id, wire))
    }

    async fn commit_outbound(&self, attachment_id: u64, entry_id: u64) {
        let mut channel = self.control.lock().await;
        if channel.active_attachment == Some(attachment_id)
            && channel.in_flight == Some(entry_id)
            && channel.queue.front().map(|frame| frame.id) == Some(entry_id)
        {
            if let Some(wire) = channel.queue.front().and_then(|frame| {
                (frame.kind == QueuedKind::PromptCompleted).then(|| Arc::clone(&frame.wire))
            }) {
                channel.last_prompt_completion = Some(wire);
            }
            channel.remove_at(0);
        }
        if channel.in_flight == Some(entry_id) {
            channel.in_flight = None;
        }
        drop(channel);
        self.control_space.notify_waiters();
    }

    async fn release_outbound(&self, entry_id: u64) {
        let mut channel = self.control.lock().await;
        if channel.in_flight == Some(entry_id) {
            channel.in_flight = None;
        }
    }
    /// Serialize a JSON value as one ndjson line to the agent's stdin.
    /// Returns false if serialization or the write failed.
    async fn write_agent_line(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        value: &serde_json::Value,
    ) -> bool {
        let mut bytes = match serde_json::to_vec(value) {
            Ok(b) => b,
            Err(_) => return false,
        };
        bytes.push(b'\n');
        let mut stdin = agent_stdin.lock().await;
        stdin.write_all(&bytes).await.is_ok() && stdin.flush().await.is_ok()
    }

    /// Issue a runner-owned JSON-RPC request to the agent and await the
    /// full response line as JSON. Used for the handshake requests the
    /// runner now owns (`initialize`, `session/new|load|fork`). Returns
    /// None if the write failed or the agent closed before answering.
    async fn agent_request(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        method: &str,
        params: serde_json::Value,
    ) -> Option<serde_json::Value> {
        let id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.pending_client_responses.lock().await.insert(id, tx);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        if !self.write_agent_line(agent_stdin, &req).await {
            self.pending_client_responses.lock().await.remove(&id);
            return None;
        }
        rx.await.ok()
    }

    /// Issue a runner-owned `session/prompt` to the agent. The response is
    /// detected asynchronously by the stdout fanout (via `prompt_requests`),
    /// which fires `PromptCompleted`. Returns the assigned request id, or
    /// None if the write failed.
    async fn agent_prompt(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        params: serde_json::Value,
    ) -> Option<i64> {
        self.clear_prompt_completion().await;
        let id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        self.prompt_requests.lock().await.insert(id);
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": PROMPT_METHOD,
            "params": params,
        });
        if !self.write_agent_line(agent_stdin, &req).await {
            self.prompt_requests.lock().await.remove(&id);
            return None;
        }
        Some(id)
    }

    /// Issue a `session/cancel` notification for the established session.
    async fn agent_cancel(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        acp_session_id: &str,
    ) {
        let note = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": acp_session_id },
        });
        let _ = self.write_agent_line(agent_stdin, &note).await;
    }

    /// Run (once) or replay (from cache) the ACP `initialize` the runner
    /// now owns. On first call it sends `initialize` to the agent and
    /// caches the raw result; later calls return the cache without touching
    /// the agent. `Ok` carries the result to hand back as
    /// `ControlBody::Initialized`; `Err` is the raw JSON-RPC error object
    /// for `ControlBody::HandshakeFailed`.
    async fn run_or_replay_initialize(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        request: serde_json::Value,
    ) -> Result<serde_json::Value, serde_json::Value> {
        let _gate = self.handshake_gate.lock().await;
        if let Some(cached) = self.handshake.lock().await.initialized.clone() {
            return Ok(cached);
        }
        let response = self
            .agent_request(agent_stdin, "initialize", request)
            .await
            .ok_or_else(|| transport_error("agent closed before answering initialize"))?;
        let result = handshake_result(&response)?;
        self.handshake.lock().await.initialized = Some(result.clone());
        Ok(result)
    }

    /// Run (once) or replay (from cache) the session-establishment request the
    /// runner now owns. `method` is `session/new|load|fork`. Caches
    /// `(acp_session_id, raw result)`; later calls replay the cache. `Ok`
    /// carries `(acp_session_id, result)`; `Err` is the raw JSON-RPC error
    /// object for `ControlBody::HandshakeFailed`.
    async fn run_or_replay_session(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        method: &str,
        request: serde_json::Value,
    ) -> Result<(String, serde_json::Value), serde_json::Value> {
        let _gate = self.handshake_gate.lock().await;
        if let Some(cached) = self.handshake.lock().await.session.clone() {
            return Ok(cached);
        }
        let response = self
            .agent_request(agent_stdin, method, request.clone())
            .await
            .ok_or_else(|| transport_error(&format!("agent closed before answering {method}")))?;
        let result = handshake_result(&response)?;
        let acp_session_id = established_session_id(method, &request, &result)?;
        self.persist_acp_session_id(&acp_session_id)
            .map_err(|error| serde_json::to_value(error).expect("JSON-RPC error serializes"))?;
        let cached = (acp_session_id, result);
        self.handshake.lock().await.session = Some(cached.clone());
        Ok(cached)
    }

    /// The ACP session id the runner established, if any.
    async fn acp_session_id(&self) -> Option<String> {
        self.handshake
            .lock()
            .await
            .session
            .as_ref()
            .map(|(id, _)| id.clone())
    }
    /// Issue a forward-lane request at the agent on one daemon attachment's
    /// behalf. Responses from a detached caller are discarded, never replayed
    /// into a later daemon whose correlation table cannot contain the call.
    async fn issue_agent_call(
        &self,
        agent_stdin: &Mutex<tokio::process::ChildStdin>,
        attachment_id: u64,
        call_id: u64,
        method: &str,
        params: serde_json::Value,
    ) {
        let req_id = self.next_req_id.fetch_add(1, Ordering::Relaxed);
        self.pending_agent_calls.lock().await.insert(
            req_id,
            PendingAgentCall {
                call_id,
                attachment_id,
                method: method.to_string(),
            },
        );
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": method,
            "params": params,
        });
        if !self.write_agent_line(agent_stdin, &request).await {
            self.pending_agent_calls.lock().await.remove(&req_id);
            self.enqueue(
                DeliveryScope::Attachment(attachment_id),
                QueuedKind::AgentReply,
                ControlBody::AgentError {
                    call_id,
                    error: control_protocol::JsonRpcError::new(
                        control_protocol::DAEMON_GONE,
                        format!("agent stdin write failed for {method}"),
                    ),
                },
            )
            .await;
        }
    }

    /// Refresh the cached handshake session when a forward-lane response
    /// carries a fresh `sessionId`, such as a conversation-reset `session/new`.
    /// Only a successful response reaches here, so a failed reset leaves the
    /// previous session cached and live.
    async fn refresh_session_from_reset(
        &self,
        method: &str,
        result: &serde_json::Value,
    ) -> std::result::Result<(), control_protocol::JsonRpcError> {
        if method != "session/new" {
            return Ok(());
        }
        let Some(sid) = result.get("sessionId").and_then(|value| value.as_str()) else {
            return Ok(());
        };
        if self
            .handshake
            .lock()
            .await
            .session
            .as_ref()
            .is_some_and(|(current, _)| current == sid)
        {
            return Ok(());
        }
        self.persist_acp_session_id(sid)?;
        info!(
            target: "acp.runner",
            new_acp_session_id = %sid,
            "daemon-driven session/new observed; refreshing handshake cache"
        );
        self.handshake.lock().await.session = Some((sid.to_string(), result.clone()));
        Ok(())
    }
}

/// Extract the `result` object from a runner-issued request's JSON-RPC
/// response. On an `error` envelope, returns the raw error object so the
/// daemon can reconstruct the crate error verbatim (preserving `data`); a
/// response with neither result nor error synthesizes a minimal error.
fn handshake_result(response: &serde_json::Value) -> Result<serde_json::Value, serde_json::Value> {
    if let Some(err) = response.get("error") {
        return Err(err.clone());
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| transport_error("response had neither result nor error"))
}

/// Resolve the session identity established by a successful session request.
/// New and fork create an identity, so their response must provide it. Load
/// reopens the identity named by the request; ACP's `LoadSessionResponse` does
/// not contain a session id. Some agents include one as an extension, which is
/// accepted only when it agrees with the requested identity.
fn established_session_id(
    method: &str,
    request: &serde_json::Value,
    result: &serde_json::Value,
) -> Result<String, serde_json::Value> {
    match method {
        "session/load" => {
            let requested = request
                .get("sessionId")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| transport_error("session/load request missing sessionId"))?;
            let result = result
                .as_object()
                .ok_or_else(|| transport_error("session/load response result was not an object"))?;
            if let Some(returned) = result.get("sessionId") {
                let returned = returned.as_str().ok_or_else(|| {
                    transport_error("session/load response sessionId was not a string")
                })?;
                if returned != requested {
                    return Err(transport_error(
                        "session/load response sessionId did not match request",
                    ));
                }
            }
            Ok(requested.to_string())
        }
        "session/new" | "session/fork" => result
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| transport_error(&format!("{method} response missing sessionId")))
            .map(str::to_string),
        _ => Err(transport_error(&format!(
            "unsupported session establishment method {method}"
        ))),
    }
}

/// A synthetic JSON-RPC error object for a runner-side transport failure
/// (agent closed, malformed response), shaped like an agent error so the
/// daemon reconstructs it as a crate error uniformly.
fn transport_error(message: &str) -> serde_json::Value {
    serde_json::json!({ "code": -32603, "message": message })
}

/// Extract `(id, method)` from a JSON-RPC request line, yielding the id as
/// the JSON value it actually is. The reverse lane has to echo the agent's
/// own id back verbatim in the response envelope, and adapters differ: a
/// numeric id must not come back as `"7"`, nor a string id as `7`. Returns
/// None for notifications (no id), responses (no method), and malformed
/// lines.
fn parse_request_value_id(line: &[u8]) -> Option<(serde_json::Value, String)> {
    let peek: JsonRpcPeek = serde_json::from_slice(line).ok()?;
    let id = peek.id?;
    let method = peek.method?;
    Some((id, method))
}

/// Parse an agent notification: a line with a `method` and no `id`. Yields
/// the method and its params so the runner can forward it opaquely.
fn parse_notification(line: &[u8]) -> Option<(String, serde_json::Value)> {
    let mut value: serde_json::Value = serde_json::from_slice(line).ok()?;
    if value.get("id").is_some_and(|id| !id.is_null()) {
        return None;
    }
    let method = value.get("method")?.as_str()?.to_string();
    let params = value
        .get_mut("params")
        .map(std::mem::take)
        .unwrap_or(serde_json::Value::Null);
    Some((method, params))
}

/// Split an agent response line into the forward-lane outcome: `Ok(result)`
/// for a success envelope, `Err(error)` for an error envelope. A response
/// carrying neither is treated as a null result, matching how the crate's
/// transport handles an empty-body ack (`session/set_mode` answers `{}`).
fn parse_agent_call_outcome(
    line: &[u8],
) -> Result<serde_json::Value, control_protocol::JsonRpcError> {
    let mut value: serde_json::Value = match serde_json::from_slice(line) {
        Ok(v) => v,
        Err(e) => {
            return Err(control_protocol::JsonRpcError::new(
                control_protocol::DAEMON_GONE,
                format!("agent response was not JSON: {e}"),
            ))
        }
    };
    if let Some(error) = value.get_mut("error").map(std::mem::take) {
        if !error.is_null() {
            return Err(serde_json::from_value(error.clone()).unwrap_or_else(|_| {
                control_protocol::JsonRpcError::new(
                    control_protocol::DAEMON_GONE,
                    format!("agent error envelope was malformed: {error}"),
                )
            }));
        }
    }
    Ok(value
        .get_mut("result")
        .map(std::mem::take)
        .unwrap_or(serde_json::Value::Null))
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
const MAX_FRAME_BYTES: usize = control_protocol::MAX_AGENT_FRAME_BYTES;

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
/// the `result.stopReason` when the response succeeded, or the `error`
/// envelope when it failed.
#[derive(Deserialize)]
struct JsonRpcResponsePeek {
    #[serde(default)]
    id: Option<serde_json::Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcErrorPeek>,
}

/// The JSON-RPC `error` object on a failed response.
#[derive(Deserialize)]
struct JsonRpcErrorPeek {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    message: String,
    #[serde(default)]
    data: Option<serde_json::Value>,
}

/// Parse a JSON-RPC response line into `(id, outcome)`. Returns None for
/// requests (a `method` is present), notifications (no `id`), non-numeric
/// ids, and malformed lines. An error-envelope response is surfaced as
/// `PromptOutcome::Error` so the daemon can report it rather than
/// collapse it into a silent stop; a success response carries the ACP
/// `stopReason` when present.
fn parse_response(line: &[u8]) -> Option<(i64, PromptOutcome)> {
    let peek: JsonRpcResponsePeek = serde_json::from_slice(line).ok()?;
    if peek.method.is_some() {
        return None;
    }
    let id = peek.id?.as_i64()?;
    let outcome = if let Some(err) = peek.error {
        PromptOutcome::Error {
            code: err.code,
            message: err.message,
            data: err.data,
        }
    } else {
        let stop_reason = peek
            .result
            .as_ref()
            .and_then(|r| r.get("stopReason"))
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        PromptOutcome::Completed { stop_reason }
    };
    Some((id, outcome))
}

/// Read agent stdout line-by-line (ndjson) and either forward to the
/// daemon or buffer.
async fn fanout_agent_stdout(
    stdout: tokio::process::ChildStdout,
    shared: Arc<RunnerShared>,
    agent_stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    session_id: String,
) {
    let mut reader = BufReader::with_capacity(STDOUT_READ_BUF, stdout);
    let mut line = Vec::with_capacity(4096);
    loop {
        // read_frame_bounded preserves the trailing newline, which ndjson
        // consumers need, and caps frame size.
        match read_frame_bounded(&mut reader, &mut line).await {
            Ok(0) => {
                debug!(target: "acp.runner", session = %session_id, "agent stdout EOF");
                break;
            }
            Ok(_) => {
                shared.deliver_line(&line, &agent_stdin).await;
            }
            Err(e) => {
                warn!(target: "acp.runner", session = %session_id, "stdout read error: {e}");
                break;
            }
        }
    }
    // The agent is gone. Fail every forward call the daemon is awaiting, so
    // it surfaces an error rather than parking on a response that will never
    // come.
    shared.abort_agent_calls(&session_id).await;
}

/// Drain the outbound queue to one daemon attachment. Queue ownership remains
/// with the runner until a full socket write and flush succeeds. A disconnect
/// before that point leaves the exact front entry available to the next attach.
async fn run_control_writer(
    mut out: tokio::net::unix::OwnedWriteHalf,
    shared: Arc<RunnerShared>,
    session_id: String,
    attachment_id: u64,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        while let Some((entry_id, wire)) = shared.next_outbound(attachment_id).await {
            let written = tokio::select! {
                biased;
                _ = shutdown.changed() => {
                    shared.release_outbound(entry_id).await;
                    return;
                }
                result = tokio::time::timeout(
                    CONTROL_WRITE_TIMEOUT,
                    control_protocol::write_encoded_frame(&mut out, &wire),
                ) => matches!(result, Ok(Ok(()))),
            };
            if !written {
                warn!(
                    target: "acp.runner",
                    session = %session_id,
                    "control write failed or timed out; retaining frame for the next attach"
                );
                shared.release_outbound(entry_id).await;
                return;
            }
            shared.commit_outbound(attachment_id, entry_id).await;
        }
        tokio::select! {
            _ = shared.control_wake.notified() => {}
            _ = shutdown.changed() => return,
        }
    }
}

enum HandshakeCommand {
    Initialize(serde_json::Value),
    EstablishSession {
        method: String,
        request: serde_json::Value,
    },
}

async fn await_handshake_or_control_loss<T>(
    control_closed: &mut watch::Receiver<bool>,
    handshake: impl std::future::Future<Output = T>,
) -> Option<T> {
    if *control_closed.borrow() {
        return None;
    }
    tokio::pin!(handshake);
    tokio::select! {
        biased;
        _ = control_closed.changed() => None,
        result = &mut handshake => Some(result),
    }
}

/// Handle one v3 control-channel attachment. Reads directly from the socket,
/// which applies transport backpressure instead of treating a valid burst as a
/// protocol failure. The writer is supervised in every socket-read wait, so a
/// stalled peer releases the serial accept slot when its write deadline fires.
async fn handle_control_connection(
    stream: UnixStream,
    shared: Arc<RunnerShared>,
    agent_stdin: Arc<Mutex<tokio::process::ChildStdin>>,
    session_id: String,
) -> bool {
    let (mut read_half, write_half) = stream.into_split();
    let mut write_half = Some(write_half);
    if !shared.install_control(&mut write_half, &session_id).await {
        return false;
    }

    let attach = tokio::time::timeout(
        CONTROL_ATTACH_TIMEOUT,
        control_protocol::read_frame(&mut read_half),
    )
    .await;
    match attach {
        Ok(Ok(Some(ControlBody::Attach {
            control_protocol_version,
        }))) if control_protocol_version == control_protocol::CONTROL_PROTOCOL_VERSION => {}
        Ok(Ok(Some(ControlBody::Attach {
            control_protocol_version,
        }))) => {
            warn!(
                target: "acp.runner",
                session = %session_id,
                daemon_version = control_protocol_version,
                runner_version = control_protocol::CONTROL_PROTOCOL_VERSION,
                "daemon attached with a mismatched control version; refusing the connection"
            );
            return false;
        }
        Ok(Ok(Some(frame))) => {
            warn!(target: "acp.runner", session = %session_id, ?frame, "first daemon frame was not Attach");
            return false;
        }
        Ok(Ok(None)) => return false,
        Ok(Err(error)) => {
            warn!(target: "acp.runner", session = %session_id, "failed to read Attach: {error}");
            return false;
        }
        Err(_) => {
            warn!(target: "acp.runner", session = %session_id, "timed out waiting for Attach");
            return false;
        }
    }

    if let Some(wire) = shared.cached_prompt_completion().await {
        let replayed = matches!(
            tokio::time::timeout(
                CONTROL_WRITE_TIMEOUT,
                control_protocol::write_encoded_frame(
                    write_half.as_mut().expect("write half present"),
                    &wire,
                ),
            )
            .await,
            Ok(Ok(()))
        );
        if !replayed {
            return false;
        }
    }

    let attachment_id = shared.begin_attachment().await;
    let (writer_shutdown_tx, writer_shutdown_rx) = watch::channel(false);
    let mut writer = tokio::spawn(run_control_writer(
        write_half
            .take()
            .expect("write half present after greeting"),
        Arc::clone(&shared),
        session_id.clone(),
        attachment_id,
        writer_shutdown_rx,
    ));
    let mut writer_finished = false;

    let (control_closed_tx, control_closed_rx) = watch::channel(false);
    let handshake_complete = Arc::new(std::sync::atomic::AtomicBool::new(
        shared.acp_session_id().await.is_some(),
    ));
    let (handshake_tx, mut handshake_rx) = mpsc::channel(8);
    let handshake_shared = Arc::clone(&shared);
    let handshake_stdin = Arc::clone(&agent_stdin);
    let handshake_done = Arc::clone(&handshake_complete);
    let handshake_session = session_id.clone();
    let handshake_worker = tokio::spawn(async move {
        let mut control_closed = control_closed_rx;
        while let Some(command) = handshake_rx.recv().await {
            let frame = match command {
                HandshakeCommand::Initialize(request) => {
                    let Some(result) = await_handshake_or_control_loss(
                        &mut control_closed,
                        handshake_shared.run_or_replay_initialize(&handshake_stdin, request),
                    )
                    .await
                    else {
                        return;
                    };
                    match result {
                        Ok(result) => ControlBody::Initialized { result },
                        Err(error) => {
                            warn!(target: "acp.runner", session = %handshake_session, "initialize failed: {error}");
                            ControlBody::HandshakeFailed { error }
                        }
                    }
                }
                HandshakeCommand::EstablishSession { method, request } => {
                    let Some(result) = await_handshake_or_control_loss(
                        &mut control_closed,
                        handshake_shared.run_or_replay_session(&handshake_stdin, &method, request),
                    )
                    .await
                    else {
                        return;
                    };
                    match result {
                        Ok((acp_session_id, result)) => {
                            handshake_done.store(true, Ordering::Release);
                            ControlBody::SessionReady {
                                acp_session_id,
                                result,
                            }
                        }
                        Err(error) => {
                            warn!(target: "acp.runner", session = %handshake_session, "{method} failed: {error}");
                            ControlBody::HandshakeFailed { error }
                        }
                    }
                }
            };
            handshake_shared
                .enqueue(
                    DeliveryScope::Attachment(attachment_id),
                    QueuedKind::Handshake,
                    frame,
                )
                .await;
        }
    });

    let terminate_runner = 'connection: loop {
        let body = tokio::select! {
            result = &mut writer => {
                writer_finished = true;
                if let Err(error) = result {
                    warn!(target: "acp.runner", session = %session_id, "control writer task failed: {error}");
                }
                break 'connection false;
            }
            result = control_protocol::read_frame(&mut read_half) => match result {
                Ok(Some(frame)) => frame,
                Ok(None) => break 'connection !handshake_complete.load(Ordering::Acquire),
                Err(error) => {
                    warn!(target: "acp.runner", session = %session_id, "control read error: {error}");
                    break 'connection !handshake_complete.load(Ordering::Acquire);
                }
            }
        };
        match body {
            ControlBody::Initialize { request } => {
                let send = handshake_tx.send(HandshakeCommand::Initialize(request));
                tokio::pin!(send);
                let sent = tokio::select! {
                    result = &mut writer => {
                        writer_finished = true;
                        if let Err(error) = result {
                            warn!(target: "acp.runner", session = %session_id, "control writer task failed: {error}");
                        }
                        false
                    }
                    result = &mut send => result.is_ok(),
                };
                if !sent {
                    break 'connection false;
                }
            }
            ControlBody::EstablishSession { method, request } => {
                let send =
                    handshake_tx.send(HandshakeCommand::EstablishSession { method, request });
                tokio::pin!(send);
                let sent = tokio::select! {
                    result = &mut writer => {
                        writer_finished = true;
                        if let Err(error) = result {
                            warn!(target: "acp.runner", session = %session_id, "control writer task failed: {error}");
                        }
                        false
                    }
                    result = &mut send => result.is_ok(),
                };
                if !sent {
                    break 'connection false;
                }
            }
            ControlBody::Prompt { request } => {
                if shared.agent_prompt(&agent_stdin, request).await.is_none() {
                    warn!(target: "acp.runner", session = %session_id, "prompt write to agent failed");
                }
            }
            ControlBody::Cancel => {
                if let Some(acp_session_id) = shared.acp_session_id().await {
                    shared.agent_cancel(&agent_stdin, &acp_session_id).await;
                }
            }
            ControlBody::ServerResult { call_id, result } => {
                shared
                    .resolve_server_call(
                        &agent_stdin,
                        attachment_id,
                        call_id,
                        Ok(result),
                        &session_id,
                    )
                    .await;
            }
            ControlBody::ServerError { call_id, error } => {
                shared
                    .resolve_server_call(
                        &agent_stdin,
                        attachment_id,
                        call_id,
                        Err(error),
                        &session_id,
                    )
                    .await;
            }
            ControlBody::AgentCall {
                call_id,
                method,
                params,
            } => {
                shared
                    .issue_agent_call(&agent_stdin, attachment_id, call_id, &method, params)
                    .await;
            }
            _ => {}
        }
    };

    let _ = control_closed_tx.send(true);
    if !writer_finished {
        let _ = writer_shutdown_tx.send(true);
        let _ = writer.await;
    }
    shared
        .disconnect_control(attachment_id, &agent_stdin, &session_id)
        .await;
    drop(handshake_tx);
    let _ = handshake_worker.await;
    terminate_runner
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
    // The rest of the env is inherited from the launching daemon, which has
    // already applied `env_clear` + the shared allowlist (see
    // `apply_env_filter` in acp_client/spawn.rs), so no second filter pass is
    // needed here.
    //
    // The one exception is the daemon's configured host environment
    // (`Config.environment`): HOME, PATH, and XDG_CONFIG_HOME are legal
    // entries there, and applying them to THIS process would move the
    // worker-registry path it writes or change which binary it loads. The
    // daemon therefore hands them over in a reserved carrier key holding
    // JSON `[[key, value], ...]`, which we strip from the child and apply
    // as the adapter's own environment. Applied after nothing else touches
    // `cmd`'s env, so a configured key outranks the inherited value —
    // matching the in-process spawn path and the terminal-view prefix.
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

    /// A `RunnerShared` plus a real agent stdin to write into.
    ///
    /// `cat` is the stand-in agent: the runner only ever writes to stdin
    /// here, and piping it back out on stdout gives the test a way to read
    /// exactly what was written without a fake ACP agent. The child is
    /// returned so the caller keeps it alive and can drain it.
    async fn shared_with_stdin() -> (
        Arc<RunnerShared>,
        Mutex<tokio::process::ChildStdin>,
        tokio::process::Child,
    ) {
        let mut child = Command::new("cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn cat");
        let stdin = child.stdin.take().expect("stdin piped");
        (Arc::new(RunnerShared::new(None)), Mutex::new(stdin), child)
    }

    /// Everything currently queued for the daemon, oldest first.
    async fn queued(shared: &RunnerShared) -> Vec<ControlBody> {
        let channel = shared.control.lock().await;
        channel
            .queue
            .iter()
            .map(|entry| {
                serde_json::from_slice(&entry.wire[4..])
                    .expect("queued control frame is valid JSON")
            })
            .collect()
    }

    /// Read back whatever the runner wrote to the agent's stdin.
    ///
    /// Reads until the echo goes quiet rather than until EOF: the caller
    /// still holds the stdin half (some tests write again afterwards), so
    /// `cat` never sees EOF and `read_to_end` would block forever. The writes
    /// under test are flushed before this is called, so one quiet window is
    /// enough to have seen all of them.
    async fn read_agent_stdin(child: &mut tokio::process::Child) -> String {
        use tokio::io::AsyncReadExt;
        let out = child.stdout.as_mut().expect("stdout piped");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        while let Ok(Ok(n)) =
            tokio::time::timeout(Duration::from_millis(150), out.read(&mut chunk)).await
        {
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        String::from_utf8(buf).expect("utf8")
    }

    #[test]
    fn parse_request_value_id_covers_id_shapes_and_non_requests() {
        let j = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        /// `(line, expected (id, method))`.
        type Case = (&'static [u8], Option<(serde_json::Value, String)>);
        let cases: Vec<Case> = vec![
            // Numeric id: the common shape.
            (
                br#"{"jsonrpc":"2.0","id":7,"method":"fs/read_text_file","params":{}}"#,
                Some((j("7"), "fs/read_text_file".into())),
            ),
            // A string id must survive as a string. The relay-era parser
            // keyed ids by their JSON rendering, which was fine as a map key
            // but would have echoed `"abc"` back to the agent as the literal
            // 5-character string including quotes.
            (
                br#"{"jsonrpc":"2.0","id":"abc","method":"terminal/create","params":{}}"#,
                Some((j(r#""abc""#), "terminal/create".into())),
            ),
            // Notification: no id.
            (
                br#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#,
                None,
            ),
            // Response: no method.
            (br#"{"jsonrpc":"2.0","id":7,"result":{}}"#, None),
            // Malformed.
            (b"not json", None),
        ];
        for (line, expected) in cases {
            assert_eq!(
                parse_request_value_id(line),
                expected,
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[test]
    fn established_session_id_is_method_sensitive() {
        let cases = [
            (
                "session/new",
                serde_json::json!({}),
                serde_json::json!({"sessionId": "new-id"}),
                Ok("new-id"),
            ),
            (
                "session/new",
                serde_json::json!({}),
                serde_json::json!({}),
                Err("session/new response missing sessionId"),
            ),
            (
                "session/fork",
                serde_json::json!({"sessionId": "parent-id"}),
                serde_json::json!({"sessionId": "fork-id"}),
                Ok("fork-id"),
            ),
            (
                "session/fork",
                serde_json::json!({"sessionId": "parent-id"}),
                serde_json::json!({}),
                Err("session/fork response missing sessionId"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!({"configOptions": []}),
                Ok("existing-id"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!({"sessionId": "existing-id"}),
                Ok("existing-id"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!({"sessionId": "different-id"}),
                Err("session/load response sessionId did not match request"),
            ),
            (
                "session/load",
                serde_json::json!({}),
                serde_json::json!({}),
                Err("session/load request missing sessionId"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!({"sessionId": 42}),
                Err("session/load response sessionId was not a string"),
            ),
            (
                "session/load",
                serde_json::json!({"sessionId": "existing-id"}),
                serde_json::json!(null),
                Err("session/load response result was not an object"),
            ),
            (
                "session/resume",
                serde_json::json!({}),
                serde_json::json!({}),
                Err("unsupported session establishment method session/resume"),
            ),
        ];

        for (method, request, result, expected) in cases {
            match (established_session_id(method, &request, &result), expected) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected, "{method}"),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual["message"], expected, "{method}")
                }
                (actual, expected) => panic!("{method}: got {actual:?}, expected {expected:?}"),
            }
        }
    }

    /// #2979, re-homed onto the forward lane: a conversation-reset
    /// `session/new` must refresh the cached handshake session so
    /// `ControlBody::Cancel` and later cache replays address the fresh
    /// conversation, not the pre-reset one. A response with no `sessionId`
    /// leaves the cache alone (the reset failed; the old session stays live).
    #[tokio::test]
    async fn reset_refresh_only_follows_a_response_carrying_a_session_id() {
        let shared = RunnerShared::new(None);
        shared.handshake.lock().await.session =
            Some(("sid-1".into(), serde_json::json!({ "sessionId": "sid-1" })));

        shared
            .refresh_session_from_reset("session/new", &serde_json::json!({"sessionId": "sid-2"}))
            .await
            .unwrap();
        assert_eq!(
            shared.acp_session_id().await.as_deref(),
            Some("sid-2"),
            "the cache follows the reset"
        );

        // An ack with no session id (set_mode, steering, a failed reset) must
        // not disturb the established session.
        shared
            .refresh_session_from_reset("session/set_mode", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(shared.acp_session_id().await.as_deref(), Some("sid-2"));
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
        assert_eq!(parse_request_value_id(b"not json"), None);
        assert_eq!(parse_response_id(b"not json"), None);
        assert_eq!(parse_response(b"not json"), None);
    }

    #[test]
    fn parse_response_extracts_stop_reason() {
        let line = br#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#;
        assert_eq!(
            parse_response(line),
            Some((
                3,
                PromptOutcome::Completed {
                    stop_reason: Some("end_turn".into())
                }
            ))
        );
    }

    #[test]
    fn parse_response_surfaces_error_envelope() {
        // An error response still ends the turn, now as a typed Error
        // outcome (preserving data) rather than a silent completion.
        let line = br#"{"jsonrpc":"2.0","id":4,"error":{"code":-32000,"message":"boom","data":{"errorKind":"rate_limit"}}}"#;
        assert_eq!(
            parse_response(line),
            Some((
                4,
                PromptOutcome::Error {
                    code: -32000,
                    message: "boom".into(),
                    data: Some(serde_json::json!({"errorKind": "rate_limit"})),
                }
            ))
        );
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

    /// The core turn-complete invariant, now end to end through the runner's
    /// own prompt path: `agent_prompt` allocates and tracks the id, and the
    /// matching agent response queues a `PromptCompleted`.
    #[tokio::test]
    async fn prompt_response_queues_completed_control_event() {
        let (shared, stdin, _child) = shared_with_stdin().await;
        let id = shared
            .agent_prompt(&stdin, serde_json::json!({"sessionId": "s"}))
            .await
            .expect("prompt written");
        assert!(shared.prompt_requests.lock().await.contains(&id));

        let resp = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"stopReason\":\"end_turn\"}}}}\n"
        );
        shared.deliver_line(resp.as_bytes(), &stdin).await;

        assert!(shared.prompt_requests.lock().await.is_empty());
        assert_eq!(
            queued(&shared).await,
            vec![ControlBody::PromptCompleted {
                prompt_req_id: id,
                outcome: PromptOutcome::Completed {
                    stop_reason: Some("end_turn".into()),
                },
            }]
        );
    }

    /// The ordering guarantee: notifications and
    /// the turn's completion share one FIFO queue in agent-stdout order, so
    /// a `Stopped` can never reach the daemon ahead of the chunks that
    /// preceded it.
    #[tokio::test]
    async fn notifications_stay_ahead_of_the_completion_that_followed_them() {
        let (shared, stdin, _child) = shared_with_stdin().await;
        let id = shared
            .agent_prompt(&stdin, serde_json::json!({"sessionId": "s"}))
            .await
            .expect("prompt written");

        for text in ["first", "second"] {
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{{\"text\":\"{text}\"}}}}\n"
            );
            shared.deliver_line(line.as_bytes(), &stdin).await;
        }
        let resp = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{{\"stopReason\":\"end_turn\"}}}}\n"
        );
        shared.deliver_line(resp.as_bytes(), &stdin).await;

        let q = queued(&shared).await;
        assert_eq!(q.len(), 3, "two notifies then the completion: {q:?}");
        assert!(matches!(q[0], ControlBody::Notify { .. }));
        assert!(matches!(q[1], ControlBody::Notify { .. }));
        assert!(matches!(q[2], ControlBody::PromptCompleted { .. }));
    }

    /// An agent-issued request becomes a `ServerCall` with a stable id, and
    /// the agent's own id is retained so the eventual answer can echo it.
    #[tokio::test]
    async fn agent_request_becomes_a_server_call() {
        let (shared, stdin, _child) = shared_with_stdin().await;
        let _attachment_id = shared.begin_attachment().await;
        let req = br#"{"jsonrpc":"2.0","id":"req-1","method":"fs/read_text_file","params":{"path":"/tmp/x"}}
"#;
        shared.deliver_line(req, &stdin).await;

        let q = queued(&shared).await;
        let ControlBody::ServerCall {
            call_id,
            method,
            params,
        } = &q[0]
        else {
            panic!("expected a ServerCall, got {q:?}");
        };
        assert_eq!(method, "fs/read_text_file");
        assert_eq!(params, &serde_json::json!({"path": "/tmp/x"}));
        let pending = shared.pending_server_calls.lock().await;
        let entry = pending.get(call_id).expect("call is tracked");
        assert_eq!(entry.agent_id, serde_json::json!("req-1"));
        assert_eq!(entry.method, "fs/read_text_file");
    }

    /// A reverse call still outstanding when the daemon drops must be
    /// answered toward the agent, never replayed: `session/request_permission`
    /// gets the semantic `cancelled` outcome and everything else a JSON-RPC
    /// error, so the agent's stdio loop cannot park on a daemon that is gone.
    /// This is the #1099 safety net, re-homed onto the control channel.
    #[tokio::test]
    async fn control_disconnect_cancels_reverse_calls_rather_than_replaying() {
        for (method, expect_cancelled) in [
            (PERMISSION_METHOD, true),
            ("fs/write_text_file", false),
            ("terminal/create", false),
        ] {
            let (shared, stdin, mut child) = shared_with_stdin().await;
            let attachment_id = shared.begin_attachment().await;
            let req = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":42,\"method\":\"{method}\",\"params\":{{}}}}\n"
            );
            shared.deliver_line(req.as_bytes(), &stdin).await;
            assert_eq!(shared.pending_server_calls.lock().await.len(), 1);

            shared
                .disconnect_control(attachment_id, &stdin, "s-1")
                .await;

            assert!(
                shared.pending_server_calls.lock().await.is_empty(),
                "{method}: the sweep drains the map"
            );
            assert!(
                !queued(&shared)
                    .await
                    .iter()
                    .any(|frame| matches!(frame, ControlBody::ServerCall { .. })),
                "{method}: the queued call must not survive to be replayed"
            );
            let assert_response = |sent: &serde_json::Value, id: i64| {
                assert_eq!(sent["id"], serde_json::json!(id), "{method}: id is echoed");
                if expect_cancelled {
                    assert_eq!(
                        sent["result"],
                        serde_json::json!({"outcome": {"outcome": "cancelled"}}),
                        "permission gets the semantic cancelled outcome"
                    );
                } else {
                    assert_eq!(
                        sent["error"]["code"],
                        serde_json::json!(control_protocol::DAEMON_GONE),
                        "{method}: gets a method-agnostic error"
                    );
                }
            };
            let written = read_agent_stdin(&mut child).await;
            let sent: serde_json::Value =
                serde_json::from_str(written.trim()).expect("a response line was written");
            assert_response(&sent, 42);

            // A request emitted after disconnect is cancelled at insertion,
            // never queued for the next daemon. The terminal/create row proves
            // a side-effecting call cannot execute twice through replay.
            let late = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":43,\"method\":\"{method}\",\"params\":{{}}}}\n"
            );
            shared.deliver_line(late.as_bytes(), &stdin).await;
            assert!(shared.pending_server_calls.lock().await.is_empty());
            assert!(!queued(&shared)
                .await
                .iter()
                .any(|frame| matches!(frame, ControlBody::ServerCall { .. })));
            let written = read_agent_stdin(&mut child).await;
            let sent: serde_json::Value =
                serde_json::from_str(written.trim()).expect("a detached response line was written");
            assert_response(&sent, 43);
        }
    }

    /// Past the cap the runner refuses the request outright instead of
    /// evicting its bookkeeping. Evicting would leave the agent parked
    /// forever on an id nothing will ever answer.
    #[tokio::test]
    async fn reverse_call_cap_refuses_rather_than_parking_the_agent() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let _attachment_id = shared.begin_attachment().await;
        for id in 0..MAX_OUTSTANDING_REQUESTS {
            let line = format!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"method\":\"fs/read_text_file\",\"params\":{{}}}}\n"
            );
            shared.deliver_line(line.as_bytes(), &stdin).await;
        }
        assert_eq!(
            shared.pending_server_calls.lock().await.len(),
            MAX_OUTSTANDING_REQUESTS
        );

        let extra = br#"{"jsonrpc":"2.0","id":999999,"method":"fs/read_text_file","params":{}}
"#;
        shared.deliver_line(extra, &stdin).await;
        assert_eq!(
            shared.pending_server_calls.lock().await.len(),
            MAX_OUTSTANDING_REQUESTS,
            "the refused call is not tracked"
        );
        let written = read_agent_stdin(&mut child).await;
        let last: serde_json::Value = serde_json::from_str(
            written
                .trim()
                .lines()
                .next_back()
                .expect("a refusal was written"),
        )
        .expect("refusal is JSON");
        assert_eq!(last["id"], serde_json::json!(999999));
        assert_eq!(
            last["error"]["code"],
            serde_json::json!(control_protocol::DAEMON_GONE)
        );
    }

    /// Queue admission accounts exact encoded bytes, sheds only notifications,
    /// and refuses to evict frames that complete outstanding work.
    #[test]
    fn queue_budget_sheds_notifications_and_preserves_correlations() {
        let wire = |len| Arc::<[u8]>::from(vec![0; len]);
        let mut channel = ControlChannel::default();
        channel.push(DeliveryScope::Persistent, QueuedKind::AgentReply, wire(16));
        channel.push(DeliveryScope::Persistent, QueuedKind::Notify, wire(8));
        channel.push(DeliveryScope::Persistent, QueuedKind::Notify, wire(8));
        channel.push(
            DeliveryScope::Persistent,
            QueuedKind::PromptCompleted,
            wire(16),
        );

        assert!(channel.make_room(MAX_CONTROL_QUEUE_BYTES - 32));
        assert_eq!(channel.queued_bytes, 32);
        assert_eq!(channel.queue.len(), 2);
        assert!(channel
            .queue
            .iter()
            .all(|frame| frame.kind != QueuedKind::Notify));
        assert!(
            !channel.make_room(MAX_CONTROL_QUEUE_BYTES - 31),
            "correlation frames must apply backpressure rather than be evicted"
        );
    }

    #[tokio::test]
    async fn outbound_frame_remains_queued_until_write_commit() {
        let shared = RunnerShared::new(None);
        shared
            .enqueue(
                DeliveryScope::Persistent,
                QueuedKind::PromptCompleted,
                ControlBody::PromptCompleted {
                    prompt_req_id: 7,
                    outcome: PromptOutcome::Aborted,
                },
            )
            .await;
        let first_attachment = shared.begin_attachment().await;
        let (entry_id, first_wire) = shared
            .next_outbound(first_attachment)
            .await
            .expect("leased frame");

        shared.release_outbound(entry_id).await;
        let second_attachment = shared.begin_attachment().await;
        let (retried_id, retried_wire) = shared
            .next_outbound(second_attachment)
            .await
            .expect("frame survives cancelled writer");
        assert_eq!(retried_id, entry_id);
        assert_eq!(retried_wire, first_wire);

        shared.commit_outbound(second_attachment, retried_id).await;
        let channel = shared.control.lock().await;
        assert!(channel.queue.is_empty());
        assert_eq!(channel.queued_bytes, 0);
    }

    #[tokio::test]
    async fn disconnect_purges_scoped_frames_and_late_forward_responses() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let old_attachment = shared.begin_attachment().await;
        assert!(
            shared
                .enqueue(
                    DeliveryScope::Attachment(old_attachment),
                    QueuedKind::Handshake,
                    ControlBody::Initialized {
                        result: serde_json::json!({}),
                    },
                )
                .await
        );
        shared
            .issue_agent_call(
                &stdin,
                old_attachment,
                9,
                "session/set_mode",
                serde_json::json!({}),
            )
            .await;
        let written = read_agent_stdin(&mut child).await;
        let request: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        let request_id = request["id"].as_i64().unwrap();

        shared.disconnect_control(old_attachment, &stdin, "s").await;
        let _new_attachment = shared.begin_attachment().await;
        assert!(
            !shared
                .enqueue(
                    DeliveryScope::Attachment(old_attachment),
                    QueuedKind::Handshake,
                    ControlBody::Initialized {
                        result: serde_json::json!({"stale": true}),
                    },
                )
                .await
        );
        let response = format!("{{\"jsonrpc\":\"2.0\",\"id\":{request_id},\"result\":{{}}}}\n");
        shared.deliver_line(response.as_bytes(), &stdin).await;

        assert!(shared.pending_agent_calls.lock().await.is_empty());
        assert!(queued(&shared).await.is_empty());
    }

    /// A forward-lane call round-trips: the runner puts its own id on the
    /// wire, and the agent's response resolves the daemon's `call_id`.
    #[tokio::test]
    async fn forward_call_round_trips_and_reset_refreshes_the_session_cache() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        shared.handshake.lock().await.session = Some((
            "old-session".into(),
            serde_json::json!({"sessionId": "old-session"}),
        ));
        shared
            .issue_agent_call(
                &stdin,
                attachment_id,
                77,
                "session/new",
                serde_json::json!({"cwd": "/tmp"}),
            )
            .await;
        let written = read_agent_stdin(&mut child).await;
        let sent: serde_json::Value =
            serde_json::from_str(written.trim()).expect("request written");
        assert_eq!(sent["method"], "session/new");
        let req_id = sent["id"].as_i64().expect("runner allocated a numeric id");

        let resp = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{req_id},\"result\":{{\"sessionId\":\"new-session\"}}}}\n"
        );
        shared.deliver_line(resp.as_bytes(), &stdin).await;

        let q = queued(&shared).await;
        assert!(
            matches!(&q[0], ControlBody::AgentResult { call_id: 77, .. }),
            "resolves the daemon's call_id: {q:?}"
        );
        assert_eq!(
            shared.acp_session_id().await.as_deref(),
            Some("new-session"),
            "a reset session/new refreshes the cache so Cancel addresses the new conversation"
        );
    }

    /// An agent error envelope on the forward lane surfaces as `AgentError`
    /// with the agent's own code preserved, not collapsed into a generic one.
    #[tokio::test]
    async fn forward_call_error_envelope_is_preserved() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        shared
            .issue_agent_call(
                &stdin,
                attachment_id,
                5,
                "session/set_mode",
                serde_json::json!({}),
            )
            .await;
        let written = read_agent_stdin(&mut child).await;
        let sent: serde_json::Value = serde_json::from_str(written.trim()).unwrap();
        let req_id = sent["id"].as_i64().unwrap();

        let resp = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":{req_id},\"error\":{{\"code\":-32000,\"message\":\"nope\"}}}}\n"
        );
        shared.deliver_line(resp.as_bytes(), &stdin).await;

        let q = queued(&shared).await;
        match &q[0] {
            ControlBody::AgentError { call_id, error } => {
                assert_eq!(*call_id, 5);
                assert_eq!(error.code, -32000);
                assert_eq!(error.message, "nope");
            }
            other => panic!("expected AgentError, got {other:?}"),
        }
    }

    /// A response id the runner never tracked (a stray or duplicate reply)
    /// must not produce a completion or any other frame.
    #[tokio::test]
    async fn untracked_response_queues_nothing() {
        let (shared, stdin, _child) = shared_with_stdin().await;
        let resp = br#"{"jsonrpc":"2.0","id":77,"result":{}}
"#;
        shared.deliver_line(resp, &stdin).await;
        assert!(queued(&shared).await.is_empty());
    }

    /// Answering the same `call_id` twice must not write a second response to
    /// the agent: it already got its one answer, and a duplicate could
    /// resolve an unrelated later request in a lax adapter.
    #[tokio::test]
    async fn duplicate_answer_for_a_resolved_call_is_dropped() {
        let (shared, stdin, mut child) = shared_with_stdin().await;
        let attachment_id = shared.begin_attachment().await;
        let req = br#"{"jsonrpc":"2.0","id":3,"method":"fs/read_text_file","params":{}}
"#;
        shared.deliver_line(req, &stdin).await;
        let call_id = *shared
            .pending_server_calls
            .lock()
            .await
            .keys()
            .next()
            .expect("tracked");

        shared
            .resolve_server_call(
                &stdin,
                attachment_id,
                call_id,
                Ok(serde_json::json!({"content": "x"})),
                "s",
            )
            .await;
        shared
            .resolve_server_call(
                &stdin,
                attachment_id,
                call_id,
                Ok(serde_json::json!({"content": "y"})),
                "s",
            )
            .await;
        let written = read_agent_stdin(&mut child).await;
        assert_eq!(
            written.trim().lines().count(),
            1,
            "exactly one response reached the agent: {written}"
        );
    }
}
