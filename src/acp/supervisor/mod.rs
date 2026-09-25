//! ACP worker supervisor: owns each structured view session's worker, bridges
//! its events into the broadcast sink, and respawns it on crash within a budget.

mod agents;
mod drain;
mod launch;
mod publish;
mod requests;
mod sink;
mod teardown;
#[cfg(test)]
mod test_support;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::warn;

use super::acp_client::{AcpClient, AcpError, SpawnConfig};
use super::agent_registry::AgentRegistry;
pub use super::runner_lifecycle::ResumeKind;
use super::runner_lifecycle::{
    Lease, LifecycleTable, ProcessControl, RunnerIdentity, SystemProcessControl, WorkerPhase,
};
use super::state::AcpSessionId;
use crate::daemon::AcpWorkerState;
use crate::session::SandboxInfo;

pub(crate) use agents::apply_agent_command_override;
pub use sink::{BroadcastSink, ChannelSink};

/// Post-startup respawns allowed within `RESTART_WINDOW` before the session is parked.
const MAX_RESPAWNS_IN_WINDOW: u32 = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// Backoff before respawning so an agent that crashes on startup cannot hot-loop.
const RESPAWN_BACKOFF: Duration = Duration::from_millis(500);
/// How long a runner request path waits for a mid-resume worker to land.
const WORKER_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the client for a spawn; swapped in tests to drive lifecycles without a runner.
pub(crate) type Launcher = Arc<
    dyn Fn(
            SpawnConfig,
            AcpSessionId,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<AcpClient, AcpError>> + Send>,
        > + Send
        + Sync,
>;

type Workers = Arc<Mutex<HashMap<String, WorkerHandle>>>;
/// Per-session seq counter; outlives workers so every publisher shares one sequence.
type SeqMap = std::sync::Mutex<HashMap<String, u64>>;
type SharedSet = Arc<std::sync::Mutex<HashSet<String>>>;

#[derive(Debug, Error)]
pub enum SupervisorError {
    #[error("session {0:?} not found")]
    UnknownSession(String),
    #[error("acp client error: {0}")]
    Acp(#[from] AcpError),
    #[error("agent {0:?} not in registry")]
    UnknownAgent(String),
    /// Registered, but refused by `[acp] allowed_agents` (a 403, not a 400).
    #[error(
        "agent {0:?} is not permitted by [acp] allowed_agents; ask the operator to allow it or pick a permitted agent"
    )]
    AgentNotAllowed(String),
    #[error("{0}")]
    InvalidAgentCommand(String),
    #[error("session {0:?} already has a running structured view worker")]
    AlreadyRunning(String),
    #[error("structured view worker capacity full ({current}/{limit}); raise [acp] max_concurrent_workers or delete an existing structured view session")]
    CapacityFull { current: usize, limit: u32 },
    /// A concurrent shutdown cancelled the resume; callers treat it as a soft success.
    #[error("resume of session {0:?} was cancelled by a concurrent shutdown")]
    SpawnCancelled(String),
    /// The previous runner is not proven dead yet.
    #[error("session {0:?} is still stopping its previous structured view worker")]
    TeardownPending(String),
}

/// What the caller does with prompt text after it was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptDisposition {
    Forward,
    /// A clear command the adapter cannot handle natively: drive
    /// [`Supervisor::reset_session_context`] instead of forwarding it.
    ResetContext,
}

/// How this supervisor acquired the worker.
enum WorkerKind {
    /// Spawned by this daemon; respawnable from its cached config.
    Runner { spawn_config: Box<SpawnConfig> },
    /// Reattached to a runner left by a previous daemon; never respawned in memory.
    Attached,
    /// In-process test fixture with no runner registry entry.
    #[cfg(test)]
    Stdio,
}

#[derive(Clone)]
pub(super) struct PendingContextReset {
    pub(super) profile: String,
    pub(super) reason: String,
    pub(super) transactions: Vec<String>,
}

struct WorkerHandle {
    client: Arc<AcpClient>,
    drain_task: JoinHandle<()>,
    /// Respawn timestamps inside the restart window; the initial spawn is not counted.
    restart_history: Vec<Instant>,
    kind: WorkerKind,
    lease: Lease,
    native_session_id: Option<String>,
}

impl From<WorkerPhase> for AcpWorkerState {
    fn from(phase: WorkerPhase) -> Self {
        match phase {
            WorkerPhase::Absent => Self::Absent,
            WorkerPhase::Resuming => Self::Resuming,
            WorkerPhase::Running => Self::Running,
            WorkerPhase::Stopping => Self::Stopping,
        }
    }
}

pub(crate) enum ResumeReservationOutcome {
    Reserved(ResumeReservation),
    /// The session is already running or mid-resume.
    AlreadyPresent,
}

pub struct Supervisor<S: BroadcastSink> {
    sink: Arc<S>,
    registry: Arc<Mutex<AgentRegistry>>,
    workers: Workers,
    next_seqs: Arc<SeqMap>,
    /// Owner of every runner epoch. Lock order: `workers` before `lifecycle`.
    lifecycle: Arc<std::sync::Mutex<LifecycleTable>>,
    process_control: Arc<dyn ProcessControl>,
    launcher: Launcher,
    /// Agents whose first spawn finished; until then spawns serialize on a
    /// per-agent lock so a lazy adapter install is not raced.
    warmed_up_agents: SharedSet,
    agent_warmup_locks: Arc<std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Wakes `wait_for_worker` whenever the workers map or lifecycle table changes.
    worker_notify: Arc<tokio::sync::Notify>,
    #[cfg(test)]
    worker_waits: tokio::sync::broadcast::Sender<String>,
    /// Build-stale sessions draining a turn before the reconciler respawns them.
    respawn_pending: SharedSet,
    /// Sessions parked on a compatibility rejection, keyed to the failing binary.
    incompatible_binaries: Arc<std::sync::Mutex<HashMap<String, String>>>,
    /// Sessions the reconciler must fresh-spawn next tick, bypassing its `attempted` guard.
    force_respawn: SharedSet,
    /// Sessions whose worker failed before establishing a session.
    startup_failures: SharedSet,
    /// Sessions whose isolated native identity is not durable yet.
    pending_context_resets: SharedSet,
    /// Sessions whose crashed worker the drain task relaunched in place.
    respawned_in_place: SharedSet,
    max_concurrent_workers: u32,
}

/// RAII guard over a `Starting` or `Respawning` epoch; dropping it before
/// install abandons the epoch so a failed resume cannot pin the session.
pub(crate) struct ResumeReservation {
    lease: Lease,
    lifecycle: Arc<std::sync::Mutex<LifecycleTable>>,
    notify: Arc<tokio::sync::Notify>,
}

impl ResumeReservation {
    pub(crate) fn lease(&self) -> &Lease {
        &self.lease
    }
}

impl Drop for ResumeReservation {
    fn drop(&mut self) {
        if lock_recover(&self.lifecycle).abandon(&self.lease) {
            self.notify.notify_waiters();
        }
    }
}

/// The instance's `command` override, applied to the registry spec so the
/// structured view launches the same binary as the terminal view.
#[derive(Debug, Clone)]
pub struct AgentCommandOverride {
    pub logical_tool: String,
    pub command: String,
}

/// Which durable continuation lane a sandboxed spawn may consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxContinuation {
    Persisted,
    ImportTerminal,
    Fresh,
}

#[derive(Debug, Clone)]
pub struct SpawnRequest {
    pub session_id: String,
    /// The ACP backend `pick_agent_for_tool` resolved.
    pub agent: String,
    /// The session's logical tool (`Instance.tool`); host hooks see this as `AOE_TOOL`.
    pub tool: String,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    pub provider_env: Vec<(String, String)>,
    pub model: Option<String>,
    pub effort: Option<String>,
    /// True for persisted user effort, not a resolved default.
    pub effort_explicit: bool,
    /// Prior ACP session id; loaded instead of a new session when the agent supports it.
    pub stored_acp_session_id: Option<String>,
    /// Parent ACP session id to `session/fork` from.
    pub fork_from: Option<String>,
    pub sandbox_continuation: SandboxContinuation,
    pub sandbox_info: Option<SandboxInfo>,
    pub source_profile: Option<String>,
    pub yolo_mode: bool,
    /// Explicit ACP mode applied after the handshake; wins over `yolo_mode`.
    pub acp_mode_id: Option<String>,
    pub agent_command_override: Option<AgentCommandOverride>,
    /// Let a `session/load` replay history into the (empty) event store for an import.
    pub seed_history_replay: bool,
    /// Claude store selected by the conversation binding for a host Claude worker.
    pub claude_store_pin: Option<crate::session::capture::ClaudeStorePin>,
}

impl<S: BroadcastSink> Supervisor<S> {
    /// Constructor with no concurrency cap.
    pub fn new(sink: Arc<S>) -> Self {
        Self::with_capacity(sink, u32::MAX)
    }

    pub fn with_capacity(sink: Arc<S>, max_concurrent_workers: u32) -> Self {
        Self {
            sink,
            registry: Arc::new(Mutex::new(AgentRegistry::with_defaults())),
            workers: Arc::default(),
            next_seqs: Arc::default(),
            lifecycle: Arc::new(std::sync::Mutex::new(LifecycleTable::new(
                chrono::Utc::now().timestamp_millis().max(1) as u64,
            ))),
            process_control: Arc::new(SystemProcessControl),
            launcher: Arc::new(|config, session_id| Box::pin(AcpClient::spawn(config, session_id))),
            warmed_up_agents: Arc::default(),
            agent_warmup_locks: Arc::default(),
            worker_notify: Arc::default(),
            #[cfg(test)]
            worker_waits: tokio::sync::broadcast::channel(64).0,
            respawn_pending: Arc::default(),
            incompatible_binaries: Arc::default(),
            force_respawn: Arc::default(),
            startup_failures: Arc::default(),
            pending_context_resets: Arc::default(),
            respawned_in_place: Arc::default(),
            max_concurrent_workers,
        }
    }

    /// Flag a build-stale worker kept alive to finish its turn.
    pub fn mark_build_respawn_pending(&self, session_id: &str) {
        lock_recover(&self.respawn_pending).insert(session_id.to_string());
    }

    pub fn respawn_pending_ids(&self) -> Vec<String> {
        lock_recover(&self.respawn_pending)
            .iter()
            .cloned()
            .collect()
    }

    pub fn clear_respawn_pending(&self, session_id: &str) {
        lock_recover(&self.respawn_pending).remove(session_id);
    }

    pub fn running_identity(&self, session_id: &str) -> Option<RunnerIdentity> {
        lock_recover(&self.lifecycle)
            .running(session_id)
            .and_then(|(_, identity)| identity)
    }

    fn mark_incompatible_binary(&self, session_id: &str, binary: &str) {
        lock_recover(&self.incompatible_binaries)
            .insert(session_id.to_string(), binary.to_string());
    }

    /// A user-initiated resume overrides a stop kept from a resume that failed before install.
    pub fn forget_stale_cancel(&self, session_id: &str) {
        lock_recover(&self.lifecycle).forget_stale_cancel(session_id);
    }

    pub fn request_respawn(&self, session_id: &str) {
        lock_recover(&self.force_respawn).insert(session_id.to_string());
    }

    pub fn take_respawn_requests(&self) -> Vec<String> {
        lock_recover(&self.force_respawn).drain().collect()
    }

    pub fn take_startup_failures(&self) -> Vec<String> {
        lock_recover(&self.startup_failures).drain().collect()
    }

    pub fn take_respawned_in_place(&self) -> Vec<String> {
        lock_recover(&self.respawned_in_place).drain().collect()
    }

    /// Sessions parked on a compatibility rejection for `binary` with no live worker.
    pub async fn incompatible_sessions_for_binary(&self, binary: &str) -> Vec<String> {
        let candidates: Vec<String> = lock_recover(&self.incompatible_binaries)
            .iter()
            .filter(|(_, b)| b.as_str() == binary)
            .map(|(id, _)| id.clone())
            .collect();
        let mut out = Vec::new();
        for id in candidates {
            if !self.is_running(&id).await {
                out.push(id);
            }
        }
        out
    }

    pub async fn worker_states_snapshot(&self) -> HashMap<String, AcpWorkerState> {
        lock_recover(&self.lifecycle)
            .snapshot()
            .into_iter()
            .map(|(id, phase)| (id, phase.into()))
            .collect()
    }

    pub async fn worker_state(&self, session_id: &str) -> AcpWorkerState {
        lock_recover(&self.lifecycle).phase(session_id).into()
    }

    /// Drop per-session bookkeeping for a deleted session.
    pub fn forget_session(&self, session_id: &str) {
        if let Ok(mut guard) = self.next_seqs.lock() {
            guard.remove(session_id);
        }
        lock_recover(&self.lifecycle).forget(session_id);
        lock_recover(&self.startup_failures).remove(session_id);
        lock_recover(&self.respawned_in_place).remove(session_id);
    }

    /// Seed seq counters from the event store's stored maxima.
    pub fn hydrate_seqs(&self, pairs: impl IntoIterator<Item = (String, u64)>) {
        if let Ok(mut guard) = self.next_seqs.lock() {
            guard.extend(pairs);
        }
    }

    /// Whether this session has a worker up or coming up.
    pub async fn is_running(&self, session_id: &str) -> bool {
        lock_recover(&self.lifecycle).is_running(session_id)
    }

    /// Whether a live frame's generation is the worker currently installed.
    pub(crate) async fn is_current_worker_generation(
        &self,
        session_id: &str,
        generation: u64,
    ) -> bool {
        self.workers
            .lock()
            .await
            .get(session_id)
            .is_some_and(|worker| worker.lease.epoch() == generation)
    }

    /// Return the native store owned by the current worker only when it still
    /// owns the ACP identity being handed back to a terminal.
    pub(crate) async fn native_handoff_store(
        &self,
        session_id: &str,
        acp_session_id: &str,
    ) -> Option<crate::session::ExecutionBinding> {
        let workers = self.workers.lock().await;
        let handle = workers.get(session_id)?;
        if !lock_recover(&self.lifecycle).is_running(session_id)
            || handle.native_session_id.as_deref() != Some(acp_session_id)
        {
            return None;
        }
        handle.client.native_store.clone()
    }

    /// Whether this daemon holds the session's lease in any phase, including stopping.
    pub async fn is_owned(&self, session_id: &str) -> bool {
        lock_recover(&self.lifecycle).is_owned(session_id)
    }

    pub async fn count(&self) -> usize {
        self.workers.lock().await.len()
    }
}

fn next_seq(next_seqs: &SeqMap, session_id: &str) -> u64 {
    let mut guard = next_seqs.lock().unwrap_or_else(|e| e.into_inner());
    let entry = guard.entry(session_id.to_string()).or_insert(0);
    *entry = entry.saturating_add(1);
    *entry
}

fn lock_recover<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| {
        warn!(
            target: "acp.supervisor",
            "recovered poisoned supervisor lock"
        );
        e.into_inner()
    })
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[tokio::test]
    async fn respawned_worker_rejects_the_prior_generation() {
        let sup = Supervisor::new(VecSink::new());
        let first = sup.test_insert_worker("s-generation").await;
        let second = sup.test_respawn_worker("s-generation").await;

        assert_ne!(first, second);
        assert!(
            !sup.is_current_worker_generation("s-generation", first)
                .await
        );
        assert!(
            sup.is_current_worker_generation("s-generation", second)
                .await
        );
    }

    #[tokio::test]
    async fn bookkeeping_sets_track_and_drain() {
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
        assert!(sup
            .incompatible_sessions_for_binary("gemini")
            .await
            .is_empty());
        sup.test_insert_worker("s-claude-1").await;
        assert_eq!(
            sup.incompatible_sessions_for_binary("claude-agent-acp")
                .await,
            vec!["s-claude-2"],
            "a session with a live worker is not blocked"
        );

        sup.request_respawn("s-1");
        sup.request_respawn("s-2");
        sup.request_respawn("s-1");
        let mut ids = sup.take_respawn_requests();
        ids.sort();
        assert_eq!(ids, vec!["s-1", "s-2"]);
        assert!(sup.take_respawn_requests().is_empty());
    }
}
