//! Acp worker supervisor.
//!
//! Owns a per-aoe-process map of session_id -> AcpClient handles. Spawns
//! the ACP agent subprocess on demand, bridges its events into the
//! per-AppState `acp_events_tx` broadcast channel, and fires push
//! notifications for ApprovalRequested events.
//!
//! Watchdog: when an agent's ACP connection task ends (subprocess exit,
//! transport break) the drain task respawns it. Up to
//! `MAX_RESPAWNS_IN_WINDOW` respawns are allowed inside `RESTART_WINDOW`;
//! beyond that the session is parked and an `AgentStartupError` event
//! is published so the UI can surface "session crashed" instead of
//! going silent.
//!
//! Producer side: `Supervisor::spawn(session_id, config)` creates an
//! AcpClient and a background task that drains its events.
//!
//! Consumer side: `Supervisor::send_prompt(session_id, text)` and
//! `Supervisor::resolve_permission(session_id, nonce, decision)` route
//! through the held client.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::{broadcast, mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::acp_client::{AcpClient, AcpError, DeleteSessionOutcome, SpawnConfig};
use super::agent_registry::{AgentRegistry, AgentSpec};
use super::approvals::{ApprovalDecision, Nonce};
use super::elicitations::{ElicitationOutcome, ElicitationResolution};
use super::state::{AcpSessionId, Event};
use crate::session::SandboxInfo;

/// Maximum number of post-startup respawns within `RESTART_WINDOW`.
/// After this many crashes the session is parked and an
/// `AgentStartupError` event is published. The initial spawn does not
/// count toward this budget — it's always allowed.
const MAX_RESPAWNS_IN_WINDOW: u32 = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// Brief backoff before respawning an exited worker so we don't
/// hot-loop when the agent process crashes immediately on startup.
const RESPAWN_BACKOFF: Duration = Duration::from_millis(500);

/// Look up the stored ACP session id for `session_id` and, if present,
/// fire the experimental `session/delete` RPC against the live worker.
/// Logs the outcome at a level matched to its severity, tagging with
/// the adapter kind so operators can tell `claude-agent-acp` apart
/// from `aoe-agent` / `codex` / `opencode` / future adapters in
/// debug.log without bouncing through the registry. All outcomes are
/// non-fatal and the caller proceeds to shutdown + SIGTERM. See
/// `AcpClient::delete_session` and #1404.
async fn try_session_delete(client: &AcpClient, session_id: &str) {
    // worker_registry::load reads from disk (sync I/O). Offload to
    // the blocking pool so we don't park a Tokio worker thread on a
    // delete path that the supervisor holds the per-instance API
    // lock through.
    let session_id_owned = session_id.to_string();
    let loaded =
        tokio::task::spawn_blocking(move || super::worker_registry::load(&session_id_owned)).await;
    let record = match loaded {
        Ok(Ok(rec)) => rec,
        Ok(Err(e)) => {
            // Registry read failed (disk error, malformed JSON, etc.).
            // Skipping `session/delete` here means we lose adapter-side
            // cleanup for a session that may have a stored ACP id, so
            // surface at warn even though shutdown still proceeds.
            warn!(
                target: "acp.protocol",
                session = %session_id,
                "skipping session/delete: worker_registry load failed: {e}"
            );
            return;
        }
        Err(e) => {
            warn!(
                target: "acp.protocol",
                session = %session_id,
                "skipping session/delete: registry load task join failed: {e}"
            );
            return;
        }
    };
    let (acp_id, adapter_kind) = match record {
        Some(rec) => (rec.stored_acp_session_id, rec.agent_key),
        None => (None, String::new()),
    };
    let Some(acp_id) = acp_id else {
        debug!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter_kind,
            "skipping session/delete: no stored ACP session id (pre-handshake or never assigned)"
        );
        return;
    };
    let started = Instant::now();
    let outcome = client.delete_session(acp_id.clone()).await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &outcome {
        DeleteSessionOutcome::Deleted => debug!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter_kind,
            acp_session_id = %acp_id,
            elapsed_ms,
            "session/delete RPC succeeded"
        ),
        DeleteSessionOutcome::UnsupportedMethod => debug!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter_kind,
            acp_session_id = %acp_id,
            "adapter does not support session/delete; proceeding to SIGTERM"
        ),
        DeleteSessionOutcome::TimedOut => warn!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter_kind,
            acp_session_id = %acp_id,
            elapsed_ms,
            "session/delete RPC timed out; proceeding to SIGTERM"
        ),
        DeleteSessionOutcome::Failed(msg) => warn!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter_kind,
            acp_session_id = %acp_id,
            elapsed_ms,
            "session/delete RPC failed: {msg}; proceeding to SIGTERM"
        ),
    }
}

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("session {0:?} not found")]
    UnknownSession(String),
    #[error("acp client error: {0}")]
    Acp(#[from] AcpError),
    #[error("agent {0:?} not in registry")]
    UnknownAgent(String),
    #[error("{0}")]
    InvalidAgentCommand(String),
    #[error("session {0:?} already has a running structured view worker")]
    AlreadyRunning(String),
    /// Configured `[acp] max_concurrent_workers` cap is full. The
    /// caller should surface this to the operator (REST: 503; CLI: a
    /// hint to delete an existing structured view session or raise the cap)
    /// rather than retrying.
    #[error("structured view worker capacity full ({current}/{limit}); raise [acp] max_concurrent_workers or delete an existing structured view session")]
    CapacityFull { current: usize, limit: u32 },
    /// The in-flight resume (spawn or attach) was cancelled by a
    /// concurrent `shutdown` call, e.g. the user clicked Disable while
    /// the ACP handshake was still in flight. The freshly-built client
    /// is dropped cleanly. Callers should treat this as a soft success:
    /// the requested end state (no worker for this session) holds.
    #[error("resume of session {0:?} was cancelled by a concurrent shutdown")]
    SpawnCancelled(String),
}

/// Frame published to the broadcast channel; mirrors
/// `crate::server::AcpBroadcastFrame` so the supervisor can be
/// tested without pulling in the server module.
///
/// Approval pushes are no longer driven from here: the structured view event
/// listener in the server module subscribes to the same broadcast and
/// matches `Event::ApprovalRequested` with `Arc<AppState>` already in
/// scope, which removed the need for a separate approval callback path.
/// See #1038.
pub trait BroadcastSink: Send + Sync + 'static {
    fn publish(&self, session_id: &str, seq: u64, event: &Event);
    /// Like `publish`, but reports whether the event write reached the
    /// durable event store. Default assumes success for sinks that don't
    /// persist (tests / in-memory fixtures).
    fn publish_persisted(&self, session_id: &str, seq: u64, event: &Event) -> bool {
        self.publish(session_id, seq, event);
        true
    }
    /// Drop all stored events for a session. Used by the import path to clear
    /// any partial replay from a prior failed attempt before re-seeding, run
    /// only after the worker slot is reserved so a duplicate spawn that hits
    /// `AlreadyRunning` can't wipe a live worker's transcript. Default no-op
    /// for test sinks without an event store. See #2276.
    fn clear_session_events(&self, _session_id: &str) {}
    /// Approval nonces from `ApprovalRequested` events on disk with no
    /// matching `ApprovalResolved`. Used by `Supervisor::attach` to
    /// cancel approvals whose responder died with the previous daemon.
    /// Default returns empty so test sinks without an event store opt
    /// out cleanly.
    fn unresolved_approval_nonces(&self, _session_id: &str) -> Vec<Nonce> {
        Vec::new()
    }
    /// Elicitation nonces from `ElicitationRequested` events on disk with
    /// no matching `ElicitationResolved`. Used by `Supervisor::attach` to
    /// cancel questions whose responder died with the previous daemon.
    /// Default returns empty so test sinks without an event store opt out
    /// cleanly.
    fn unresolved_elicitation_nonces(&self, _session_id: &str) -> Vec<Nonce> {
        Vec::new()
    }
    /// Persist one prompt attachment blob keyed to the seq of the
    /// `UserPromptSent` it rides with, so the retention prune and
    /// session delete drop it in lockstep. Default no-op so test sinks
    /// without an event store opt out cleanly, mirroring
    /// `unresolved_approval_nonces`. See #1000 / #965.
    fn record_attachment(
        &self,
        _session_id: &str,
        _seq: u64,
        _blob: &crate::acp::event_store::AttachmentBlob,
    ) -> bool {
        true
    }
    /// Roll back blobs for one prompt seq. Used when publishing the
    /// matching `UserPromptSent` fails durability, so refs and blobs
    /// never diverge on disk.
    fn delete_attachments_for_seq(&self, _session_id: &str, _seq: u64) {}
}

/// How this supervisor acquired the worker. Drives both reap (which
/// kinds the user-stop poller treats as runner-managed) and respawn
/// (only `Runner` carries a `SpawnConfig` and participates in the
/// restart budget). Replaces an older `spawn_config: Option<...>` plus
/// `socket_path.is_some()` filter that conflated "runner-managed" with
/// "auto-respawnable" and missed attached workers in both filters.
enum WorkerKind {
    /// Fresh spawn owned by this daemon. Watchdog respawns on crash
    /// within `MAX_RESPAWNS_IN_WINDOW`. Boxed because `SpawnConfig` is
    /// significantly larger than the unit variants, and keeping it
    /// inline trips `clippy::large_enum_variant`.
    Runner { spawn_config: Box<SpawnConfig> },
    /// Reattached to an already-running runner from a previous daemon
    /// (see `Supervisor::attach`). No auto-respawn from in-memory
    /// state; the reconciler handles a fresh spawn on its next tick.
    /// Still backed by a runner-registry entry, so user-stop detection
    /// via the registry-gone signal applies.
    Attached,
    /// In-process stdio fixture inserted by tests. No registry, no
    /// auto-respawn. The reap poller skips this kind so legacy stdio
    /// fixtures aren't torn down on every tick. Test-only: production
    /// spawn always passes through a runner socket.
    #[cfg(test)]
    Stdio,
}

struct WorkerHandle {
    /// Shared with all callers that need to issue an ACP request to
    /// this worker. Stored as `Arc<AcpClient>` (no surrounding Mutex)
    /// because every method on `AcpClient` takes `&self` and forwards
    /// to an `mpsc::Sender<ClientCmd>` whose consumer is the
    /// connection task. Ordering across multiple senders is whatever
    /// the channel scheduler picks; the agent serialises within a
    /// turn anyway. The single writer (respawn) replaces the whole
    /// `Arc` rather than mutating the inner client.
    client: Arc<AcpClient>,
    /// Background task draining events from the client. Aborted on
    /// shutdown.
    drain_task: JoinHandle<()>,
    /// Restart bookkeeping: timestamps of recent respawns (post-
    /// initial-spawn). Used by the watchdog to enforce
    /// `MAX_RESPAWNS_IN_WINDOW`. Empty on first spawn so the initial
    /// boot doesn't consume the budget.
    restart_history: Vec<Instant>,
    kind: WorkerKind,
}

/// Per-session monotonically-increasing seq counter. Lives at the
/// supervisor level (not on `WorkerHandle`) so it survives shutdown
/// and respawn cycles, and also covers the no-worker
/// `publish_startup_error` path. Without this, both publishers
/// would start from seq=1 and collide in the replay buffer, which
/// the client-side `applyEvent` dedupe then turned into a silent
/// loss of the agent's first message after a retry.
type SeqMap = std::sync::Mutex<HashMap<String, u64>>;

/// Public lifecycle state for a structured view worker, surfaced via
/// `SessionResponse.acp_worker_state` so the sidebar + structured view
/// can show a "Resuming…" affordance while the reconciler is mid-spawn
/// or mid-attach. Deliberately not persisted to the structured view event log:
/// daemon lifecycle is ephemeral, transcript replay should not carry
/// it. See #1088.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcpWorkerState {
    /// No worker for this session and no resume in flight.
    Absent,
    /// A spawn or attach is in progress; the UI shows the "Resuming…"
    /// banner + sidebar chip.
    Resuming,
    /// Worker is online and reachable.
    Running,
}

/// Which code path put a reservation into `pending_resumes`. The UI
/// treats both as `Resuming`; the supervisor only uses the kind for
/// capacity accounting (only `Spawn` reservations count toward
/// `max_concurrent_workers`, attach reattaches an existing runner).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeKind {
    /// Reattaching to a live runner via `Supervisor::attach`.
    Attach,
    /// Fresh `Supervisor::spawn` after a missing/dead runner.
    Spawn,
}

/// Outcome of `Supervisor::begin_resume`: either the caller now holds a
/// fresh `pending_resumes` reservation it must carry into `spawn_inner`
/// (or `attach`), or a worker is already running / already mid-resume so
/// there is nothing to reserve. `CapacityFull` surfaces as the `Err` arm.
pub(crate) enum ResumeReservationOutcome {
    /// A reservation was placed; the caller owns the RAII guard.
    Reserved(ResumeReservation),
    /// The session is already in `workers` or `pending_resumes`.
    AlreadyPresent,
}

pub struct Supervisor<S: BroadcastSink> {
    sink: Arc<S>,
    registry: Arc<Mutex<AgentRegistry>>,
    workers: Arc<Mutex<HashMap<String, WorkerHandle>>>,
    next_seqs: Arc<SeqMap>,
    /// Reservation map: a session_id present here means another task is
    /// mid-resume (spawn OR attach) for it. The `ResumeKind` lets the
    /// capacity check distinguish fresh spawns (which contribute to the
    /// worker pool) from reattaches (which take over an existing live
    /// runner). `AcpClient::spawn` takes 2-3s for the initial handshake
    /// and `attach` can block up to ~3s on socket dial; without this
    /// reservation, two concurrent callers both pass the empty-`workers`
    /// check and race to insert. The RAII `ResumeReservation` guard
    /// removes the entry on success, error, or panic.
    pending_resumes: Arc<std::sync::Mutex<HashMap<String, ResumeKind>>>,
    /// Session ids whose in-flight resume (spawn or attach) should
    /// bail out instead of inserting the freshly-built `WorkerHandle`.
    /// Set by `shutdown` when it observes a session that's in
    /// `pending_resumes`, either with no live runner record (the
    /// `pending_has_it` path) or against an existing runner about to
    /// be SIGTERMed (the registry-terminate path). Without this, an
    /// `acp_disable` arriving during the 2-3s ACP handshake
    /// would no-op while the in-flight resume still completed a few
    /// seconds later, producing an orphaned worker the user can no
    /// longer manage.
    ///
    /// Lock-order invariant (see #1848): this mutex is taken
    /// while `workers` is held. Writers in `shutdown_with_reason`
    /// insert before `drop(workers)`; readers in `spawn` and
    /// `attach` consume the breadcrumb (via `HashSet::remove`) after
    /// `workers.lock().await` and before their own `drop(workers)`.
    /// The lock pair (tokio `workers` outside, std `cancelled_resumes`
    /// inside) sequences the writer's seed inside its `workers`
    /// critical section: any resumer whose `workers.lock().await`
    /// completes after the writer's `drop(workers)` is then guaranteed
    /// to observe the seed when it next locks `cancelled_resumes`,
    /// so its pre-insert check consumes the breadcrumb instead of
    /// finding an empty set.
    cancelled_resumes: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Per-agent install gate. claude-agent-acp lazy-installs its
    /// native binary on first ever run; two concurrent `session/new`
    /// calls against a partially-installed SDK race the install and
    /// the second fails with "Claude Code native binary not found".
    /// Tracking which agents have already been warmed up in this
    /// process lifetime lets every subsequent spawn proceed in
    /// parallel without the gate. Reset on every `aoe serve` restart
    /// (warm-cache restarts pay one serial spawn). See #1088.
    warmed_up_agents: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Per-agent warm-up locks. The first `spawn` for an agent name
    /// that is not yet in `warmed_up_agents` acquires the matching
    /// lock for the duration of its handshake, then inserts the agent
    /// into the warm-up set. Subsequent concurrent callers `await`
    /// the lock, see the agent is now warmed up, and proceed without
    /// re-acquiring it. See #1088.
    agent_warmup_locks: Arc<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Wakes `wait_for_worker` whenever the (workers, pending_resumes)
    /// snapshot changes. Notified after every workers.insert and after
    /// every ResumeReservation drop. Replaces the previous 50 ms poll
    /// loop with edge triggered wakeups, so a request that arrives
    /// just after the spawn handshake finishes resumes within a
    /// scheduler tick instead of waiting up to 50 ms.
    worker_notify: Arc<tokio::sync::Notify>,
    /// Sessions whose adopted worker is running an older binary than the
    /// daemon (detected at reconcile after `aoe update`) and carried an
    /// in-flight turn, so the reconciler attached to drain it rather than
    /// hard-killing the turn. The reconciler's per-tick drain check
    /// respawns these on the current binary once the turn finishes. See
    /// #1754.
    build_respawn_pending: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Sessions currently parked on a per-adapter compatibility rejection,
    /// mapped to the binary that failed the check. Populated at every
    /// `IncompatibleAgent` publish site, cleared on a successful (re)spawn.
    /// Lets the web "Update & restart" install endpoint find every other
    /// session blocked on the same adapter and respawn them all at once, so
    /// one global `npm install -g` clears every red X without a per-session
    /// manual restart. See #2109.
    incompatible_binaries: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Sessions an out-of-band caller (the web install endpoint) wants the
    /// reconciler to fresh-spawn on its next tick, regardless of the
    /// `attempted` guard that otherwise pins a permanently-failing spawn.
    /// Used to clear every red X after a global adapter install without a
    /// per-session manual restart. See #2109.
    force_respawn: Arc<std::sync::Mutex<HashSet<String>>>,
    /// Cap on concurrently-running workers, snapshotted from
    /// `[acp] max_concurrent_workers` at startup. Enforced in
    /// `spawn`; new workers past the cap return `CapacityFull`.
    /// Tests use `Supervisor::new` (effectively unbounded); production
    /// uses `Supervisor::with_capacity`.
    max_concurrent_workers: u32,
}

/// RAII guard: ensures a session_id is removed from `pending_resumes`
/// when `spawn` or `attach` returns or unwinds, no matter which path
/// was taken. Without this, a panic or early-return mid-resume would
/// leave a phantom reservation that blocks every future resume for
/// that session AND keeps the UI stuck on "Resuming…".
pub(crate) struct ResumeReservation {
    pending: Arc<std::sync::Mutex<HashMap<String, ResumeKind>>>,
    session_id: String,
    /// Wakes any `wait_for_worker` parked on the supervisor's
    /// `worker_notify`. Cloned from the supervisor at construction
    /// so Drop never has to reach back into `&Supervisor`.
    notify: Arc<tokio::sync::Notify>,
}

impl Drop for ResumeReservation {
    fn drop(&mut self) {
        // Sync remove against std::sync::Mutex; constant time, never
        // blocks on an await. The previous shape spawned a detached
        // task to release a tokio::sync::Mutex, which required a live
        // runtime at drop time and orphaned the entry if the runtime
        // was already shutting down. `lock_recover` handles the case
        // where another holder panicked while owning the guard so the
        // reservation is still cleared instead of leaking.
        let session_id = std::mem::take(&mut self.session_id);
        lock_recover(&self.pending).remove(&session_id);
        // Wake any wait_for_worker parked on the notify. Notify with
        // no waiters is a no-op so the cost on the hot path is just
        // the atomic store inside Notify.
        self.notify.notify_waiters();
    }
}

/// Instance-level command override carried from the stored session
/// (`Instance.command`, populated by `session.agent_command_override`
/// or `--cmd-override` in `aoe add`). The tmux view already
/// honors `Instance.command`; this lets the structured view do the
/// same so a session launches the same binary regardless of view.
/// `logical_tool` is the instance's tool (e.g. `opencode`), kept
/// separate from the launched binary so agent-name-keyed behavior
/// (tool-kind mapping, status, `_meta`) stays correct. See #1766.
#[derive(Debug, Clone)]
pub struct AgentCommandOverride {
    pub logical_tool: String,
    pub command: String,
}

/// Inputs to `Supervisor::spawn`. A struct (rather than seven
/// positional params with `#[allow(clippy::too_many_arguments)]`)
/// because the previous signature was the kind that produces real
/// bugs the next time someone adds a field; the auto-spawn caller in
/// `create_session` had to thread six identical values through the
/// API plus a seventh on this PR.
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub session_id: String,
    pub agent: String,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    pub provider_env: Vec<(String, String)>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// ACP session id from a previous run; when `Some` and the agent
    /// advertises `load_session = true`, the spawn calls
    /// `LoadSessionRequest` instead of `NewSessionRequest`.
    pub stored_acp_session_id: Option<String>,
    /// When `Some`, the agent runs inside the named Docker container.
    /// The supervisor wraps the agent argv in `docker exec` and the
    /// daemon-side fs/terminal handlers route across the container
    /// boundary using the container_workdir / mount map derived from
    /// `Instance`'s container_config. `None` keeps the legacy host
    /// spawn behavior.
    pub sandbox_info: Option<SandboxInfo>,
    /// Source profile of the session. Used (with `sandbox_info`) to
    /// resolve profile-level `sandbox.environment` so structured view-sandbox
    /// env matches the tmux view. `None` for non-sandboxed
    /// sessions; falls back to the user's default profile when set
    /// to `Some("")`.
    pub source_profile: Option<String>,
    /// When true, switch the session to `bypassPermissions` mode
    /// immediately after `session/new` succeeds, so a profile with
    /// `yolo_mode_default = true` skips permission prompts in structured view
    /// the same way `--dangerously-skip-permissions` does in tmux mode.
    /// Best-effort: adapters that don't advertise bypass mode log a
    /// warning and stay in default. See #1142.
    pub yolo_mode: bool,
    /// When `Some`, overlay the instance's resolved launch command on
    /// the registry `AgentSpec` so structured view honors
    /// `session.agent_command_override` like tmux does. Applied only
    /// to registry-backed, same-tool specs whose binary matches the
    /// tool's built-in binary (see `apply_agent_command_override`).
    /// See #1766.
    pub agent_command_override: Option<AgentCommandOverride>,
    /// When true and this is a `session/load` spawn, do NOT suppress the
    /// agent's history replay; let it populate the (empty) event store so
    /// an imported transcript renders. Normal reattach leaves this false so
    /// the replay is suppressed against the already-stored transcript,
    /// avoiding a duplicate-key panic. The caller computes it from the
    /// session's `import_pending` flag. See #2276.
    pub seed_history_replay: bool,
}

/// True when `command` names the same executable as `binary`, comparing
/// the file name so an absolute path (`/usr/local/bin/opencode`) still
/// matches the built-in binary name (`opencode`).
fn command_matches_binary(command: &str, binary: &str) -> bool {
    command == binary
        || std::path::Path::new(command)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name == binary)
}

/// Resolve the MCP servers to forward for a spawn, lowest precedence first: the
/// agent's native config, merged under the global `<app_dir>/mcp.json`, merged
/// under the session's per-profile `<profile_dir>/mcp.json` (issue #1986),
/// merged under the trusted project-local `.mcp.json` (issue #1985). Runs on a
/// blocking thread (callers spawn_blocking it) because a native config can be
/// large. Each layer is isolated: a missing, unreadable, or malformed source
/// warns and contributes nothing rather than aborting, so a single broken file
/// never blocks the spawn. `profile` is the session's `source_profile`; an empty
/// or `None` value resolves to the default profile. `cwd` is the session's
/// working directory, from which the project-local repo (and its `.mcp.json`)
/// is resolved.
fn resolve_mcp_layers(
    agent_key: &str,
    session_id: &str,
    profile: Option<&str>,
    cwd: &std::path::Path,
) -> Vec<agent_client_protocol::schema::McpServer> {
    use crate::session::mcp_model::{resolve_effective, summarize};

    // One resolver for forwarding and the management surfaces (#1996): assemble
    // the trust-gated, provenance-tagged effective set, then convert only the
    // winning definitions to ACP wire values just before forwarding.
    let merged = resolve_effective(agent_key, profile, cwd);
    if !merged.is_empty() {
        info!(
            target: "acp.mcp",
            session = %session_id,
            count = merged.len(),
            servers = %summarize(&merged),
            "forwarding MCP servers"
        );
    }
    crate::acp::mcp_config::project_servers_to_acp(merged.into_iter().map(|s| s.def).collect())
}

/// Overlay an instance command override onto a resolved `AgentSpec`.
///
/// Applies only when the override is safe: the spec came from the
/// built-in registry, the selected agent is the override's logical
/// tool, and the spec's command is that tool's built-in binary. This
/// keeps `agent_command_override.opencode = "opencode-plannotator"`
/// working (registry `opencode` → binary `opencode`) while leaving
/// adapter-backed agents like Claude (`claude-agent-acp`) untouched,
/// where `agent_acp_cmd` is the right knob for a full argv swap.
///
/// The override is treated as a command prefix: its first word replaces
/// `spec.command`, any remaining words are prepended to `spec.args`, so
/// the registry's ACP args (e.g. `acp`) are preserved
/// (`opencode-plannotator` → `opencode-plannotator acp`). See #1766.
pub(crate) fn apply_agent_command_override(
    selected_agent: &str,
    spec_from_registry: bool,
    ovr: &AgentCommandOverride,
    spec: &mut AgentSpec,
) -> Result<(), SupervisorError> {
    if !spec_from_registry || selected_agent != ovr.logical_tool {
        return Ok(());
    }
    let Some(agent_def) = crate::agents::get_agent(&ovr.logical_tool) else {
        return Ok(());
    };
    if !command_matches_binary(&spec.command, agent_def.binary) {
        return Ok(());
    }
    let mut argv = shell_words::split(&ovr.command)
        .map_err(|e| SupervisorError::InvalidAgentCommand(format!("{e}")))?;
    if argv.is_empty() || argv[0].trim().is_empty() {
        return Ok(());
    }
    spec.command = argv.remove(0);
    argv.append(&mut spec.args);
    spec.args = argv;
    Ok(())
}

impl<S: BroadcastSink> Supervisor<S> {
    /// Constructor with no concurrency cap. Used in tests; production
    /// callers should use [`Supervisor::with_capacity`] so the
    /// configured `[acp] max_concurrent_workers` actually limits
    /// the worker pool.
    pub fn new(sink: Arc<S>) -> Self {
        Self::with_capacity(sink, u32::MAX)
    }

    pub fn with_capacity(sink: Arc<S>, max_concurrent_workers: u32) -> Self {
        Self {
            sink,
            registry: Arc::new(Mutex::new(AgentRegistry::with_defaults())),
            workers: Arc::new(Mutex::new(HashMap::new())),
            next_seqs: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending_resumes: Arc::new(std::sync::Mutex::new(HashMap::new())),
            cancelled_resumes: Arc::new(std::sync::Mutex::new(HashSet::new())),
            warmed_up_agents: Arc::new(std::sync::Mutex::new(HashSet::new())),
            agent_warmup_locks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            worker_notify: Arc::new(tokio::sync::Notify::new()),
            build_respawn_pending: Arc::new(std::sync::Mutex::new(HashSet::new())),
            incompatible_binaries: Arc::new(std::sync::Mutex::new(HashMap::new())),
            force_respawn: Arc::new(std::sync::Mutex::new(HashSet::new())),
            max_concurrent_workers,
        }
    }

    /// Flag a session whose build-stale worker was adopted to drain an
    /// in-flight turn; the reconciler respawns it on the current binary
    /// at the next idle boundary. Idempotent. See #1754.
    pub fn mark_build_respawn_pending(&self, session_id: &str) {
        lock_recover(&self.build_respawn_pending).insert(session_id.to_string());
    }

    /// Snapshot the sessions awaiting a post-drain respawn. The reconciler
    /// polls this each tick and respawns those that have gone idle.
    pub fn build_respawn_pending_ids(&self) -> Vec<String> {
        lock_recover(&self.build_respawn_pending)
            .iter()
            .cloned()
            .collect()
    }

    /// Drop a session from the pending set once it has been respawned (or
    /// is gone). Idempotent.
    pub fn clear_build_respawn_pending(&self, session_id: &str) {
        lock_recover(&self.build_respawn_pending).remove(session_id);
    }

    /// Record that a session is parked on a compatibility rejection for
    /// `binary`. Overwrites any prior entry. Cleared by
    /// `clear_incompatible_binary` on a successful (re)spawn. See #2109.
    fn mark_incompatible_binary(&self, session_id: &str, binary: &str) {
        lock_recover(&self.incompatible_binaries)
            .insert(session_id.to_string(), binary.to_string());
    }

    /// Drop a session's compatibility-rejection record once it spawns
    /// cleanly (or is gone). Idempotent. See #2109.
    fn clear_incompatible_binary(&self, session_id: &str) {
        lock_recover(&self.incompatible_binaries).remove(session_id);
    }

    /// Ask the reconciler to fresh-spawn these sessions on its next tick,
    /// bypassing the `attempted` guard. Idempotent. See #2109.
    pub fn request_respawn(&self, session_id: &str) {
        lock_recover(&self.force_respawn).insert(session_id.to_string());
    }

    /// Drain the pending force-respawn requests. Called once per reconciler
    /// tick; the ids are removed from `attempted` so the resume pass treats
    /// them as fresh. See #2109.
    pub fn take_respawn_requests(&self) -> Vec<String> {
        let mut set = lock_recover(&self.force_respawn);
        let ids = set.iter().cloned().collect();
        set.clear();
        ids
    }

    /// Session ids currently parked on a compatibility rejection for
    /// `binary` that have no live worker. The `is_running` filter is the
    /// safety net against a stale entry: a session that already recovered
    /// (or was stopped by the user) is running or absent-but-not-parked, so
    /// it is never resurrected by a bulk install-and-respawn. See #2109.
    pub async fn incompatible_sessions_for_binary(&self, binary: &str) -> Vec<String> {
        let candidates: Vec<String> = {
            let map = lock_recover(&self.incompatible_binaries);
            map.iter()
                .filter(|(_, b)| b.as_str() == binary)
                .map(|(id, _)| id.clone())
                .collect()
        };
        let mut out = Vec::new();
        for id in candidates {
            if !self.is_running(&id).await {
                out.push(id);
            }
        }
        out
    }

    /// Snapshot the lifecycle state of every structured view session known to
    /// the supervisor (running OR mid-resume). Cheap: one lock per map.
    /// Used by `GET /api/sessions` to fill `acp_worker_state` so the
    /// sidebar + structured view can render a "Resuming…" affordance
    /// without polling per-session. See #1088.
    pub async fn worker_states_snapshot(&self) -> HashMap<String, AcpWorkerState> {
        let mut out = HashMap::new();
        for id in self.workers.lock().await.keys() {
            out.insert(id.clone(), AcpWorkerState::Running);
        }
        for id in lock_recover(&self.pending_resumes).keys() {
            // Running wins over Resuming if both maps happen to carry
            // the id during a hand-off; the WorkerHandle is the
            // authoritative "online" signal.
            out.entry(id.clone()).or_insert(AcpWorkerState::Resuming);
        }
        out
    }

    /// Single-session lifecycle query. Prefer `worker_states_snapshot`
    /// for batch reads (the API layer overlays the snapshot onto every
    /// `SessionResponse`); this method is convenient for tests + the
    /// occasional one-off query.
    pub async fn worker_state(&self, session_id: &str) -> AcpWorkerState {
        if self.workers.lock().await.contains_key(session_id) {
            return AcpWorkerState::Running;
        }
        if lock_recover(&self.pending_resumes).contains_key(session_id) {
            return AcpWorkerState::Resuming;
        }
        AcpWorkerState::Absent
    }

    /// Resolve the agent spec from the registry. Surfaces UnknownAgent
    /// when the caller picks a name that hasn't been configured.
    pub async fn resolve_agent(&self, name: &str) -> Result<AgentSpec, SupervisorError> {
        self.registry
            .lock()
            .await
            .get(name)
            .cloned()
            .ok_or_else(|| SupervisorError::UnknownAgent(name.into()))
    }

    /// Resolve the agent spec for a structured view session, overlaying the
    /// session's (already profile-resolved) config onto the built-in
    /// registry. Built-in agents resolve from the registry; a custom
    /// agent resolves from its `agent_acp_cmd` entry, parsed into
    /// argv. Never mutates the registry, so two profiles defining the
    /// same custom name with different commands don't clobber each other
    /// and `/acp/switch-agent` validation stays per-session.
    ///
    /// The returned bool is true when the spec came from the built-in
    /// registry (vs an `agent_acp_cmd` custom spec); callers use it
    /// to decide whether a command override may overlay the spec without
    /// taking the registry lock a second time. See #1766.
    pub async fn resolve_agent_spec(
        &self,
        name: &str,
        config: &crate::session::config::SessionConfig,
    ) -> Result<(AgentSpec, bool), SupervisorError> {
        if let Some(spec) = self.registry.lock().await.get(name).cloned() {
            return Ok((spec, true));
        }
        if let Some(cmd) = config.agent_acp_cmd.get(name) {
            let spec =
                AgentSpec::from_acp_cmd(name, cmd).map_err(SupervisorError::InvalidAgentCommand)?;
            return Ok((spec, false));
        }
        Err(SupervisorError::UnknownAgent(name.into()))
    }

    /// Pick the agent name to spawn for an instance. Precedence:
    ///   1. explicit `agent_name` override on the instance
    ///   2. registry entry keyed on the instance's tool name
    ///      (so `tool="opencode"` → registry `"opencode"` →
    ///      `opencode acp`, etc.)
    ///   3. custom agent declaring an ACP command via
    ///      `agent_acp_cmd` in the session's profile config
    ///   4. legacy fallback: `claude` for the claude tool, otherwise
    ///      `aoe-agent` (our bundled multi-provider agent)
    ///
    /// `profile` is the session's source profile (`""` resolves the
    /// user's default) and `project_path` is its working directory;
    /// both are consulted for step 3 so repo-local `agent_acp_cmd`
    /// overrides are honored.
    pub async fn pick_agent_for_tool(
        &self,
        tool: &str,
        explicit_override: Option<&str>,
        profile: &str,
        project_path: &std::path::Path,
    ) -> String {
        if let Some(name) = explicit_override {
            if !name.is_empty() {
                return name.to_string();
            }
        }
        // Step 2: tool-keyed registry lookup.
        {
            let reg = self.registry.lock().await;
            if reg.get(tool).is_some() {
                return tool.to_string();
            }
        }
        // Step 3: custom agent with a configured ACP command resolves to
        // its own name; spawn builds the spec from config (no registry
        // mutation).
        if self
            .custom_agent_has_acp_cmd(tool, profile, project_path)
            .await
        {
            return tool.to_string();
        }
        // Step 4: legacy fallbacks.
        if tool == "claude" {
            "claude".into()
        } else {
            "aoe-agent".into()
        }
    }

    /// True iff `tool` is a custom agent that declares an
    /// `agent_acp_cmd` in its profile + repo-resolved config.
    pub async fn custom_agent_has_acp_cmd(
        &self,
        tool: &str,
        profile: &str,
        project_path: &std::path::Path,
    ) -> bool {
        let tool = tool.to_string();
        let profile = profile.to_string();
        let project_path = project_path.to_path_buf();
        tokio::task::spawn_blocking(move || {
            crate::session::repo_config::resolve_config_with_repo_or_warn(&profile, &project_path)
                .session
                .agent_acp_cmd
                .get(&tool)
                .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(&tool, cmd).is_ok())
        })
        .await
        .unwrap_or(false)
    }

    pub async fn registry_snapshot(&self) -> AgentRegistry {
        self.registry.lock().await.clone()
    }

    /// True iff `name` is registered as an ACP agent. Used by the
    /// `/acp/switch-agent` endpoint to validate the target before
    /// tearing down the current worker; otherwise an unknown agent
    /// would only surface at spawn time, leaving the session without a
    /// worker.
    pub async fn registry_has_agent(&self, name: &str) -> bool {
        self.registry.lock().await.get(name).is_some()
    }

    /// Publish a synthetic AgentStartupError event for a session whose
    /// worker never came online. Used by the auto-spawn-after-create
    /// path so the UI shows a remediation hint instead of an empty,
    /// silent conversation when `claude-agent-acp` isn't installed (or
    /// `npx -y` is still downloading on first run).
    pub fn publish_startup_error(&self, session_id: &str, message: String) {
        let seq = next_seq(&self.next_seqs, session_id);
        self.sink
            .publish(session_id, seq, &Event::AgentStartupError { message });
    }

    /// Mirror an `AcpError::IncompatibleAgent` onto the broadcast sink
    /// and tear down the detached runner. Called from every spawn-
    /// failure site so the structured detail reaches the reducer (the
    /// in-process event_tx on the failed AcpClient never delivers) and
    /// socket-mode workers don't survive a compatibility rejection. On
    /// non-compat errors this is a no-op.
    fn publish_compat_rejection(&self, session_id: &str, err: &AcpError) {
        let AcpError::IncompatibleAgent(payload) = err else {
            return;
        };
        let detail_seq = next_seq(&self.next_seqs, session_id);
        self.sink.publish(
            session_id,
            detail_seq,
            &Event::IncompatibleAgent {
                detail: payload.detail.clone(),
            },
        );
        let msg_seq = next_seq(&self.next_seqs, session_id);
        self.sink.publish(
            session_id,
            msg_seq,
            &Event::AgentStartupError {
                message: payload.message.clone(),
            },
        );
        // SIGTERM the detached runner so a stale claude-agent-acp@0.32.0
        // child doesn't keep the worker socket alive. `terminate_runner_for_session`
        // also deletes the registry entry so a retry via the API doesn't
        // hit AlreadyRunning. Idempotent: it's a no-op if the registry
        // entry is missing or the PID is dead. No-op on non-unix.
        terminate_runner_for_session(session_id);
    }

    /// Publish a synthetic `AgentSwitched` event after a successful
    /// `/acp/switch-agent` operation. Carries the prior and new
    /// agent registry keys plus the reason (e.g. `"rate_limited"`).
    /// The reducer uses this to drop transient state tied to the prior
    /// backend (rate-limit banner, in-flight tool, usage). See #1282.
    pub fn publish_agent_switched(
        &self,
        session_id: &str,
        from: String,
        to: String,
        reason: String,
    ) -> u64 {
        let seq = next_seq(&self.next_seqs, session_id);
        self.sink
            .publish(session_id, seq, &Event::AgentSwitched { from, to, reason });
        seq
    }

    /// Publish a `RateLimitAutoResumed` breadcrumb for a session the
    /// reconciler is about to auto-respawn after a rate-limit park. The
    /// `resets_at` is the adapter-reported reset time that gated the
    /// resume. This event doubles as the supersede marker: it becomes the
    /// session's latest status event (see `latest_status_event`'s filter),
    /// so the next reconciler tick no longer sees `Stopped{rate_limited}`
    /// and falls through to a fresh spawn instead of re-parking. The web
    /// reducer also keys off it to clear the rate-limit banner and drain a
    /// queued prompt. See #1722.
    pub fn publish_rate_limit_auto_resumed(
        &self,
        session_id: &str,
        resets_at: chrono::DateTime<chrono::Utc>,
    ) -> u64 {
        let seq = next_seq(&self.next_seqs, session_id);
        self.sink
            .publish(session_id, seq, &Event::RateLimitAutoResumed { resets_at });
        seq
    }

    /// Like `shutdown` but waits for the runner process to actually exit
    /// before returning, so a subsequent `spawn` for the same session id
    /// doesn't race the SIGTERM and collide on the worker socket file.
    /// Bounded by `deadline`; on timeout the worker is still removed
    /// from the in-memory map, so a subsequent spawn won't return
    /// AlreadyRunning, but the caller should treat it as best-effort
    /// cleanup. Used by the `/acp/switch-agent` path so the new
    /// agent's spawn binds a clean socket. See #1282.
    pub async fn shutdown_and_wait(
        &self,
        session_id: &str,
        deadline: std::time::Duration,
    ) -> Result<(), SupervisorError> {
        // Snapshot the runner's PID BEFORE shutdown removes the registry
        // entry, so we can poll for the process to actually die.
        let pid_before = super::worker_registry::load(session_id)
            .ok()
            .flatten()
            .map(|r| r.pid);
        match self.shutdown(session_id).await {
            Ok(()) => {}
            Err(SupervisorError::UnknownSession(_)) => {
                // Nothing to wait on; the caller can move on to spawn.
                return Ok(());
            }
            Err(e) => return Err(e),
        }
        // Poll for the runner subprocess to exit so its socket file
        // releases. ~deadline/100ms tick; usually claude-agent-acp dies
        // in <500ms once SIGTERM lands.
        #[cfg(unix)]
        if let Some(pid) = pid_before {
            let start = std::time::Instant::now();
            while start.elapsed() < deadline {
                if !super::worker_registry::is_pid_alive(pid) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            // Best-effort socket file removal: the new spawn will bind
            // <workers_dir>/<session_id>.sock, so a stale inode from
            // the old runner would collide. terminate_runner_for_session
            // already removed the registry entry; this cleans up the
            // socket. Failures (already gone, no perms) are non-fatal.
            if let Ok(socket_path) = super::worker_registry::socket_path_for(session_id) {
                if socket_path.exists() {
                    let _ = std::fs::remove_file(&socket_path);
                }
            }
        }
        // Acp currently runs over Unix sockets only; reaching this
        // function on a non-Unix host means somebody added a non-Unix
        // backend without porting the PID-wait + socket-cleanup above.
        // Warn loudly so the gap is visible in the log rather than
        // silently returning Ok and leaking a stale socket. Mirrors the
        // precedent at the agent_unresponsive escalation site below.
        #[cfg(not(unix))]
        {
            let _ = pid_before;
            tracing::warn!(
                target: "acp.supervisor",
                session = %session_id,
                "shutdown_and_wait called on non-Unix host; PID-wait and \
                 socket cleanup are unimplemented for this platform"
            );
        }
        Ok(())
    }

    /// Publish a synthetic `Stopped` event for a session whose turn was
    /// in flight when the previous `aoe serve` died. Called at startup
    /// from the reconciler when the on-disk event store shows an
    /// orphaned `UserPromptSent` (no terminating `Stopped` or
    /// `AgentStartupError` after it) AND there is no live runner to
    /// reattach to (which would deliver the Stopped via the resume-idle
    /// watchdog instead). Without this the UI's "thinking" indicator
    /// for the dead turn stays on indefinitely after restart.
    pub fn synthesize_stopped_for_orphan(&self, session_id: &str, reason: &str) {
        let seq = next_seq(&self.next_seqs, session_id);
        info!(
            target: "acp.supervisor",
            session = %session_id,
            seq,
            %reason,
            "publishing synthetic Stopped for orphaned in-flight turn"
        );
        self.sink.publish(
            session_id,
            seq,
            &Event::Stopped {
                reason: reason.to_string(),
            },
        );
    }

    /// Publish a UserPromptSent event before forwarding the prompt to
    /// the ACP agent. The replay buffer (and on-disk event store) needs
    /// the user's side of the conversation in the same stream as agent
    /// chunks; otherwise a reconnecting client sees only assistant text
    /// and every turn concatenates into one giant message.
    ///
    /// Also detects the conversation-reset slash command (claude's
    /// `/clear`, codex's / opencode's `/new`) and emits a follow-up
    /// `Event::SessionCleared` so the UI can fold the pre-clear
    /// transcript and drop now-stale session-scoped capability caches.
    /// Adapters don't emit a structured signal for these, so detection
    /// is text-based but routed through the session's `AgentProfile`
    /// so each agent's aliases match the right surface. See #1101.
    pub async fn publish_user_prompt(&self, session_id: &str, text: String) {
        self.publish_user_prompt_with_attachments(session_id, text, &[])
            .await;
    }

    /// Like `publish_user_prompt` but also persists the prompt's
    /// attachment blobs (keyed to the same seq as the `UserPromptSent`)
    /// and records metadata-only refs on the event so replay can render
    /// them. The bytes never enter the event JSON; only the refs do.
    /// See #1000 / #965.
    pub async fn publish_user_prompt_with_attachments(
        &self,
        session_id: &str,
        text: String,
        attachments: &[crate::acp::event_store::AttachmentBlob],
    ) {
        let agent_key = self.agent_key_for_session(session_id).await;
        let profile = super::agent_profiles::resolve(&agent_key);
        let is_clear = profile.is_clear_command(&text);
        let seq = next_seq(&self.next_seqs, session_id);
        let mut refs = Vec::with_capacity(attachments.len());
        for blob in attachments {
            if !self.sink.record_attachment(session_id, seq, blob) {
                // A blob failed to persist; roll back any siblings already
                // written for this seq and abort before publishing, so the
                // UserPromptSent never carries refs load_attachment() can't serve.
                self.sink.delete_attachments_for_seq(session_id, seq);
                return;
            }
            refs.push(crate::acp::state::PromptAttachmentRef {
                id: blob.id.clone(),
                kind: blob.kind,
                mime_type: blob.mime_type.clone(),
                name: blob.name.clone(),
                size: blob.data.len() as u64,
            });
        }
        let persisted = self.sink.publish_persisted(
            session_id,
            seq,
            &Event::UserPromptSent {
                text,
                attachments: refs,
            },
        );
        if !persisted {
            self.sink.delete_attachments_for_seq(session_id, seq);
            return;
        }
        if is_clear {
            let seq = next_seq(&self.next_seqs, session_id);
            self.sink.publish(session_id, seq, &Event::SessionCleared);
        }
    }

    /// Publish a "Send diff comments" submission as a typed
    /// `Event::UserDiffCommentsPrompt`. Unlike `publish_user_prompt`
    /// this skips the `/clear` detection: assembled diff-comment
    /// markdown is never a clear command, and treating it as one would
    /// wrongly fold the transcript. The caller forwards
    /// `assembled_markdown` to the agent separately, exactly as it does
    /// the plain text of a normal prompt.
    pub async fn publish_user_diff_comments_prompt(
        &self,
        session_id: &str,
        intro: String,
        outro: String,
        is_multi_repo: bool,
        comments: Vec<super::state::DiffComment>,
        assembled_markdown: String,
    ) {
        let seq = next_seq(&self.next_seqs, session_id);
        self.sink.publish(
            session_id,
            seq,
            &Event::UserDiffCommentsPrompt {
                intro,
                outro,
                is_multi_repo,
                comments,
                assembled_markdown,
            },
        );
    }

    /// Resolve the agent registry key for a session. Reads the live
    /// `Runner` handle's `SpawnConfig` directly when available;
    /// otherwise loads the on-disk record so an `Attached` worker (or
    /// a session whose handle has been dropped) still resolves its
    /// profile correctly. Returns `"claude"` as a last-resort default
    /// for sessions whose record predates the `agent_key` field
    /// (empty after the serde default).
    async fn agent_key_for_session(&self, session_id: &str) -> String {
        if let Some(handle) = self.workers.lock().await.get(session_id) {
            if let WorkerKind::Runner { spawn_config } = &handle.kind {
                return spawn_config.agent_key.clone();
            }
        }
        if let Ok(Some(record)) = super::worker_registry::load(session_id) {
            if !record.agent_key.is_empty() {
                return record.agent_key;
            }
        }
        "claude".to_string()
    }

    /// Drop per-session bookkeeping (replay seq counter). Called when
    /// the session is deleted or its view is switched away from
    /// structured view, so the next acp_enable starts a fresh conversation
    /// from seq=1 with a clean replay buffer.
    pub fn forget_session(&self, session_id: &str) {
        if let Ok(mut guard) = self.next_seqs.lock() {
            guard.remove(session_id);
        }
    }

    /// Pre-populate `next_seqs` from `(session_id, max_seq)` pairs.
    /// Used at server startup to seed the counter from the on-disk
    /// event store so a fresh publish gets max_seq + 1, not 1, and
    /// doesn't collide with restored history.
    pub fn hydrate_seqs(&self, pairs: impl IntoIterator<Item = (String, u64)>) {
        if let Ok(mut guard) = self.next_seqs.lock() {
            for (session_id, seq) in pairs {
                guard.insert(session_id, seq);
            }
        }
    }

    pub async fn upsert_agent(&self, name: String, spec: AgentSpec) {
        self.registry.lock().await.upsert(name, spec);
    }

    /// Spawn a structured view worker for the given session. Returns Err if a
    /// worker is already running for that session, if a spawn for
    /// the same session is already in progress, or if the
    /// `max_concurrent_workers` cap is full.
    ///
    /// Concurrency: `AcpClient::spawn` performs the ACP handshake
    /// (initialize + session/new), which takes 2-3s while no lock is
    /// held. Without the `pending_spawns` reservation below, two
    /// concurrent callers for the same session_id would both pass
    /// the empty-`workers` check, both finish the handshake, and
    /// both insert into `workers` — the second insert silently
    /// overwriting the first WorkerHandle. The dropped client's
    /// cmd_tx would then close, its connection task would exit
    /// cleanly, and the orphaned drain task would burn the restart
    /// budget respawning a worker the supervisor no longer points
    /// at. The reservation makes the second caller fail fast with
    /// AlreadyRunning instead.
    pub async fn spawn(&self, req: SpawnRequest) -> Result<(), SupervisorError> {
        let reservation = match self
            .begin_resume(&req.session_id, ResumeKind::Spawn)
            .await?
        {
            ResumeReservationOutcome::Reserved(r) => r,
            ResumeReservationOutcome::AlreadyPresent => {
                return Err(SupervisorError::AlreadyRunning(req.session_id));
            }
        };
        self.spawn_inner(req, reservation).await
    }

    /// Synchronously reserve a `pending_resumes` slot BEFORE any async
    /// resume work begins, so a caller that goes on to drive a detached
    /// spawn (the idle-dormant prompt-wake path, see #1748) makes
    /// `wait_for_worker` observe the reservation immediately rather than
    /// failing fast. Returns `AlreadyPresent` when a worker is already
    /// running or another task is mid-resume, and `Err(CapacityFull)`
    /// when the worker cap is reached. The `workers` and `pending_resumes`
    /// maps are checked under the same critical section so the
    /// `(workers ∪ pending)` set is observed atomically; the existing
    /// `spawn`/`attach` reservation logic now routes through here.
    pub(crate) async fn begin_resume(
        &self,
        session_id: &str,
        kind: ResumeKind,
    ) -> Result<ResumeReservationOutcome, SupervisorError> {
        let workers = self.workers.lock().await;
        if workers.contains_key(session_id) {
            return Ok(ResumeReservationOutcome::AlreadyPresent);
        }
        let mut pending = lock_recover(&self.pending_resumes);
        if pending.contains_key(session_id) {
            return Ok(ResumeReservationOutcome::AlreadyPresent);
        }
        match kind {
            ResumeKind::Spawn => {
                // Capacity check counts both running (in-memory) and
                // detached (on-disk-only) workers PLUS any in-flight Spawn
                // reservations, so a parallel reconciler can't pass the
                // limit check for N concurrent callers before any have
                // inserted into `workers`. Attach reservations don't
                // contribute: they reattach to an existing live runner that
                // is already counted in `registry_count`. See #1088.
                let registry_count = super::worker_registry::list()
                    .map(|recs| {
                        recs.into_iter()
                            .filter(|r| {
                                super::worker_registry::is_record_live(r)
                                    && !workers.contains_key(&r.session_id)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                let pending_spawn_count = pending
                    .values()
                    .filter(|k| matches!(k, ResumeKind::Spawn))
                    .count();
                let combined = workers.len() + registry_count + pending_spawn_count;
                if combined >= self.max_concurrent_workers as usize {
                    return Err(SupervisorError::CapacityFull {
                        current: combined,
                        limit: self.max_concurrent_workers,
                    });
                }
            }
            ResumeKind::Attach => {
                // No capacity gate: attach takes over an already-running
                // detached runner (already counted in `registry_count`),
                // it does not create a new worker. Rejecting it would
                // strand a live runner after a restart or after lowering
                // `max_concurrent_workers`, leaving the session unmanaged.
            }
        }
        pending.insert(session_id.to_string(), kind);
        Ok(ResumeReservationOutcome::Reserved(ResumeReservation {
            pending: Arc::clone(&self.pending_resumes),
            session_id: session_id.to_string(),
            notify: Arc::clone(&self.worker_notify),
        }))
    }

    /// Spawn body proper, run while holding a `pending_resumes`
    /// reservation acquired by `begin_resume`. Split out so the
    /// prompt-wake path (#1748) can reserve synchronously, then drive
    /// this in a detached task without re-reserving.
    pub(crate) async fn spawn_inner(
        &self,
        req: SpawnRequest,
        _reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        let SpawnRequest {
            session_id,
            agent,
            cwd,
            additional_dirs,
            provider_env,
            model,
            effort,
            stored_acp_session_id,
            sandbox_info,
            source_profile,
            yolo_mode,
            agent_command_override,
            seed_history_replay,
        } = req;

        // Per-agent install gate. claude-agent-acp lazy-installs its
        // native binary on first ever run; two concurrent `session/new`
        // calls against a partially-installed SDK race the install and
        // the second fails with "Claude Code native binary not found".
        // The first caller for an agent name that is not yet in
        // `warmed_up_agents` holds an `Arc<Mutex<()>>` keyed on agent
        // name for the duration of the handshake; subsequent callers
        // await the lock, see the agent is warmed up, and proceed in
        // parallel. The set is process-lifetime only, so cold-start
        // warm-cache restarts pay one serial spawn before the rest
        // parallelize. See #1088.
        let warmup_guard = {
            if lock_recover(&self.warmed_up_agents).contains(&agent) {
                None
            } else {
                let lock = lock_recover(&self.agent_warmup_locks)
                    .entry(agent.clone())
                    .or_insert_with(|| Arc::new(Mutex::new(())))
                    .clone();
                Some(lock.lock_owned().await)
            }
        };

        // Resolve the spec config-aware: built-ins come from the
        // registry, custom agents from this session's profile + repo
        // resolved `agent_acp_cmd`. Read off-thread; config
        // resolution touches disk.
        let profile_for_cfg = source_profile.clone().unwrap_or_default();
        let cwd_for_cfg = cwd.clone();
        let resolved_cfg = tokio::task::spawn_blocking(move || {
            crate::session::repo_config::resolve_config_with_repo_or_warn(
                &profile_for_cfg,
                &cwd_for_cfg,
            )
        })
        .await
        .map_err(|e| {
            SupervisorError::InvalidAgentCommand(format!("config load task failed: {e}"))
        })?;
        // `spec_from_registry` distinguishes a built-in registry spec
        // from an `agent_acp_cmd` custom spec: the command override
        // only overlays registry specs (custom ACP commands own their
        // full argv already). Returned by `resolve_agent_spec` so the
        // registry is locked once, not raced across two reads.
        let (mut spec, spec_from_registry) = self
            .resolve_agent_spec(&agent, &resolved_cfg.session)
            .await?;
        // Overlay the instance command override (e.g. opencode →
        // opencode-plannotator from `session.agent_command_override`)
        // so structured view launches the same binary tmux would. See #1766.
        if let Some(ref ovr) = agent_command_override {
            apply_agent_command_override(&agent, spec_from_registry, ovr, &mut spec)?;
        }
        // Apply ${aoe_data_dir} placeholder substitution against the
        // appropriate path; if the placeholder is not consumed it stays
        // as-is and the spawn will fail with a clear error.
        if spec.command.contains("${aoe_data_dir}") {
            if let Ok(data_dir) = crate::session::get_app_dir() {
                spec.command = spec
                    .command
                    .replace("${aoe_data_dir}", &data_dir.to_string_lossy());
            }
        }

        let mut env = provider_env;
        if let Some(model) = model {
            env.push(("AOE_AGENT_MODEL".into(), model));
        }

        // Every structured view worker runs through `aoe __acp-runner` so it
        // survives `aoe serve --stop`. The runner binds the socket path
        // computed here and the daemon dials it.
        let socket_path = super::worker_registry::socket_path_for(&session_id).map_err(|e| {
            SupervisorError::Acp(AcpError::Spawn(format!("worker socket path: {e}")))
        })?;

        // Resolve the MCP servers to forward on session/new and session/load:
        // the agent's own native config (lowest precedence) merged under the
        // global `<app_dir>/mcp.json`, the per-profile `<profile_dir>/mcp.json`
        // (#1986), and the trusted project-local `.mcp.json` (#1985), so a server
        // defined in several is taken from the highest layer. The project-local
        // layer is only forwarded when the repo is trusted for the file's current
        // fingerprint; otherwise it is skipped and logged. Disk reads and parsing
        // run off the async runtime because a native config (e.g. `~/.claude.json`)
        // can be large. Any broken layer warns and contributes nothing rather than
        // failing the spawn.
        let mcp_agent = agent.clone();
        let mcp_session = session_id.clone();
        let mcp_profile = source_profile.clone();
        let mcp_cwd = cwd.clone();
        let mcp_servers = tokio::task::spawn_blocking(move || {
            resolve_mcp_layers(&mcp_agent, &mcp_session, mcp_profile.as_deref(), &mcp_cwd)
        })
        .await
        .unwrap_or_else(|e| {
            warn!(
                target: "acp.mcp",
                session = %session_id,
                error = %e,
                "MCP resolution task failed; forwarding no servers"
            );
            Vec::new()
        });

        let config = SpawnConfig {
            agent_key: agent.clone(),
            spec,
            cwd,
            additional_dirs,
            provider_env: env,
            default_effort: effort,
            socket_path: Some(socket_path),
            stored_acp_session_id: stored_acp_session_id.clone(),
            sandbox_info,
            source_profile,
            mcp_servers,
            seed_history_replay,
            artifact_dir: crate::session::artifacts::session_artifact_dir(&session_id).ok(),
        };

        debug!(
            target: "acp.supervisor",
            session = %session_id,
            stored_id = ?stored_acp_session_id,
            "spawning structured view worker"
        );

        // Import seeding: clear any partial replay from a prior failed attempt
        // before session/load re-emits the transcript. Done here, after the
        // spawn reservation is held, rather than in the REST handler, so a
        // duplicate import spawn that bails with AlreadyRunning can't wipe a
        // live worker's stored transcript. See #2276.
        if seed_history_replay {
            self.sink.clear_session_events(&session_id);
        }

        let acp_session_id = AcpSessionId(session_id.clone());
        let mut client = match AcpClient::spawn(config.clone(), acp_session_id.clone()).await {
            Ok(c) => c,
            Err(err) => {
                if matches!(err, AcpError::IncompatibleAgent(_)) {
                    self.mark_incompatible_binary(&session_id, &config.spec.command);
                }
                self.publish_compat_rejection(&session_id, &err);
                return Err(SupervisorError::Acp(err));
            }
        };

        // First spawn for this agent succeeded; record it in
        // `warmed_up_agents` so subsequent concurrent callers skip the
        // per-agent install lock and run fully parallel. Only on
        // success: a failed warm-up should leave the next caller to
        // retry the gate. See #1088.
        if warmup_guard.is_some() {
            lock_recover(&self.warmed_up_agents).insert(agent.clone());
        }
        drop(warmup_guard);

        info!(target: "acp.supervisor", session = %session_id, "structured view worker spawned");
        self.clear_incompatible_binary(&session_id);

        // Move the inbound receiver out so the drain task can poll events
        // without holding the client mutex (which would deadlock
        // send_prompt: drain holds the lock across recv().await). The
        // receiver is always Some on a freshly-spawned client.
        let inbound = client
            .take_inbound()
            .expect("freshly spawned AcpClient always has inbound receiver");
        let client = Arc::new(client);

        let mut workers = self.workers.lock().await;
        // Belt-and-braces: even with the pending_spawns reservation,
        // re-check that no worker has been inserted under our nose.
        // If it has, drop the freshly-spawned client (its Drop will
        // close cmd_tx and tear down the subprocess cleanly) and
        // surface AlreadyRunning rather than overwriting the live
        // WorkerHandle.
        if workers.contains_key(&session_id) {
            drop(workers);
            drop(client);
            return Err(SupervisorError::AlreadyRunning(session_id));
        }
        // Cancellation: a concurrent shutdown observed this session
        // mid-handshake and asked us to bail. Drop the client cleanly
        // and skip the workers insert so the user's "disable" actually
        // takes effect instead of being silently overwritten by the
        // 2-3s-late spawn completion.
        if lock_recover(&self.cancelled_resumes).remove(&session_id) {
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                "spawn cancelled by concurrent shutdown; dropping freshly-spawned client"
            );
            drop(workers);
            drop(client);
            return Err(SupervisorError::SpawnCancelled(session_id));
        }
        let drain_task = self.start_drain_task(session_id.clone(), inbound);
        let client_for_yolo = yolo_mode.then(|| Arc::clone(&client));
        workers.insert(
            session_id.clone(),
            WorkerHandle {
                client,
                drain_task,
                // Empty: the initial spawn doesn't count toward the
                // restart budget. Each crash-and-respawn appends one
                // entry; budget burns when entries-in-window exceed
                // MAX_RESPAWNS_IN_WINDOW.
                restart_history: vec![],
                kind: WorkerKind::Runner {
                    spawn_config: Box::new(config),
                },
            },
        );
        drop(workers);
        // Wake any wait_for_worker parked on this session. The drop
        // above made the WorkerHandle observable to a fresh lock; the
        // notify ensures a parked waiter recheck happens within a
        // scheduler tick instead of on the next 50 ms poll.
        self.worker_notify.notify_waiters();

        // Honor the wizard's "Auto-approve" / profile `yolo_mode_default`
        // by switching the ACP session to the adapter's bypass mode. The
        // tmux path achieves the same with `--dangerously-skip-permissions`
        // (see `apply_yolo_mode()` in `src/session/instance.rs`); structured view
        // can't pass CLI flags through the ACP adapter, so we set the
        // mode via `session/set_mode` instead. The mode id is adapter-specific
        // (claude: `bypassPermissions`, codex: `agent-full-access`, gemini: `yolo`),
        // so resolve it from the agent profile rather than hard-coding Claude's
        // id; codex advertises `agent-full-access`, not `bypassPermissions`, so
        // a hard-coded or stale id is silently dropped by the
        // not-advertised guard and left codex sessions in their default
        // (approval-prompting) preset. Best-effort: the call is
        // fire-and-forget through cmd_tx, the connection loop warns on
        // failure, and adapters with no known bypass mode (`yolo_mode_id:
        // None`) stay in default. See #1142.
        if let Some(client) = client_for_yolo {
            if let Some(mode_id) = super::agent_profiles::resolve(&agent).yolo_mode_id {
                if let Err(e) = client.set_mode(mode_id).await {
                    warn!(
                        target: "acp.supervisor",
                        session = %session_id,
                        "set_mode({mode_id}) after spawn failed: {e}"
                    );
                }
            }
        }
        Ok(())
    }

    /// Drain events from a worker into the broadcast sink. When the
    /// inbound channel closes (subprocess exit / transport break) the
    /// drain task asks the supervisor to respawn the worker, falling
    /// back to a parked-error state if the restart budget is burned.
    fn start_drain_task(
        &self,
        session_id: String,
        initial_inbound: mpsc::Receiver<Event>,
    ) -> JoinHandle<()> {
        let sink = Arc::clone(&self.sink);
        let workers = Arc::clone(&self.workers);
        let next_seqs = Arc::clone(&self.next_seqs);
        let incompatible_binaries = Arc::clone(&self.incompatible_binaries);
        crate::task_util::spawn_supervised(
            "supervisor.drain",
            crate::task_util::PanicPolicy::Log,
            async move {
                let mut inbound = initial_inbound;
                loop {
                    // Tracks whether the connection task ended because the
                    // cancel-escalation watchdog declared the agent
                    // unresponsive (see acp_client.rs's CANCEL_ESCALATION_GRACE)
                    // OR because the silent-orphan watchdog detected the
                    // adapter dropped PromptResponse (see #1240). Both
                    // failure modes need the same recovery: SIGTERM the
                    // wedged runner before respawning so the next
                    // `session/load` doesn't attach to the same wedged
                    // process. The Stopped reason in the published event
                    // preserves the distinction; this flag only gates the
                    // local kill behavior.
                    let mut agent_unresponsive = false;
                    // Set when the connection task signals a non-crash exit
                    // due to a provider quota / rate-limit hit. The acp_client
                    // classifies `errorKind == "rate_limit"` from the adapter
                    // and emits `Stopped { reason: "rate_limited" }` before
                    // letting the loop end. Respawning the runner immediately
                    // would hit the same limit on the next `session/prompt`
                    // and burn restart budget for nothing, so the drain task
                    // short-circuits `restart_decision` and removes the
                    // worker handle. The user retries explicitly via
                    // `/acp/spawn` after reset, or hands off to a
                    // different ACP backend via `/acp/switch-agent`. See
                    // #1281.
                    let mut rate_limited = false;
                    while let Some(event) = inbound.recv().await {
                        if let Event::Stopped { reason } = &event {
                            if reason == "agent_unresponsive"
                                || reason == "prompt_orphaned"
                                || reason == "user_forced"
                            {
                                // `user_forced` is the explicit "Force stop":
                                // same recovery as a wedged agent (kill the
                                // worker process group, respawn, session/load).
                                // See #1727.
                                agent_unresponsive = true;
                            } else if reason == "rate_limited" {
                                rate_limited = true;
                            }
                        }
                        // Mirror the agent-assigned id into the cached
                        // spawn_config so a subsequent crash respawn picks
                        // up the latest id and calls session/load instead
                        // of session/new. Mirror SessionContextReset the
                        // other way so a load failure on this run doesn't
                        // keep retrying the same dead id on the next
                        // respawn.
                        match &event {
                            Event::AcpSessionAssigned { acp_session_id } => {
                                let mut guard = workers.lock().await;
                                if let Some(handle) = guard.get_mut(&session_id) {
                                    if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                                        info!(
                                            target: "acp.supervisor",
                                            session = %session_id,
                                            acp_session_id = %acp_session_id,
                                            "caching agent-assigned id for future respawn"
                                        );
                                        spawn_config.stored_acp_session_id =
                                            Some(acp_session_id.clone());
                                        // Import replay is one-shot: only the
                                        // first successful session/load needs
                                        // it. Clear it so an automatic respawn
                                        // (crash/drain) suppresses replay against
                                        // the now-populated event store instead
                                        // of duplicating the transcript. See
                                        // #2276.
                                        spawn_config.seed_history_replay = false;
                                    }
                                }
                                // Mirror into the on-disk registry so a fresh
                                // `aoe serve` after a daemon restart issues
                                // `session/load` instead of `session/new`.
                                super::worker_registry::update_stored_acp_session_id(
                                    &session_id,
                                    Some(acp_session_id),
                                );
                            }
                            Event::SessionContextReset { reason } => {
                                let mut guard = workers.lock().await;
                                if let Some(handle) = guard.get_mut(&session_id) {
                                    if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                                        info!(
                                            target: "acp.supervisor",
                                            session = %session_id,
                                            %reason,
                                            "clearing cached id after session/load failure"
                                        );
                                        spawn_config.stored_acp_session_id = None;
                                    }
                                }
                                super::worker_registry::update_stored_acp_session_id(
                                    &session_id,
                                    None,
                                );
                            }
                            _ => {}
                        }
                        let seq = next_seq(&next_seqs, &session_id);
                        sink.publish(&session_id, seq, &event);
                    }

                    // Channel closed: the agent's connection task ended.
                    // Either the subprocess exited or the transport broke.
                    // Try to respawn within the restart budget; otherwise
                    // park the session with a synthetic error event.
                    warn!(
                        target: "acp.supervisor",
                        session = %session_id,
                        agent_unresponsive,
                        "drain channel closed (agent connection task ended); evaluating respawn"
                    );
                    // The connection task observed the cancel-escalation
                    // watchdog fire: the agent ignored `session/cancel` for
                    // CANCEL_ESCALATION_GRACE while a prompt was in flight.
                    // The runner subprocess is still alive but wedged on a
                    // tool call the agent never cancelled, and the next
                    // `AcpClient::spawn` reuses the same UNIX socket path
                    // (`<workers_dir>/<session_id>.sock`), so a respawn
                    // before the old runner exits either binds against a
                    // collided socket or reconnects to the wedged process.
                    //
                    // Sequence here:
                    //   1. SIGTERM the old PID.
                    //   2. Poll for PID death + socket file removal (cap 3s).
                    //   3. SIGKILL if the wedged runner is still alive past
                    //      the SIGTERM grace.
                    //   4. Best-effort `remove_file` on the socket so the
                    //      respawn binds cleanly.
                    //
                    // Do NOT call `terminate_runner_for_session` here: that
                    // helper deletes the worker_registry entry, which
                    // makes `restart_decision` interpret it as a
                    // user-initiated stop and skip the respawn. See #1196.
                    if agent_unresponsive {
                        #[cfg(unix)]
                        {
                            use nix::sys::signal::{killpg, Signal};
                            use nix::unistd::Pid;
                            let old_pid = super::worker_registry::load(&session_id)
                                .ok()
                                .flatten()
                                .map(|r| r.pid);
                            if let Some(pid) = old_pid {
                                // The runner is spawned via `setsid`, so it
                                // leads its own process group (PGID == PID).
                                // Signal the whole GROUP, not just the runner
                                // PID, so a tool the agent ran internally (a
                                // monitor/until loop spawned as a grandchild
                                // of claude-agent-acp) dies too instead of
                                // surviving the restart orphaned. See #1727.
                                if super::worker_registry::is_pid_alive(pid) {
                                    info!(
                                        target: "acp.supervisor",
                                        session = %session_id,
                                        pid,
                                        "SIGTERM wedged runner process group before respawn (agent_unresponsive)"
                                    );
                                    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGTERM);
                                }
                                // Poll for the runner to exit before
                                // proceeding to respawn. ~3s budget, 100ms
                                // tick. claude-agent-acp's shutdown path
                                // is fast in practice; if it's truly
                                // unkillable by SIGTERM we escalate to
                                // SIGKILL below.
                                for _ in 0..30 {
                                    if !super::worker_registry::is_pid_alive(pid) {
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_millis(100)).await;
                                }
                                if super::worker_registry::is_pid_alive(pid) {
                                    warn!(
                                        target: "acp.supervisor",
                                        session = %session_id,
                                        pid,
                                        "wedged runner survived SIGTERM grace; escalating to SIGKILL"
                                    );
                                    let _ = killpg(Pid::from_raw(pid as i32), Signal::SIGKILL);
                                    // One more brief tick for the kernel
                                    // to reap and the socket inode to
                                    // drop. We don't loop forever; spawn
                                    // will surface its own error if the
                                    // process is somehow still around.
                                    tokio::time::sleep(Duration::from_millis(200)).await;
                                }
                            }
                            if let Ok(socket_path) =
                                super::worker_registry::socket_path_for(&session_id)
                            {
                                if socket_path.exists() {
                                    let _ = std::fs::remove_file(&socket_path);
                                }
                            }
                        }
                        // Acp's runner transport is UNIX-socket-only today
                        // (see `worker_registry::socket_path_for`), so a
                        // non-unix daemon cannot reach this branch in practice.
                        // Warn if it ever does so the assumption is loud.
                        #[cfg(not(unix))]
                        {
                            warn!(
                                target: "acp.supervisor",
                                session = %session_id,
                                "agent_unresponsive escalation on non-unix: wedged runner kill not implemented; respawn may collide on the runner socket"
                            );
                        }
                    }
                    // Rate-limit park: the connection task already emitted
                    // RateLimit + Stopped{rate_limited}. Skip restart_decision
                    // entirely so the restart budget stays whole, no synthetic
                    // AgentStartupError gets published, and the WorkerHandle
                    // is dropped so a follow-up `/acp/spawn` (or the new
                    // `/acp/switch-agent` path) doesn't see AlreadyRunning.
                    // See #1281.
                    if rate_limited {
                        info!(
                            target: "acp.supervisor",
                            session = %session_id,
                            "rate-limited; dropping worker handle without respawn"
                        );
                        let mut guard = workers.lock().await;
                        guard.remove(&session_id);
                        return;
                    }
                    let mut respawn_config: SpawnConfig =
                        match restart_decision(&workers, &session_id).await {
                            RestartDecision::Respawn(cfg) => {
                                info!(
                                    target: "acp.supervisor",
                                    session = %session_id,
                                    command = %cfg.spec.command,
                                    stored_id = ?cfg.stored_acp_session_id,
                                    "respawn approved; sleeping {}ms before restart",
                                    RESPAWN_BACKOFF.as_millis()
                                );
                                *cfg
                            }
                            RestartDecision::BudgetBurned => {
                                warn!(
                                    target: "acp.supervisor",
                                    session = %session_id,
                                    max_respawns = MAX_RESPAWNS_IN_WINDOW,
                                    window_secs = RESTART_WINDOW.as_secs(),
                                    "restart budget burned; parking session"
                                );
                                let seq = next_seq(&next_seqs, &session_id);
                                sink.publish(
                                    &session_id,
                                    seq,
                                    &Event::AgentStartupError {
                                        message: format!(
                                            "ACP agent crashed more than {} times in {}s; \
                                     not respawning. Use the web dashboard to retry.",
                                            MAX_RESPAWNS_IN_WINDOW,
                                            RESTART_WINDOW.as_secs()
                                        ),
                                    },
                                );
                                // Remove the dead WorkerHandle so a retry
                                // (POST /api/sessions/:id/acp/spawn) doesn't
                                // hit AlreadyRunning. The seq counter and replay
                                // buffer survive so the retry's events stay
                                // monotonic and the user keeps the conversation
                                // log up to the crash point.
                                let mut guard = workers.lock().await;
                                guard.remove(&session_id);
                                return;
                            }
                            RestartDecision::Gone => {
                                // The worker entry was removed (shutdown / delete).
                                // Exit quietly.
                                return;
                            }
                            RestartDecision::UserStopped => {
                                info!(
                                    target: "acp.supervisor",
                                    session = %session_id,
                                    "worker registry deleted by user (`aoe acp stop|kill`); \
                                     dropping WorkerHandle without respawn"
                                );
                                // Emit a Stopped so the UI clears any
                                // "thinking" indicator the user might have
                                // been staring at when they ran `aoe acp
                                // stop`. The reconciler will spawn a fresh
                                // worker on its next tick if the session is
                                // still structured_view.
                                let seq = next_seq(&next_seqs, &session_id);
                                sink.publish(
                                    &session_id,
                                    seq,
                                    &Event::Stopped {
                                        reason: "user_stopped".into(),
                                    },
                                );
                                let mut guard = workers.lock().await;
                                guard.remove(&session_id);
                                return;
                            }
                        };

                    tokio::time::sleep(RESPAWN_BACKOFF).await;

                    // Re-resolve the MCP layers rather than reusing the list
                    // cached at first spawn: edits to the agent's native config,
                    // `<app_dir>/mcp.json`, the per-profile `mcp.json`, or the
                    // trusted project-local `.mcp.json` made since then are
                    // forwarded on `session/load` too, so a respawn must pick them
                    // up. The project-local trust gate runs here as well, so an
                    // edited `.mcp.json` is re-locked until the repo is re-trusted.
                    let mcp_agent = respawn_config.agent_key.clone();
                    let mcp_session = session_id.clone();
                    let mcp_profile = respawn_config.source_profile.clone();
                    let mcp_cwd = respawn_config.cwd.clone();
                    respawn_config.mcp_servers = tokio::task::spawn_blocking(move || {
                        resolve_mcp_layers(
                            &mcp_agent,
                            &mcp_session,
                            mcp_profile.as_deref(),
                            &mcp_cwd,
                        )
                    })
                    .await
                    .unwrap_or_else(|e| {
                        warn!(
                            target: "acp.mcp",
                            session = %session_id,
                            error = %e,
                            "MCP re-resolution on respawn failed; forwarding no servers"
                        );
                        Vec::new()
                    });

                    let acp_session_id = AcpSessionId(session_id.clone());
                    let mut new_client =
                        match AcpClient::spawn(respawn_config.clone(), acp_session_id).await {
                            Ok(c) => c,
                            Err(e) => {
                                warn!(
                                    target: "acp.supervisor",
                                    session = %session_id,
                                    "respawn failed: {e}"
                                );
                                // If the respawn was rejected by the
                                // compatibility check (e.g. operator
                                // downgraded the adapter under us), surface
                                // the structured detail so the UI lands on
                                // the StartupErrorScreen instead of the
                                // generic red banner. Then tear down the
                                // stale runner before dropping the worker
                                // entry.
                                if let AcpError::IncompatibleAgent(payload) = &e {
                                    lock_recover(&incompatible_binaries).insert(
                                        session_id.clone(),
                                        respawn_config.spec.command.clone(),
                                    );
                                    let seq = next_seq(&next_seqs, &session_id);
                                    sink.publish(
                                        &session_id,
                                        seq,
                                        &Event::IncompatibleAgent {
                                            detail: payload.detail.clone(),
                                        },
                                    );
                                    let seq = next_seq(&next_seqs, &session_id);
                                    sink.publish(
                                        &session_id,
                                        seq,
                                        &Event::AgentStartupError {
                                            message: payload.message.clone(),
                                        },
                                    );
                                    terminate_runner_for_session(&session_id);
                                } else {
                                    let seq = next_seq(&next_seqs, &session_id);
                                    sink.publish(
                                        &session_id,
                                        seq,
                                        &Event::AgentStartupError {
                                            message: format!("ACP agent respawn failed: {e}"),
                                        },
                                    );
                                }
                                // Drop the dead WorkerHandle so the user can
                                // retry via POST /api/sessions/:id/acp/spawn
                                // without hitting AlreadyRunning. Without this
                                // the entry sticks around with a closed cmd_tx
                                // and every send_prompt fails until the daemon
                                // restarts. Mirrors the BudgetBurned and
                                // missing-inbound branches.
                                let mut guard = workers.lock().await;
                                guard.remove(&session_id);
                                return;
                            }
                        };
                    let new_inbound = match new_client.take_inbound() {
                        Some(rx) => rx,
                        None => {
                            // Belt-and-braces: AcpClient::spawn pairs the
                            // inbound receiver with the client today, so
                            // this branch never fires. Logging instead of
                            // panicking guards the daemon if a future
                            // refactor breaks the invariant.
                            warn!(
                                target: "acp.supervisor",
                                session = %session_id,
                                "respawned client missing inbound receiver; parking",
                            );
                            let seq = next_seq(&next_seqs, &session_id);
                            sink.publish(
                                &session_id,
                                seq,
                                &Event::AgentStartupError {
                                    message: "respawned ACP client had no inbound channel".into(),
                                },
                            );
                            let mut guard = workers.lock().await;
                            guard.remove(&session_id);
                            return;
                        }
                    };

                    {
                        let mut guard = workers.lock().await;
                        let Some(handle) = guard.get_mut(&session_id) else {
                            return;
                        };
                        handle.client = Arc::new(new_client);
                    }

                    info!(
                        target: "acp.supervisor",
                        session = %session_id,
                        "structured view worker respawned"
                    );
                    lock_recover(&incompatible_binaries).remove(&session_id);
                    inbound = new_inbound;
                }
            },
        )
    }

    /// Wait until the worker for `session_id` is fully spawned, or the
    /// pending spawn drops out (failed/cancelled), or `deadline` elapses.
    /// Returns true if the worker is now in the map.
    ///
    /// Hooks for the prompt/cancel/set_mode REST handlers: the user can
    /// click Send right after enabling structured view, while `Supervisor::spawn`
    /// is still in the 2-3s ACP handshake. Without this wait, those
    /// requests would 404 because the WorkerHandle isn't in `workers`
    /// yet, even though it's about to be.
    ///
    /// Uses `tokio::sync::Notify` for edge triggered wakeups instead
    /// of polling. The previous shape woke every 50 ms, which added
    /// up to 50 ms of avoidable latency on the request path that
    /// triggers `send_prompt` immediately after a session spawn. The
    /// double-check pattern (subscribe to `notified()` BEFORE peeking
    /// the maps) prevents lost wakeups: if the spawn finishes between
    /// the peek and the await, the notify is buffered and the await
    /// returns immediately.
    async fn wait_for_worker(&self, session_id: &str, deadline: std::time::Duration) -> bool {
        let started = std::time::Instant::now();
        loop {
            let notified = self.worker_notify.notified();
            tokio::pin!(notified);

            if self.workers.lock().await.contains_key(session_id) {
                return true;
            }
            // No worker yet. If a resume (spawn or attach) is in
            // flight, wait for it; otherwise the worker is not coming
            // and we should fail fast rather than burn the deadline.
            if !lock_recover(&self.pending_resumes).contains_key(session_id) {
                return false;
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return false;
            }
            if tokio::time::timeout(remaining, &mut notified)
                .await
                .is_err()
            {
                return false;
            }
        }
    }

    /// Resolve a `session_id` to its `AcpClient`, holding `self.workers`
    /// only long enough to clone the `Arc<AcpClient>` so the caller can
    /// `.await` on the agent without serializing every other supervisor
    /// operation behind that lock. Centralizing this also routes every
    /// caller through the same `UnknownSession` error variant.
    async fn client_for_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        let workers = self.workers.lock().await;
        workers
            .get(session_id)
            .map(|h| Arc::clone(&h.client))
            .ok_or_else(|| SupervisorError::UnknownSession(session_id.into()))
    }

    /// Send a user prompt (with optional attachments) to a running
    /// structured view worker.
    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        attachments: &[crate::acp::event_store::AttachmentBlob],
    ) -> Result<(), SupervisorError> {
        self.wait_for_worker(session_id, std::time::Duration::from_secs(10))
            .await;
        let client = self.client_for_session(session_id).await?;
        client.send_prompt(text, attachments).await?;
        Ok(())
    }

    /// Cancel the current turn for a running structured view worker. Best-effort:
    /// returns Ok if the worker exists even when no turn is in flight.
    pub async fn cancel_prompt(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.wait_for_worker(session_id, std::time::Duration::from_secs(10))
            .await;
        let client = self.client_for_session(session_id).await?;
        client.cancel_prompt().await?;
        Ok(())
    }

    /// User-initiated "Force stop". Two failure modes to cover:
    ///
    /// 1. A turn is genuinely in flight and the agent is ignoring
    ///    `session/cancel` (a monitor/until loop). `force_cancel` ends the
    ///    turn with `Stopped { reason: "user_forced" }` through the drain
    ///    task, which kills the worker process group and respawns with
    ///    `session/load`. This is what actually stops the runaway loop.
    /// 2. The daemon already finished the turn but the UI is wedged on a
    ///    `Stopped` it never saw (#1100). The synthetic `Stopped` published
    ///    below frees `turnActive` for every connected UI immediately.
    ///
    /// Both are best-effort and idempotent: the synthetic `Stopped` bypasses
    /// the drain (so it never triggers a restart on its own), and a second
    /// `Stopped` is a capped no-op for the reducer. See #1727 / #1100.
    pub async fn force_end_turn(&self, session_id: &str) {
        if let Ok(client) = self.client_for_session(session_id).await {
            let _ = client.force_cancel().await;
        }
        let seq = next_seq(&self.next_seqs, session_id);
        self.sink.publish(
            session_id,
            seq,
            &Event::Stopped {
                reason: "user_forced".into(),
            },
        );
    }

    /// Set the active session mode via ACP session/set_mode.
    pub async fn set_mode(&self, session_id: &str, mode_id: &str) -> Result<(), SupervisorError> {
        self.wait_for_worker(session_id, std::time::Duration::from_secs(10))
            .await;
        let client = self.client_for_session(session_id).await?;
        client.set_mode(mode_id).await?;
        Ok(())
    }

    /// Set a per-session selector via ACP session/set_config_option.
    /// Delegates to the per-session AcpClient. See #1403.
    pub async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) -> Result<(), SupervisorError> {
        self.wait_for_worker(session_id, std::time::Duration::from_secs(10))
            .await;
        let client = self.client_for_session(session_id).await?;
        client.set_config_option(config_id, value).await?;
        Ok(())
    }

    /// Resolve a pending approval.
    pub async fn resolve_permission(
        &self,
        session_id: &str,
        nonce: Nonce,
        decision: ApprovalDecision,
    ) -> Result<(), SupervisorError> {
        let client = self.client_for_session(session_id).await?;
        client.resolve_permission(nonce, decision).await?;
        Ok(())
    }

    /// Resolve a pending `AskUserQuestion` elicitation by nonce, unblocking
    /// the parked `elicitation/create` callback with the user's answer.
    pub async fn resolve_elicitation(
        &self,
        session_id: &str,
        nonce: Nonce,
        resolution: ElicitationResolution,
    ) -> Result<(), SupervisorError> {
        let client = self.client_for_session(session_id).await?;
        client.resolve_elicitation(nonce, resolution).await?;
        Ok(())
    }

    /// Shutdown a single structured view worker, preserving its agent-side
    /// transcript so the next respawn can resume it via `session/load`.
    ///
    /// This is the temporary-teardown path: structured view stop, snooze,
    /// archive, idle auto-stop, and supersede all funnel here. They are
    /// reversible, so we must NOT fire `session/delete` (which deletes
    /// the agent's on-disk transcript); doing so left every snooze /
    /// archive / idle-stop unable to resume, resetting context on the
    /// next prompt (#1710). For permanent removal use
    /// [`Self::shutdown_and_delete`].
    pub async fn shutdown(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.shutdown_with_reason(session_id, "user_stopped", false)
            .await
    }

    /// Like `shutdown`, but tags the synthetic `Stopped` event with
    /// `reason: "idle_auto_stop"` so the structured view timeline shows the
    /// worker was reclaimed for inactivity rather than user-stopped.
    /// Used by the reconciler's idle-reap pass (#1689). Seamless: no UI
    /// banner, the next prompt respawns the worker. Preserves the
    /// agent transcript so that respawn resumes instead of resetting.
    pub async fn shutdown_idle(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.shutdown_with_reason(session_id, "idle_auto_stop", false)
            .await
    }

    /// Shutdown a worker AND release its agent-side persisted state via
    /// the experimental `session/delete` RPC. Use only when the session
    /// is being permanently discarded (session delete, or disabling
    /// structured view mode), never for reversible teardown, which must keep the
    /// transcript resumable. See #1710 and `shutdown`.
    pub async fn shutdown_and_delete(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.shutdown_with_reason(session_id, "user_stopped", true)
            .await
    }

    async fn shutdown_with_reason(
        &self,
        session_id: &str,
        stop_reason: &str,
        delete_adapter_state: bool,
    ) -> Result<(), SupervisorError> {
        // Hold workers + pending_resumes simultaneously so the spawn
        // can't observe an empty workers map, finish the handshake,
        // and insert a WorkerHandle while we're walking through this
        // function. Lock order matches `spawn`: workers, then pending.
        let mut workers = self.workers.lock().await;
        let pending_has_it = lock_recover(&self.pending_resumes).contains_key(session_id);
        if let Some(handle) = workers.remove(session_id) {
            // Worker is alive — tear it down.
            drop(workers);
            // Best-effort experimental `session/delete` RPC before
            // SIGTERM. Adapters that advertise
            // `sessionCapabilities.delete: {}` (claude-agent-acp >= 0.36)
            // release adapter-side persisted state here, deleting the
            // agent's on-disk transcript. Other adapters return -32601
            // and we proceed to the existing kill path either way.
            // Bounded by `ACP_SESSION_DELETE_TIMEOUT` inside
            // `delete_session` (~2s) so a wedged adapter cannot stall
            // the caller. See #1404.
            //
            // Gated on `delete_adapter_state`: only permanent removal
            // (session delete / disabling structured view) deletes the
            // transcript. Reversible teardown (snooze, archive, idle
            // auto-stop, stop, supersede) must keep it so the next
            // respawn resumes via `session/load` instead of resetting
            // the conversation. See #1710.
            if delete_adapter_state {
                try_session_delete(&handle.client, session_id).await;
            }
            let _ = handle.client.shutdown().await;
            handle.drain_task.abort();
            // SIGTERM the runner (if there is one) so the agent
            // subprocess dies; the runner cleans up its own files but
            // we also delete the registry entry here to handle the
            // case where the runner is wedged.
            terminate_runner_for_session(session_id);
            // Publish `Stopped` so the UI clears any "thinking" state
            // and renders the reconnect banner immediately, instead of
            // waiting for the next reap tick. Skipped for stdio test
            // fixtures since they have no UI to update and the seq
            // counter is shared with respawn-budget tests.
            let should_publish = match &handle.kind {
                WorkerKind::Runner { .. } | WorkerKind::Attached => true,
                #[cfg(test)]
                WorkerKind::Stdio => false,
            };
            if should_publish {
                let seq = next_seq(&self.next_seqs, session_id);
                self.sink.publish(
                    session_id,
                    seq,
                    &Event::Stopped {
                        reason: stop_reason.into(),
                    },
                );
            }
            return Ok(());
        }
        // No in-memory worker, but there may still be a detached
        // runner in the registry (e.g. a previous daemon detached and
        // shutdown is called against the disk-only entry).
        if super::worker_registry::load(session_id)
            .ok()
            .flatten()
            .is_some()
        {
            // If a resume is mid-handshake against this same runner,
            // the SIGTERM below races the handshake; mark the session
            // so attach's (or spawn's) pre-insert check bails instead
            // of installing a worker pointing at a dying agent. Set
            // the breadcrumb under the workers lock so the racing
            // resume that re-acquires workers cannot observe an empty
            // cancelled_resumes between our drop and its read.
            // Sibling test: `shutdown_holds_workers_lock_across_cancelled_resumes_seed`.
            if pending_has_it {
                lock_recover(&self.cancelled_resumes).insert(session_id.to_string());
            }
            drop(workers);
            terminate_runner_for_session(session_id);
            return Ok(());
        }
        if pending_has_it {
            // Resume is mid-handshake. Mark it cancelled so the
            // resume's pre-insert check (in `spawn` or `attach`)
            // bails instead of installing an orphaned worker. Insert
            // under the workers lock so a resume that re-acquires
            // workers cannot observe an empty cancelled_resumes
            // between our drop and its read. The reservation cleanup
            // (ResumeReservation::Drop) clears `pending_resumes` on
            // exit, so we don't have to.
            // Sibling test: `shutdown_holds_workers_lock_across_cancelled_resumes_seed`.
            lock_recover(&self.cancelled_resumes).insert(session_id.to_string());
            drop(workers);
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                "shutdown: resume in flight; marked for cancellation"
            );
            return Ok(());
        }
        Err(SupervisorError::UnknownSession(session_id.into()))
    }

    /// Shutdown every worker. Called when the user explicitly terminates
    /// all structured view workers (e.g. `aoe acp stop --all`); sends ACP
    /// shutdown to each connected client, aborts the drain task, AND
    /// signals every per-session `aoe __acp-runner` so the agent
    /// subprocess dies. For the everyday `aoe serve --stop` flow, use
    /// `detach_all` instead so workers outlive the daemon.
    pub async fn shutdown_all(&self) {
        let registry_pids: Vec<(String, u32)> = super::worker_registry::list()
            .unwrap_or_default()
            .into_iter()
            .map(|r| (r.session_id, r.pid))
            .collect();

        let drained: Vec<(String, WorkerHandle)> = {
            let mut workers = self.workers.lock().await;
            workers.drain().collect()
        };
        for (id, handle) in drained {
            debug!(target: "acp.supervisor", session = %id, "shutting down");
            let _ = handle.client.shutdown().await;
            handle.drain_task.abort();
        }

        // Group-SIGTERM every runner we knew about, so detached agents that
        // outlived a previous daemon (and their node/SDK grandchildren) are
        // also taken down by an explicit "kill them all" request, not left
        // orphaned under PID 1. See #1689.
        for (session_id, pid) in registry_pids {
            crate::process::worker::terminate_process_group(pid);
            super::worker_registry::delete(&session_id).ok();
        }
        #[cfg(not(unix))]
        let _ = registry_pids;
    }

    /// Drop the daemon-side handle to every worker without killing the
    /// runner or its agent. Used on `aoe serve` graceful shutdown so the
    /// agents keep running and the next `aoe serve` reattaches.
    ///
    /// Concretely: closes the unix-socket connection (via `client
    /// .shutdown()` which sends `ClientCmd::Shutdown` to the connection
    /// task), aborts the drain task, and writes `detached_at` into each
    /// registry entry. The runner observes EOF on its socket read,
    /// clears its active outbound, and goes back to accepting.
    pub async fn detach_all(&self) {
        let drained: Vec<(String, WorkerHandle)> = {
            let mut workers = self.workers.lock().await;
            let drained: Vec<(String, WorkerHandle)> = workers.drain().collect();
            info!(
                target: "acp.supervisor",
                count = drained.len(),
                "detaching structured view workers; they continue running. \
                 Use `aoe acp stop` to terminate."
            );
            drained
        };
        for (id, handle) in drained {
            debug!(target: "acp.supervisor", session = %id, "detaching");
            let _ = handle.client.shutdown().await;
            handle.drain_task.abort();
            super::worker_registry::mark_detached(&id);
        }
    }

    /// Reattach to an already-running worker by dialing its existing
    /// runner socket. Used by `reconcile_acp_workers` on `aoe serve`
    /// startup before falling back to a fresh spawn.
    ///
    /// `in_flight_turn` should be true when the on-disk event store
    /// shows the session was mid-prompt at the moment the previous
    /// daemon detached. It arms a watchdog in the connection task that
    /// emits a synthetic `Event::Stopped { reason: "reattach_idle" }`
    /// after a quiet window, so the UI's "thinking" indicator clears
    /// even though the agent's eventual response to the orphaned
    /// prompt is dropped silently by the underlying transport (its
    /// request id was issued by the previous daemon's client and is
    /// unknown to this one).
    pub async fn attach(
        &self,
        session_id: String,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        in_flight_turn: bool,
        sandbox: Option<SandboxInfo>,
    ) -> Result<(), SupervisorError> {
        // Reserve a `pending_resumes` slot for the duration of the
        // attach so the UI shows "Resuming…" while the socket dial +
        // resume handshake runs, AND the capacity check in a concurrent
        // `spawn` sees this id and avoids over-allocating. RAII guard
        // removes the entry on every exit path. Reserving before the
        // registry probe keeps the `(workers ∪ pending)` set atomic.
        let _reservation = match self.begin_resume(&session_id, ResumeKind::Attach).await? {
            ResumeReservationOutcome::Reserved(r) => r,
            ResumeReservationOutcome::AlreadyPresent => {
                return Err(SupervisorError::AlreadyRunning(session_id));
            }
        };

        let record = match super::worker_registry::load(&session_id)
            .map_err(|e| SupervisorError::Acp(AcpError::Spawn(format!("registry load: {e}"))))?
        {
            Some(r) if super::worker_registry::is_record_live(&r) => r,
            Some(_) | None => {
                return Err(SupervisorError::UnknownSession(session_id));
            }
        };

        // Resume requires a known ACP session id (the runner was holding
        // the agent loaded against it). If the registry doesn't carry
        // one yet, e.g. the previous daemon crashed before the first
        // `session/new` response was processed, there's nothing to
        // resume against; bail so the reconciler falls through to a
        // fresh spawn.
        let Some(stored_acp_session_id) = record.stored_acp_session_id.clone() else {
            return Err(SupervisorError::Acp(AcpError::Spawn(
                "runner registry has no stored_acp_session_id; need fresh spawn".into(),
            )));
        };

        let acp_session_id = AcpSessionId(session_id.clone());
        // Reattach: read the original profile from the persisted
        // `WorkerRecord` so `terminal/create` env resolution stays on the
        // session's actual profile across daemon restarts. Legacy records
        // written before the field existed serialize to `None`, in which
        // case `current_env_entries` warns and falls back to the global
        // default profile (matching pre-persistence behavior).
        let sandbox_resources = match sandbox {
            Some(info) => {
                // `from_info` resolves the container workdir, which touches git2
                // and (for a legacy session with no pinned workdir) shells out to
                // `docker inspect`. Run it off the async executor, mirroring how
                // `ensure_container_for_session` wraps its docker work.
                let cwd = cwd.clone();
                let profile = record.source_profile.clone();
                Some(
                    tokio::task::spawn_blocking(move || {
                        super::acp_client::SessionSandbox::from_info(&info, cwd.as_path(), profile)
                    })
                    .await
                    .map_err(|e| {
                        AcpError::Spawn(format!("sandbox resolve task panicked: {e}"))
                    })??,
                )
            }
            None => None,
        };
        // Prefer the persisted registry key; fall back to the legacy
        // `agent_name` field for records written before `agent_key`
        // existed. A truly stale entry without either resolves to
        // DEFAULT inside `agent_profiles::resolve`, which is the safe
        // pass-through behavior.
        let attach_agent_key = if record.agent_key.is_empty() {
            record.agent_name.clone()
        } else {
            record.agent_key.clone()
        };
        let mut client = AcpClient::attach(
            record.socket_path.clone(),
            cwd,
            additional_dirs,
            stored_acp_session_id,
            in_flight_turn,
            acp_session_id,
            sandbox_resources,
            attach_agent_key,
            record.source_profile.clone(),
        )
        .await?;
        super::worker_registry::mark_attached(&session_id);

        let inbound = client
            .take_inbound()
            .expect("freshly attached AcpClient always has inbound receiver");
        let client = Arc::new(client);
        let mut workers = self.workers.lock().await;
        if workers.contains_key(&session_id) {
            drop(workers);
            drop(client);
            return Err(SupervisorError::AlreadyRunning(session_id));
        }
        // Cancellation: a concurrent shutdown observed this session
        // mid-attach (or terminated its runner) and asked us to bail.
        // Mirrors `spawn`'s pre-insert check so the breadcrumb
        // shutdown sets in either the `pending_has_it` path or the
        // registry-terminate path is honored regardless of ResumeKind.
        if lock_recover(&self.cancelled_resumes).remove(&session_id) {
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                "attach cancelled by concurrent shutdown; dropping freshly-attached client"
            );
            drop(workers);
            drop(client);
            return Err(SupervisorError::SpawnCancelled(session_id));
        }
        let drain_task = self.start_drain_task(session_id.clone(), inbound);
        workers.insert(
            session_id.clone(),
            WorkerHandle {
                client,
                drain_task,
                restart_history: vec![],
                // Attached: if the worker dies, the drain task sees
                // EOF and we let the reconciler spawn a fresh runner
                // on the next tick rather than auto-respawning from
                // this in-memory state. Registry-backed, so user-stop
                // detection still applies.
                kind: WorkerKind::Attached,
            },
        );
        info!(
            target: "acp.supervisor",
            session = %session_id,
            socket = %record.socket_path.display(),
            pid = record.pid,
            "reattached to existing structured view worker"
        );
        drop(workers);
        self.worker_notify.notify_waiters();

        self.cancel_orphaned_approvals(&session_id);
        self.cancel_orphaned_elicitations(&session_id);
        Ok(())
    }

    /// Cancel approvals that were on screen when the previous daemon
    /// died. The responder oneshot was parked in the old daemon's
    /// `pending_responders` map and dropped with the process. Without
    /// this sweep, clicking allow/deny in the UI races the new daemon's
    /// empty map and 404s, leaving the agent wedged on the original
    /// JSON-RPC request id. Emit a synthetic `ApprovalResolved {
    /// decision: Cancelled }` per dead nonce so the frontend reducer
    /// drops the card, then a single synthetic `Stopped { reason:
    /// "approval_cancelled_on_restart" }` so the sidebar dot flips to
    /// Idle and the in-structured view "Working" spinner clears (the turn the
    /// approval was parked on is over; the agent was unblocked by a
    /// synthetic Cancelled response when the previous daemon died).
    /// The reason string is distinct so it does NOT trip the
    /// "Stopped" / "Restarting…" banners (those gate on
    /// reason="user_stopped" / "restart_pending"). The agent-side
    /// wedge (parked `session/request_permission`) is unblocked
    /// separately by the runner's outstanding-request cancellation on
    /// detach. No-op when there are no stale nonces.
    fn cancel_orphaned_approvals(&self, session_id: &str) {
        let stale_nonces = self.sink.unresolved_approval_nonces(session_id);
        if stale_nonces.is_empty() {
            return;
        }
        info!(
            target: "acp.supervisor",
            session = %session_id,
            stale = stale_nonces.len(),
            "cancelling approvals orphaned by daemon restart"
        );
        for nonce in stale_nonces {
            let seq = next_seq(&self.next_seqs, session_id);
            self.sink.publish(
                session_id,
                seq,
                &Event::ApprovalResolved {
                    nonce,
                    decision: ApprovalDecision::Cancelled,
                },
            );
        }
        let seq = next_seq(&self.next_seqs, session_id);
        self.sink.publish(
            session_id,
            seq,
            &Event::Stopped {
                reason: "approval_cancelled_on_restart".to_string(),
            },
        );
    }

    /// Cancel elicitations (AskUserQuestion) that were on screen when the
    /// previous daemon died. The parallel of [`Self::cancel_orphaned_approvals`]:
    /// the responder oneshot was parked in the old daemon's
    /// `pending_responders` map and dropped with the process, so a stale
    /// `ElicitationRequested` would otherwise replay as a dead card that
    /// 404s on submit. Publish a synthetic `ElicitationResolved { outcome:
    /// Cancelled }` per dead nonce so the reducer drops the card. The
    /// agent-side wedge is unblocked separately by the runner's
    /// outstanding-request cancellation on detach; no synthetic `Stopped`
    /// is needed here because `cancel_orphaned_approvals` already emits one
    /// when the same restart had a parked approval, and an elicitation
    /// without an approval rides whatever turn state the replay rebuilt.
    /// No-op when there are no stale nonces.
    fn cancel_orphaned_elicitations(&self, session_id: &str) {
        let stale_nonces = self.sink.unresolved_elicitation_nonces(session_id);
        if stale_nonces.is_empty() {
            return;
        }
        info!(
            target: "acp.supervisor",
            session = %session_id,
            stale = stale_nonces.len(),
            "cancelling elicitations orphaned by daemon restart"
        );
        for nonce in stale_nonces {
            let seq = next_seq(&self.next_seqs, session_id);
            self.sink.publish(
                session_id,
                seq,
                &Event::ElicitationResolved {
                    nonce,
                    outcome: ElicitationOutcome::Cancelled,
                    answers: Vec::new(),
                },
            );
        }
    }

    /// Whether this session has a running structured view worker, or a resume
    /// (spawn or attach) currently in-flight. The pending check
    /// prevents the reconciler from racing the auto-spawn-after-create
    /// path: a freshly-created structured view session takes 2-3s for the ACP
    /// handshake to insert the WorkerHandle, and during that window
    /// `workers.contains_key` is false.
    pub async fn is_running(&self, session_id: &str) -> bool {
        if self.workers.lock().await.contains_key(session_id) {
            return true;
        }
        lock_recover(&self.pending_resumes).contains_key(session_id)
    }

    /// Return the number of running workers (for the doctor + stats).
    pub async fn count(&self) -> usize {
        self.workers.lock().await.len()
    }

    /// Reap workers whose on-disk registry entry has disappeared while
    /// the in-memory `WorkerHandle` is still installed. This is the
    /// out-of-band stop signal: `aoe acp stop|kill|restart` (a
    /// separate process from the daemon) deletes the registry entry,
    /// then SIGTERMs the runner. The daemon's protocol-layer connection
    /// task blocks on `cmd_rx.recv()` while idle, so socket EOF does
    /// NOT propagate back into the closure — `event_tx` never drops,
    /// the drain task never observes inbound closure, and
    /// `restart_decision` never runs. Without an explicit poll, the UI
    /// stays stuck on "thinking" with a phantom worker recorded in the
    /// supervisor.
    ///
    /// Called by the reconciler every 2s. For each runner-managed worker
    /// whose registry entry is gone:
    ///   - if a `.restart` sentinel sits next to the (now-deleted)
    ///     registry entry, publishes `Stopped { reason:
    ///     "restart_pending" }` and reports the id back to the caller so
    ///     the reconciler can clear its `attempted` set and let the next
    ///     spawn pass run (transcript continuity via the cached
    ///     `acp_session_id`);
    ///   - otherwise publishes `Stopped { reason: "user_stopped" }` so
    ///     the frontend offers a "Reconnect" button and the daemon
    ///     stays out of the respawn business.
    ///
    /// Either way: the WorkerHandle is dropped, ACP Shutdown is sent so
    /// the connection task exits cleanly, and the drain task is aborted.
    /// The stdio-only test path is skipped because those handles have
    /// no registry entry by construction.
    ///
    /// Returns the list of restart-pending session ids so the reconciler
    /// can re-enable auto-spawn for them on the next tick.
    pub async fn reap_user_stopped(&self) -> Vec<String> {
        // Snapshot candidate session ids without holding the workers lock
        // across the registry read or the publish/teardown — the
        // teardown takes the client lock and we don't want to nest.
        let candidates: Vec<String> = {
            let workers = self.workers.lock().await;
            workers
                .iter()
                .filter(|(_, h)| matches!(h.kind, WorkerKind::Runner { .. } | WorkerKind::Attached))
                .map(|(id, _)| id.clone())
                .filter(|id| matches!(super::worker_registry::load(id), Ok(None)))
                .collect()
        };

        let mut restart_pending: Vec<String> = Vec::new();
        for id in candidates {
            let removed = self.workers.lock().await.remove(&id);
            let Some(handle) = removed else { continue };
            // Consume the restart marker (if any) and pick the publish
            // reason. The marker is cleared regardless of which branch
            // fires so a leaked file (e.g. from a CLI that crashed
            // between `mark_restart_pending` and `delete`) can't poison
            // a subsequent user-initiated stop.
            let is_restart = super::worker_registry::take_restart_marker(&id);
            let reason = if is_restart {
                "restart_pending"
            } else {
                "user_stopped"
            };
            info!(
                target: "acp.supervisor",
                session = %id,
                reason,
                "registry entry gone while worker handle live; tearing down"
            );
            let seq = next_seq(&self.next_seqs, &id);
            self.sink.publish(
                &id,
                seq,
                &Event::Stopped {
                    reason: reason.to_string(),
                },
            );
            // Send ACP Shutdown so the connection task's closure breaks
            // out of its cmd_rx loop and the underlying transport closes
            // cleanly (avoids a leaked socket fd until the daemon dies).
            let _ = handle.client.shutdown().await;
            handle.drain_task.abort();
            if is_restart {
                restart_pending.push(id);
            }
        }
        restart_pending
    }
}

/// SIGTERM the per-session runner if its registry entry has a live PID,
/// then delete the entry. Used by `shutdown` and `shutdown_all` to take
/// down detached workers explicitly.
fn terminate_runner_for_session(session_id: &str) {
    // Group-kill (runner + agent + grandchildren) then delete the entry.
    // Single-pid SIGTERM here used to orphan the agent's node/SDK children
    // under PID 1; see worker_registry::terminate and #1689.
    super::worker_registry::terminate(session_id);
}

#[derive(Debug)]
enum RestartDecision {
    // Boxed because `SpawnConfig` is significantly larger than the
    // unit variants — clippy::large_enum_variant flags the size
    // imbalance, and the indirection costs nothing on the cold-path
    // respawn flow.
    Respawn(Box<SpawnConfig>),
    BudgetBurned,
    /// The worker entry was removed (e.g. shutdown).
    Gone,
    /// The on-disk worker registry entry for this session was deleted
    /// while the in-memory WorkerHandle still exists. Signals that the
    /// user (or a peer process) explicitly stopped this worker via
    /// `aoe acp stop|kill`. The drain task removes the WorkerHandle
    /// and emits a soft `Stopped` event instead of burning the restart
    /// budget with respawns of an agent the user just terminated.
    UserStopped,
}

async fn restart_decision(
    workers: &Arc<Mutex<HashMap<String, WorkerHandle>>>,
    session_id: &str,
) -> RestartDecision {
    let mut guard = workers.lock().await;
    let Some(handle) = guard.get_mut(session_id) else {
        debug!(
            target: "acp.supervisor",
            session = %session_id,
            "restart_decision: worker entry gone (shutdown / delete)"
        );
        return RestartDecision::Gone;
    };
    // Registry-deletion signal: if the on-disk record for this session
    // was removed but we still hold a WorkerHandle, the user terminated
    // the runner externally (`aoe acp stop|kill`). Don't respawn;
    // the reconciler will handle a fresh spawn on its next tick if the
    // session is still `structured_view = true`. Returning `UserStopped`
    // both skips the respawn budget bookkeeping and lets the drain task
    // emit a non-crash `Stopped` so the UI clears any "thinking" state
    // instead of showing the budget-burned red banner.
    //
    // Both `Runner` (fresh spawn) and `Attached` (reattached to an
    // existing runner) are backed by a runner-registry entry, so the
    // registry-gone signal is meaningful for both. `Stdio` test
    // fixtures have no registry entry by construction and must be
    // skipped here so the "gone" check doesn't tear them down.
    let runner_managed = matches!(
        handle.kind,
        WorkerKind::Runner { .. } | WorkerKind::Attached
    );
    if runner_managed {
        let registry_gone = matches!(super::worker_registry::load(session_id), Ok(None));
        if registry_gone {
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                "restart_decision: registry entry gone, treating as user-initiated stop"
            );
            return RestartDecision::UserStopped;
        }
    }
    let now = Instant::now();
    let window_start = now - RESTART_WINDOW;
    let pre_count = handle.restart_history.len();
    handle.restart_history.retain(|t| *t >= window_start);
    let pruned = pre_count - handle.restart_history.len();
    handle.restart_history.push(now);
    let count = handle.restart_history.len() as u32;
    debug!(
        target: "acp.supervisor",
        session = %session_id,
        respawns_in_window = count,
        max_in_window = MAX_RESPAWNS_IN_WINDOW,
        window_secs = RESTART_WINDOW.as_secs(),
        pruned_old_entries = pruned,
        "restart_decision: tallied recent crashes"
    );
    if count > MAX_RESPAWNS_IN_WINDOW {
        return RestartDecision::BudgetBurned;
    }
    match &handle.kind {
        WorkerKind::Runner { spawn_config } => RestartDecision::Respawn(spawn_config.clone()),
        // Attached: the previous daemon owned the runner and we have
        // no spawn config to respawn from. The reconciler will pick
        // this session back up on the next tick if it's still
        // `structured_view = true`.
        WorkerKind::Attached => RestartDecision::BudgetBurned,
        // Stdio: in-proc test fixture with no subprocess to respawn.
        #[cfg(test)]
        WorkerKind::Stdio => RestartDecision::BudgetBurned,
    }
}

/// Increment and return the per-session seq counter. Lives at the
/// supervisor level so the no-worker `publish_startup_error` path
/// and the drain task share a single source of truth — otherwise
/// both used to start at seq=1 and collide in the replay buffer
/// after a retry, which the client-side dedupe then rendered as a
/// silently-lost first message.
fn next_seq(next_seqs: &SeqMap, session_id: &str) -> u64 {
    let mut guard = match next_seqs.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    let entry = guard.entry(session_id.to_string()).or_insert(0);
    *entry = entry.saturating_add(1);
    *entry
}

/// Take a `std::sync::Mutex` guard, recovering the inner data if
/// the lock is poisoned. The supervisor maps wrapped in `std::sync::Mutex`
/// only ever hold short, panic-free critical sections (HashMap inserts
/// or removes), so a poisoned lock from an unrelated panic on the same
/// state is recoverable rather than fatal.
fn lock_recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| {
        warn!(
            target: "acp.supervisor",
            "recovered poisoned supervisor lock"
        );
        e.into_inner()
    })
}

/// A `BroadcastSink` impl backed by a tokio broadcast channel. The
/// AppState in the server module wires this so structured view events flow
/// straight into the existing WebSocket fanout, and snapshots them
/// into the per-session replay buffer used by the snapshot endpoint.
///
/// The replay buffer uses a `std::sync::Mutex` so the publish path
/// stays synchronous: ordering matters (the buffer must observe seqs
/// in publish order) and `tokio::spawn` does not preserve task
/// ordering. The lock is held only long enough to push a single
/// event, which is bounded; the REST snapshot handler also takes
/// this lock briefly.
pub struct ChannelSink {
    pub tx: broadcast::Sender<crate::server::AcpBroadcastFrame>,
    /// Disk-backed event log. The single source of truth for replay:
    /// the WS-on-connect drain, the `/acp/replay` REST endpoint,
    /// and the supervisor's startup `hydrate_seqs` all read from here.
    /// Each publish has a monotonic seq from `Supervisor::next_seqs`
    /// which is hydrated from this store at startup, so seqs survive
    /// `aoe serve` restart without coordination.
    pub event_store: Arc<crate::acp::event_store::EventStore>,
}

impl BroadcastSink for ChannelSink {
    fn publish(&self, session_id: &str, seq: u64, event: &Event) {
        let _ = self.publish_persisted(session_id, seq, event);
    }

    fn clear_session_events(&self, session_id: &str) {
        self.event_store.delete_session(session_id);
    }

    fn publish_persisted(&self, session_id: &str, seq: u64, event: &Event) -> bool {
        // Persist FIRST so a disk failure can be surfaced before
        // broadcast subscribers see an event the on-disk log doesn't
        // have. If the write fails the seq is already burned (the
        // caller allocated it via next_seq), so we publish a typed
        // gap event in its place — the frontend reducer can render a
        // "history truncated at seq N" notice and the user can
        // reload to recover via the `/acp/replay` endpoint.
        //
        // Wrap the synchronous rusqlite write in `block_in_place` so
        // the multi-thread runtime can migrate other tasks off this
        // worker for the duration of the fsync. Ordering is preserved
        // because the call is still synchronous from the caller's
        // perspective; switching to `spawn_blocking` would break the
        // "publish in seq order" contract that the on-disk replay
        // relies on. `block_in_place` panics on `current_thread`, so
        // tests (which default to that flavor) fall back to a direct
        // call. The daemon runs on `#[tokio::main]` default which is
        // `multi_thread` and gets the runtime aware variant.
        let event_to_publish: Event;
        let record_result = match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor())
        {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
                tokio::task::block_in_place(|| self.event_store.record(session_id, seq, event))
            }
            _ => self.event_store.record(session_id, seq, event),
        };
        let persisted = record_result.is_ok();
        let event_ref: &Event = match record_result {
            Ok(()) => event,
            Err(e) => {
                tracing::warn!(
                    target: "acp.event_store",
                    session = %session_id,
                    seq,
                    "event store write failed; substituting AgentStartupError so the gap is visible: {e}"
                );
                event_to_publish = Event::AgentStartupError {
                    message: format!("event store write failed at seq {seq}: {e}"),
                };
                &event_to_publish
            }
        };

        let frame = crate::server::AcpBroadcastFrame {
            session_id: session_id.to_string(),
            seq,
            event: Arc::new(event_ref.clone()),
        };
        let _ = self.tx.send(frame);
        persisted
    }

    fn unresolved_approval_nonces(&self, session_id: &str) -> Vec<Nonce> {
        self.event_store.unresolved_approval_nonces(session_id)
    }

    fn unresolved_elicitation_nonces(&self, session_id: &str) -> Vec<Nonce> {
        self.event_store.unresolved_elicitation_nonces(session_id)
    }

    fn record_attachment(
        &self,
        session_id: &str,
        seq: u64,
        blob: &crate::acp::event_store::AttachmentBlob,
    ) -> bool {
        match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(|| {
                self.event_store.record_attachment(session_id, seq, blob)
            }),
            _ => self.event_store.record_attachment(session_id, seq, blob),
        }
    }

    fn delete_attachments_for_seq(&self, session_id: &str, seq: u64) {
        match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
            Ok(tokio::runtime::RuntimeFlavor::MultiThread) => {
                tokio::task::block_in_place(|| {
                    self.event_store.delete_attachments_for_seq(session_id, seq)
                });
            }
            _ => self.event_store.delete_attachments_for_seq(session_id, seq),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(command: &str, args: &[&str]) -> AgentSpec {
        AgentSpec {
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            description: "test".into(),
            env_allowlist: None,
        }
    }

    fn ovr(tool: &str, command: &str) -> AgentCommandOverride {
        AgentCommandOverride {
            logical_tool: tool.into(),
            command: command.into(),
        }
    }

    #[test]
    fn command_override_replaces_binary_and_preserves_registry_args() {
        // The #1766 case: opencode → opencode-plannotator, keep `acp`.
        let mut s = spec("opencode", &["acp"]);
        apply_agent_command_override(
            "opencode",
            true,
            &ovr("opencode", "opencode-plannotator"),
            &mut s,
        )
        .unwrap();
        assert_eq!(s.command, "opencode-plannotator");
        assert_eq!(s.args, vec!["acp".to_string()]);
    }

    #[test]
    fn command_override_splits_args_and_prepends_before_registry_args() {
        let mut s = spec("opencode", &["acp"]);
        apply_agent_command_override(
            "opencode",
            true,
            &ovr("opencode", "opencode-plannotator --profile plan"),
            &mut s,
        )
        .unwrap();
        assert_eq!(s.command, "opencode-plannotator");
        assert_eq!(s.args, vec!["--profile", "plan", "acp"]);
    }

    #[test]
    fn command_override_skips_non_registry_spec() {
        let mut s = spec("opencode", &["acp"]);
        apply_agent_command_override(
            "opencode",
            false,
            &ovr("opencode", "opencode-plannotator"),
            &mut s,
        )
        .unwrap();
        assert_eq!(s.command, "opencode");
        assert_eq!(s.args, vec!["acp".to_string()]);
    }

    #[test]
    fn command_override_skips_adapter_binary_mismatch() {
        // Claude's structured view binary is the adapter `claude-agent-acp`, not
        // `claude`, so a terminal `agent_command_override.claude` must
        // not rewrite the adapter command.
        let mut s = spec("claude-agent-acp", &[]);
        apply_agent_command_override("claude", true, &ovr("claude", "claude-wrapper"), &mut s)
            .unwrap();
        assert_eq!(s.command, "claude-agent-acp");
        assert!(s.args.is_empty());
    }

    #[test]
    fn command_override_skips_when_agent_differs_from_logical_tool() {
        let mut s = spec("aoe-agent", &[]);
        apply_agent_command_override(
            "aoe-agent",
            true,
            &ovr("opencode", "opencode-plannotator"),
            &mut s,
        )
        .unwrap();
        assert_eq!(s.command, "aoe-agent");
    }

    /// In-memory sink that captures published frames.
    struct VecSink {
        frames: std::sync::Mutex<Vec<(String, u64, Event)>>,
        stale_nonces: std::sync::Mutex<Vec<Nonce>>,
        stale_elicitation_nonces: std::sync::Mutex<Vec<Nonce>>,
    }
    impl VecSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                frames: std::sync::Mutex::new(Vec::new()),
                stale_nonces: std::sync::Mutex::new(Vec::new()),
                stale_elicitation_nonces: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn with_stale_nonces(nonces: Vec<Nonce>) -> Arc<Self> {
            Arc::new(Self {
                frames: std::sync::Mutex::new(Vec::new()),
                stale_nonces: std::sync::Mutex::new(nonces),
                stale_elicitation_nonces: std::sync::Mutex::new(Vec::new()),
            })
        }
        fn with_stale_elicitation_nonces(nonces: Vec<Nonce>) -> Arc<Self> {
            Arc::new(Self {
                frames: std::sync::Mutex::new(Vec::new()),
                stale_nonces: std::sync::Mutex::new(Vec::new()),
                stale_elicitation_nonces: std::sync::Mutex::new(nonces),
            })
        }
    }
    impl BroadcastSink for VecSink {
        fn publish(&self, session_id: &str, seq: u64, event: &Event) {
            self.frames
                .lock()
                .unwrap()
                .push((session_id.to_string(), seq, event.clone()));
        }
        fn unresolved_approval_nonces(&self, _session_id: &str) -> Vec<Nonce> {
            self.stale_nonces.lock().unwrap().clone()
        }
        fn unresolved_elicitation_nonces(&self, _session_id: &str) -> Vec<Nonce> {
            self.stale_elicitation_nonces.lock().unwrap().clone()
        }
    }

    #[tokio::test]
    async fn spawn_unknown_agent_errors_cleanly() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        let result = sup
            .spawn(SpawnRequest {
                session_id: "s-1".into(),
                agent: "no-such-agent".into(),
                cwd: std::env::temp_dir(),
                additional_dirs: vec![],
                provider_env: vec![],
                model: None,
                effort: None,
                stored_acp_session_id: None,
                seed_history_replay: false,
                sandbox_info: None,
                source_profile: None,
                yolo_mode: false,
                agent_command_override: None,
            })
            .await;
        assert!(matches!(result, Err(SupervisorError::UnknownAgent(_))));
    }

    #[tokio::test]
    async fn double_spawn_returns_already_running() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        // Inject a fake worker by inserting directly into the workers
        // map. We can't actually spawn without a real agent binary
        // here; this verifies the guard path.
        let mut workers = sup.workers.lock().await;
        let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-1".into()));
        let drain = tokio::spawn(async {});
        workers.insert(
            "s-1".into(),
            WorkerHandle {
                client: Arc::new(client),
                drain_task: drain,
                restart_history: vec![Instant::now()],
                kind: WorkerKind::Stdio,
            },
        );
        drop(workers);

        let result = sup
            .spawn(SpawnRequest {
                session_id: "s-1".into(),
                agent: "claude-code".into(),
                cwd: std::env::temp_dir(),
                additional_dirs: vec![],
                provider_env: vec![],
                model: None,
                effort: None,
                stored_acp_session_id: None,
                seed_history_replay: false,
                sandbox_info: None,
                source_profile: None,
                yolo_mode: false,
                agent_command_override: None,
            })
            .await;
        assert!(matches!(result, Err(SupervisorError::AlreadyRunning(_))));
    }

    #[tokio::test]
    async fn count_and_is_running() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        assert_eq!(sup.count().await, 0);
        assert!(!sup.is_running("anything").await);
    }

    /// `resolve_mcp_layers` is the supervisor's own resolver: it reads the
    /// agent's native config (HOME-relative), the global `<app_dir>/mcp.json`,
    /// and the session's per-profile `<profile_dir>/mcp.json` (#1986), then
    /// merges lowest-first so the per-profile layer wins, then global, then
    /// native. The integration test in `tests/integration/acp_mcp.rs` precomputes
    /// the merge itself and so never covers this wiring; this test exercises it
    /// end to end against temp dirs.
    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_mcp_layers_merges_native_global_and_profile() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `#[serial]`; subsequent serial tests reassign
        // these env vars, which is the existing pattern in this module.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }

        // Native (Claude) layer: defines "native-only" and "shared".
        std::fs::write(
            tmp.path().join(".claude.json"),
            r#"{ "mcpServers": {
                "native-only": { "command": "n" },
                "shared": { "command": "from-native" }
            } }"#,
        )
        .unwrap();

        // Global layer in the resolved app dir: adds "global-only" and overrides
        // "shared". Resolve the dir via the same call the resolver uses so the
        // namespace (release vs dev) always matches.
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("mcp.json"),
            r#"{ "mcpServers": {
                "global-only": { "command": "g" },
                "shared": { "command": "from-global" }
            } }"#,
        )
        .unwrap();

        // Per-profile layer for profile "work": adds "profile-only" and overrides
        // "shared" again. Highest precedence, so it must win "shared".
        let profile_dir = crate::session::get_profile_dir_path("work").unwrap();
        std::fs::create_dir_all(&profile_dir).unwrap();
        std::fs::write(
            profile_dir.join("mcp.json"),
            r#"{ "mcpServers": {
                "profile-only": { "command": "p" },
                "shared": { "command": "from-profile" }
            } }"#,
        )
        .unwrap();

        // cwd with no `.mcp.json`: the project-local layer contributes nothing,
        // so this case still resolves to native + global + profile only.
        let cwd = tmp.path().to_path_buf();
        let merged = tokio::task::spawn_blocking(move || {
            resolve_mcp_layers("claude", "resolve-test", Some("work"), &cwd)
        })
        .await
        .unwrap();

        let val = serde_json::to_value(&merged).unwrap();
        let arr = val.as_array().expect("mcp_servers serializes to an array");
        assert_eq!(arr.len(), 4, "native + global + profile union, got {val}");
        let shared = arr
            .iter()
            .find(|s| s["name"] == "shared")
            .expect("shared server present");
        assert_eq!(
            shared["command"], "from-profile",
            "per-profile must win the name collision, got {val}"
        );
        for expected in ["native-only", "global-only", "profile-only"] {
            assert!(
                arr.iter().any(|s| s["name"] == expected),
                "{expected} must survive the merge, got {val}"
            );
        }
    }

    /// Project-local `.mcp.json` (#1985) is the top layer, but gated on repo
    /// trust: untrusted -> skipped; trusted at the file's fingerprint -> wins
    /// every other layer on a name collision.
    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_mcp_layers_gates_project_local_on_trust() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `#[serial]`; matches the sibling resolver test.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }

        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("mcp.json"),
            r#"{ "mcpServers": { "shared": { "command": "from-global" } } }"#,
        )
        .unwrap();

        // A repo dir (no .git, so it is its own trust source) with a project file
        // that defines "project-only" and overrides "shared".
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(
            repo.join(".mcp.json"),
            r#"{ "mcpServers": {
                "project-only": { "command": "pl" },
                "shared": { "command": "from-project" }
            } }"#,
        )
        .unwrap();

        // Untrusted: project-local is skipped, "shared" stays the global value.
        let cwd = repo.clone();
        let merged = tokio::task::spawn_blocking(move || {
            resolve_mcp_layers("claude", "resolve-test", None, &cwd)
        })
        .await
        .unwrap();
        let val = serde_json::to_value(&merged).unwrap();
        let arr = val.as_array().unwrap();
        assert!(
            !arr.iter().any(|s| s["name"] == "project-only"),
            "untrusted project-local must be skipped, got {val}"
        );
        assert_eq!(
            arr.iter().find(|s| s["name"] == "shared").unwrap()["command"],
            "from-global",
            "untrusted project-local must not override global, got {val}"
        );

        // Trust the repo at the file's current fingerprint, then re-resolve.
        let servers = crate::session::project_mcp::load_project_mcp_servers(&repo).unwrap();
        let hash = crate::session::project_mcp::fingerprint(&servers);
        crate::session::repo_config::trust_repo(&repo, None, Some(&hash)).unwrap();

        let cwd = repo.clone();
        let merged = tokio::task::spawn_blocking(move || {
            resolve_mcp_layers("claude", "resolve-test", None, &cwd)
        })
        .await
        .unwrap();
        let val = serde_json::to_value(&merged).unwrap();
        let arr = val.as_array().unwrap();
        assert!(
            arr.iter().any(|s| s["name"] == "project-only"),
            "trusted project-local must be forwarded, got {val}"
        );
        assert_eq!(
            arr.iter().find(|s| s["name"] == "shared").unwrap()["command"],
            "from-project",
            "trusted project-local must win the name collision, got {val}"
        );
    }

    /// Watchdog: after MAX_RESPAWNS_IN_WINDOW respawn attempts inside
    /// RESTART_WINDOW, `restart_decision` returns `BudgetBurned` so the
    /// drain task parks the session instead of hot-looping.
    ///
    /// `restart_decision` short-circuits to `UserStopped` for runner-
    /// managed kinds when the on-disk registry entry is gone, so this
    /// test isolates HOME and saves a live record so the budget path
    /// is the one being exercised.
    #[tokio::test]
    #[serial_test::serial]
    async fn restart_budget_burns_after_threshold() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `#[serial]`; subsequent serial tests
        // reassign these env vars, which is the existing pattern.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        // Build a worker handle with a real-looking spawn_config so the
        // budget path returns Respawn until we exhaust the window.
        let dummy_spec = AgentSpec {
            command: "/bin/true".into(),
            args: vec![],
            description: "test fixture".into(),
            env_allowlist: None,
        };
        let socket_path = tmp.path().join("budget.sock");
        let dummy_config = SpawnConfig {
            agent_key: "claude".into(),
            spec: dummy_spec,
            cwd: std::env::temp_dir(),
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: Some(socket_path.clone()),
            stored_acp_session_id: None,
            seed_history_replay: false,
            artifact_dir: None,
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        // Save a registry record so the runner-managed `registry_gone`
        // check returns false and we exercise the budget path.
        let record = crate::acp::worker_registry::WorkerRecord::new(
            "s-1".into(),
            std::process::id(),
            socket_path,
            "claude-agent-acp".into(),
            "claude-code".into(),
            std::env::temp_dir(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        crate::acp::worker_registry::save(&record).unwrap();
        {
            let mut workers = sup.workers.lock().await;
            let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-1".into()));
            let drain = tokio::spawn(async {});
            workers.insert(
                "s-1".into(),
                WorkerHandle {
                    client: Arc::new(client),
                    drain_task: drain,
                    restart_history: vec![],
                    kind: WorkerKind::Runner {
                        spawn_config: Box::new(dummy_config),
                    },
                },
            );
        }

        for i in 0..MAX_RESPAWNS_IN_WINDOW {
            let decision = restart_decision(&sup.workers, "s-1").await;
            assert!(
                matches!(decision, RestartDecision::Respawn(_)),
                "decision #{i} should be Respawn",
            );
        }
        // One more push past the threshold should burn the budget.
        let decision = restart_decision(&sup.workers, "s-1").await;
        assert!(matches!(decision, RestartDecision::BudgetBurned));
    }

    /// Regression: `aoe acp stop|kill` deletes the registry entry,
    /// then SIGTERMs the runner. The daemon's drain task sees socket EOF
    /// and consults `restart_decision`. With the registry entry gone but
    /// the in-memory `WorkerHandle` still installed, `restart_decision`
    /// must return `UserStopped` so the drain task drops the handle and
    /// emits a soft `Stopped` event — NOT `Respawn` (which would race
    /// the SIGTERM and crash-loop until the budget burned) and NOT
    /// `BudgetBurned` (which would surface the scary red banner the
    /// user originally hit).
    ///
    /// The gate is `WorkerKind::Runner | Attached` (runner-managed)
    /// + registry entry absent; we install a `Runner` kind here so the
    /// production code path fires.
    #[tokio::test]
    #[serial_test::serial]
    async fn restart_decision_returns_user_stopped_when_registry_deleted() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `#[serial]`; the test fixture restores
        // env on the next serial test by reassignment. `get_app_dir`
        // also creates the dir on first call, so an isolated HOME
        // guarantees an isolated registry root.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        let dummy_spec = AgentSpec {
            command: "/bin/true".into(),
            args: vec![],
            description: "test fixture".into(),
            env_allowlist: None,
        };
        let dummy_config = SpawnConfig {
            agent_key: "claude".into(),
            spec: dummy_spec,
            cwd: std::env::temp_dir(),
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: Some(tmp.path().join("dummy.sock")),
            stored_acp_session_id: None,
            seed_history_replay: false,
            artifact_dir: None,
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        {
            let mut workers = sup.workers.lock().await;
            let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-stop".into()));
            let drain = tokio::spawn(async {});
            workers.insert(
                "s-stop".into(),
                WorkerHandle {
                    client: Arc::new(client),
                    drain_task: drain,
                    restart_history: vec![],
                    kind: WorkerKind::Runner {
                        spawn_config: Box::new(dummy_config),
                    },
                },
            );
        }
        // No registry entry for "s-stop" — production code reads this
        // as a user-initiated stop signal.
        let decision = restart_decision(&sup.workers, "s-stop").await;
        assert!(
            matches!(decision, RestartDecision::UserStopped),
            "expected UserStopped when registry entry is absent, got {decision:?}"
        );
    }

    /// `reap_user_stopped` is the polling fallback that catches the
    /// `aoe acp stop|kill` case the drain task cannot detect on its
    /// own (idle connection task blocks on `cmd_rx.recv()`, so socket
    /// EOF never propagates back). When a runner-managed worker's
    /// registry entry vanishes, the reaper must:
    ///   1. Publish a `Stopped { reason: "user_stopped" }` so the UI
    ///      clears its spinner and shows the reconnect banner.
    ///   2. Remove the WorkerHandle so the next reconcile_acp_workers
    ///      tick won't see a phantom worker and skip the auto-spawn path.
    ///
    /// We don't assert anything about the drain task's abort here because
    /// the fixture installs a no-op JoinHandle; the production drain task
    /// holds the client clone and exits when its inbound channel closes
    /// (which happens after `client.shutdown()` propagates Shutdown to
    /// the connection task). Covered indirectly by the
    /// `restart_decision_returns_user_stopped_when_registry_deleted`
    /// regression.
    #[tokio::test]
    #[serial_test::serial]
    async fn reap_user_stopped_emits_event_and_drops_handle() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `#[serial]`. Isolating HOME keeps this
        // test's worker_registry lookups away from the developer's real
        // dev-mode entries.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let dummy_spec = AgentSpec {
            command: "/bin/true".into(),
            args: vec![],
            description: "test fixture".into(),
            env_allowlist: None,
        };
        let dummy_config = SpawnConfig {
            agent_key: "claude".into(),
            spec: dummy_spec,
            cwd: std::env::temp_dir(),
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: Some(tmp.path().join("dummy.sock")),
            stored_acp_session_id: None,
            seed_history_replay: false,
            artifact_dir: None,
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        {
            let mut workers = sup.workers.lock().await;
            let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-reap".into()));
            let drain = tokio::spawn(async {});
            workers.insert(
                "s-reap".into(),
                WorkerHandle {
                    client: Arc::new(client),
                    drain_task: drain,
                    restart_history: vec![],
                    kind: WorkerKind::Runner {
                        spawn_config: Box::new(dummy_config),
                    },
                },
            );
        }
        // No registry entry → reaper treats this as user-initiated stop.
        sup.reap_user_stopped().await;

        // WorkerHandle dropped.
        assert!(
            !sup.workers.lock().await.contains_key("s-reap"),
            "reaper must remove the WorkerHandle"
        );
        // Stopped event published with the correct reason.
        let frames = sink.frames.lock().unwrap();
        let stopped = frames
            .iter()
            .find(|(id, _, _)| id == "s-reap")
            .expect("expected a published frame for s-reap");
        match &stopped.2 {
            Event::Stopped { reason } => {
                assert_eq!(reason, "user_stopped", "wrong stop reason");
            }
            other => panic!("expected Event::Stopped, got {other:?}"),
        }
    }

    /// `reap_user_stopped` distinguishes `aoe acp restart` from `stop`
    /// via the `.restart` sentinel: the CLI's restart path writes the
    /// marker BEFORE deleting the registry, and the reaper consumes it
    /// to (a) publish `restart_pending` instead of `user_stopped`, and
    /// (b) return the id so the reconciler can clear its `attempted`
    /// set and let the next 2s tick auto-respawn the worker (transcript
    /// continuity via the cached `acp_session_id`).
    #[tokio::test]
    #[serial_test::serial]
    async fn reap_user_stopped_reports_restart_pending_when_marker_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let dummy_spec = AgentSpec {
            command: "/bin/true".into(),
            args: vec![],
            description: "test fixture".into(),
            env_allowlist: None,
        };
        let dummy_config = SpawnConfig {
            agent_key: "claude".into(),
            spec: dummy_spec,
            cwd: std::env::temp_dir(),
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: Some(tmp.path().join("dummy.sock")),
            stored_acp_session_id: None,
            seed_history_replay: false,
            artifact_dir: None,
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        {
            let mut workers = sup.workers.lock().await;
            let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-restart".into()));
            let drain = tokio::spawn(async {});
            workers.insert(
                "s-restart".into(),
                WorkerHandle {
                    client: Arc::new(client),
                    drain_task: drain,
                    restart_history: vec![],
                    kind: WorkerKind::Runner {
                        spawn_config: Box::new(dummy_config),
                    },
                },
            );
        }
        // Simulate `aoe acp restart`: registry already deleted (no
        // file at record_path); marker file written before delete.
        crate::acp::worker_registry::mark_restart_pending("s-restart");

        let pending = sup.reap_user_stopped().await;

        assert_eq!(
            pending,
            vec!["s-restart".to_string()],
            "reaper must report the restart-pending session"
        );
        assert!(
            !sup.workers.lock().await.contains_key("s-restart"),
            "reaper must remove the WorkerHandle on restart too"
        );
        let frames = sink.frames.lock().unwrap();
        let stopped = frames
            .iter()
            .find(|(id, _, _)| id == "s-restart")
            .expect("expected published frame");
        match &stopped.2 {
            Event::Stopped { reason } => {
                assert_eq!(
                    reason, "restart_pending",
                    "marker must steer publish reason to restart_pending"
                );
            }
            other => panic!("expected Event::Stopped, got {other:?}"),
        }
        // Marker must be consumed so a subsequent stop on the same id
        // isn't accidentally treated as a restart.
        let marker_path = crate::acp::worker_registry::restart_marker_path("s-restart").unwrap();
        assert!(
            !marker_path.exists(),
            "restart marker must be removed by the reaper"
        );
    }

    /// `reap_user_stopped` must NOT touch stdio-only workers: those have
    /// no registry entry by construction, so the registry-gone check
    /// would always fire and tear down every legacy test fixture.
    /// `WorkerKind::Stdio` is the explicit gate.
    #[tokio::test]
    #[serial_test::serial]
    async fn reap_user_stopped_skips_stdio_workers() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        {
            let mut workers = sup.workers.lock().await;
            let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-stdio".into()));
            let drain = tokio::spawn(async {});
            workers.insert(
                "s-stdio".into(),
                WorkerHandle {
                    client: Arc::new(client),
                    drain_task: drain,
                    restart_history: vec![],
                    kind: WorkerKind::Stdio,
                },
            );
        }
        sup.reap_user_stopped().await;
        assert!(
            sup.workers.lock().await.contains_key("s-stdio"),
            "stdio worker must survive the reaper"
        );
        assert!(
            sink.frames.lock().unwrap().is_empty(),
            "reaper must not publish for stdio workers"
        );
    }

    /// `Supervisor::shutdown` against a live runner-kind worker must
    /// publish `Stopped { reason: "user_stopped" }` synchronously so
    /// the dashboard clears any "thinking" state immediately instead
    /// of waiting for the next reap tick. Covers the REST stop path
    /// (issue #1095 (C)).
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_publishes_stopped_event() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let dummy_spec = AgentSpec {
            command: "/bin/true".into(),
            args: vec![],
            description: "test fixture".into(),
            env_allowlist: None,
        };
        let dummy_config = SpawnConfig {
            agent_key: "claude".into(),
            spec: dummy_spec,
            cwd: std::env::temp_dir(),
            additional_dirs: vec![],
            provider_env: vec![],
            default_effort: None,
            socket_path: Some(tmp.path().join("dummy.sock")),
            stored_acp_session_id: None,
            seed_history_replay: false,
            artifact_dir: None,
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        {
            let mut workers = sup.workers.lock().await;
            let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-stop".into()));
            let drain = tokio::spawn(async {});
            workers.insert(
                "s-stop".into(),
                WorkerHandle {
                    client: Arc::new(client),
                    drain_task: drain,
                    restart_history: vec![],
                    kind: WorkerKind::Runner {
                        spawn_config: Box::new(dummy_config),
                    },
                },
            );
        }

        sup.shutdown("s-stop")
            .await
            .expect("shutdown should succeed");

        let frames = sink.frames.lock().unwrap();
        let stopped = frames
            .iter()
            .find(|(id, _, _)| id == "s-stop")
            .expect("shutdown must publish a frame for the stopped session");
        match &stopped.2 {
            Event::Stopped { reason } => {
                assert_eq!(reason, "user_stopped");
            }
            other => panic!("expected Event::Stopped, got {other:?}"),
        }
    }

    /// Drain task must short-circuit `restart_decision` when the
    /// connection task ends with `Stopped { reason: "rate_limited" }`.
    /// Verifies the producer/supervisor contract for #1281: rate-limit
    /// is a non-crash terminal state. The drain task drops the worker
    /// handle so the next `/acp/spawn` or `/acp/switch-agent`
    /// doesn't hit AlreadyRunning, and does NOT emit a synthetic
    /// AgentStartupError (which would flip the sidebar to Error).
    #[tokio::test]
    async fn drain_skips_restart_when_stopped_rate_limited() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());

        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<Event>(16);
        let drain = sup.start_drain_task("s-rl".into(), inbound_rx);
        {
            let mut workers = sup.workers.lock().await;
            let (client, _client_tx) = AcpClient::fake_for_test(AcpSessionId("s-rl".into()));
            workers.insert(
                "s-rl".into(),
                WorkerHandle {
                    client: Arc::new(client),
                    // Drain task installed above owns the only handle we
                    // care about; this field is just a placeholder so
                    // the WorkerHandle compiles.
                    drain_task: tokio::spawn(async {}),
                    restart_history: vec![],
                    kind: WorkerKind::Stdio,
                },
            );
        }

        // Producer hands off the rate-limit signal before exiting.
        inbound_tx
            .send(Event::Stopped {
                reason: "rate_limited".into(),
            })
            .await
            .unwrap();
        // Closing the channel mirrors the connection task ending
        // cleanly with Ok(()) after the rate-limit emission.
        drop(inbound_tx);

        // Drain task must observe the terminal signal and exit.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), drain)
            .await
            .expect("drain task should exit within 2s of inbound close");

        assert!(
            !sup.workers.lock().await.contains_key("s-rl"),
            "rate-limited worker handle must be dropped from the workers map"
        );

        let frames = sink.frames.lock().unwrap();
        assert!(
            frames.iter().any(
                |(_, _, ev)| matches!(ev, Event::Stopped { reason } if reason == "rate_limited")
            ),
            "the Stopped{{rate_limited}} signal must be published to the sink"
        );
        assert!(
            !frames
                .iter()
                .any(|(_, _, ev)| matches!(ev, Event::AgentStartupError { .. })),
            "no synthetic AgentStartupError should be emitted on rate-limit"
        );
    }

    /// `Supervisor::shutdown` against an `Stdio` test fixture must NOT
    /// publish a `Stopped` event (the seq counter is shared with
    /// budget-tally tests; spurious publishes corrupt their assertions).
    #[tokio::test]
    async fn shutdown_does_not_publish_for_stdio_workers() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        {
            let mut workers = sup.workers.lock().await;
            let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-stdio".into()));
            let drain = tokio::spawn(async {});
            workers.insert(
                "s-stdio".into(),
                WorkerHandle {
                    client: Arc::new(client),
                    drain_task: drain,
                    restart_history: vec![],
                    kind: WorkerKind::Stdio,
                },
            );
        }
        sup.shutdown("s-stdio")
            .await
            .expect("shutdown should succeed");
        assert!(
            sink.frames.lock().unwrap().is_empty(),
            "shutdown must not publish for stdio fixtures"
        );
    }

    /// Reversible teardown must keep the agent transcript resumable, so
    /// `shutdown` (snooze, archive, idle auto-stop, stop, supersede)
    /// must NOT fire `session/delete`; only permanent removal via
    /// `shutdown_and_delete` may. Regression test for the snooze /
    /// archive / idle-stop context loss in #1710. Isolates HOME so the
    /// registry record (which carries the stored ACP id that
    /// `try_session_delete` needs) stays out of real state, and uses a
    /// reaped, never-leader pid so the teardown's process-group SIGTERM
    /// resolves to a harmless ESRCH.
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_skips_session_delete_permanent_delete_fires_it() {
        use std::sync::atomic::Ordering;
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `#[serial]`; matches the existing pattern.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }

        // Dead pid that was never a process-group leader, so the
        // teardown's killpg/kill signal nothing (ESRCH).
        let dead_pid = {
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .expect("spawn helper");
            let id = child.id();
            let _ = child.wait();
            id
        };

        async fn register(
            sup: &Supervisor<VecSink>,
            session: &str,
            pid: u32,
            socket: std::path::PathBuf,
        ) -> std::sync::Arc<std::sync::atomic::AtomicBool> {
            let record = crate::acp::worker_registry::WorkerRecord::new(
                session.into(),
                pid,
                socket,
                "claude-agent-acp".into(),
                "claude-code".into(),
                std::env::temp_dir(),
                None,
                vec![],
                vec![],
                Some("acp-test-id".into()),
                None,
            );
            crate::acp::worker_registry::save(&record).unwrap();
            let (client, _tx, saw_delete) =
                AcpClient::fake_for_test_recording(AcpSessionId(session.into()));
            let mut workers = sup.workers.lock().await;
            workers.insert(
                session.into(),
                WorkerHandle {
                    client: Arc::new(client),
                    drain_task: tokio::spawn(async {}),
                    restart_history: vec![],
                    kind: WorkerKind::Stdio,
                },
            );
            saw_delete
        }

        let sup = Supervisor::new(VecSink::new());

        let keep = register(&sup, "s-keep", dead_pid, tmp.path().join("keep.sock")).await;
        sup.shutdown("s-keep").await.expect("shutdown ok");
        assert!(
            !keep.load(Ordering::SeqCst),
            "shutdown must NOT send session/delete; the transcript must \
             stay resumable so the next respawn restores context (#1710)"
        );

        let purge = register(&sup, "s-del", dead_pid, tmp.path().join("del.sock")).await;
        sup.shutdown_and_delete("s-del")
            .await
            .expect("shutdown_and_delete ok");
        assert!(
            purge.load(Ordering::SeqCst),
            "shutdown_and_delete (permanent removal) must send session/delete"
        );
    }

    /// Claude profile's `is_clear_command` matches the user's `/clear`
    /// invocation in the shapes the adapter accepts: bare, with flags,
    /// surrounded by whitespace. Anything else falls through so a
    /// prompt that merely mentions /clear (e.g. quoting a help string)
    /// doesn't trip the divider. See #1101. The profile-keyed check
    /// itself is unit-tested in `acp::agent_profiles::tests`.
    #[test]
    fn claude_profile_is_clear_command_matches_invocations() {
        let claude = &super::super::agent_profiles::CLAUDE;
        assert!(claude.is_clear_command("/clear"));
        assert!(claude.is_clear_command(" /clear "));
        assert!(claude.is_clear_command("/clear\n"));
        assert!(claude.is_clear_command("/clear --foo"));
        assert!(!claude.is_clear_command("clear"));
        assert!(!claude.is_clear_command("/cleart"));
        assert!(!claude.is_clear_command("hello /clear world"));
        assert!(!claude.is_clear_command(""));
    }

    /// `publish_user_prompt` emits a synthetic `SessionCleared` event
    /// immediately after the `UserPromptSent` for a `/clear`
    /// invocation, so the UI can fold the pre-clear transcript and
    /// drop stale capability caches without waiting for an upstream
    /// signal the adapter doesn't send. See #1101.
    #[tokio::test]
    async fn publish_user_prompt_emits_session_cleared_for_clear_command() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_user_prompt("s-1", "/clear".into()).await;
        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 2);
        assert!(matches!(
            &frames[0].2,
            Event::UserPromptSent { text, .. } if text == "/clear"
        ));
        assert!(matches!(&frames[1].2, Event::SessionCleared));
        assert_eq!(frames[1].1, 2, "SessionCleared must use the next seq");
    }

    /// Regression: `agent_key_for_session` must resolve the registry
    /// key (e.g. `"codex"`), not the binary command stored in
    /// `agent_name` (e.g. `"codex-acp"`), when only the on-disk
    /// `WorkerRecord` is available. Before this was wired through,
    /// `Attached` workers fell back to `agent_name`, which never
    /// matched a real profile and silently dropped per-agent gates
    /// like `/new` boundary detection across daemon restarts.
    #[tokio::test]
    #[serial_test::serial]
    async fn publish_user_prompt_uses_agent_key_from_registry_for_attached_worker() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `#[serial]`; subsequent serial tests
        // reassign these env vars, which is the existing pattern.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let session_id = "attached-codex-1";
        let dir = super::super::worker_registry::workers_dir().unwrap();
        let record = super::super::worker_registry::WorkerRecord::new(
            session_id.into(),
            std::process::id(),
            dir.join(format!("{session_id}.sock")),
            "codex-acp".into(),
            "codex".into(),
            std::env::temp_dir(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        super::super::worker_registry::save(&record).unwrap();

        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_user_prompt(session_id, "/new".into()).await;
        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(
            frames.len(),
            2,
            "expected UserPromptSent + SessionCleared, got {frames:?}"
        );
        assert!(matches!(&frames[0].2, Event::UserPromptSent { .. }));
        assert!(
            matches!(&frames[1].2, Event::SessionCleared),
            "codex /new must clear when agent_key resolves to the codex profile"
        );
        // Sanity: claude's `/clear` must NOT fire for a codex-keyed
        // session, since codex's profile doesn't list it as an alias.
        let sink2 = VecSink::new();
        let sup2 = Supervisor::new(sink2.clone());
        sup2.publish_user_prompt(session_id, "/clear".into()).await;
        let frames2 = sink2.frames.lock().unwrap().clone();
        assert_eq!(
            frames2.len(),
            1,
            "no SessionCleared expected for /clear on codex"
        );
        super::super::worker_registry::delete(session_id).ok();
    }

    /// Legacy registry records (written before the `agent_key` field
    /// existed) fall back to `"claude"` so existing claude sessions
    /// keep working through the rollout. The supervisor falls through
    /// the empty `agent_key` and lands on the default claude profile.
    #[tokio::test]
    #[serial_test::serial]
    async fn publish_user_prompt_falls_back_to_claude_for_legacy_record() {
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let session_id = "legacy-claude-1";
        let dir = super::super::worker_registry::workers_dir().unwrap();
        // Hand-craft a legacy record: pre-`agent_key` schema (empty
        // string after serde default).
        let legacy = serde_json::json!({
            "runner_version": super::super::worker_registry::RUNNER_VERSION,
            "session_id": session_id,
            "pid": std::process::id(),
            "socket_path": dir.join(format!("{session_id}.sock")),
            "agent_name": "claude-agent-acp",
            "cwd": std::env::temp_dir(),
            "model": null,
            "additional_dirs": [],
            "provider_env_keys": [],
            "stored_acp_session_id": null,
            "started_at": 0,
            "last_attached_at": null,
            "detached_at": null
        });
        std::fs::write(
            dir.join(format!("{session_id}.json")),
            serde_json::to_string(&legacy).unwrap(),
        )
        .unwrap();

        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_user_prompt(session_id, "/clear".into()).await;
        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 2);
        assert!(matches!(&frames[1].2, Event::SessionCleared));
        super::super::worker_registry::delete(session_id).ok();
    }

    /// A regular user prompt must not emit `SessionCleared`. Sanity
    /// check that the detection isn't trigger-happy.
    #[tokio::test]
    async fn publish_user_prompt_does_not_emit_session_cleared_for_normal_prompts() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_user_prompt("s-1", "tell me about /clear".into())
            .await;
        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 1);
        assert!(matches!(&frames[0].2, Event::UserPromptSent { .. }));
    }

    /// Incompatible-session tracking (#2109): a session marked for one
    /// binary is returned only for that binary's query and only while it
    /// has no live worker; clearing drops it. None of the test sessions
    /// have a worker, so the `is_running` filter passes them all through.
    #[tokio::test]
    async fn incompatible_sessions_tracked_and_filtered_by_binary() {
        let sup = Supervisor::new(VecSink::new());
        sup.mark_incompatible_binary("s-claude-1", "claude-agent-acp");
        sup.mark_incompatible_binary("s-claude-2", "claude-agent-acp");
        sup.mark_incompatible_binary("s-codex", "codex-acp");

        let mut claude = sup
            .incompatible_sessions_for_binary("claude-agent-acp")
            .await;
        claude.sort();
        assert_eq!(claude, vec!["s-claude-1", "s-claude-2"]);
        assert_eq!(
            sup.incompatible_sessions_for_binary("codex-acp").await,
            vec!["s-codex"]
        );
        // Unknown binary matches nothing.
        assert!(sup
            .incompatible_sessions_for_binary("gemini")
            .await
            .is_empty());

        // A clean (re)spawn clears the entry.
        sup.clear_incompatible_binary("s-claude-1");
        assert_eq!(
            sup.incompatible_sessions_for_binary("claude-agent-acp")
                .await,
            vec!["s-claude-2"]
        );
    }

    /// Force-respawn requests round-trip through the supervisor and drain
    /// to empty so a second tick does not re-respawn the same session. See
    /// #2109.
    #[test]
    fn force_respawn_requests_drain_once() {
        let sup = Supervisor::new(VecSink::new());
        sup.request_respawn("s-1");
        sup.request_respawn("s-2");
        sup.request_respawn("s-1"); // idempotent
        let mut ids = sup.take_respawn_requests();
        ids.sort();
        assert_eq!(ids, vec!["s-1", "s-2"]);
        // Drained: nothing left for the next tick.
        assert!(sup.take_respawn_requests().is_empty());
    }

    /// `next_seq` increments per-session and is independent of the
    /// `workers` map (so `publish_startup_error` and the drain task
    /// share a counter even though the former runs while no
    /// WorkerHandle exists).
    #[tokio::test]
    async fn next_seq_is_per_session_and_persistent() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        assert_eq!(next_seq(&sup.next_seqs, "s-1"), 1);
        assert_eq!(next_seq(&sup.next_seqs, "s-1"), 2);
        // Different session has its own counter.
        assert_eq!(next_seq(&sup.next_seqs, "s-2"), 1);
        // s-1 keeps incrementing.
        assert_eq!(next_seq(&sup.next_seqs, "s-1"), 3);
    }

    /// `publish_user_prompt` writes a `UserPromptSent { text }` event
    /// through the sink with a fresh seq. The handler invokes this
    /// before forwarding to the agent so the on-disk store has the
    /// user side of the conversation; if seq weren't allocated here,
    /// the agent's first reply chunk would collide on the same seq
    /// and the client-side dedupe would silently drop one of them.
    #[tokio::test]
    async fn publish_user_prompt_emits_event_and_increments_seq() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_user_prompt("s-1", "first prompt".into()).await;
        sup.publish_user_prompt("s-1", "second prompt".into()).await;

        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(frames.len(), 2);
        let (sid, seq, event) = &frames[0];
        assert_eq!(sid, "s-1");
        assert_eq!(*seq, 1);
        assert!(matches!(
            event,
            Event::UserPromptSent { text, .. } if text == "first prompt"
        ));
        let (_, seq2, event2) = &frames[1];
        assert_eq!(*seq2, 2);
        assert!(matches!(
            event2,
            Event::UserPromptSent { text, .. } if text == "second prompt"
        ));
    }

    /// After `hydrate_seqs` (called at startup with the on-disk
    /// max-seq map), the next publish for that session must return
    /// stored_max + 1, not 1. Without this, restoring from a
    /// non-empty event store would re-issue seq=1 and the INSERT OR
    /// IGNORE on the disk path would silently drop the new event.
    #[tokio::test]
    async fn hydrate_seqs_resumes_from_stored_max() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        // Simulate: we've persisted up to seq=42 for s-1 and seq=7 for s-2.
        sup.hydrate_seqs([("s-1".to_string(), 42), ("s-2".to_string(), 7)]);

        sup.publish_user_prompt("s-1", "after restart".into()).await;
        sup.publish_startup_error("s-2", "retry".into());

        let frames = sink.frames.lock().unwrap().clone();
        let s1_seq = frames
            .iter()
            .find_map(|(sid, seq, _)| (sid == "s-1").then_some(*seq));
        let s2_seq = frames
            .iter()
            .find_map(|(sid, seq, _)| (sid == "s-2").then_some(*seq));
        assert_eq!(
            s1_seq,
            Some(43),
            "s-1 should resume at stored_max + 1 = 43, not 1"
        );
        assert_eq!(
            s2_seq,
            Some(8),
            "s-2 should resume at stored_max + 1 = 8, not 1"
        );
    }

    /// Regression: `ResumeReservation::drop` must not need a tokio
    /// runtime. The previous shape detached a `tokio::spawn` to
    /// release a `tokio::sync::Mutex`; that pattern panicked or
    /// orphaned the entry when drop ran outside any runtime (e.g.
    /// during runtime shutdown or in synchronous teardown).
    #[test]
    fn resume_reservation_drop_is_synchronous_no_runtime_needed() {
        let pending: Arc<std::sync::Mutex<HashMap<String, ResumeKind>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        pending
            .lock()
            .unwrap()
            .insert("s-sync-drop".into(), ResumeKind::Spawn);

        let reservation = ResumeReservation {
            pending: Arc::clone(&pending),
            session_id: "s-sync-drop".into(),
            notify: Arc::new(tokio::sync::Notify::new()),
        };
        drop(reservation);

        assert!(
            !pending.lock().unwrap().contains_key("s-sync-drop"),
            "Drop must remove the reservation synchronously"
        );
    }

    /// Regression: `ResumeReservation::drop` must recover from a
    /// poisoned `std::sync::Mutex` rather than panic. The maps it
    /// touches only carry simple Clone state, so an unrelated panic
    /// while another holder owned the guard must not cascade into a
    /// drop-time panic that would crash the runtime worker.
    #[test]
    fn resume_reservation_drop_recovers_from_poisoned_mutex() {
        let pending: Arc<std::sync::Mutex<HashMap<String, ResumeKind>>> =
            Arc::new(std::sync::Mutex::new(HashMap::new()));
        pending
            .lock()
            .unwrap()
            .insert("s-poison".into(), ResumeKind::Spawn);

        let p_clone = Arc::clone(&pending);
        let _ = std::panic::catch_unwind(|| {
            let _guard = p_clone.lock().unwrap();
            panic!("intentional panic to poison the mutex");
        });
        assert!(pending.is_poisoned(), "test setup: lock must be poisoned");

        let reservation = ResumeReservation {
            pending: Arc::clone(&pending),
            session_id: "s-poison".into(),
            notify: Arc::new(tokio::sync::Notify::new()),
        };
        drop(reservation);

        let map = pending.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !map.contains_key("s-poison"),
            "Drop must recover the poisoned lock and remove the reservation"
        );
    }

    /// Regression: `wait_for_worker` must wake on `notify_waiters`
    /// rather than at the next 50 ms poll. The previous shape woke
    /// at the next 50 ms poll, which delayed every caller (send_prompt
    /// etc.) that happened to race the spawn or attach finishing.
    #[tokio::test]
    async fn wait_for_worker_wakes_on_reservation_drop() {
        let sink = VecSink::new();
        let sup = Arc::new(Supervisor::new(sink));

        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-notify".into(), ResumeKind::Spawn);
        let reservation = ResumeReservation {
            pending: Arc::clone(&sup.pending_resumes),
            session_id: "s-notify".into(),
            notify: Arc::clone(&sup.worker_notify),
        };

        let sup_clone = Arc::clone(&sup);
        let waiter = tokio::spawn(async move {
            sup_clone
                .wait_for_worker("s-notify", std::time::Duration::from_secs(60))
                .await
        });

        tokio::task::yield_now().await;
        let dropped_at = std::time::Instant::now();
        drop(reservation);

        let result = tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("waiter must wake on notify well under the old 50 ms poll")
            .expect("waiter task must not panic");
        let elapsed = dropped_at.elapsed();

        assert!(
            !result,
            "wait_for_worker must return false when the reservation drops without a worker landing"
        );
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "wait_for_worker must wake within 50 ms (= old poll interval), got {elapsed:?}"
        );
    }

    /// #1748: `begin_resume` must place a `pending_resumes` reservation
    /// synchronously so a subsequent `wait_for_worker` BLOCKS until the
    /// worker lands instead of failing fast. This is the core of the
    /// idle-dormant prompt-wake fix: before it, the prompt handler cleared
    /// dormancy but started no resume, so `send_prompt`'s `wait_for_worker`
    /// returned false immediately (no pending entry) and the prompt 404'd.
    #[tokio::test]
    async fn begin_resume_reserves_so_wait_for_worker_blocks() {
        let sink = VecSink::new();
        let sup = Arc::new(Supervisor::new(sink));

        // Pre-fix shape: no worker and no reservation, so `wait_for_worker`
        // returns false immediately. This is exactly what made the wake
        // prompt 404 before the fix.
        assert!(
            !sup.wait_for_worker("s-1748", std::time::Duration::from_secs(60))
                .await,
            "with no reservation, wait_for_worker must fail fast"
        );
        assert!(!sup.is_running("s-1748").await);

        // Reserve synchronously, the way the prompt-wake path now does
        // before driving the detached spawn.
        let reservation = match sup
            .begin_resume("s-1748", ResumeKind::Spawn)
            .await
            .expect("begin_resume must not error under capacity")
        {
            ResumeReservationOutcome::Reserved(r) => r,
            ResumeReservationOutcome::AlreadyPresent => panic!("expected a fresh reservation"),
        };
        assert!(
            sup.is_running("s-1748").await,
            "a reservation must count as running-ish so the reconciler skips it"
        );
        assert!(matches!(
            sup.worker_state("s-1748").await,
            AcpWorkerState::Resuming
        ));

        // A second begin_resume for the same id must not double-reserve.
        assert!(matches!(
            sup.begin_resume("s-1748", ResumeKind::Spawn).await.unwrap(),
            ResumeReservationOutcome::AlreadyPresent
        ));

        // With the reservation held, `wait_for_worker` BLOCKS: the worker
        // is mid-resume, so it must not return within a short window.
        let sup_clone = Arc::clone(&sup);
        let waiter = tokio::spawn(async move {
            sup_clone
                .wait_for_worker("s-1748", std::time::Duration::from_secs(60))
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !waiter.is_finished(),
            "wait_for_worker must block while the reservation is held"
        );

        // Dropping the reservation without a worker landing (the spawn
        // failed) wakes the waiter, which then returns false.
        drop(reservation);
        let woke = tokio::time::timeout(std::time::Duration::from_millis(200), waiter)
            .await
            .expect("waiter must wake on reservation drop")
            .expect("waiter task must not panic");
        assert!(
            !woke,
            "no worker landed, so wait_for_worker returns false after the reservation drops"
        );
        assert!(matches!(
            sup.worker_state("s-1748").await,
            AcpWorkerState::Absent
        ));
    }

    /// Regression: `shutdown` arriving while a spawn is mid-handshake
    /// must mark the in-flight spawn for cancellation, so the spawn's
    /// pre-insert check drops the freshly-built client instead of
    /// installing an orphaned worker. This test exercises the
    /// supervisor-side state machine without a real ACP handshake by
    /// pre-seeding `pending_resumes` and asserting `shutdown`'s effect.
    #[tokio::test]
    async fn shutdown_during_pending_spawn_marks_for_cancellation() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        // Simulate "spawn in flight": session is in pending_resumes
        // but no WorkerHandle yet. This is the exact window where
        // the bug used to bite, shutdown returned UnknownSession
        // and the late spawn completion installed an orphan.
        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-cancel".into(), ResumeKind::Spawn);
        assert!(sup.is_running("s-cancel").await);

        // The new shutdown contract: success (Ok(())), and the id is
        // recorded in cancelled_resumes so the spawn's pre-insert
        // check can bail.
        sup.shutdown("s-cancel")
            .await
            .expect("shutdown of pending spawn should succeed");
        assert!(
            sup.cancelled_resumes.lock().unwrap().contains("s-cancel"),
            "shutdown must mark the pending spawn for cancellation"
        );

        // Sanity: a session that was never pending or running still
        // returns UnknownSession.
        match sup.shutdown("s-never").await {
            Err(SupervisorError::UnknownSession(id)) => assert_eq!(id, "s-never"),
            other => panic!("expected UnknownSession, got {other:?}"),
        }
    }

    /// Regression: `shutdown` arriving while an `attach` is
    /// mid-handshake must also set the cancellation breadcrumb, so
    /// `attach`'s pre-insert check bails before installing a worker
    /// against a SIGTERMed runner. Mirrors the spawn variant; the
    /// breadcrumb is kind-agnostic by design.
    #[tokio::test]
    async fn shutdown_during_pending_attach_marks_for_cancellation() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-attach-cancel".into(), ResumeKind::Attach);
        sup.shutdown("s-attach-cancel")
            .await
            .expect("shutdown of pending attach should succeed");
        assert!(
            sup.cancelled_resumes
                .lock()
                .unwrap()
                .contains("s-attach-cancel"),
            "shutdown must mark the pending attach for cancellation regardless of ResumeKind"
        );
    }

    /// Park until `shutter` is alive and holding `workers` (the
    /// steady state both lock-pair regression tests probe), or
    /// finish, or hit a 5s deadline. Polling avoids a fixed sleep
    /// that flakes on contended CI runners; the caller's asserts
    /// distinguish the three exit shapes.
    ///
    /// `clippy::await_holding_lock` is suppressed at each call site
    /// because the test holds `cancelled_guard` (a std `MutexGuard`)
    /// at fn scope across the inner sleep; the guard's lint scope
    /// follows the guard, not the `await`.
    async fn wait_for_shutter_park<T>(
        shutter: &tokio::task::JoinHandle<T>,
        sup: &Supervisor<VecSink>,
    ) {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !shutter.is_finished() && sup.workers.try_lock().is_ok() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await;
    }

    /// Regression for #1848: `shutdown_with_reason` must hold the
    /// `workers` lock across its `cancelled_resumes.insert(...)`,
    /// otherwise a concurrent `spawn` or `attach` that reacquires
    /// `workers` between the drop and the insert observes an empty
    /// breadcrumb set and installs an orphan worker the user cannot
    /// disable. Locks down the lock-pair invariant under typical
    /// scheduling: the test holds `cancelled_resumes` before
    /// spawning a shutter, so `shutdown_with_reason` parks at its
    /// own breadcrumb insert; `wait_for_shutter_park` polls until
    /// the shutter is parked inside `workers`;
    /// `workers.try_lock()` from the test then samples the invariant
    /// directly. `Err(_)` means the seed runs while `workers` is
    /// held (the bug shape #1848 closed); `Ok(_)` means a future
    /// reorder put the seed after the drop and the assert fails
    /// with the embedded message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[allow(clippy::await_holding_lock)]
    async fn shutdown_holds_workers_lock_across_cancelled_resumes_seed() {
        let sup = Arc::new(Supervisor::new(VecSink::new()));
        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-race".into(), ResumeKind::Spawn);

        let cancelled_guard = sup.cancelled_resumes.lock().unwrap();

        let shutter = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move { sup.shutdown("s-race").await })
        };

        wait_for_shutter_park(&shutter, &sup).await;

        // Sanity probe: distinguishes a real #1848 regression from a
        // test-environment timing failure. If shutter completed, the
        // breadcrumb seed cannot have parked on the held std mutex,
        // and the assertion below would mis-attribute that failure
        // mode to the regression. Fail with a clearer message.
        assert!(
            !shutter.is_finished(),
            "shutter completed unexpectedly; cancelled_resumes parking \
             did not engage (test environment timing issue, not a \
             #1848 regression)"
        );

        assert!(
            sup.workers.try_lock().is_err(),
            "regression #1848: shutdown released `workers` before \
             writing the `cancelled_resumes` breadcrumb"
        );

        drop(cancelled_guard);
        shutter
            .await
            .expect("shutter task panicked")
            .expect("shutdown should succeed");
        assert!(
            sup.cancelled_resumes.lock().unwrap().contains("s-race"),
            "shutdown must seed cancelled_resumes for the in-flight resume"
        );
    }

    /// Regression for #1848 (registry-terminate sibling of
    /// `shutdown_holds_workers_lock_across_cancelled_resumes_seed`):
    /// the registry-terminate writer branch in `shutdown_with_reason`
    /// must also seed `cancelled_resumes` while still holding `workers`,
    /// otherwise a concurrent `spawn` or `attach` reacquiring `workers`
    /// between the drop and the insert observes an empty breadcrumb set
    /// and installs an orphan worker against the SIGTERMed runner. Same
    /// lock-hold mechanism as the pending-only sibling. HOME is
    /// isolated so `worker_registry::save` writes under the tempdir;
    /// the saved record carries a sentinel PID well above `PID_MAX` on
    /// macOS and Linux so `killpg`/`kill` in
    /// `terminate_runner_for_session` return `ESRCH`, which
    /// `signal_runner_group` discards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    #[allow(clippy::await_holding_lock)]
    async fn shutdown_holds_workers_lock_across_cancelled_resumes_seed_registry_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: `std::env::set_var` is unsound (Rust 1.80+) under
        // concurrent env reads. `#[serial]` excludes other
        // HOME-mutating tests in this crate (notably
        // `restart_budget_burns_after_threshold`); non-`#[serial]`
        // parallel readers of HOME via `get_app_dir()` are not
        // excluded.
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }

        let sup = Arc::new(Supervisor::new(VecSink::new()));

        // Save a registry record so `shutdown_with_reason` enters the
        // registry-terminate branch (`worker_registry::load` returns Some).
        // Sentinel PID is above macOS PID_MAX (99998) and Linux default
        // pid_max (4_194_304), so signal_runner_group's killpg+kill both
        // ESRCH and the test never signals an unrelated process.
        let socket_path = tmp.path().join("registry-race.sock");
        let record = crate::acp::worker_registry::WorkerRecord::new(
            "s-registry-race".into(),
            999_999_999,
            socket_path,
            "claude-agent-acp".into(),
            "claude-code".into(),
            std::env::temp_dir(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        crate::acp::worker_registry::save(&record).unwrap();

        // The registry-terminate branch only seeds the breadcrumb when
        // `pending_has_it` is true, mirroring the writer at line 2264.
        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-registry-race".into(), ResumeKind::Spawn);

        let cancelled_guard = sup.cancelled_resumes.lock().unwrap();

        let shutter = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move { sup.shutdown("s-registry-race").await })
        };

        wait_for_shutter_park(&shutter, &sup).await;

        assert!(
            !shutter.is_finished(),
            "shutter completed unexpectedly; cancelled_resumes parking \
             did not engage on the registry-terminate path (test \
             environment timing issue, not a #1848 regression)"
        );

        assert!(
            sup.workers.try_lock().is_err(),
            "regression #1848 (registry-terminate path): shutdown \
             released `workers` before writing the `cancelled_resumes` \
             breadcrumb"
        );

        drop(cancelled_guard);
        shutter
            .await
            .expect("shutter task panicked")
            .expect("shutdown should succeed");
        assert!(
            sup.cancelled_resumes
                .lock()
                .unwrap()
                .contains("s-registry-race"),
            "shutdown must seed cancelled_resumes for the in-flight \
             resume on the registry-terminate path"
        );
    }

    /// Regression: `publish_startup_error` and a subsequent drain-task
    /// publish must not collide on seq=1, otherwise the client-side
    /// dedupe (`frame.seq <= state.lastSeq → drop`) eats the agent's
    /// first message after a retry.
    #[tokio::test]
    async fn startup_error_then_drain_publish_have_distinct_seqs() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.publish_startup_error("s-1", "boom".into());
        // Simulate the drain task publishing the agent's first event
        // after a successful retry.
        let drained_seq = next_seq(&sup.next_seqs, "s-1");
        let frames = sink.frames.lock().unwrap();
        let startup_seq = frames
            .iter()
            .find_map(|(sid, seq, _)| if sid == "s-1" { Some(*seq) } else { None });
        assert_eq!(startup_seq, Some(1));
        assert_eq!(drained_seq, 2, "drain seq must follow startup-error seq");
    }

    /// `publish_rate_limit_auto_resumed` must emit a `RateLimitAutoResumed`
    /// carrying the exact `resets_at` and allocate monotonic per-session
    /// seqs, so the reconciler breadcrumb supersedes `Stopped{rate_limited}`
    /// in the replay/store ordering. See #1722.
    #[tokio::test]
    async fn publish_rate_limit_auto_resumed_emits_event_with_monotonic_seq() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let resets_at = chrono::Utc::now();

        let seq1 = sup.publish_rate_limit_auto_resumed("s-rl", resets_at);
        let seq2 = sup.publish_rate_limit_auto_resumed("s-rl", resets_at);
        assert_eq!(seq1, 1);
        assert_eq!(seq2, 2, "seq must be monotonic per session");

        let frames = sink.frames.lock().unwrap();
        let first = frames
            .iter()
            .find(|(sid, seq, _)| sid == "s-rl" && *seq == 1)
            .expect("first breadcrumb frame published");
        assert!(matches!(
            &first.2,
            Event::RateLimitAutoResumed { resets_at: ts } if *ts == resets_at
        ));
    }

    /// `with_capacity` enforces the configured cap. Past the cap,
    /// new spawns return `CapacityFull` instead of starting another
    /// worker. The error must include `current` and `limit` so the
    /// REST surface can return a useful 503 body.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_full_returns_after_limit() {
        // Isolate HOME so registry entries from the developer's real
        // dev profile (or other tests) don't bleed into the spawn
        // path's combined-count check.
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let sink = VecSink::new();
        let sup = Supervisor::with_capacity(sink, 1);
        // Pre-load one fake worker so the cap is full.
        let mut workers = sup.workers.lock().await;
        let (client, _tx) = AcpClient::fake_for_test(AcpSessionId("s-1".into()));
        let drain = tokio::spawn(async {});
        workers.insert(
            "s-1".into(),
            WorkerHandle {
                client: Arc::new(client),
                drain_task: drain,
                restart_history: vec![],
                kind: WorkerKind::Stdio,
            },
        );
        drop(workers);

        let result = sup
            .spawn(SpawnRequest {
                session_id: "s-2".into(),
                agent: "claude-code".into(),
                cwd: std::env::temp_dir(),
                additional_dirs: vec![],
                provider_env: vec![],
                model: None,
                effort: None,
                stored_acp_session_id: None,
                seed_history_replay: false,
                sandbox_info: None,
                source_profile: None,
                yolo_mode: false,
                agent_command_override: None,
            })
            .await;
        match result {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!(current, 1);
                assert_eq!(limit, 1);
            }
            other => panic!("expected CapacityFull, got {other:?}"),
        }
    }

    /// Capacity must count detached (registry-only) workers, not just
    /// in-memory ones. Issue #1037 called this out explicitly: a fresh
    /// daemon spawn must not race the reconciler and over-spawn while
    /// it's still attaching to live runners. Without this, two
    /// consecutive `aoe serve` invocations could push the worker count
    /// past `max_concurrent_workers`.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_counts_detached_registry_entries() {
        let tmp = tempfile::TempDir::new().unwrap();
        // SAFETY: serialised by `#[serial]`; isolating HOME keeps this
        // test's registry writes away from the developer's real
        // dev-mode entries (and any other tests in this file).
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }
        let sink = VecSink::new();
        let sup = Supervisor::with_capacity(sink, 1);

        // No in-memory workers. Just a single registry entry that
        // `is_record_live` will accept: PID = current process (so
        // pid_alive is true) and a real file at the socket path (so
        // socket_exists is true).
        let registry_dir = crate::acp::worker_registry::workers_dir().unwrap();
        let socket_path = registry_dir.join("detached-1.sock");
        std::fs::write(&socket_path, b"").unwrap();
        let record = crate::acp::worker_registry::WorkerRecord::new(
            "detached-1".into(),
            std::process::id(),
            socket_path,
            "claude-agent-acp".into(),
            "claude-code".into(),
            std::env::temp_dir(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        crate::acp::worker_registry::save(&record).unwrap();

        // Pre-condition: registry entry must be live for the capacity
        // path to count it. If this fails, the test setup is wrong.
        assert!(
            crate::acp::worker_registry::is_record_live(&record),
            "registry record must be live for the capacity path to count it"
        );

        let result = sup
            .spawn(SpawnRequest {
                session_id: "fresh".into(),
                agent: "claude-code".into(),
                cwd: std::env::temp_dir(),
                additional_dirs: vec![],
                provider_env: vec![],
                model: None,
                effort: None,
                stored_acp_session_id: None,
                seed_history_replay: false,
                sandbox_info: None,
                source_profile: None,
                yolo_mode: false,
                agent_command_override: None,
            })
            .await;
        match result {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!(current, 1, "detached registry entry must count");
                assert_eq!(limit, 1);
            }
            other => panic!("expected CapacityFull, got {other:?}"),
        }
    }

    /// `forget_session` drops the seq counter so the next conversation
    /// (e.g. acp_disable → acp_enable) starts fresh from seq=1.
    #[tokio::test]
    async fn forget_session_resets_seq_counter() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);
        assert_eq!(next_seq(&sup.next_seqs, "s-1"), 1);
        assert_eq!(next_seq(&sup.next_seqs, "s-1"), 2);
        sup.forget_session("s-1");
        assert_eq!(next_seq(&sup.next_seqs, "s-1"), 1);
    }

    /// End-to-end: build a real `ChannelSink` (broadcast tx + on-disk
    /// EventStore) and verify a single `publish` call reaches both —
    /// broadcast subscribers AND the SQLite store. The on-disk path is
    /// the durable mirror that the WS-on-connect drain and the
    /// `/acp/replay` REST endpoint both serve from.
    #[tokio::test]
    async fn channel_sink_publishes_to_broadcast_and_disk() {
        use crate::acp::event_store::EventStore;
        use tempfile::TempDir;
        use tokio::sync::broadcast;

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("acp.db");
        let event_store = Arc::new(EventStore::open(&db_path, 1000).unwrap());
        let (tx, mut rx) = broadcast::channel(16);
        let sink = Arc::new(ChannelSink {
            tx,
            event_store: event_store.clone(),
        });

        sink.publish(
            "s-42",
            1,
            &Event::UserPromptSent {
                text: "hello world".into(),
                attachments: Vec::new(),
            },
        );
        sink.publish(
            "s-42",
            2,
            &Event::AgentMessageChunk {
                text: "agent reply".into(),
            },
        );

        // Broadcast subscribers see both frames in seq order.
        let frame1 = rx.try_recv().expect("broadcast frame 1");
        let frame2 = rx.try_recv().expect("broadcast frame 2");
        assert_eq!(frame1.session_id, "s-42");
        assert_eq!(frame1.seq, 1);
        assert_eq!(frame2.seq, 2);

        // On-disk store has the same two events.
        let stored = event_store.replay_from("s-42", 0);
        assert_eq!(stored.len(), 2);
        assert_eq!(stored[0].0, 1);
        assert!(matches!(
            stored[0].1,
            Event::UserPromptSent { ref text, .. } if text == "hello world"
        ));
        assert_eq!(stored[1].0, 2);
        assert!(matches!(
            stored[1].1,
            Event::AgentMessageChunk { ref text } if text == "agent reply"
        ));
    }

    /// Restart simulation: publish through one Supervisor, drop it,
    /// reopen the EventStore at the same path, hydrate a fresh
    /// Supervisor's seqs from disk, and verify the next publish gets
    /// stored_max + 1 (not 1). This is exactly what `aoe serve`
    /// startup does after an unclean shutdown.
    #[tokio::test]
    async fn supervisor_resumes_seq_counter_from_disk_after_restart() {
        use crate::acp::event_store::EventStore;
        use tempfile::TempDir;
        use tokio::sync::broadcast;

        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("acp.db");

        // First "process": publish a few events, then drop everything.
        {
            let event_store = Arc::new(EventStore::open(&db_path, 1000).unwrap());
            let (tx, _rx) = broadcast::channel(16);
            let sink = Arc::new(ChannelSink {
                tx,
                event_store: event_store.clone(),
            });
            let sup = Supervisor::new(sink);
            sup.publish_user_prompt("s-99", "first".into()).await;
            sup.publish_user_prompt("s-99", "second".into()).await;
            sup.publish_user_prompt("s-99", "third".into()).await;
            // sup, sink, and the in-memory replay ring drop here.
        }

        // Second "process": reopen the store at the same path,
        // hydrate the supervisor from disk, and publish.
        let event_store = Arc::new(EventStore::open(&db_path, 1000).unwrap());
        // Disk should still hold seqs 1..=3.
        assert_eq!(event_store.highest_seq("s-99"), 3);

        let (tx, mut rx) = broadcast::channel(16);
        let sink = Arc::new(ChannelSink {
            tx,
            event_store: event_store.clone(),
        });
        let sup = Supervisor::new(sink);
        sup.hydrate_seqs(event_store.all_session_seqs());
        sup.publish_user_prompt("s-99", "after restart".into())
            .await;

        // The fresh publish must be seq=4, not seq=1. A seq=1
        // publish would be a no-op on disk (INSERT OR IGNORE) and
        // the client-side dedupe would silently drop it.
        let frame = rx.try_recv().expect("post-restart frame");
        assert_eq!(frame.seq, 4);

        // Disk now holds 1..=4, with the user prompt text preserved.
        let stored = event_store.replay_from("s-99", 0);
        let texts: Vec<String> = stored
            .iter()
            .filter_map(|(_, ev)| match ev {
                Event::UserPromptSent { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, vec!["first", "second", "third", "after restart"]);
    }

    /// `worker_state` returns Resuming while an entry sits in
    /// `pending_resumes`, regardless of ResumeKind (attach or spawn).
    /// The UI uses the same indicator for both lifecycle paths so the
    /// kind distinction is supervisor-internal only. See #1088.
    #[tokio::test]
    async fn worker_state_resuming_for_spawn_and_attach() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink);

        assert_eq!(sup.worker_state("s-spawn").await, AcpWorkerState::Absent);

        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-spawn".into(), ResumeKind::Spawn);
        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-attach".into(), ResumeKind::Attach);

        assert_eq!(sup.worker_state("s-spawn").await, AcpWorkerState::Resuming);
        assert_eq!(sup.worker_state("s-attach").await, AcpWorkerState::Resuming);

        let snap = sup.worker_states_snapshot().await;
        assert_eq!(snap.get("s-spawn"), Some(&AcpWorkerState::Resuming));
        assert_eq!(snap.get("s-attach"), Some(&AcpWorkerState::Resuming));
    }

    /// Capacity must count in-flight Spawn reservations alongside
    /// in-memory workers and detached registry entries. Without this,
    /// the parallel reconciler can pass N concurrent callers through
    /// the limit check before any worker insert lands, allowing
    /// `max_concurrent_workers` to be exceeded. Attach reservations do
    /// NOT contribute (they take over an existing live runner already
    /// counted in registry_count). See #1088.
    #[tokio::test]
    async fn capacity_counts_pending_spawn_reservations() {
        let sink = VecSink::new();
        let sup = Supervisor::with_capacity(sink, 2);

        // Pre-seed two pending Spawn reservations, simulating two
        // concurrent reconciler tasks that have passed the workers
        // check and are mid-handshake.
        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-a".into(), ResumeKind::Spawn);
        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-b".into(), ResumeKind::Spawn);

        // A third spawn must fail the capacity check rather than slip
        // through the pre-insert window.
        let result = sup
            .spawn(SpawnRequest {
                session_id: "s-c".into(),
                agent: "claude".into(),
                cwd: std::env::temp_dir(),
                additional_dirs: vec![],
                provider_env: vec![],
                model: None,
                effort: None,
                stored_acp_session_id: None,
                seed_history_replay: false,
                sandbox_info: None,
                source_profile: None,
                yolo_mode: false,
                agent_command_override: None,
            })
            .await;
        match result {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!(limit, 2);
                assert!(current >= 2, "expected combined >= limit, got {current}");
            }
            other => panic!("expected CapacityFull, got {other:?}"),
        }
    }

    /// An Attach reservation must NOT count toward the spawn capacity:
    /// reattach takes over an existing live runner which is already
    /// counted via `registry_count`. Counting attach reservations
    /// alongside registry entries would double-count. See #1088.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_ignores_pending_attach_reservations() {
        // Isolate HOME so registry writes from other tests don't
        // pollute this test's capacity count.
        let tmp = tempfile::TempDir::new().unwrap();
        unsafe {
            std::env::set_var("HOME", tmp.path());
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        }

        let sink = VecSink::new();
        let sup = Supervisor::with_capacity(sink, 1);

        sup.pending_resumes
            .lock()
            .unwrap()
            .insert("s-attach".into(), ResumeKind::Attach);

        // With max=1 and one Attach pending, a fresh spawn for a
        // different id must still pass the capacity check; the spawn
        // will fail later (in this test, after the capacity gate, with
        // UnknownAgent because the test agent isn't registered), but
        // NOT with CapacityFull.
        let result = sup
            .spawn(SpawnRequest {
                session_id: "s-spawn".into(),
                agent: "definitely-not-a-real-agent-xyz".into(),
                cwd: std::env::temp_dir(),
                additional_dirs: vec![],
                provider_env: vec![],
                model: None,
                effort: None,
                stored_acp_session_id: None,
                seed_history_replay: false,
                sandbox_info: None,
                source_profile: None,
                yolo_mode: false,
                agent_command_override: None,
            })
            .await;
        match result {
            Err(SupervisorError::CapacityFull { .. }) => {
                panic!("Attach reservation must not count toward spawn capacity");
            }
            Err(SupervisorError::UnknownAgent(_)) | Ok(_) => {
                // Expected: capacity gate passed, then spawn failed
                // downstream on agent resolution. Either path proves
                // the capacity check didn't reject us.
            }
            other => panic!("unexpected error path: {other:?}"),
        }
    }

    /// Orphaned-approval sweep must publish one `ApprovalResolved {
    /// decision: Cancelled }` per stale nonce AND a terminal
    /// `Stopped { reason: "approval_cancelled_on_restart" }`. Without
    /// the Stopped, the latest status-affecting event in the store
    /// stays at the pre-restart `ApprovalRequested` / `UserPromptSent`,
    /// so the sidebar dot keeps spinning green and the in-structured view
    /// "Working" rattle keeps running until the next user prompt.
    /// The reason string must be distinct from "user_stopped" /
    /// "restart_pending" so the WorkerStoppedBanner / WorkerRestarting
    /// banners do not fire.
    #[tokio::test]
    async fn cancel_orphaned_approvals_publishes_resolved_and_stopped() {
        let sink =
            VecSink::with_stale_nonces(vec![Nonce("nonce-a".into()), Nonce("nonce-b".into())]);
        let sup = Supervisor::new(sink.clone());
        sup.cancel_orphaned_approvals("s-attach");
        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(
            frames.len(),
            3,
            "expected 2 ApprovalResolved + 1 Stopped, got {frames:?}"
        );
        match &frames[0].2 {
            Event::ApprovalResolved { nonce, decision } => {
                assert_eq!(nonce.0, "nonce-a");
                assert!(matches!(decision, ApprovalDecision::Cancelled));
            }
            other => panic!("frame 0: expected ApprovalResolved, got {other:?}"),
        }
        match &frames[1].2 {
            Event::ApprovalResolved { nonce, decision } => {
                assert_eq!(nonce.0, "nonce-b");
                assert!(matches!(decision, ApprovalDecision::Cancelled));
            }
            other => panic!("frame 1: expected ApprovalResolved, got {other:?}"),
        }
        match &frames[2].2 {
            Event::Stopped { reason } => {
                assert_eq!(reason, "approval_cancelled_on_restart");
            }
            other => panic!("frame 2: expected Stopped, got {other:?}"),
        }
        // Seqs must be strictly monotonic per session.
        assert!(
            frames[0].1 < frames[1].1 && frames[1].1 < frames[2].1,
            "seqs must be monotonic, got {:?}",
            frames.iter().map(|f| f.1).collect::<Vec<_>>()
        );
    }

    /// Orphaned-elicitation sweep publishes one `ElicitationResolved {
    /// outcome: Cancelled }` per stale nonce so a dead question card does
    /// not linger on replay. Unlike approvals it emits no synthetic
    /// `Stopped` (see `cancel_orphaned_elicitations`).
    #[tokio::test]
    async fn cancel_orphaned_elicitations_publishes_resolved() {
        let sink =
            VecSink::with_stale_elicitation_nonces(vec![Nonce("e-a".into()), Nonce("e-b".into())]);
        let sup = Supervisor::new(sink.clone());
        sup.cancel_orphaned_elicitations("s-attach");
        let frames = sink.frames.lock().unwrap().clone();
        assert_eq!(
            frames.len(),
            2,
            "expected 2 ElicitationResolved, got {frames:?}"
        );
        for (frame, expected) in frames.iter().zip(["e-a", "e-b"]) {
            match &frame.2 {
                Event::ElicitationResolved { nonce, outcome, .. } => {
                    assert_eq!(nonce.0, expected);
                    assert!(matches!(outcome, ElicitationOutcome::Cancelled));
                }
                other => panic!("expected ElicitationResolved, got {other:?}"),
            }
        }
        assert!(frames[0].1 < frames[1].1, "seqs must be monotonic");
    }

    #[tokio::test]
    async fn cancel_orphaned_elicitations_noop_when_empty() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.cancel_orphaned_elicitations("s-attach");
        assert!(sink.frames.lock().unwrap().is_empty());
    }

    /// Empty stale-nonce list must be a no-op: do NOT publish a stray
    /// Stopped, because the session may have been mid-turn with no
    /// pending approvals and a real Stopped is still expected from the
    /// agent. Publishing here would clobber the in-flight spinner.
    #[tokio::test]
    async fn cancel_orphaned_approvals_noop_when_empty() {
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.cancel_orphaned_approvals("s-attach");
        assert!(
            sink.frames.lock().unwrap().is_empty(),
            "no nonces means no published frames"
        );
    }
}
