//! Per-session ownership of a structured-view runner.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Registry generation and kernel birth identity of one runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunnerIdentity {
    pub pid: u32,
    pub generation: u64,
    pub birth_fingerprint: Option<crate::process::ProcessBirthFingerprint>,
}

impl RunnerIdentity {
    pub fn unattested(pid: u32, generation: u64) -> Self {
        Self {
            pid,
            generation,
            birth_fingerprint: None,
        }
    }

    pub fn from_record(record: &crate::process::worker_registry::WorkerRecord) -> Self {
        Self {
            pid: record.pid,
            generation: record.generation,
            birth_fingerprint: record
                .runner_identity
                .as_ref()
                .map(|identity| identity.fingerprint()),
        }
    }

    pub fn owns_record(&self, record: &crate::process::worker_registry::WorkerRecord) -> bool {
        self.pid == record.pid
            && self.generation == record.generation
            && self.birth_fingerprint
                == record
                    .runner_identity
                    .as_ref()
                    .map(|identity| identity.fingerprint())
    }
    pub fn disowns_record(&self, record: &crate::process::worker_registry::WorkerRecord) -> bool {
        !self.matches_record(record.pid, record.generation)
            || self
                .birth_fingerprint
                .zip(
                    record
                        .runner_identity
                        .as_ref()
                        .map(|identity| identity.fingerprint()),
                )
                .is_some_and(|(expected, current)| expected != current)
    }

    /// Legacy zero generations do not establish destructive authority.
    pub fn matches_record(&self, pid: u32, generation: u64) -> bool {
        self.pid == pid
            && (self.generation == 0 || generation == 0 || self.generation == generation)
    }
}

/// Frozen authority for native processes outside the runner's process group.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum PrimeOwnership {
    #[default]
    Unresolved,
    NotPrime,
    Owned {
        runtime: Box<crate::process::prime::PrimeRuntime>,
    },
}

impl PrimeOwnership {
    pub(crate) fn from_record(record: &crate::process::worker_registry::WorkerRecord) -> Self {
        match &record.prime_runtime {
            Some(runtime) => Self::Owned {
                runtime: Box::new(runtime.clone()),
            },
            None if record.agent_key != "prime-agent" => Self::NotPrime,
            None => Self::Unresolved,
        }
    }

    pub(crate) fn ensure_stopped(&self) -> anyhow::Result<()> {
        match self {
            Self::NotPrime => Ok(()),
            Self::Owned { runtime } => runtime.verify_stopped(),
            Self::Unresolved => anyhow::bail!("Native runner ownership is unresolved"),
        }
    }

    pub(crate) fn retire(&self) -> anyhow::Result<()> {
        match self {
            Self::NotPrime => Ok(()),
            Self::Owned { runtime } => runtime.cleanup(),
            Self::Unresolved => anyhow::bail!("Native runner ownership is unresolved"),
        }
    }
}

/// Which code path is bringing a worker up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeKind {
    Attach,
    /// A spawn for a session with a stored ACP session id.
    Spawn,
    /// A spawn for a session that has no stored ACP session id yet.
    SpawnFirst,
}

/// When the current start was admitted, and whether it is a session's first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerStart {
    pub first_start: bool,
    pub started_at: DateTime<Utc>,
}

/// Authority token for one epoch of one session.
#[derive(Debug, Clone)]
pub struct Lease {
    session_id: String,
    epoch: u64,
    construction: Arc<Construction>,
}

#[derive(Debug, Clone)]
pub(crate) struct StopCompletion {
    pub(crate) reason: Option<String>,
    pub(crate) done: bool,
}

struct Construction {
    id: uuid::Uuid,
    cancellation: CancellationToken,
    retirement_authorized: std::sync::atomic::AtomicBool,
    runner: Mutex<Option<RunnerIdentity>>,
    scopes: Mutex<Vec<crate::process::worker_registry::ScopeOwnership>>,
    admission: Mutex<Option<crate::acp::supervisor::LaunchAdmission>>,
    prime: Mutex<PrimeOwnership>,
    cancelled_before_birth: std::sync::atomic::AtomicBool,
    adapter_route: Mutex<Option<Arc<AdapterRetirementRoute>>>,
    adapter_delete_handled: std::sync::atomic::AtomicBool,
    stop_completion: Mutex<Option<StopCompletion>>,
    retirement: crate::acp::supervisor::RetirementResources,
    previous_runner: Mutex<Option<RunnerIdentity>>,
    retire_previous: std::sync::atomic::AtomicBool,
    spawn_attestor:
        Mutex<Option<tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<bool>>>>,
}

impl Default for Construction {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4(),
            cancellation: CancellationToken::new(),
            retirement_authorized: std::sync::atomic::AtomicBool::new(true),
            runner: Mutex::default(),
            scopes: Mutex::default(),
            admission: Mutex::default(),
            prime: Mutex::default(),
            cancelled_before_birth: std::sync::atomic::AtomicBool::new(false),
            adapter_route: Mutex::default(),
            adapter_delete_handled: std::sync::atomic::AtomicBool::new(false),
            stop_completion: Mutex::default(),
            retirement: crate::acp::supervisor::RetirementResources::default(),
            previous_runner: Mutex::default(),
            retire_previous: std::sync::atomic::AtomicBool::new(false),
            spawn_attestor: Mutex::default(),
        }
    }
}

impl std::fmt::Debug for Construction {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt.debug_struct("Construction").finish_non_exhaustive()
    }
}

impl PartialEq for Lease {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.epoch == other.epoch
            && self.producer_id() == other.producer_id()
    }
}

impl Eq for Lease {}
pub(crate) struct AdapterRetirementRoute {
    pub(crate) client: Arc<crate::acp::acp_client::AcpClient>,
    pub(crate) identity: RunnerIdentity,
    pub(crate) acp_id: String,
    pub(crate) adapter: String,
    pub(crate) gate: tokio::sync::Mutex<()>,
    pub(crate) retired: std::sync::atomic::AtomicBool,
}

impl Lease {
    /// An attach observes the original runner until installation or an explicit stop.
    pub(crate) fn is_borrowed_attach(&self) -> bool {
        !self
            .construction
            .retirement_authorized
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn claim_runner_ownership(&self) {
        self.construction
            .retirement_authorized
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn retirement(&self) -> &crate::acp::supervisor::RetirementResources {
        &self.construction.retirement
    }

    pub(crate) fn owe_stop_completion(&self, reason: Option<&str>) {
        let mut slot = self
            .construction
            .stop_completion
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match slot.as_mut() {
            None => {
                *slot = Some(StopCompletion {
                    reason: reason.map(str::to_owned),
                    done: false,
                })
            }
            Some(debt) if debt.reason.is_none() && reason.is_some() => {
                debt.reason = reason.map(str::to_owned);
                debt.done = false;
            }
            Some(_) => {}
        }
    }

    pub(crate) fn pending_stop_completion(&self) -> Option<StopCompletion> {
        self.construction
            .stop_completion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|debt| !debt.done)
            .cloned()
    }

    pub(crate) fn stop_completion_pending(&self) -> bool {
        self.construction
            .stop_completion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .is_some_and(|debt| !debt.done)
    }

    pub(crate) fn stop_completion_done(&self) {
        if let Some(debt) = self
            .construction
            .stop_completion
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_mut()
        {
            debt.done = true;
        }
    }

    pub(crate) fn previous_runner(&self) -> Option<RunnerIdentity> {
        *self
            .construction
            .previous_runner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Freeze the predecessor role, never a later registry observation.
    pub(crate) fn owe_previous_retirement(&self) {
        self.construction
            .retire_previous
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// The original captured predecessor, only after its retirement debt is owed.
    pub(crate) fn retirement_previous_runner(&self) -> Option<RunnerIdentity> {
        self.construction
            .retire_previous
            .load(std::sync::atomic::Ordering::Acquire)
            .then(|| self.previous_runner())
            .flatten()
    }

    pub(crate) fn retirement_runners(&self) -> impl Iterator<Item = RunnerIdentity> {
        let current = self.spawned_runner().filter(|_| !self.is_borrowed_attach());
        let previous = self.retirement_previous_runner();
        [
            current,
            previous.filter(|previous| Some(*previous) != current),
        ]
        .into_iter()
        .flatten()
    }

    fn commit_owned_child_attestation(&self, candidate: RunnerIdentity) -> bool {
        if candidate.birth_fingerprint.is_none() {
            return false;
        }
        let mut runner = self
            .construction
            .runner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(current) = runner.as_mut() else {
            return false;
        };
        if current.pid != candidate.pid || current.generation != candidate.generation {
            return false;
        }
        match current.birth_fingerprint {
            None => {
                *current = candidate;
                true
            }
            Some(birth) => Some(birth) == candidate.birth_fingerprint,
        }
    }

    /// Keep an unattested child's PID reserved until its owner can sample birth.
    pub(crate) fn record_spawned_child(
        &self,
        mut child: tokio::process::Child,
        identity: RunnerIdentity,
    ) {
        {
            let mut runner = self
                .construction
                .runner
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            assert!(runner.is_none(), "one physical child per construction");
            *runner = Some(identity);
        }
        if identity.birth_fingerprint.is_some() {
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            return;
        }
        let (requests, mut receive) =
            tokio::sync::mpsc::unbounded_channel::<tokio::sync::oneshot::Sender<bool>>();
        *self
            .construction
            .spawn_attestor
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(requests);
        let owner = self.clone();
        tokio::spawn(async move {
            // No wait, try_wait or Child drop may precede a successful sample.
            while let Some(reply) = receive.recv().await {
                let attested = crate::process::process_birth_identity(identity.pid)
                    .ok()
                    .flatten()
                    .map(|birth| RunnerIdentity {
                        pid: identity.pid,
                        generation: identity.generation,
                        birth_fingerprint: Some(birth.fingerprint()),
                    })
                    .is_some_and(|candidate| owner.commit_owned_child_attestation(candidate));
                let _ = reply.send(attested);
                if attested {
                    break;
                }
            }
            let _ = child.wait().await;
        });
    }

    pub(crate) async fn attest_runner(&self) -> bool {
        let Some(current) = self.spawned_runner() else {
            return true;
        };
        if current.birth_fingerprint.is_some() {
            return true;
        }
        let sender = self
            .construction
            .spawn_attestor
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let Some(sender) = sender else {
            return false;
        };
        let (reply, response) = tokio::sync::oneshot::channel();
        if sender.send(reply).is_err() {
            return false;
        }
        response.await.unwrap_or(false)
    }

    pub(crate) fn record_adapter_route(&self, route: AdapterRetirementRoute) {
        let mut slot = self
            .construction
            .adapter_route
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if slot.is_none() {
            *slot = Some(Arc::new(route));
        }
    }

    pub(crate) fn adapter_route(&self) -> Option<Arc<AdapterRetirementRoute>> {
        self.construction
            .adapter_route
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub(crate) fn producer_id(&self) -> uuid::Uuid {
        self.construction.id
    }

    pub(crate) fn cancellation(&self) -> &CancellationToken {
        &self.construction.cancellation
    }

    pub(crate) fn set_admission(&self, admission: Option<crate::acp::supervisor::LaunchAdmission>) {
        *self
            .construction
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = admission;
    }

    pub(crate) fn admission(&self) -> Option<crate::acp::supervisor::LaunchAdmission> {
        self.construction
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn record_scope(&self, scope: crate::process::worker_registry::ScopeOwnership) {
        let mut scopes = self
            .construction
            .scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }

    pub(crate) fn scopes(&self) -> Vec<crate::process::worker_registry::ScopeOwnership> {
        self.construction
            .scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn lock_birth(
        &self,
    ) -> std::sync::MutexGuard<'_, Vec<crate::process::worker_registry::ScopeOwnership>> {
        self.construction
            .scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Freeze adopted identity; metadata cannot upgrade an existing birth.
    pub(crate) fn record_runner(&self, identity: RunnerIdentity) -> bool {
        let mut runner = self
            .construction
            .runner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match *runner {
            None => {
                *runner = Some(identity);
                true
            }
            Some(current) => current == identity,
        }
    }

    pub(crate) fn spawned_runner(&self) -> Option<RunnerIdentity> {
        let _birth = self.lock_birth();
        *self
            .construction
            .runner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(crate) fn record_prime(&self, ownership: PrimeOwnership) {
        let mut prime = self
            .construction
            .prime
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !self.was_cancelled_before_birth() {
            *prime = ownership;
        }
    }

    pub(crate) fn prime(&self) -> PrimeOwnership {
        self.construction
            .prime
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(crate) fn constructed_ownership(&self) -> (Option<RunnerIdentity>, PrimeOwnership) {
        let _birth = self.lock_birth();
        let identity = *self
            .construction
            .runner
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (identity, self.prime())
    }

    fn cancel_before_birth(&self) -> bool {
        self.cancellation().cancel();
        let _birth = self.lock_birth();
        if self
            .construction
            .runner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .is_some()
        {
            return false;
        }
        let mut prime = self
            .construction
            .prime
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *prime = PrimeOwnership::NotPrime;
        self.construction
            .cancelled_before_birth
            .store(true, std::sync::atomic::Ordering::Release);
        true
    }

    pub(crate) fn was_cancelled_before_birth(&self) -> bool {
        self.construction
            .cancelled_before_birth
            .load(std::sync::atomic::Ordering::Acquire)
    }
}

/// Public lifecycle state, surfaced as `acp_worker_state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerPhase {
    Absent,
    Resuming,
    Running,
    Stopping,
}

#[derive(Debug)]
enum Phase {
    Starting {
        kind: ResumeKind,
        /// Wall clock, so it survives a client reload. A daemon restart that
        /// re-adopts a starting worker admits again and resets it.
        started_at: DateTime<Utc>,
        cancel: Option<String>,
    },
    Running {
        identity: Option<RunnerIdentity>,
    },
    Respawning {
        started_at: DateTime<Utc>,
        cancel: Option<String>,
    },
    Stopping {
        /// Teardown attempts already made under this epoch.
        attempts: u32,
        /// When this teardown was claimed, so one whose driver went away
        /// (a dropped request future) can be reclaimed by the retry pass.
        since: Instant,
    },
    TeardownRetry {
        identity: Option<RunnerIdentity>,
        attempts: u32,
    },
    RetirementComplete,
}

#[derive(Debug)]
struct Entry {
    epoch: u64,
    phase: Phase,
    delete_adapter_state: bool,
    journal_ack_required: bool,
    construction: Arc<Construction>,
    /// An explicit stop, or a failed first launch, owes a scope sweep before
    /// the settle. Kept through retries, so a reclaimed teardown sweeps too.
    sweep_owed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmitError {
    /// A worker is running or another task is mid-resume.
    AlreadyPresent,
    /// A previous runner has not been proven dead yet.
    TeardownPending,
    /// A stop was asked of a resume that then failed before it installed;
    /// the stop stands against this one admission (the reconciler's
    /// fallback), carrying its reason.
    Cancelled(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallError {
    Stale,
    /// A stop arrived while the worker was coming up.
    Cancelled {
        reason: String,
    },
}

#[derive(Debug)]
pub enum StopDecision {
    NotOwned,
    /// The in-flight resume or respawn will tear down what it built.
    CancelRequested,
    /// The caller now owns teardown of the running worker.
    TearDown {
        lease: Lease,
        identity: Option<RunnerIdentity>,
    },
    AlreadyStopping,
}

/// Runtime process-group signalling and exit proof.
pub trait ProcessControl: Send + Sync + 'static {
    fn is_alive(&self, identity: RunnerIdentity) -> bool;
    fn terminate_group(&self, identity: RunnerIdentity);
    fn kill_group(&self, identity: RunnerIdentity);
    fn forget(&self, identity: RunnerIdentity);
    /// Retire only captured scopes and prove them empty; failure retains the lease.
    fn release_scope(
        &self,
        session_id: &str,
        scopes: &[crate::process::worker_registry::ScopeOwnership],
    ) -> bool;
    fn scopes_stopped(
        &self,
        session_id: &str,
        scopes: &[crate::process::worker_registry::ScopeOwnership],
    ) -> bool;
}

#[derive(Default)]
pub struct SystemProcessControl {
    guards: std::sync::Mutex<HashMap<RunnerIdentity, crate::process::RunnerProcessGuard>>,
}

impl SystemProcessControl {
    fn with_guard<T>(
        &self,
        identity: RunnerIdentity,
        action: impl FnOnce(&crate::process::RunnerProcessGuard) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut guards = self
            .guards
            .lock()
            .map_err(|_| anyhow::anyhow!("runner guard lock poisoned"))?;
        if let std::collections::hash_map::Entry::Vacant(entry) = guards.entry(identity) {
            entry.insert(crate::process::RunnerProcessGuard::capture(
                identity.pid,
                identity.birth_fingerprint,
            )?);
        }
        action(&guards[&identity])
    }
}

impl ProcessControl for SystemProcessControl {
    fn is_alive(&self, identity: RunnerIdentity) -> bool {
        if identity.pid == 0 {
            return false;
        }
        self.with_guard(identity, |guard| guard.executing())
            .unwrap_or(true)
    }

    fn terminate_group(&self, identity: RunnerIdentity) {
        if let Err(error) = self.with_guard(identity, |guard| guard.signal(libc::SIGTERM)) {
            tracing::warn!(pid = identity.pid, %error, "runner termination unproven");
        }
    }

    fn kill_group(&self, identity: RunnerIdentity) {
        if let Err(error) = self.with_guard(identity, |guard| guard.signal(libc::SIGKILL)) {
            tracing::warn!(pid = identity.pid, %error, "runner termination unproven");
        }
    }

    fn forget(&self, identity: RunnerIdentity) {
        if let Ok(mut guards) = self.guards.lock() {
            guards.remove(&identity);
        }
    }

    fn release_scope(
        &self,
        session_id: &str,
        scopes: &[crate::process::worker_registry::ScopeOwnership],
    ) -> bool {
        scopes
            .iter()
            .all(|scope| crate::process::systemd::retire_scope(session_id, scope).is_ok())
    }

    fn scopes_stopped(
        &self,
        session_id: &str,
        scopes: &[crate::process::worker_registry::ScopeOwnership],
    ) -> bool {
        scopes
            .iter()
            .all(|scope| crate::process::systemd::scope_stopped(session_id, scope).unwrap_or(false))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// Process-group exit and registry cleanup were proven.
    Proven,
    /// Exit or registry ownership is unproven; retain the lease and retry.
    Unproven(Option<RunnerIdentity>),
}

/// Pending teardown a retry pass should drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryClaim {
    pub lease: Lease,
    /// `None` when the runner identity could not be read or recovered.
    pub identity: Option<RunnerIdentity>,
    pub attempts: u32,
}

pub struct LifecycleTable {
    entries: HashMap<String, Entry>,
    /// Also the generation stamped on the next spawned runner, so it must
    /// stay unique across daemon restarts; the supervisor seeds it from
    /// the wall clock.
    next_epoch: u64,
    /// Highest generation ever admitted or observed per session.
    last_generation: HashMap<String, u64>,
    /// Stops asked of resumes that were abandoned before they installed,
    /// consumed by the next `admit` so the reconciler cannot spawn over
    /// the user's stop.
    stale_cancels: HashMap<String, String>,
}

impl LifecycleTable {
    pub fn new(seed_epoch: u64) -> Self {
        Self {
            entries: HashMap::new(),
            next_epoch: seed_epoch.max(1),
            last_generation: HashMap::new(),
            stale_cancels: HashMap::new(),
        }
    }

    /// Next epoch.
    fn mint(&mut self, session_id: &str, stamped: bool) -> u64 {
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        if stamped {
            self.note_generation(session_id, epoch);
        }
        epoch
    }

    fn lease(&self, session_id: &str, epoch: u64) -> Lease {
        Lease {
            session_id: session_id.to_string(),
            epoch,
            construction: Arc::clone(&self.entries[session_id].construction),
        }
    }

    pub(crate) fn is_starting_spawn(&self, lease: &Lease) -> bool {
        self.entries.get(lease.session_id()).is_some_and(|entry| {
            entry.epoch == lease.epoch()
                && Arc::ptr_eq(&entry.construction, &lease.construction)
                && match &entry.phase {
                    Phase::Starting { kind, .. } => *kind != ResumeKind::Attach,
                    _ => false,
                }
        })
    }
    pub(crate) fn current_lease(&self, session_id: &str) -> Option<Lease> {
        self.entries
            .get(session_id)
            .map(|entry| self.lease(session_id, entry.epoch))
    }
    fn current(&mut self, lease: &Lease) -> Option<&mut Entry> {
        self.entries.get_mut(&lease.session_id).filter(|entry| {
            entry.epoch == lease.epoch && Arc::ptr_eq(&entry.construction, &lease.construction)
        })
    }

    /// Record a generation observed on disk so marker authority tracks
    /// runners this daemon did not spawn.
    pub fn note_generation(&mut self, session_id: &str, generation: u64) {
        let slot = self
            .last_generation
            .entry(session_id.to_string())
            .or_default();
        *slot = (*slot).max(generation);
    }

    pub fn forget_stale_cancel(&mut self, session_id: &str) {
        self.stale_cancels.remove(session_id);
    }

    pub fn forget(&mut self, session_id: &str) -> bool {
        if self.entries.contains_key(session_id) {
            return false;
        }
        self.last_generation.remove(session_id);
        self.stale_cancels.remove(session_id);
        true
    }

    pub fn last_generation(&self, session_id: &str) -> u64 {
        self.last_generation.get(session_id).copied().unwrap_or(0)
    }

    /// Reserve the session for a spawn or attach.
    pub fn admit(&mut self, session_id: &str, kind: ResumeKind) -> Result<Lease, AdmitError> {
        match self.entries.get(session_id).map(|e| &e.phase) {
            None => {}
            Some(
                Phase::Stopping { .. } | Phase::TeardownRetry { .. } | Phase::RetirementComplete,
            ) => return Err(AdmitError::TeardownPending),
            Some(_) => return Err(AdmitError::AlreadyPresent),
        }
        if let Some(reason) = self.stale_cancels.remove(session_id) {
            return Err(AdmitError::Cancelled(reason));
        }
        let epoch = self.mint(session_id, kind != ResumeKind::Attach);
        self.entries.insert(
            session_id.to_string(),
            Entry {
                epoch,
                phase: Phase::Starting {
                    kind,
                    started_at: Utc::now(),
                    cancel: None,
                },
                delete_adapter_state: false,
                journal_ack_required: false,
                construction: Arc::new(Construction {
                    retirement_authorized: std::sync::atomic::AtomicBool::new(
                        kind != ResumeKind::Attach,
                    ),
                    ..Construction::default()
                }),
                sweep_owed: false,
            },
        );
        Ok(self.lease(session_id, epoch))
    }

    /// Install only the construction's canonical identity.
    pub fn install(
        &mut self,
        lease: &Lease,
        identity: Option<RunnerIdentity>,
    ) -> Result<(), InstallError> {
        let Some(entry) = self.current(lease) else {
            return Err(InstallError::Stale);
        };
        let cancel = match &mut entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel, .. } => cancel.take(),
            _ => return Err(InstallError::Stale),
        };
        if let Some(reason) = cancel {
            entry.phase = Phase::Stopping {
                attempts: 0,
                since: Instant::now(),
            };
            entry.sweep_owed = true;
            return Err(InstallError::Cancelled { reason });
        }
        if identity.is_some_and(|identity| !lease.record_runner(identity)) {
            if lease.is_borrowed_attach() {
                return Err(InstallError::Stale);
            }
            entry.phase = Phase::Stopping {
                attempts: 0,
                since: Instant::now(),
            };
            entry.sweep_owed = true;
            lease.owe_stop_completion(None);
            return Err(InstallError::Stale);
        }
        lease.claim_runner_ownership();
        entry.phase = Phase::Running { identity };
        if let Some(identity) = identity {
            self.note_generation(&lease.session_id, identity.generation);
        }
        Ok(())
    }

    /// Give up a starting or respawning epoch that built nothing.
    pub fn abandon(&mut self, lease: &Lease) -> bool {
        let Some(entry) = self.current(lease) else {
            return false;
        };
        let cancel = match &entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel, .. } => cancel.clone(),
            _ => return false,
        };
        if matches!(
            entry.phase,
            Phase::Starting { kind, .. } if kind != ResumeKind::Attach
        ) && !lease.cancel_before_birth()
        {
            return false;
        }
        if lease.stop_completion_pending() {
            // A cancelled constructor owes the same terminal events as an installed worker.
            // Keep its lease until the existing retry pass finishes them.
            if lease.spawned_runner().is_none() {
                lease.cancel_before_birth();
            }
            let identity = lease.retirement_runners().next();
            entry.sweep_owed = true;
            entry.phase = Phase::TeardownRetry {
                identity,
                attempts: 0,
            };
            if let Some(reason) = cancel {
                self.stale_cancels.insert(lease.session_id.clone(), reason);
            }
            return true;
        }
        if entry.journal_ack_required && lease.was_cancelled_before_birth() {
            entry.phase = Phase::RetirementComplete;
            return true;
        }
        self.entries.remove(&lease.session_id);
        if let Some(reason) = cancel {
            self.stale_cancels.insert(lease.session_id.clone(), reason);
        }
        true
    }

    /// Drop a running worker whose runner is left alive on disk (a
    /// rate-limit park, a burned budget).
    pub fn release_running(&mut self, lease: &Lease) -> bool {
        let Some(entry) = self.current(lease) else {
            return false;
        };
        if matches!(entry.phase, Phase::Running { .. }) {
            self.entries.remove(&lease.session_id);
            return true;
        }
        false
    }

    /// A committed stop may outlive a borrowed observation. Displace only another
    /// uninstalled observer, never a construction or an installed replacement.
    pub(crate) fn prepare_captured_stop(
        &mut self,
        session_id: &str,
        captured: Option<&Lease>,
    ) -> bool {
        let current = self.current_lease(session_id);
        if current.as_ref() == captured {
            return true;
        }
        if let Some(observer) = current {
            if !self.pending_attach(&observer) || !observer.is_borrowed_attach() {
                return false;
            }
            observer.cancellation().cancel();
            if !self.abandon(&observer) {
                return false;
            }
        }
        if let Some(captured) = captured {
            self.entries.insert(
                session_id.into(),
                Entry {
                    epoch: captured.epoch(),
                    phase: Phase::Running {
                        identity: captured.spawned_runner(),
                    },
                    delete_adapter_state: false,
                    journal_ack_required: false,
                    construction: Arc::clone(&captured.construction),
                    sweep_owed: true,
                },
            );
        }
        true
    }

    /// Ask for the session's worker to stop.
    pub fn begin_stop(
        &mut self,
        session_id: &str,
        reason: &str,
        delete_adapter_state: bool,
    ) -> StopDecision {
        let Some(lease) = self.current_lease(session_id) else {
            return StopDecision::NotOwned;
        };
        let entry = self
            .current(&lease)
            .expect("lease was read from this table");
        entry.delete_adapter_state |= delete_adapter_state;
        if matches!(entry.phase, Phase::RetirementComplete) {
            return StopDecision::NotOwned;
        }
        // Freeze retirement authority before waking an attaching observer.
        lease.claim_runner_ownership();
        lease.owe_stop_completion(Some(reason));
        lease.owe_previous_retirement();
        match &mut entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel, .. } => {
                cancel.get_or_insert_with(|| reason.to_string());
                lease.cancellation().cancel();
                StopDecision::CancelRequested
            }
            Phase::Running { identity } => {
                let identity = *identity;
                entry.phase = Phase::Stopping {
                    attempts: 0,
                    since: Instant::now(),
                };
                entry.sweep_owed = true;
                StopDecision::TearDown { lease, identity }
            }
            Phase::Stopping { .. } | Phase::TeardownRetry { .. } => {
                entry.sweep_owed = true;
                StopDecision::AlreadyStopping
            }
            Phase::RetirementComplete => unreachable!("handled above"),
        }
    }

    /// Cancel an admitted constructor before the caller waits for its request
    /// guards. Keep the phase and retirement ownership intact until the caller
    /// captures and commits the stop.
    pub(crate) fn request_stop(&mut self, session_id: &str, reason: &str) -> bool {
        let Some(lease) = self.current_lease(session_id) else {
            return false;
        };
        let Some(entry) = self.current(&lease) else {
            return false;
        };
        let cancel = match &mut entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel, .. } => cancel,
            _ => return false,
        };
        cancel.get_or_insert_with(|| reason.to_owned());
        lease.claim_runner_ownership();
        lease.owe_stop_completion(Some(reason));
        lease.owe_previous_retirement();
        lease.cancellation().cancel();
        true
    }

    pub(crate) fn require_retirement_ack(&mut self, lease: &Lease) -> bool {
        if let Some(entry) = self.entries.get_mut(lease.session_id()) {
            if entry.epoch != lease.epoch()
                || !Arc::ptr_eq(&entry.construction, &lease.construction)
            {
                return false;
            }
            entry.journal_ack_required = true;
            return true;
        }
        if lease.was_cancelled_before_birth() {
            let (identity, prime) = lease.constructed_ownership();
            if identity.is_some() || prime != PrimeOwnership::NotPrime {
                return false;
            }
            self.entries.insert(
                lease.session_id().into(),
                Entry {
                    epoch: lease.epoch(),
                    phase: Phase::RetirementComplete,
                    delete_adapter_state: true,
                    journal_ack_required: true,
                    construction: Arc::clone(&lease.construction),
                    sweep_owed: false,
                },
            );
        }
        true
    }

    /// The caller has durably acknowledged this exact completed construction.
    pub(crate) fn acknowledge_retirement(&mut self, lease: &Lease) -> bool {
        match self.entries.get(lease.session_id()) {
            None => return true,
            Some(entry)
                if entry.epoch == lease.epoch()
                    && Arc::ptr_eq(&entry.construction, &lease.construction)
                    && matches!(entry.phase, Phase::RetirementComplete) => {}
            Some(_) => return false,
        }
        self.entries.remove(lease.session_id());
        true
    }

    /// Take ownership of a disk-only runner so its teardown is tracked.
    pub fn adopt_for_stop(&mut self, session_id: &str) -> Option<Lease> {
        if self.entries.contains_key(session_id) {
            return None;
        }
        let epoch = self.mint(session_id, false);
        self.entries.insert(
            session_id.to_string(),
            Entry {
                epoch,
                delete_adapter_state: false,
                journal_ack_required: false,
                construction: Arc::new(Construction::default()),
                phase: Phase::Stopping {
                    attempts: 0,
                    since: Instant::now(),
                },
                sweep_owed: true,
            },
        );
        Some(self.lease(session_id, epoch))
    }

    /// Finish a teardown the lease holder drove.
    pub fn settle(&mut self, lease: &Lease, settlement: Settlement) {
        let Some(entry) = self.current(lease) else {
            return;
        };
        let attempts = match entry.phase {
            Phase::Stopping { attempts, .. } | Phase::TeardownRetry { attempts, .. } => attempts,
            _ => return,
        };
        match settlement {
            Settlement::Proven => {
                if lease.stop_completion_pending()
                    || (entry.delete_adapter_state
                        && !entry
                            .construction
                            .adapter_delete_handled
                            .load(std::sync::atomic::Ordering::SeqCst))
                {
                    entry.phase = Phase::TeardownRetry {
                        identity: lease.spawned_runner(),
                        attempts: attempts + 1,
                    };
                    return;
                }
                entry
                    .construction
                    .adapter_route
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take();
                lease.retirement().clear();
                if entry.journal_ack_required {
                    entry.phase = Phase::RetirementComplete;
                } else {
                    self.entries.remove(&lease.session_id);
                }
            }
            Settlement::Unproven(identity) => {
                entry.phase = Phase::TeardownRetry {
                    identity,
                    attempts: attempts + 1,
                };
            }
        }
    }

    /// Make the current stopping epoch sweep its scope before the settle.
    pub fn owe_sweep(&mut self, lease: &Lease) {
        if let Some(entry) = self.current(lease) {
            if matches!(entry.phase, Phase::Stopping { .. }) {
                entry.sweep_owed = true;
            }
        }
    }

    /// Retain scope debt until physical proof completes.
    pub fn sweep_owed(&mut self, lease: &Lease) -> Option<bool> {
        self.current(lease).map(|entry| entry.sweep_owed)
    }

    /// Move a running worker into its respawn epoch.
    pub fn begin_respawn(
        &mut self,
        lease: &Lease,
    ) -> Result<(Lease, Option<RunnerIdentity>), InstallError> {
        let session_id = lease.session_id.clone();
        let Some(entry) = self.current(lease) else {
            return Err(InstallError::Stale);
        };
        let Phase::Running { identity } = entry.phase else {
            return Err(InstallError::Stale);
        };
        let previous = identity;
        let admission = lease.admission();
        let scopes = lease.scopes();
        let prime = lease.prime();
        let epoch = self.mint(&session_id, true);
        let entry = self
            .entries
            .get_mut(&session_id)
            .expect("entry checked above");
        entry.epoch = epoch;
        entry.phase = Phase::Respawning {
            started_at: Utc::now(),
            cancel: None,
        };
        entry.journal_ack_required = false;
        entry.construction = Arc::new(Construction::default());
        *entry
            .construction
            .previous_runner
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = previous;
        *entry
            .construction
            .admission
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = admission;
        *entry
            .construction
            .scopes
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = scopes;
        *entry
            .construction
            .prime
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = prime;
        Ok((self.lease(&session_id, epoch), identity))
    }

    /// Turn a starting or respawning epoch that did build a runner into a
    /// teardown owned by the lease holder, who must then `settle`.
    pub fn convert_to_stopping(&mut self, lease: &Lease) -> bool {
        let Some(entry) = self.current(lease) else {
            return false;
        };
        if !matches!(
            entry.phase,
            Phase::Starting { .. } | Phase::Respawning { .. }
        ) {
            return false;
        }
        // Only a pending stop owes the sweep: a failed respawn alone keeps
        // its previous runner, which the session's scope glob also matches.
        entry.sweep_owed = matches!(
            &entry.phase,
            Phase::Starting {
                cancel: Some(_),
                ..
            } | Phase::Respawning {
                cancel: Some(_),
                ..
            }
        );
        entry.phase = Phase::Stopping {
            attempts: 0,
            since: Instant::now(),
        };
        true
    }

    /// The stop reason recorded against an in-flight resume or respawn.
    pub fn cancel_requested(&self, lease: &Lease) -> Option<String> {
        let entry = self.entries.get(&lease.session_id)?;
        if entry.epoch != lease.epoch || !Arc::ptr_eq(&entry.construction, &lease.construction) {
            return None;
        }
        match &entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel, .. } => cancel.clone(),
            _ => None,
        }
    }

    /// Permanent retirement applies only to the matching runner lease.
    pub fn delete_adapter_state_requested(&self, lease: &Lease) -> bool {
        self.entries.get(&lease.session_id).is_some_and(|entry| {
            entry.epoch == lease.epoch
                && Arc::ptr_eq(&entry.construction, &lease.construction)
                && entry.delete_adapter_state
        })
    }

    pub(crate) fn adapter_delete_pending(&self, lease: &Lease) -> bool {
        self.delete_adapter_state_requested(lease)
            && !lease
                .construction
                .adapter_delete_handled
                .load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(crate) fn adapter_delete_handled(&mut self, lease: &Lease) {
        if let Some(entry) = self.current(lease) {
            entry
                .construction
                .adapter_delete_handled
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Claim a parked teardown for another attempt, or one still marked
    /// `Stopping` after `orphaned_after`: its driver was dropped (a request
    /// future cancelled mid-teardown) and would otherwise never settle.
    pub fn claim_retry(
        &mut self,
        session_id: &str,
        orphaned_after: Duration,
    ) -> Option<RetryClaim> {
        let entry = self.entries.get_mut(session_id)?;
        let (identity, attempts) = match entry.phase {
            Phase::TeardownRetry { identity, attempts } => (identity, attempts),
            Phase::Stopping { attempts, since } if since.elapsed() >= orphaned_after => {
                (None, attempts)
            }
            _ => return None,
        };
        entry.phase = Phase::Stopping {
            attempts,
            since: Instant::now(),
        };
        let epoch = entry.epoch;
        Some(RetryClaim {
            lease: self.lease(session_id, epoch),
            identity,
            attempts: attempts + 1,
        })
    }

    /// Sessions the retry pass should look at: parked teardowns, and any
    /// teardown still claimed but older than `orphaned_after`.
    pub fn retry_ids_after(&self, orphaned_after: Duration) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| match e.phase {
                Phase::TeardownRetry { .. } => true,
                Phase::Stopping { since, .. } => since.elapsed() >= orphaned_after,
                _ => false,
            })
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Backdate a claimed teardown, for tests of the orphan reclaim.
    #[cfg(test)]
    pub fn age_stopping(&mut self, session_id: &str, by: Duration) {
        if let Some(entry) = self.entries.get_mut(session_id) {
            if let Phase::Stopping { since, .. } = &mut entry.phase {
                *since = since.checked_sub(by).unwrap_or(*since);
            }
        }
    }

    /// The running epoch and identity, for a reaper that must revalidate
    /// before removing.
    pub fn running(&self, session_id: &str) -> Option<(Lease, Option<RunnerIdentity>)> {
        let entry = self.entries.get(session_id)?;
        match entry.phase {
            Phase::Running { identity } => Some((self.lease(session_id, entry.epoch), identity)),
            _ => None,
        }
    }

    pub fn phase(&self, session_id: &str) -> WorkerPhase {
        match self.entries.get(session_id).map(|e| &e.phase) {
            None | Some(Phase::RetirementComplete) => WorkerPhase::Absent,
            Some(Phase::Starting { .. } | Phase::Respawning { .. }) => WorkerPhase::Resuming,
            Some(Phase::Running { .. }) => WorkerPhase::Running,
            Some(Phase::Stopping { .. } | Phase::TeardownRetry { .. }) => WorkerPhase::Stopping,
        }
    }

    /// The start of a worker that is coming up, `None` in every other phase.
    pub fn start(&self, session_id: &str) -> Option<WorkerStart> {
        match self.entries.get(session_id).map(|e| &e.phase) {
            Some(Phase::Starting {
                kind, started_at, ..
            }) => Some(WorkerStart {
                first_start: *kind == ResumeKind::SpawnFirst,
                started_at: *started_at,
            }),
            Some(Phase::Respawning { started_at, .. }) => Some(WorkerStart {
                first_start: false,
                started_at: *started_at,
            }),
            _ => None,
        }
    }

    pub fn snapshot(&self) -> HashMap<String, (WorkerPhase, Option<WorkerStart>)> {
        self.entries
            .keys()
            .map(|id| (id.clone(), (self.phase(id), self.start(id))))
            .collect()
    }

    /// Whether a worker is up or coming up.
    pub fn is_running(&self, session_id: &str) -> bool {
        matches!(
            self.phase(session_id),
            WorkerPhase::Resuming | WorkerPhase::Running
        )
    }

    pub fn is_owned(&self, session_id: &str) -> bool {
        self.entries
            .get(session_id)
            .is_some_and(|entry| !matches!(entry.phase, Phase::RetirementComplete))
    }

    pub(crate) fn construction_is_pending(&self, lease: &Lease) -> bool {
        self.entries.get(lease.session_id()).is_some_and(|entry| {
            entry.epoch == lease.epoch()
                && Arc::ptr_eq(&entry.construction, &lease.construction)
                && matches!(
                    entry.phase,
                    Phase::Starting { .. } | Phase::Respawning { .. }
                )
        })
    }

    pub(crate) fn pending_attach(&self, lease: &Lease) -> bool {
        self.entries.get(lease.session_id()).is_some_and(|entry| {
            entry.epoch == lease.epoch()
                && Arc::ptr_eq(&entry.construction, &lease.construction)
                && matches!(
                    entry.phase,
                    Phase::Starting {
                        kind: ResumeKind::Attach,
                        ..
                    }
                )
        })
    }

    pub(crate) fn begin_connection_retirement(&mut self, lease: &Lease) -> StopDecision {
        let Some(entry) = self.current(lease) else {
            return StopDecision::NotOwned;
        };
        let Phase::Running { identity } = entry.phase else {
            return StopDecision::AlreadyStopping;
        };
        lease.owe_stop_completion(None);
        entry.sweep_owed = true;
        entry.phase = Phase::Stopping {
            attempts: 0,
            since: Instant::now(),
        };
        StopDecision::TearDown {
            lease: lease.clone(),
            identity,
        }
    }

    /// Worker slots this daemon holds: everything but an attach in
    /// flight, whose runner the registry already counts.
    pub fn occupied_slots(&self) -> usize {
        self.entries
            .values()
            .filter(|e| {
                !matches!(
                    e.phase,
                    Phase::Starting {
                        kind: ResumeKind::Attach,
                        ..
                    } | Phase::RetirementComplete
                )
            })
            .count()
    }

    /// Whether a live registry record for this session should count toward
    /// capacity on top of `occupied_slots`.
    pub fn counts_registry_record(&self, session_id: &str) -> bool {
        match self.entries.get(session_id).map(|e| &e.phase) {
            None | Some(Phase::RetirementComplete) => true,
            Some(Phase::Starting {
                kind: ResumeKind::Attach,
                ..
            }) => true,
            Some(_) => false,
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// Scripted process control for lifecycle tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{ProcessControl, RunnerIdentity, WorkerPhase};
    use std::collections::HashSet;
    use std::sync::Mutex;

    type PhaseProbe = Box<dyn Fn(&str) -> WorkerPhase + Send + Sync>;
    type ForgetProbe = Box<dyn FnOnce(RunnerIdentity) + Send + Sync>;

    #[derive(Default)]
    pub(crate) struct FakeProcessControl {
        alive: Mutex<HashSet<u32>>,
        /// Pids that ignore SIGTERM and SIGKILL.
        immortal: Mutex<HashSet<u32>>,
        signals: Mutex<Vec<(u32, &'static str)>>,
        terminated: tokio::sync::Notify,
        /// Each scope release, with the session's phase at that moment.
        releases: Mutex<Vec<(String, Option<WorkerPhase>)>>,
        failed_scopes: Mutex<HashSet<String>>,
        scope_units: Mutex<Vec<String>>,
        phase_probe: Mutex<Option<PhaseProbe>>,
        forget_probe: Mutex<Option<ForgetProbe>>,
    }

    impl FakeProcessControl {
        pub(crate) fn alive(&self, pid: u32) -> &Self {
            self.alive.lock().unwrap().insert(pid);
            self
        }

        pub(crate) fn immortal(&self, pid: u32) -> &Self {
            self.alive(pid);
            self.immortal.lock().unwrap().insert(pid);
            self
        }

        /// Let an immortal process finally exit.
        pub(crate) fn exit(&self, pid: u32) {
            self.alive.lock().unwrap().remove(&pid);
        }

        pub(crate) fn signals(&self) -> Vec<(u32, &'static str)> {
            self.signals.lock().unwrap().clone()
        }

        /// Resolves once a group was sent SIGTERM since the last call.
        pub(crate) async fn terminated(&self) {
            self.terminated.notified().await;
        }

        /// Record the lifecycle phase each release observes.
        pub(crate) fn probe_phase(
            &self,
            probe: impl Fn(&str) -> WorkerPhase + Send + Sync + 'static,
        ) {
            *self.phase_probe.lock().unwrap() = Some(Box::new(probe));
        }

        pub(crate) fn probe_forget(
            &self,
            probe: impl FnOnce(RunnerIdentity) + Send + Sync + 'static,
        ) {
            *self.forget_probe.lock().unwrap() = Some(Box::new(probe));
        }

        pub(crate) fn releases(&self) -> Vec<(String, Option<WorkerPhase>)> {
            self.releases.lock().unwrap().clone()
        }

        pub(crate) fn fail_scope(&self, unit: &str) {
            self.failed_scopes.lock().unwrap().insert(unit.to_owned());
        }
        pub(crate) fn exit_scope(&self, unit: &str) {
            self.failed_scopes.lock().unwrap().remove(unit);
        }
        pub(crate) fn scope_units(&self) -> Vec<String> {
            self.scope_units.lock().unwrap().clone()
        }
    }

    impl ProcessControl for FakeProcessControl {
        fn is_alive(&self, identity: RunnerIdentity) -> bool {
            self.alive.lock().unwrap().contains(&identity.pid)
        }

        fn terminate_group(&self, identity: RunnerIdentity) {
            let pid = identity.pid;
            self.signals.lock().unwrap().push((pid, "TERM"));
            if !self.immortal.lock().unwrap().contains(&pid) {
                self.alive.lock().unwrap().remove(&pid);
            }
            self.terminated.notify_one();
        }

        fn kill_group(&self, identity: RunnerIdentity) {
            let pid = identity.pid;
            self.signals.lock().unwrap().push((pid, "KILL"));
            if !self.immortal.lock().unwrap().contains(&pid) {
                self.alive.lock().unwrap().remove(&pid);
            }
        }

        fn forget(&self, identity: RunnerIdentity) {
            let probe = self.forget_probe.lock().unwrap().take();
            if let Some(probe) = probe {
                probe(identity);
            }
        }

        fn release_scope(
            &self,
            session_id: &str,
            scopes: &[crate::process::worker_registry::ScopeOwnership],
        ) -> bool {
            let phase = self
                .phase_probe
                .lock()
                .unwrap()
                .as_ref()
                .map(|p| p(session_id));
            self.releases
                .lock()
                .unwrap()
                .push((session_id.to_string(), phase));
            scopes.iter().all(|scope| match scope {
                crate::process::worker_registry::ScopeOwnership::Unscoped => true,
                crate::process::worker_registry::ScopeOwnership::Unresolved => false,
                crate::process::worker_registry::ScopeOwnership::Owned { unit } => {
                    self.scope_units.lock().unwrap().push(unit.clone());
                    !self.failed_scopes.lock().unwrap().contains(unit)
                }
            })
        }

        fn scopes_stopped(
            &self,
            _session_id: &str,
            scopes: &[crate::process::worker_registry::ScopeOwnership],
        ) -> bool {
            scopes.iter().all(|scope| match scope {
                crate::process::worker_registry::ScopeOwnership::Unscoped => true,
                crate::process::worker_registry::ScopeOwnership::Unresolved => false,
                crate::process::worker_registry::ScopeOwnership::Owned { unit } => {
                    !self.failed_scopes.lock().unwrap().contains(unit)
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "s-1";

    #[test]
    fn late_permanent_stop_debt_blocks_proven_settlement_until_classified() {
        let mut table = LifecycleTable::new(1);
        let lease = table.admit(ID, ResumeKind::SpawnFirst).unwrap();
        table.install(&lease, None).unwrap();
        table.begin_stop(ID, "stop", false);
        table.begin_stop(ID, "purge", true);
        lease.stop_completion_done();
        table.settle(&lease, Settlement::Proven);
        assert!(
            table.is_owned(ID),
            "physical proof cannot erase unhandled adapter deletion"
        );
        let retry = table.claim_retry(ID, Duration::ZERO).unwrap();
        assert!(table.adapter_delete_pending(&retry.lease));
        table.adapter_delete_handled(&retry.lease);
        table.settle(&retry.lease, Settlement::Proven);
        assert!(!table.is_owned(ID));
    }

    fn identity(pid: u32, generation: u64) -> RunnerIdentity {
        RunnerIdentity::unattested(pid, generation)
    }

    #[cfg(unix)]
    #[test]
    fn system_control_keeps_a_leaderless_group_alive() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut leader = Command::new("sleep")
            .arg("120")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = leader.id();
        let control = SystemProcessControl::default();
        let mut member = None;
        let observed = (|| -> anyhow::Result<(RunnerIdentity, bool)> {
            member = Some(
                Command::new("sleep")
                    .arg("120")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(pid as i32)
                    .spawn()?,
            );
            let birth = crate::process::process_birth_identity(pid)?
                .ok_or_else(|| anyhow::anyhow!("group leader has no birth identity"))?;
            let identity = RunnerIdentity {
                pid,
                generation: 1,
                birth_fingerprint: Some(birth.fingerprint()),
            };
            control.with_guard(identity, |_| Ok(()))?;
            leader.kill()?;
            leader.wait()?;
            Ok((identity, control.is_alive(identity)))
        })();
        if let Some(mut member) = member {
            let _ = member.kill();
            member.wait().unwrap();
        }
        let _ = leader.kill();
        leader.wait().unwrap();
        let (identity, observed) = observed.unwrap();
        assert!(
            observed,
            "a surviving group member must prevent teardown settlement"
        );
        assert!(
            !control.is_alive(identity),
            "a reaped group must permit settlement"
        );
    }

    #[test]
    fn forgetting_cannot_release_active_or_unproven_runner_ownership() {
        let mut table = LifecycleTable::new(1);
        let initial = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert!(!table.forget(ID));
        assert_eq!(
            table.admit(ID, ResumeKind::Spawn).unwrap_err(),
            AdmitError::AlreadyPresent
        );
        table
            .install(&initial, Some(identity(11, initial.epoch())))
            .unwrap();
        assert!(!table.forget(ID));
        let (replacement, _) = table.begin_respawn(&initial).unwrap();
        assert!(!table.forget(ID));
        assert!(matches!(
            table.begin_stop(ID, "user_stopped", false),
            StopDecision::CancelRequested
        ));
        let runner = identity(12, replacement.epoch());
        assert!(matches!(
            table.install(&replacement, Some(runner)),
            Err(InstallError::Cancelled { .. })
        ));
        assert!(!table.forget(ID));
        table.settle(&replacement, Settlement::Unproven(Some(runner)));
        assert!(!table.forget(ID));
        assert_eq!(
            table.admit(ID, ResumeKind::Spawn).unwrap_err(),
            AdmitError::TeardownPending
        );
        let retry = table.claim_retry(ID, Duration::ZERO).unwrap();
        assert!(!table.forget(ID));
        retry.lease.stop_completion_done();
        table.settle(&retry.lease, Settlement::Proven);
        assert!(table.forget(ID));
        let next = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert!(next.epoch() > replacement.epoch());
        assert!(matches!(
            table.install(&replacement, Some(runner)),
            Err(InstallError::Stale)
        ));
    }

    #[test]
    fn identity_matches_record_with_legacy_generation() {
        let cases = [
            (identity(7, 5), 7, 5, true),
            (identity(7, 5), 7, 6, false),
            (identity(7, 5), 8, 5, false),
            (identity(7, 0), 7, 9, true),
            (identity(7, 5), 7, 0, true),
        ];
        for (id, pid, generation, expected) in cases {
            assert_eq!(id.matches_record(pid, generation), expected, "{id:?}");
        }
    }

    #[test]
    fn start_reports_kind_and_a_stable_time_only_while_coming_up() {
        for (kind, first_start) in [
            (ResumeKind::SpawnFirst, true),
            (ResumeKind::Spawn, false),
            (ResumeKind::Attach, false),
        ] {
            let mut table = LifecycleTable::new(100);
            let lease = table.admit(ID, kind).unwrap();
            let start = table.start(ID).expect("starting reports a start");
            assert_eq!(start.first_start, first_start, "{kind:?}");
            assert_eq!(table.start(ID), Some(start), "stable across reads");
            assert_eq!(
                table.snapshot().get(ID),
                Some(&(WorkerPhase::Resuming, Some(start)))
            );

            table.install(&lease, None).unwrap();
            assert_eq!(table.start(ID), None, "cleared on install");

            let (respawn, _) = table.begin_respawn(&lease).unwrap();
            let start = table.start(ID).expect("respawning reports a start");
            assert!(!start.first_start, "a respawn is a resume");
            table.abandon(&respawn);
            assert_eq!(table.start(ID), None, "cleared on abandon");
        }

        let mut table = LifecycleTable::new(100);
        let lease = table.admit(ID, ResumeKind::SpawnFirst).unwrap();
        table.install(&lease, None).unwrap();
        table.begin_stop(ID, "user", false);
        assert_eq!(table.start(ID), None, "cleared on stop");
    }

    #[test]
    fn admit_install_and_stop_walk_one_epoch() {
        let mut table = LifecycleTable::new(100);
        let lease = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert_eq!(lease.epoch(), 100);
        assert_eq!(table.phase(ID), WorkerPhase::Resuming);
        assert_eq!(
            table.admit(ID, ResumeKind::Spawn),
            Err(AdmitError::AlreadyPresent)
        );

        table.install(&lease, Some(identity(42, 100))).unwrap();
        assert_eq!(table.phase(ID), WorkerPhase::Running);
        assert!(table.is_running(ID));

        let StopDecision::TearDown {
            lease: stop,
            identity: id,
        } = table.begin_stop(ID, "user_stopped", false)
        else {
            panic!("running worker must hand teardown to the stopper");
        };
        assert_eq!(stop, lease);
        assert_eq!(id, Some(identity(42, 100)));
        assert_eq!(table.phase(ID), WorkerPhase::Stopping);
        assert!(!table.is_running(ID));
        assert!(table.is_owned(ID));
        assert!(matches!(
            table.begin_stop(ID, "again", false),
            StopDecision::AlreadyStopping
        ));
        assert_eq!(
            table.admit(ID, ResumeKind::Spawn),
            Err(AdmitError::TeardownPending)
        );

        stop.stop_completion_done();
        table.settle(&stop, Settlement::Proven);
        assert_eq!(table.phase(ID), WorkerPhase::Absent);
        assert_eq!(table.last_generation(ID), 100);
    }

    #[test]
    fn stop_during_start_cancels_and_install_hands_back_teardown() {
        let mut table = LifecycleTable::new(1);
        let lease = table.admit(ID, ResumeKind::Attach).unwrap();
        assert!(matches!(
            table.begin_stop(ID, "archived", false),
            StopDecision::CancelRequested
        ));
        assert_eq!(table.cancel_requested(&lease).as_deref(), Some("archived"));
        assert!(matches!(
            table.begin_stop(ID, "later", false),
            StopDecision::CancelRequested
        ));
        assert_eq!(
            table.cancel_requested(&lease).as_deref(),
            Some("archived"),
            "the first stop reason wins"
        );

        assert_eq!(
            table.install(&lease, Some(identity(9, 1))),
            Err(InstallError::Cancelled {
                reason: "archived".into()
            })
        );
        assert_eq!(table.phase(ID), WorkerPhase::Stopping);
        table.settle(&lease, Settlement::Unproven(Some(identity(9, 1))));
        assert_eq!(table.phase(ID), WorkerPhase::Stopping);

        let grace = Duration::from_secs(15);
        let claim = table.claim_retry(ID, grace).unwrap();
        assert_eq!(claim.attempts, 2);
        assert_eq!(claim.identity, Some(identity(9, 1)));
        assert!(
            table.claim_retry(ID, grace).is_none(),
            "a claimed retry is Stopping until it goes stale"
        );
        table.age_stopping(ID, grace);
        let orphan = table.claim_retry(ID, grace).unwrap();
        assert_eq!(
            orphan.identity, None,
            "a stale claim is reclaimed without an identity"
        );
        table.settle(&orphan.lease, Settlement::Unproven(Some(identity(9, 1))));
        let again = table.claim_retry(ID, grace).unwrap();
        assert_eq!(again.attempts, 3, "attempts accumulate per settled retry");
        again.lease.stop_completion_done();
        table.settle(&again.lease, Settlement::Proven);
        assert!(!table.is_owned(ID));
    }

    #[test]
    fn stale_leases_are_refused_everywhere() {
        let mut table = LifecycleTable::new(1);
        let old = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert!(table.abandon(&old));
        let new = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert_ne!(old.epoch(), new.epoch());

        assert_eq!(table.install(&old, None), Err(InstallError::Stale));
        assert!(!table.abandon(&old));
        assert!(!table.release_running(&old));
        assert!(table.begin_respawn(&old).is_err());
        assert!(!table.convert_to_stopping(&old));
        table.settle(&old, Settlement::Proven);
        assert_eq!(table.phase(ID), WorkerPhase::Resuming);

        table.install(&new, None).unwrap();
        assert_eq!(table.running(ID).map(|(l, _)| l), Some(new.clone()));
        assert!(table.release_running(&new));
        assert!(!table.is_owned(ID));
    }

    #[test]
    fn respawn_mints_a_new_epoch_and_honors_a_cancel() {
        let mut table = LifecycleTable::new(10);
        let first = table.admit(ID, ResumeKind::Spawn).unwrap();
        table.install(&first, Some(identity(1, 10))).unwrap();

        let (respawn, previous) = table.begin_respawn(&first).unwrap();
        assert_eq!(respawn.epoch(), 11);
        assert_eq!(previous, Some(identity(1, 10)));
        assert_eq!(table.phase(ID), WorkerPhase::Resuming);
        assert_eq!(table.install(&first, None), Err(InstallError::Stale));
        assert!(table.begin_respawn(&first).is_err());

        assert!(matches!(
            table.begin_stop(ID, "user_stopped", false),
            StopDecision::CancelRequested
        ));
        assert_eq!(
            table.install(&respawn, Some(identity(2, 11))),
            Err(InstallError::Cancelled {
                reason: "user_stopped".into()
            })
        );
        respawn.stop_completion_done();
        table.settle(&respawn, Settlement::Proven);
        assert_eq!(table.phase(ID), WorkerPhase::Absent);
        assert_eq!(table.last_generation(ID), 11);
    }

    #[test]
    fn a_failed_start_that_built_a_runner_converts_to_a_teardown() {
        let mut table = LifecycleTable::new(1);
        let lease = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert!(table.convert_to_stopping(&lease));
        assert_eq!(table.phase(ID), WorkerPhase::Stopping);
        assert!(
            !table.abandon(&lease),
            "a teardown in progress is not abandoned"
        );
        table.settle(&lease, Settlement::Proven);
        assert!(!table.is_owned(ID));
    }

    #[test]
    fn adopt_for_stop_tracks_a_disk_only_runner() {
        let mut table = LifecycleTable::new(1);
        let lease = table.adopt_for_stop(ID).unwrap();
        assert_eq!(table.phase(ID), WorkerPhase::Stopping);
        assert!(table.adopt_for_stop(ID).is_none());
        table.settle(&lease, Settlement::Unproven(Some(identity(3, 0))));
        assert_eq!(
            table.retry_ids_after(Duration::from_secs(15)),
            vec![ID.to_string()]
        );
        assert!(matches!(
            table.begin_stop(ID, "x", false),
            StopDecision::AlreadyStopping
        ));
    }

    #[test]
    fn capacity_counts_everything_but_an_attach_in_flight() {
        let mut table = LifecycleTable::new(1);
        let spawn = table.admit("a", ResumeKind::Spawn).unwrap();
        let attach = table.admit("b", ResumeKind::Attach).unwrap();
        let running = table.admit("c", ResumeKind::Spawn).unwrap();
        table.install(&running, None).unwrap();
        let stopping = table.adopt_for_stop("d").unwrap();
        assert_eq!(table.occupied_slots(), 3);
        assert!(table.counts_registry_record("b"));
        assert!(!table.counts_registry_record("a"));
        assert!(!table.counts_registry_record("c"));
        assert!(table.counts_registry_record("unknown"));

        table.install(&attach, Some(identity(5, 0))).unwrap();
        assert_eq!(table.occupied_slots(), 4);
        table.settle(&stopping, Settlement::Proven);
        assert!(table.abandon(&spawn));
        assert_eq!(table.occupied_slots(), 2);

        let snap = table.snapshot();
        assert_eq!(snap.get("b"), Some(&(WorkerPhase::Running, None)));
        assert_eq!(snap.get("c"), Some(&(WorkerPhase::Running, None)));
        assert_eq!(snap.len(), 2);
    }

    #[test]
    fn a_stop_asked_of_an_abandoned_resume_refuses_the_next_admit_once() {
        let mut table = LifecycleTable::new(1);
        let lease = table.admit(ID, ResumeKind::Attach).unwrap();
        assert!(matches!(
            table.begin_stop(ID, "user_stopped", false),
            StopDecision::CancelRequested
        ));
        assert!(table.abandon(&lease), "the attach failed before install");
        assert_eq!(
            table.admit(ID, ResumeKind::Spawn),
            Err(AdmitError::TeardownPending)
        );
        let claim = table
            .claim_retry(ID, Duration::ZERO)
            .expect("cancelled construction retains completion debt");
        claim.lease.stop_completion_done();
        table.settle(&claim.lease, Settlement::Proven);
        assert_eq!(
            table.admit(ID, ResumeKind::Spawn),
            Err(AdmitError::Cancelled("user_stopped".into())),
            "the fallback spawn is refused with the stop's reason"
        );
        assert!(
            table.admit(ID, ResumeKind::Spawn).is_ok(),
            "the stop is honored once; a later resume proceeds"
        );
    }

    #[test]
    fn an_attach_epoch_is_not_a_generation() {
        let mut table = LifecycleTable::new(100);
        let lease = table.admit(ID, ResumeKind::Attach).unwrap();
        assert_eq!(table.last_generation(ID), 0, "nothing stamped yet");
        table.install(&lease, Some(identity(1, 5))).unwrap();
        assert_eq!(table.last_generation(ID), 5, "the runner's own generation");
        assert!(table.release_running(&lease));
        let stop = table.adopt_for_stop(ID).unwrap();
        assert_eq!(
            table.last_generation(ID),
            5,
            "an adopted stop stamps nothing"
        );
        table.settle(&stop, Settlement::Proven);
        let spawn = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert_eq!(
            table.last_generation(ID),
            spawn.epoch(),
            "a spawn stamps its epoch"
        );
    }
}
