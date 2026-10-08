//! Per-session ownership of a structured-view runner.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Holds the lifecycle decision through the disk CAS; a failed persistence
/// restores the exact cancellation without admitting or consuming another scope.
pub(crate) struct PreparationAuthorization<'a> {
    table: std::sync::MutexGuard<'a, LifecycleTable>,
    removed: Option<(String, ResumeCancellation)>,
    committed: bool,
}

impl<'a> PreparationAuthorization<'a> {
    pub(crate) fn acquire(
        mut table: std::sync::MutexGuard<'a, LifecycleTable>,
        lease: &Lease,
        original: &Arc<crate::session::runner_journal::LaunchOrigin>,
        override_cancel: bool,
        commit: crate::session::runner_journal::PreparationCommit<'_>,
    ) -> anyhow::Result<Self> {
        let removed =
            table.commit_original_preparation(lease, original, override_cancel, commit)?;
        Ok(Self {
            table,
            removed,
            committed: false,
        })
    }
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for PreparationAuthorization<'_> {
    fn drop(&mut self) {
        if !self.committed {
            if let Some((id, cancel)) = self.removed.take() {
                self.table.stale_cancels.entry(id).or_insert(cancel);
            }
        }
    }
}

/// Captured execution ticket of a runner. Legacy PID/generation alone are not authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunnerIdentity {
    pub pid: u32,
    pub generation: u64,
    pub launch_nonce: Option<uuid::Uuid>,
    pub incarnation: Option<crate::process::ProcessIncarnation>,
    pub profile_identity: Option<crate::session::DirectoryIdentity>,
    pub boot: Option<[u8; 16]>,
}

impl RunnerIdentity {
    pub(crate) fn birth_is_complete(&self) -> bool {
        self.launch_nonce.is_some()
            && self.boot.is_some()
            && self
                .profile_identity
                .is_some_and(|identity| identity.is_durable())
            && self.incarnation.is_some()
    }

    /// Missing legacy evidence is uncertainty, not evidence of a different owner.
    pub(crate) fn proves_different_record(
        &self,
        record: &crate::process::worker_registry::WorkerRecord,
    ) -> bool {
        self.birth_is_complete()
            && (self.pid != record.pid
                || self.generation != record.generation
                || self
                    .launch_nonce
                    .zip(record.launch_nonce)
                    .is_some_and(|(before, after)| before != after)
                || self
                    .boot
                    .zip(record.boot)
                    .is_some_and(|(before, after)| before != after)
                || self
                    .incarnation
                    .zip(record.incarnation)
                    .is_some_and(|(before, after)| before != after)
                || self
                    .profile_identity
                    .zip(record.profile_identity)
                    .is_some_and(|(before, after)| before != after))
    }

    /// Whether a registry record still describes this runner.
    pub fn matches_record(&self, record: &crate::process::worker_registry::WorkerRecord) -> bool {
        self.birth_is_complete()
            && self.launch_nonce == record.launch_nonce
            && self.pid == record.pid
            && self.generation == record.generation
            && self.boot.is_some()
            && self.boot == record.boot
            && self.incarnation.is_some()
            && self.incarnation == record.incarnation
            && self.profile_identity.is_some()
            && self.profile_identity == record.profile_identity
    }
}

#[derive(Debug, Default)]
struct AdmissionState {
    identity: Option<RunnerIdentity>,
    cancelled: bool,
    shutdown_requested: bool,
    cancelled_stop: Option<Arc<crate::session::runner_journal::OwnedStop>>,
    jobs: usize,
    origin: Option<Arc<crate::session::runner_journal::LaunchOrigin>>,
    prepared: Option<Arc<crate::session::runner_journal::LaunchOrigin>>,
    retirement: Option<AdmissionRetirement>,
    preparation: Option<crate::session::runner_journal::PreparationCustody>,
    preparation_retirement: Option<tokio::sync::watch::Receiver<Option<bool>>>,
}

#[derive(Debug)]
struct Admission {
    state: Mutex<AdmissionState>,
    changed: tokio::sync::Notify,
}

#[derive(Debug, Clone)]
pub struct ExecutionAdmission {
    inner: Arc<Admission>,
}

pub(crate) struct AdmissionRetirement {
    pub lease: Lease,
    pub lifecycle: Arc<Mutex<LifecycleTable>>,
    pub notify: Arc<tokio::sync::Notify>,
    pub execution: Option<RunnerIdentity>,
}

impl std::fmt::Debug for AdmissionRetirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionRetirement")
            .field("lease", &self.lease)
            .field("execution", &self.execution)
            .finish_non_exhaustive()
    }
}

impl AdmissionRetirement {
    fn finish(self, issued: Option<RunnerIdentity>) {
        let execution = issued.or(self.execution);
        let changed = {
            let mut table = self
                .lifecycle
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            match execution {
                Some(identity) => {
                    if table.convert_to_stopping(&self.lease, Some(identity)) {
                        table.settle(&self.lease, Settlement::Unproven(Some(identity)));
                        true
                    } else {
                        false
                    }
                }
                None => table.abandon(&self.lease),
            }
        };
        if changed {
            self.notify.notify_waiters();
        }
    }
}

pub(crate) struct ExecutionJob(ExecutionAdmission);

impl Drop for ExecutionJob {
    fn drop(&mut self) {
        let (retirement, preparation, completed) = {
            let mut state = self
                .0
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.jobs -= 1;
            if state.jobs == 0 {
                (
                    state
                        .retirement
                        .take()
                        .map(|retirement| (retirement, state.identity)),
                    state.preparation.take(),
                    true,
                )
            } else {
                (None, None, false)
            }
        };
        drop(preparation);
        if let Some((retirement, identity)) = retirement {
            retirement.finish(identity);
        }
        if completed {
            self.0.inner.changed.notify_waiters();
        }
    }
}

impl ExecutionAdmission {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Admission {
                state: Mutex::new(AdmissionState::default()),
                changed: tokio::sync::Notify::new(),
            }),
        }
    }
    pub(crate) fn same_owner(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(crate) async fn cancelled(&self) {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .cancelled
            {
                return;
            }
            changed.await;
        }
    }

    fn is_drained(&self) -> bool {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.jobs == 0
            && state.preparation.is_none()
            && state
                .preparation_retirement
                .as_ref()
                .map_or(state.prepared.is_none(), |retirement| {
                    *retirement.borrow() == Some(true)
                })
    }

    pub async fn drain(&self) -> anyhow::Result<()> {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if !self.has_active_job() {
                break;
            }
            changed.await;
        }
        let retirement = {
            let state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            anyhow::ensure!(
                state.preparation.is_none(),
                "native preparation custody has not completed"
            );
            anyhow::ensure!(
                state.prepared.is_none() || state.preparation_retirement.is_some(),
                "issued preparation lost its actual retirement receiver"
            );
            state.preparation_retirement.clone()
        };
        if let Some(retirement) = retirement {
            crate::session::runner_journal::PreparationCustody::await_retired(retirement).await?;
        }
        Ok(())
    }

    pub(crate) fn register_preparation_retirement(
        &self,
        retirement: tokio::sync::watch::Receiver<Option<bool>>,
    ) -> anyhow::Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            state.preparation_retirement.is_none(),
            "admission already retains an original preparation retirement"
        );
        state.preparation_retirement = Some(retirement);
        Ok(())
    }

    pub(crate) fn set_origin(
        &self,
        origin: Arc<crate::session::runner_journal::LaunchOrigin>,
    ) -> anyhow::Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(original) = &state.origin {
            anyhow::ensure!(
                Arc::ptr_eq(original, &origin),
                "cannot replace an admission original scope"
            );
        } else {
            state.origin = Some(origin);
        }
        Ok(())
    }

    pub(crate) fn set_prepared_origin(
        &self,
        origin: Arc<crate::session::runner_journal::LaunchOrigin>,
        preparation: crate::session::runner_journal::PreparationCustody,
    ) -> anyhow::Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let baseline = state
            .origin
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("native preparation lost its registered baseline"))?;
        anyhow::ensure!(
            state
                .prepared
                .as_ref()
                .is_some_and(|issued| Arc::ptr_eq(issued, &origin))
                && state.preparation.is_none()
                && origin.is_prepared_from(baseline),
            "native preparation replaced its operation lineage"
        );
        anyhow::ensure!(
            state
                .preparation_retirement
                .as_ref()
                .is_some_and(|retirement| retirement.same_channel(preparation.retirement())),
            "native preparation replaced its original retirement receiver"
        );
        state.preparation = Some(preparation);
        Ok(())
    }

    pub(crate) fn record_produced_origin(
        &self,
        expected: &Arc<crate::session::runner_journal::LaunchOrigin>,
        produced: Arc<crate::session::runner_journal::LaunchOrigin>,
        identity: Option<RunnerIdentity>,
    ) -> anyhow::Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            state
                .prepared
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, expected))
                && produced.is_output_from(expected),
            "native output changed its original prepared admission"
        );
        state
            .preparation
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("native output lost its exact preparation custody"))?
            .produced(produced.clone())?;
        if let Some(identity) = identity {
            state.identity = Some(identity);
        }
        state.prepared = Some(produced);
        Ok(())
    }

    pub(crate) fn preparation_nonce(&self) -> Option<[u8; 16]> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .preparation
            .as_ref()
            .map(|ticket| ticket.nonce)
    }

    pub(crate) fn preparation_retirement(
        &self,
    ) -> Option<tokio::sync::watch::Receiver<Option<bool>>> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .preparation_retirement
            .clone()
    }

    pub(crate) fn origin(&self) -> Option<Arc<crate::session::runner_journal::LaunchOrigin>> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.prepared.as_ref().or(state.origin.as_ref()).cloned()
    }

    pub(crate) fn original_baseline(
        &self,
    ) -> Option<Arc<crate::session::runner_journal::LaunchOrigin>> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .origin
            .clone()
    }

    pub(crate) fn check_active(&self) -> anyhow::Result<()> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(!state.cancelled, "native operation was cancelled");
        Ok(())
    }

    pub(crate) fn capture(&self, identity: RunnerIdentity) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .identity = Some(identity);
    }

    pub(crate) fn snapshot(&self) -> Option<RunnerIdentity> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .identity
    }

    fn cancel_for_shutdown(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.shutdown_requested = true;
        state.cancelled = true;
        drop(state);
        self.inner.changed.notify_waiters();
    }

    pub(crate) fn is_shutdown_cancelled(&self) -> bool {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.shutdown_requested && state.cancelled_stop.is_none()
    }

    pub(crate) async fn shutdown_cancelled(&self) {
        loop {
            let changed = self.inner.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.is_shutdown_cancelled() {
                return;
            }
            changed.await;
        }
    }

    pub(crate) fn cancel(&self) {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancelled = true;
        self.inner.changed.notify_waiters();
    }

    fn cancel_from_stop(&self, stop: Arc<crate::session::runner_journal::OwnedStop>) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.cancelled = true;
        if state.cancelled_stop.is_none() {
            if let Some(preparation) = &state.preparation {
                if let Err(error) = preparation.stopped(stop.clone()) {
                    tracing::warn!(target: "acp.supervisor", session = %stop.session_id(), %error, "original preparation Stop acknowledgement remains unproven");
                }
            }
            state.cancelled_stop = Some(stop);
        }
        drop(state);
        self.inner.changed.notify_waiters();
    }

    fn take_cancelled_stop(&self) -> Option<Arc<crate::session::runner_journal::OwnedStop>> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancelled_stop
            .take()
    }

    pub(crate) fn commit_preparation<T>(
        &self,
        prepared: Arc<crate::session::runner_journal::LaunchOrigin>,
        execution: Option<RunnerIdentity>,
        claim: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            !state.cancelled && state.prepared.is_none(),
            "runner admission was cancelled or already prepared"
        );
        let result = claim()?;
        state.prepared = Some(prepared);
        if let Some(execution) = execution {
            state.identity = Some(execution);
        }
        Ok(result)
    }
    pub(crate) fn commit_effect<T>(
        &self,
        effect: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        anyhow::ensure!(
            !state.cancelled,
            "runner admission was cancelled before effect"
        );
        effect()
    }

    pub(crate) fn begin_job(&self) -> ExecutionJob {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .jobs += 1;
        ExecutionJob(self.clone())
    }

    pub(crate) fn has_active_job(&self) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .jobs
            != 0
    }

    pub(crate) fn retire(&self, retirement: AdmissionRetirement) {
        let (identity, preparation) = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state.jobs != 0 {
                state.cancelled = true;
                state.retirement = Some(retirement);
                return;
            }
            (state.identity, state.preparation.take())
        };
        drop(preparation);
        retirement.finish(identity);
    }

    pub(crate) fn authorize(
        &self,
        identity: RunnerIdentity,
        issue: impl FnOnce() -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.identity = Some(identity);
        if state.cancelled {
            return Err(std::io::Error::other("runner admission was cancelled"));
        }
        issue()
    }
}

/// Which code path is bringing a worker up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeKind {
    Attach,
    Spawn,
}

/// Native Attach carries the resident witnessed before its first await; the
/// memory table's classification alone is never a resource capability.
#[derive(Debug, Clone)]
pub(crate) enum NativeResume {
    Spawn,
    Attach(Arc<crate::process::worker_registry::WorkerRecord>),
}

impl NativeResume {
    pub(crate) fn kind(&self) -> ResumeKind {
        match self {
            Self::Spawn => ResumeKind::Spawn,
            Self::Attach(_) => ResumeKind::Attach,
        }
    }
}

/// Authority token for one epoch of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    session_id: String,
    epoch: u64,
}

impl Lease {
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
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
        cancel: Option<String>,
    },
    Running {
        identity: Option<RunnerIdentity>,
    },
    Respawning {
        cancel: Option<String>,
    },
    Stopping {
        identity: Option<RunnerIdentity>,
        attempts: u32,
        since: Instant,
    },
    TeardownRetry {
        identity: Option<RunnerIdentity>,
        attempts: u32,
    },
}

#[derive(Debug)]
struct Entry {
    epoch: u64,
    phase: Phase,
    admission: ExecutionAdmission,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdmitError {
    /// A worker is running or another task is mid-resume.
    #[error("worker already present or mid-resume")]
    AlreadyPresent,
    /// A previous runner has not been proven dead yet.
    #[error("previous runner teardown remains pending")]
    TeardownPending,
    /// A stop was asked of a resume that then failed before it installed;
    /// the stop stands against this one admission (the reconciler's
    /// fallback), carrying its reason.
    #[error("native resume cancelled: {0}")]
    Cancelled(String),
    #[error("native admissions are closed for daemon shutdown")]
    ShuttingDown,
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
struct ResumeCancellation {
    reason: String,
    scope: Option<Arc<crate::session::runner_journal::OwnedStop>>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// Process-group exit and registry cleanup were proven.
    Proven,
    /// The execution or its ownership remains unproven; keep protection and retry.
    Unproven(Option<RunnerIdentity>),
}

/// Pending teardown a retry pass should drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryClaim {
    pub lease: Lease,
    /// None means no captured authenticated execution ticket; do not reload a
    /// replacement identity or synthesize a PID to fill the gap.
    pub identity: Option<RunnerIdentity>,
    pub attempts: u32,
}

pub struct LifecycleTable {
    entries: HashMap<String, Entry>,
    closing: bool,
    released_admissions: Vec<ExecutionAdmission>,
    /// Also the generation stamped on the next spawned runner, so it must
    /// stay unique across daemon restarts; the supervisor seeds it from
    /// the wall clock.
    next_epoch: u64,
    /// Highest actual native birth generation observed per session.
    last_generation: HashMap<String, u64>,
    /// Stops asked of resumes that were abandoned before they installed,
    /// consumed only after canonical original-scope validation, so an unrelated
    /// admission cannot consume or override the original user's Stop.
    stale_cancels: HashMap<String, ResumeCancellation>,
}

impl LifecycleTable {
    pub fn new(seed_epoch: u64) -> Self {
        Self {
            entries: HashMap::new(),
            closing: false,
            released_admissions: Vec::new(),
            next_epoch: seed_epoch.max(1),
            last_generation: HashMap::new(),
            stale_cancels: HashMap::new(),
        }
    }

    fn remember_released(&mut self, admission: ExecutionAdmission) {
        self.released_admissions
            .retain(|issued| !issued.is_drained());
        if !admission.is_drained() {
            self.released_admissions.push(admission);
        }
    }

    fn remove_entry(&mut self, id: &str) {
        if let Some(entry) = self.entries.remove(id) {
            self.remember_released(entry.admission);
        }
    }

    /// Fence future admissions; this barrier is lifetime custody, not native-death proof.
    pub fn close_admissions(&mut self) -> Vec<ExecutionAdmission> {
        self.closing = true;
        for entry in self.entries.values() {
            if matches!(
                entry.phase,
                Phase::Starting { .. } | Phase::Respawning { .. }
            ) {
                entry.admission.cancel_for_shutdown();
            }
        }
        self.released_admissions
            .retain(|issued| !issued.is_drained());
        let mut issued = std::mem::take(&mut self.released_admissions);
        issued.reserve(self.entries.len());
        issued.extend(
            self.entries
                .values()
                .filter(|entry| !entry.admission.is_drained())
                .map(|entry| entry.admission.clone()),
        );
        issued
    }

    /// Next epoch.
    fn mint(&mut self) -> u64 {
        let epoch = self.next_epoch;
        self.next_epoch += 1;
        epoch
    }

    fn lease(&self, session_id: &str, epoch: u64) -> Lease {
        Lease {
            session_id: session_id.to_string(),
            epoch,
        }
    }

    fn current(&mut self, lease: &Lease) -> Option<&mut Entry> {
        self.entries
            .get_mut(&lease.session_id)
            .filter(|e| e.epoch == lease.epoch)
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

    /// Called only while the original baseline holds the canonical profile,
    /// workspace, identity and row fences, before its preparation CAS.
    fn commit_original_preparation(
        &mut self,
        lease: &Lease,
        original: &Arc<crate::session::runner_journal::LaunchOrigin>,
        override_cancel: bool,
        commit: crate::session::runner_journal::PreparationCommit<'_>,
    ) -> anyhow::Result<Option<(String, ResumeCancellation)>> {
        let current = self
            .entries
            .get(&lease.session_id)
            .filter(|entry| entry.epoch == lease.epoch);
        let scope = current.and_then(|entry| entry.admission.original_baseline());
        if !scope.is_some_and(|scope| Arc::ptr_eq(&scope, original))
            || !current.is_some_and(|entry| commit.belongs_to(&entry.admission))
        {
            return Err(AdmitError::TeardownPending.into());
        }
        let mut removed = None;
        commit.commit(|| {
            let Some(cancel) = self.stale_cancels.get(&lease.session_id) else {
                return Ok(());
            };
            let Some(scope) = &cancel.scope else {
                return Err(AdmitError::Cancelled(cancel.reason.clone()).into());
            };
            if !scope.cancellation_origin().same_scope(original) {
                return Ok(());
            }
            if !override_cancel {
                return Err(AdmitError::Cancelled(cancel.reason.clone()).into());
            }
            removed = self.stale_cancels.remove_entry(&lease.session_id);
            Ok(())
        })?;
        Ok(removed)
    }

    pub(crate) fn forget(
        &mut self,
        original: &crate::session::runner_journal::LaunchOrigin,
    ) -> bool {
        let session_id = original.session_id();
        if self.entries.get(session_id).is_some_and(|entry| {
            matches!(
                entry.phase,
                Phase::Starting { .. } | Phase::Respawning { .. }
            ) || entry.admission.has_active_job()
                || !entry
                    .admission
                    .origin()
                    .is_some_and(|scope| scope.same_scope(original))
        }) || self.stale_cancels.get(session_id).is_some_and(|cancel| {
            !cancel
                .scope
                .as_ref()
                .is_some_and(|scope| scope.cancellation_origin().same_scope(original))
        }) {
            return false;
        }
        self.remove_entry(session_id);
        self.last_generation.remove(session_id);
        // Only the validated preparation CAS consumes an explicit resume override.
        true
    }

    pub fn last_generation(&self, session_id: &str) -> u64 {
        self.last_generation.get(session_id).copied().unwrap_or(0)
    }

    pub(crate) fn execution_admission(&self, lease: &Lease) -> ExecutionAdmission {
        self.entries
            .get(&lease.session_id)
            .filter(|entry| entry.epoch == lease.epoch)
            .expect("execution admission requires a current lease")
            .admission
            .clone()
    }

    /// Reserve the session for a spawn or attach.
    pub fn admit(&mut self, session_id: &str, kind: ResumeKind) -> Result<Lease, AdmitError> {
        if self.closing {
            return Err(AdmitError::ShuttingDown);
        }
        match self.entries.get(session_id).map(|e| &e.phase) {
            None => {}
            Some(Phase::Stopping { .. } | Phase::TeardownRetry { .. }) => {
                return Err(AdmitError::TeardownPending)
            }
            Some(_) => return Err(AdmitError::AlreadyPresent),
        }
        let epoch = self.mint();
        self.entries.insert(
            session_id.to_string(),
            Entry {
                epoch,
                phase: Phase::Starting { kind, cancel: None },
                admission: ExecutionAdmission::new(),
            },
        );
        Ok(self.lease(session_id, epoch))
    }

    /// Promote a starting or respawning worker to running.
    pub fn install(
        &mut self,
        lease: &Lease,
        identity: Option<RunnerIdentity>,
    ) -> Result<(), InstallError> {
        let Some(entry) = self.current(lease) else {
            return Err(InstallError::Stale);
        };
        let cancel = match &mut entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel } => cancel.take(),
            _ => return Err(InstallError::Stale),
        };
        if let Some(reason) = cancel {
            entry.phase = Phase::Stopping {
                identity,
                attempts: 0,
                since: Instant::now(),
            };
            return Err(InstallError::Cancelled { reason });
        }
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
        if entry.admission.has_active_job() {
            return false;
        }
        let cancel = match &entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel } => cancel.clone(),
            _ => return false,
        };
        let scope = entry.admission.take_cancelled_stop();
        self.remove_entry(&lease.session_id);
        if let Some(reason) = cancel {
            self.stale_cancels.insert(
                lease.session_id.clone(),
                ResumeCancellation { reason, scope },
            );
        }
        true
    }

    /// Release a running lease after the caller has proven its captured execution retired.
    pub fn release_running(&mut self, lease: &Lease) -> bool {
        let Some(entry) = self.current(lease) else {
            return false;
        };
        if matches!(entry.phase, Phase::Running { .. }) {
            self.remove_entry(&lease.session_id);
            return true;
        }
        false
    }

    /// Cancel or stop only the admission whose immutable original authorized this Stop.
    pub(crate) fn begin_owned_stop(
        &mut self,
        stop: &Arc<crate::session::runner_journal::OwnedStop>,
        reason: &str,
    ) -> StopDecision {
        let session_id = stop.session_id();
        let Some(entry) = self.entries.get(session_id) else {
            return StopDecision::NotOwned;
        };
        if !entry
            .admission
            .origin()
            .is_some_and(|scope| scope.same_scope(stop.original()))
        {
            return StopDecision::NotOwned;
        }
        entry.admission.cancel_from_stop(stop.clone());
        self.begin_stop_inner(session_id, reason)
    }
    pub(crate) fn owned_stop_has_jobs(
        &self,
        stop: &crate::session::runner_journal::OwnedStop,
    ) -> bool {
        self.entries.get(stop.session_id()).is_some_and(|entry| {
            entry
                .admission
                .origin()
                .is_some_and(|scope| scope.same_scope(stop.original()))
                && entry.admission.has_active_job()
        })
    }

    pub(crate) fn claim_owned_stop_retry(
        &mut self,
        stop: &crate::session::runner_journal::OwnedStop,
    ) -> Option<RetryClaim> {
        let entry = self.entries.get(stop.session_id())?;
        if !entry
            .admission
            .origin()
            .is_some_and(|scope| scope.same_scope(stop.original()))
        {
            return None;
        }
        self.claim_retry(stop.session_id(), Duration::ZERO)
    }

    pub(crate) fn retry_stop(
        &self,
        lease: &Lease,
    ) -> Option<Arc<crate::session::runner_journal::OwnedStop>> {
        let entry = self
            .entries
            .get(lease.session_id())
            .filter(|entry| entry.epoch == lease.epoch)?;
        entry
            .admission
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .cancelled_stop
            .clone()
    }

    pub(crate) fn begin_lease_stop(&mut self, lease: &Lease, reason: &str) -> StopDecision {
        if self.current(lease).is_none() {
            return StopDecision::NotOwned;
        }
        self.begin_stop_inner(&lease.session_id, reason)
    }

    #[cfg(test)]
    fn begin_stop(&mut self, session_id: &str, reason: &str) -> StopDecision {
        self.begin_stop_inner(session_id, reason)
    }

    fn begin_stop_inner(&mut self, session_id: &str, reason: &str) -> StopDecision {
        let Some(entry) = self.entries.get_mut(session_id) else {
            return StopDecision::NotOwned;
        };
        let epoch = entry.epoch;
        match &mut entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel } => {
                entry.admission.cancel();
                cancel.get_or_insert_with(|| reason.to_string());
                StopDecision::CancelRequested
            }
            Phase::Running { identity } => {
                let identity = *identity;
                entry.phase = Phase::Stopping {
                    identity,
                    attempts: 0,
                    since: Instant::now(),
                };
                StopDecision::TearDown {
                    lease: self.lease(session_id, epoch),
                    identity,
                }
            }
            Phase::Stopping { .. } | Phase::TeardownRetry { .. } => StopDecision::AlreadyStopping,
        }
    }

    /// Take ownership of a disk-only runner so its teardown is tracked.
    pub fn adopt_for_stop(&mut self, session_id: &str) -> Option<Lease> {
        if self.entries.contains_key(session_id) {
            return None;
        }
        let epoch = self.mint();
        self.entries.insert(
            session_id.to_string(),
            Entry {
                epoch,
                admission: ExecutionAdmission::new(),
                phase: Phase::Stopping {
                    identity: None,
                    attempts: 0,
                    since: Instant::now(),
                },
            },
        );
        Some(self.lease(session_id, epoch))
    }

    /// Finish a teardown the lease holder drove.
    pub fn settle(&mut self, lease: &Lease, settlement: Settlement) {
        let Some(entry) = self.current(lease) else {
            return;
        };
        if entry.admission.has_active_job() {
            return;
        }
        let Phase::Stopping {
            identity: captured,
            attempts,
            ..
        } = entry.phase
        else {
            return;
        };
        match settlement {
            Settlement::Proven => {
                self.remove_entry(&lease.session_id);
            }
            Settlement::Unproven(identity) => {
                entry.phase = Phase::TeardownRetry {
                    identity: identity.or(captured),
                    attempts: attempts + 1,
                };
            }
        }
    }

    /// Move a running worker into its respawn epoch.
    pub fn begin_respawn(
        &mut self,
        lease: &Lease,
    ) -> Result<(Lease, Option<RunnerIdentity>), InstallError> {
        if self.closing {
            return Err(InstallError::Stale);
        }
        let session_id = lease.session_id.clone();
        let Some(entry) = self.current(lease) else {
            return Err(InstallError::Stale);
        };
        let Phase::Running { identity } = entry.phase else {
            return Err(InstallError::Stale);
        };
        let epoch = self.mint();
        let entry = self
            .entries
            .get_mut(&session_id)
            .expect("entry checked above");
        entry.epoch = epoch;
        let previous_admission = std::mem::replace(&mut entry.admission, ExecutionAdmission::new());
        entry.phase = Phase::Respawning { cancel: None };
        self.remember_released(previous_admission);
        Ok((self.lease(&session_id, epoch), identity))
    }

    /// Turn a starting or respawning epoch that did build a runner into a
    /// teardown owned by the lease holder, who must then `settle`.
    pub fn convert_to_stopping(&mut self, lease: &Lease, identity: Option<RunnerIdentity>) -> bool {
        let Some(entry) = self.current(lease) else {
            return false;
        };
        if !matches!(
            entry.phase,
            Phase::Starting { .. } | Phase::Respawning { .. }
        ) {
            return false;
        }
        entry.phase = Phase::Stopping {
            identity,
            attempts: 0,
            since: Instant::now(),
        };
        true
    }

    /// The stop reason recorded against an in-flight resume or respawn.
    pub fn cancel_requested(&self, lease: &Lease) -> Option<String> {
        let entry = self.entries.get(&lease.session_id)?;
        if entry.epoch != lease.epoch {
            return None;
        }
        match &entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel } => cancel.clone(),
            _ => None,
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
        if entry.admission.has_active_job() {
            return None;
        }
        let (identity, attempts) = match entry.phase {
            Phase::TeardownRetry { identity, attempts } => (identity, attempts),
            Phase::Stopping {
                identity,
                attempts,
                since,
            } if since.elapsed() >= orphaned_after => (identity, attempts),
            _ => return None,
        };
        entry.phase = Phase::Stopping {
            identity,
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

    pub(crate) fn running_origin(
        &self,
        session_id: &str,
    ) -> Option<Arc<crate::session::runner_journal::LaunchOrigin>> {
        let entry = self.entries.get(session_id)?;
        matches!(entry.phase, Phase::Running { .. })
            .then(|| entry.admission.origin())
            .flatten()
    }

    pub(crate) fn present_admission(&self, id: &str) -> Option<(Lease, ExecutionAdmission)> {
        let entry = self.entries.get(id)?;
        matches!(
            entry.phase,
            Phase::Starting { .. } | Phase::Running { .. } | Phase::Respawning { .. }
        )
        .then(|| (self.lease(id, entry.epoch), entry.admission.clone()))
    }

    pub fn phase(&self, session_id: &str) -> WorkerPhase {
        match self.entries.get(session_id).map(|e| &e.phase) {
            None => WorkerPhase::Absent,
            Some(Phase::Starting { .. } | Phase::Respawning { .. }) => WorkerPhase::Resuming,
            Some(Phase::Running { .. }) => WorkerPhase::Running,
            Some(Phase::Stopping { .. } | Phase::TeardownRetry { .. }) => WorkerPhase::Stopping,
        }
    }

    /// Whether the session holds a spawn that has not installed a worker yet.
    ///
    /// Narrower than [`WorkerPhase::Resuming`] on purpose. A respawn's previous
    /// worker may still be alive, and an `Attach` epoch is by construction a live
    /// runner re-dialled from disk, which is why `occupied_slots` and
    /// `counts_registry_record` both special-case that kind. Only a first spawn
    /// has no runner at all, and nothing drains the ACP connection before
    /// `install`, so only a first spawn is publishing nothing.
    pub fn spawn_without_runner(&self, session_id: &str) -> bool {
        matches!(
            self.entries.get(session_id).map(|e| &e.phase),
            Some(Phase::Starting {
                kind: ResumeKind::Spawn,
                ..
            })
        )
    }

    pub fn snapshot(&self) -> HashMap<String, WorkerPhase> {
        self.entries
            .keys()
            .map(|id| (id.clone(), self.phase(id)))
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
        self.entries.contains_key(session_id)
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
                    }
                )
            })
            .count()
    }

    /// Whether a live registry record for this session should count toward
    /// capacity on top of `occupied_slots`.
    pub fn counts_registry_record(&self, session_id: &str) -> bool {
        match self.entries.get(session_id).map(|e| &e.phase) {
            None => true,
            Some(Phase::Starting {
                kind: ResumeKind::Attach,
                ..
            }) => true,
            Some(_) => false,
        }
    }

    pub fn clear(&mut self) {
        for entry in std::mem::take(&mut self.entries).into_values() {
            self.remember_released(entry.admission);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "s-1";

    fn identity(pid: u32, generation: u64) -> RunnerIdentity {
        RunnerIdentity {
            pid,
            generation,
            launch_nonce: Some(uuid::Uuid::from_u128(generation as u128 + 1)),
            incarnation: None,
            profile_identity: None,
            boot: None,
        }
    }

    /// `spawn_without_runner` must not answer for a respawn, whose previous
    /// worker may still be alive, nor for an attach, which re-dials a runner
    /// that never stopped: a destructive caller reading either as "no runner"
    /// would remove the ACP event store and the managed worktree under it.
    #[test]
    fn spawn_without_runner_excludes_an_installed_worker_a_respawn_and_an_attach() {
        let mut table = LifecycleTable::new(1);
        let lease = table.admit(ID, ResumeKind::Spawn).expect("admitted");
        assert!(table.spawn_without_runner(ID), "a spawn has no runner yet");

        table
            .install(&lease, Some(identity(4242, 1)))
            .expect("installed");
        assert!(
            !table.spawn_without_runner(ID),
            "an installed worker is a runner"
        );

        table.begin_respawn(&lease).expect("respawn");
        assert!(
            !table.spawn_without_runner(ID),
            "a respawn is not a spawn: the previous worker may still be alive"
        );

        let mut attach = LifecycleTable::new(1);
        attach
            .admit("attached", ResumeKind::Attach)
            .expect("admitted");
        assert!(
            !attach.spawn_without_runner("attached"),
            "an attach epoch re-dials a live runner, so it is never runner-less"
        );
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
        } = table.begin_stop(ID, "user_stopped")
        else {
            panic!("running worker must hand teardown to the stopper");
        };
        assert_eq!(stop, lease);
        assert_eq!(id, Some(identity(42, 100)));
        assert_eq!(table.phase(ID), WorkerPhase::Stopping);
        assert!(!table.is_running(ID));
        assert!(table.is_owned(ID));
        assert!(matches!(
            table.begin_stop(ID, "again"),
            StopDecision::AlreadyStopping
        ));
        assert_eq!(
            table.admit(ID, ResumeKind::Spawn),
            Err(AdmitError::TeardownPending)
        );

        table.settle(&stop, Settlement::Proven);
        assert_eq!(table.phase(ID), WorkerPhase::Absent);
        assert_eq!(table.last_generation(ID), 100);
    }

    #[test]
    fn stop_during_start_cancels_and_install_hands_back_teardown() {
        let mut table = LifecycleTable::new(1);
        let lease = table.admit(ID, ResumeKind::Attach).unwrap();
        assert!(matches!(
            table.begin_stop(ID, "archived"),
            StopDecision::CancelRequested
        ));
        assert_eq!(table.cancel_requested(&lease).as_deref(), Some("archived"));
        assert!(matches!(
            table.begin_stop(ID, "later"),
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

        table.settle(&orphan.lease, Settlement::Unproven(Some(identity(9, 1))));
        let again = table.claim_retry(ID, grace).unwrap();
        assert_eq!(again.attempts, 3, "attempts accumulate per settled retry");
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
        assert!(!table.convert_to_stopping(&old, None));
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
        assert_eq!(previous, Some(identity(1, 10)));
        assert_eq!(table.phase(ID), WorkerPhase::Resuming);
        assert_eq!(table.install(&first, None), Err(InstallError::Stale));
        assert!(table.begin_respawn(&first).is_err());

        assert!(matches!(
            table.begin_stop(ID, "user_stopped"),
            StopDecision::CancelRequested
        ));
        assert_eq!(
            table.install(&respawn, Some(identity(2, 11))),
            Err(InstallError::Cancelled {
                reason: "user_stopped".into()
            })
        );
        table.settle(&respawn, Settlement::Proven);
        assert_eq!(table.phase(ID), WorkerPhase::Absent);
    }

    #[test]
    fn a_failed_start_that_built_a_runner_converts_to_a_teardown() {
        let mut table = LifecycleTable::new(1);
        let lease = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert!(table.convert_to_stopping(&lease, Some(identity(42, lease.epoch()))));
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
            table.begin_stop(ID, "x"),
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
        assert_eq!(snap.get("b"), Some(&WorkerPhase::Running));
        assert_eq!(snap.get("c"), Some(&WorkerPhase::Running));
        assert_eq!(snap.len(), 2);
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
            5,
            "unpublished admission is not a native birth"
        );
        table.install(&spawn, Some(identity(2, 6))).unwrap();
        assert_eq!(
            table.last_generation(ID),
            6,
            "only the actual published birth advances generation"
        );
    }
}
