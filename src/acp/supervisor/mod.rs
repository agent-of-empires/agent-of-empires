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
pub(crate) mod test_support;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::acp_client::{AcpClient, AcpError, SpawnConfig};
use super::agent_registry::AgentRegistry;
pub use super::runner_lifecycle::ResumeKind;
use super::runner_lifecycle::{
    Lease, LifecycleTable, ProcessControl, RunnerIdentity, SystemProcessControl, WorkerPhase,
    WorkerStart,
};
use super::state::AcpSessionId;
use crate::daemon::{AcpWorkerStart, AcpWorkerStartKind, AcpWorkerState};
use crate::session::SandboxInfo;
pub(crate) use teardown::{RetirementResources, RetirementTarget};

pub(crate) use agents::apply_agent_command_override;
pub(crate) use launch::{host_spawn_environment, with_durable_launch};
pub use sink::{BroadcastSink, ChannelSink};

/// Post-startup respawns allowed within `RESTART_WINDOW` before the session is parked.
const MAX_RESPAWNS_IN_WINDOW: u32 = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(60);
/// Backoff before respawning so an agent that crashes on startup cannot hot-loop.
const RESPAWN_BACKOFF: Duration = Duration::from_millis(500);
/// How long a runner request path waits for a mid-resume worker to land.
pub(crate) const WORKER_READY_TIMEOUT: Duration = Duration::from_secs(10);

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
type RestartRequests = Arc<std::sync::Mutex<HashMap<String, (u64, CancellationToken)>>>;
type SpawnRequests = Arc<std::sync::Mutex<SpawnRequestState>>;

#[derive(Default)]
struct SpawnRequestState {
    /// Requests that have not reached lifecycle admission yet. A stop cancels
    /// these before it waits on any durable launch work.
    pending: HashMap<String, HashMap<u64, CancellationToken>>,
    /// A stop fence remains held while the request owns its submission and
    /// instance guards, preventing a replacement from slipping through.
    stop_fences: HashMap<String, (u64, usize)>,
    switches: HashMap<String, usize>,
}

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
    #[error("session {0:?} has a restart in progress")]
    RestartInProgress(String),
    #[error("structured view worker capacity full ({current}/{limit}); raise [acp] max_concurrent_workers or delete an existing structured view session")]
    CapacityFull { current: usize, limit: u32 },
    /// A concurrent shutdown cancelled the resume; callers treat it as a soft success.
    #[error("resume of session {0:?} was cancelled by a concurrent shutdown")]
    SpawnCancelled(String),
    /// The previous runner is not proven dead yet.
    #[error("session {0:?} is still stopping its previous structured view worker")]
    TeardownPending(String),
    /// The worker a caller observed is no longer the running one.
    #[error("session {0:?} worker changed before the stop claimed it")]
    Superseded(String),
    #[error(transparent)]
    Blocked(crate::session::StartBlocked),
    #[error("session {0:?} no longer exists")]
    SessionGone(String),
    #[error("native session authority unavailable")]
    NativeStoreUnavailable,
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
    launch_epoch: u64,
    lease: Lease,
    native_session_id: Option<String>,
}

impl From<WorkerStart> for AcpWorkerStart {
    fn from(start: WorkerStart) -> Self {
        Self {
            kind: if start.first_start {
                AcpWorkerStartKind::FirstStart
            } else {
                AcpWorkerStartKind::Resume
            },
            started_at: start.started_at.to_rfc3339(),
        }
    }
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
    /// Explicit restart requests currently operating on each session.
    restart_requests: RestartRequests,
    next_restart_request_id: AtomicU64,
    spawn_requests: SpawnRequests,
    next_spawn_request_id: AtomicU64,
    next_stop_request_id: AtomicU64,
    process_control: Arc<dyn ProcessControl>,
    launcher: Launcher,
    native_admission_required: bool,
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

/// RAII guard over a `Starting` or `Respawning` epoch. A dropped attach
/// abandons only its observation unless an explicit stop claimed the runner.
pub(crate) struct ResumeReservation {
    lease: Lease,
    pub(crate) expected_lifecycle_generation: Option<u64>,
    attach_record: Option<Box<crate::process::worker_registry::WorkerRecord>>,
    lifecycle: Arc<std::sync::Mutex<LifecycleTable>>,
    notify: Arc<tokio::sync::Notify>,
}

/// Keeps concurrent explicit restart requests from restarting the same worker twice.
pub(crate) struct RestartRequestGuard {
    session_id: String,
    request_id: u64,
    cancellation: CancellationToken,
    requests: RestartRequests,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartClaimConflict {
    Switch,
    Restart,
}

/// Cancellation for a request that is still acquiring launch guards or
/// waiting to be admitted into the Lease table.
pub(crate) struct PendingSpawnGuard {
    session_id: String,
    request_id: u64,
    cancellation: CancellationToken,
    requests: SpawnRequests,
}

pub(crate) struct SwitchRequestGuard {
    session_id: String,
    requests: SpawnRequests,
}

impl Drop for SwitchRequestGuard {
    fn drop(&mut self) {
        let mut requests = lock_recover(&self.requests);
        if let Some(count) = requests.switches.get_mut(&self.session_id) {
            *count -= 1;
            if *count == 0 {
                requests.switches.remove(&self.session_id);
            }
        }
    }
}

/// A stop fence that outlives the HTTP request and cancels pending spawns.
pub(crate) struct StopRequestGuard {
    session_id: String,
    request_id: u64,
    requests: SpawnRequests,
    cancelled_pending: usize,
    cancelled_launch: bool,
}

impl Drop for PendingSpawnGuard {
    fn drop(&mut self) {
        let mut requests = lock_recover(&self.requests);
        if let Some(pending) = requests.pending.get_mut(&self.session_id) {
            pending.remove(&self.request_id);
            if pending.is_empty() {
                requests.pending.remove(&self.session_id);
            }
        }
    }
}

impl PendingSpawnGuard {
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }
}

impl Drop for StopRequestGuard {
    fn drop(&mut self) {
        let mut requests = lock_recover(&self.requests);
        if let Some((request_id, owners)) = requests.stop_fences.get_mut(&self.session_id) {
            if *request_id == self.request_id {
                *owners -= 1;
                if *owners == 0 {
                    requests.stop_fences.remove(&self.session_id);
                }
            }
        }
    }
}

impl StopRequestGuard {
    pub(crate) fn cancelled_target(&self) -> bool {
        self.cancelled_pending > 0 || self.cancelled_launch
    }

    pub(crate) fn cancelled_launch(&self) -> bool {
        self.cancelled_launch
    }
}

impl Drop for RestartRequestGuard {
    fn drop(&mut self) {
        let mut requests = lock_recover(&self.requests);
        if requests
            .get(&self.session_id)
            .is_some_and(|(request_id, _)| *request_id == self.request_id)
        {
            requests.remove(&self.session_id);
        }
    }
}

impl RestartRequestGuard {
    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
}

impl ResumeReservation {
    pub(crate) fn lease(&self) -> &Lease {
        &self.lease
    }

    pub(crate) fn cancellation(&self) -> &CancellationToken {
        self.lease.cancellation()
    }
}

impl Drop for ResumeReservation {
    fn drop(&mut self) {
        let mut lifecycle = lock_recover(&self.lifecycle);
        if !lifecycle.construction_is_pending(&self.lease) {
            return;
        }
        self.lease.cancellation().cancel();
        if self.lease.retirement_runners().next().is_none() && lifecycle.abandon(&self.lease) {
            self.notify.notify_waiters();
            return;
        }
        // abandon can refuse because a producer crossed the birth gate while
        // we inspected it. Convert that same construction instead of losing it.
        if lifecycle.convert_to_stopping(&self.lease) {
            let identity = self.lease.retirement_runners().next();
            lifecycle.settle(
                &self.lease,
                super::runner_lifecycle::Settlement::Unproven(identity),
            );
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
    pub(crate) launch_admission: Option<LaunchAdmission>,
    pub(crate) expected_lifecycle_generation: u64,
    pub session_id: String,
    /// The ACP backend `pick_agent_for_tool` resolved.
    pub agent: String,
    /// The session's logical tool (`Instance.tool`); host hooks see this as `AOE_TOOL`.
    pub tool: String,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    pub provider_env: Vec<(String, String)>,
    /// LLM backend pinned on the session row, one of
    /// `session::environment::AGENT_PROVIDERS`; `None` defers to the host.
    pub provider: Option<String>,
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
    /// The stored row's profile. Every production launch sets it, and the launch rechecks that
    /// row before and after the handshake (#4116).
    pub source_profile: Option<String>,
    pub yolo_mode: bool,
    /// Explicit ACP mode applied after the handshake; wins over `yolo_mode`.
    pub acp_mode_id: Option<String>,
    pub agent_command_override: Option<AgentCommandOverride>,
    /// Let a `session/load` replay history into the (empty) event store for an import.
    pub seed_history_replay: bool,
    /// A conversation existed before this launch, so replacing it must be announced.
    pub prior_conversation: bool,
    /// Claude store selected by the conversation binding for a host Claude worker.
    pub claude_store_pin: Option<crate::session::capture::ClaudeStorePin>,
}

#[derive(Clone)]
pub(crate) struct LaunchAdmission {
    pub store: Arc<dyn crate::session::SessionStore>,
    pub generation: u64,
    pub namespace: Option<Arc<tokio::sync::RwLock<()>>>,
}

impl std::fmt::Debug for LaunchAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LaunchAdmission")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
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
            restart_requests: Arc::default(),
            next_restart_request_id: AtomicU64::new(1),
            spawn_requests: Arc::default(),
            next_spawn_request_id: AtomicU64::new(1),
            next_stop_request_id: AtomicU64::new(1),
            process_control: Arc::new(SystemProcessControl::default()),
            launcher: Arc::new(|config, session_id| Box::pin(AcpClient::spawn(config, session_id))),
            native_admission_required: false,
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

    pub(crate) fn with_native_admission(mut self) -> Self {
        self.native_admission_required = true;
        self
    }
    pub(crate) fn max_concurrent_workers(&self) -> u32 {
        self.max_concurrent_workers
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

    pub async fn worker_states_snapshot(
        &self,
    ) -> HashMap<String, (AcpWorkerState, Option<AcpWorkerStart>)> {
        lock_recover(&self.lifecycle)
            .snapshot()
            .into_iter()
            .map(|(id, (phase, start))| (id, (phase.into(), start.map(Into::into))))
            .collect()
    }

    pub async fn worker_state(&self, session_id: &str) -> AcpWorkerState {
        lock_recover(&self.lifecycle).phase(session_id).into()
    }

    /// Drop bookkeeping only after the matching runner retirement is proven.
    pub fn forget_session(&self, session_id: &str) -> Result<(), SupervisorError> {
        let mut lifecycle = lock_recover(&self.lifecycle);
        if lifecycle.phase(session_id) == WorkerPhase::Resuming {
            // Keep the lease until its cancelled producer has retired any runner
            // and scope it created; the launch path will settle this stop debt.
            let _ = lifecycle.begin_stop(session_id, "user_stopped", false);
            drop(lifecycle);
            self.worker_notify.notify_waiters();
            return Err(SupervisorError::TeardownPending(session_id.into()));
        }
        if !lifecycle.forget(session_id) {
            return Err(SupervisorError::TeardownPending(session_id.into()));
        }
        drop(lifecycle);
        if let Ok(mut guard) = self.next_seqs.lock() {
            guard.remove(session_id);
        }
        lock_recover(&self.startup_failures).remove(session_id);
        lock_recover(&self.respawned_in_place).remove(session_id);
        Ok(())
    }

    /// Seed seq counters from the event store's stored maxima.
    pub fn hydrate_seqs(&self, pairs: impl IntoIterator<Item = (String, u64)>) {
        if let Ok(mut guard) = self.next_seqs.lock() {
            guard.extend(pairs);
        }
    }

    /// The running epoch, for a caller that must stop only that epoch.
    pub(crate) fn running_lease(&self, session_id: &str) -> Option<Lease> {
        lock_recover(&self.lifecycle)
            .running(session_id)
            .map(|(lease, _)| lease)
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

    /// Automatic respawns retain the originating launch; a new launch supersedes it.
    pub(crate) async fn is_current_launch(&self, session_id: &str, launch_epoch: u64) -> bool {
        self.workers
            .lock()
            .await
            .get(session_id)
            .is_some_and(|worker| worker.launch_epoch == launch_epoch)
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

    /// Register an HTTP spawn before it acquires submission and instance
    /// guards. The stop endpoint uses this token to wake that path promptly.
    pub(crate) fn begin_spawn_request(
        &self,
        session_id: &str,
    ) -> Result<PendingSpawnGuard, SupervisorError> {
        let mut requests = lock_recover(&self.spawn_requests);
        if requests.stop_fences.contains_key(session_id) {
            return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
        }
        if lock_recover(&self.restart_requests).contains_key(session_id) {
            return Err(SupervisorError::RestartInProgress(session_id.to_string()));
        }
        let request_id = self.next_spawn_request_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        requests
            .pending
            .entry(session_id.to_string())
            .or_default()
            .insert(request_id, cancellation.clone());
        Ok(PendingSpawnGuard {
            session_id: session_id.to_string(),
            request_id,
            cancellation,
            requests: Arc::clone(&self.spawn_requests),
        })
    }

    pub(crate) fn begin_switch_request(
        &self,
        session_id: &str,
    ) -> Result<SwitchRequestGuard, SupervisorError> {
        let mut requests = lock_recover(&self.spawn_requests);
        if lock_recover(&self.restart_requests).contains_key(session_id) {
            return Err(SupervisorError::RestartInProgress(session_id.to_string()));
        }
        *requests.switches.entry(session_id.to_string()).or_default() += 1;
        Ok(SwitchRequestGuard {
            session_id: session_id.to_string(),
            requests: Arc::clone(&self.spawn_requests),
        })
    }

    /// Fence replacement admission and cancel every spawn that has not yet
    /// acquired a Lease. The lifecycle stop still owns any already-admitted
    /// launch and its physical teardown.
    pub(crate) async fn begin_stop_request(
        &self,
        session_id: &str,
        reason: &str,
        expected_lease: Option<&Lease>,
        captured: bool,
    ) -> StopRequestGuard {
        self.begin_stop_request_with_pending_cancellation(
            session_id,
            reason,
            expected_lease,
            captured,
            true,
        )
        .await
    }

    /// Resume already owns launch exclusion. Fence its captured retirement
    /// without cancelling the pending request that is performing that resume.
    pub(super) async fn begin_retirement_request(
        &self,
        session_id: &str,
        expected_lease: Option<&Lease>,
    ) -> StopRequestGuard {
        self.begin_stop_request_with_pending_cancellation(
            session_id,
            "user_stopped",
            expected_lease,
            true,
            false,
        )
        .await
    }

    async fn begin_stop_request_with_pending_cancellation(
        &self,
        session_id: &str,
        reason: &str,
        expected_lease: Option<&Lease>,
        captured: bool,
        cancel_pending: bool,
    ) -> StopRequestGuard {
        let (request_id, cancelled_pending) = {
            let mut requests = lock_recover(&self.spawn_requests);
            let request_id =
                if let Some((request_id, owners)) = requests.stop_fences.get_mut(session_id) {
                    *owners += 1;
                    *request_id
                } else {
                    let request_id = self.next_stop_request_id.fetch_add(1, Ordering::Relaxed);
                    requests
                        .stop_fences
                        .insert(session_id.to_string(), (request_id, 1));
                    request_id
                };
            let pending = requests
                .pending
                .get(session_id)
                .filter(|_| cancel_pending)
                .map(|pending| pending.values().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let cancelled_pending = pending
                .iter()
                .filter(|cancellation| !cancellation.is_cancelled())
                .count();
            for cancellation in pending {
                cancellation.cancel();
            }
            (request_id, cancelled_pending)
        };
        // Install the guard before the first await. If acquiring the worker
        // gate is cancelled, Drop must release the fence owner that was just
        // registered above.
        let mut stop_request = StopRequestGuard {
            session_id: session_id.to_string(),
            request_id,
            requests: Arc::clone(&self.spawn_requests),
            cancelled_pending,
            cancelled_launch: false,
        };
        let workers = self.workers.lock().await;
        let cancelled_launch = {
            let mut lifecycle = lock_recover(&self.lifecycle);
            let matches_target = expected_lease.is_some_and(|expected| {
                lifecycle.current_lease(session_id).as_ref() == Some(expected)
            });
            if !captured || matches_target {
                let was_cancelled = lifecycle
                    .current_lease(session_id)
                    .is_some_and(|lease| lifecycle.cancel_requested(&lease).is_some());
                lifecycle.request_stop(session_id, reason) && !was_cancelled
            } else {
                false
            }
        };
        drop(workers);
        if cancelled_launch {
            self.worker_notify.notify_waiters();
        }
        stop_request.cancelled_launch = cancelled_launch;
        stop_request
    }

    /// Claim the session's restart lane while holding the same gate used by
    /// `begin_resume`, so an explicit launch cannot slip past the claim.
    pub(crate) async fn try_begin_restart_with_reason(
        &self,
        session_id: &str,
    ) -> Result<RestartRequestGuard, RestartClaimConflict> {
        let _workers = self.workers.lock().await;
        let spawn_requests = lock_recover(&self.spawn_requests);
        if spawn_requests.switches.contains_key(session_id) {
            return Err(RestartClaimConflict::Switch);
        }
        let mut requests = lock_recover(&self.restart_requests);
        if requests.contains_key(session_id) {
            return Err(RestartClaimConflict::Restart);
        }
        let request_id = self.next_restart_request_id.fetch_add(1, Ordering::Relaxed);
        let cancellation = CancellationToken::new();
        requests.insert(session_id.to_string(), (request_id, cancellation.clone()));
        Ok(RestartRequestGuard {
            session_id: session_id.to_string(),
            request_id,
            cancellation,
            requests: Arc::clone(&self.restart_requests),
        })
    }

    /// Serialize with resume admission, then cancel any restart that has not settled yet.
    pub(crate) async fn cancel_restart_request(&self, session_id: &str) {
        let _workers = self.workers.lock().await;
        if let Some((_, cancellation)) = lock_recover(&self.restart_requests).get(session_id) {
            cancellation.cancel();
        }
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
    async fn explicit_restart_guard_serializes_same_session_and_releases_on_drop() {
        let sup = Supervisor::new(VecSink::new());
        let first = sup
            .try_begin_restart_with_reason("s-restart")
            .await
            .expect("first restart claims the session");

        assert!(sup
            .try_begin_restart_with_reason("s-restart")
            .await
            .is_err());
        assert!(sup.try_begin_restart_with_reason("s-other").await.is_ok());

        drop(first);
        assert!(sup.try_begin_restart_with_reason("s-restart").await.is_ok());
    }

    #[tokio::test]
    async fn restart_and_switch_claims_exclude_each_other() {
        let sup = Supervisor::new(VecSink::new());
        let switching = sup.begin_switch_request("s").unwrap();
        assert!(sup.try_begin_restart_with_reason("s").await.is_err());
        drop(switching);
        let restart = sup.try_begin_restart_with_reason("s").await.unwrap();
        assert!(matches!(
            sup.begin_switch_request("s"),
            Err(SupervisorError::RestartInProgress(_))
        ));
        assert!(matches!(
            sup.begin_spawn_request("s"),
            Err(SupervisorError::RestartInProgress(_))
        ));
        assert!(sup.begin_switch_request("other").is_ok());
        drop(restart);
        assert!(sup.begin_switch_request("s").is_ok());
        assert!(sup.begin_spawn_request("s").is_ok());
    }

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

    #[tokio::test]
    async fn cancelled_stop_request_drops_fence_before_fresh_admission() {
        let sup = Arc::new(Supervisor::new(VecSink::new()));
        let workers = sup.workers.lock().await;
        let stopping = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.begin_stop_request("s-fence-cancel", "user_stopped", None, false)
                    .await
            })
        };

        // The stop request must publish its fence before waiting for workers.
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while !lock_recover(&sup.spawn_requests)
                .stop_fences
                .contains_key("s-fence-cancel")
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stop fence is published");
        assert!(lock_recover(&sup.spawn_requests)
            .stop_fences
            .contains_key("s-fence-cancel"));
        assert!(matches!(
            sup.begin_spawn_request("s-fence-cancel"),
            Err(SupervisorError::SpawnCancelled(_))
        ));

        stopping.abort();
        assert!(matches!(stopping.await, Err(join) if join.is_cancelled()));
        drop(workers);
        assert!(!lock_recover(&sup.spawn_requests)
            .stop_fences
            .contains_key("s-fence-cancel"));
        let fresh = sup
            .begin_spawn_request("s-fence-cancel")
            .expect("cancelled stop must not strand admission");
        drop(fresh);
    }

    #[tokio::test]
    async fn stop_fence_cancellation_helper_cancels_pending_spawn_before_worker_wait() {
        let sup = Arc::new(Supervisor::new(VecSink::new()));
        let pending = sup.begin_spawn_request("s-fence-helper").unwrap();
        let workers = sup.workers.lock().await;
        let stopping = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.begin_stop_request("s-fence-helper", "user_stopped", None, false)
                    .await
            })
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            pending.cancellation().cancelled(),
        )
        .await
        .expect("pending spawn is cancelled");
        assert!(pending.cancellation().is_cancelled());
        stopping.abort();
        assert!(matches!(stopping.await, Err(join) if join.is_cancelled()));
        drop(workers);
        drop(pending);
        assert!(!lock_recover(&sup.spawn_requests)
            .stop_fences
            .contains_key("s-fence-helper"));
    }
}
