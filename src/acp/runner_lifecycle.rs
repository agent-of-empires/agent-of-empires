//! Per-session ownership of a structured-view runner.
//!
//! Every effect on a runner (spawn install, drain respawn, shutdown,
//! process-group signal, registry delete, reaper removal) must present a
//! [`Lease`] minted by this table for the session's current epoch. A lease
//! from an older epoch is refused, so a late spawn, a stale reaper snapshot,
//! or a drain respawn racing a shutdown cannot install or remove a runner
//! it does not own.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Exact identity of a runner process: its pid plus the generation stamped
/// into its registry record at spawn. Records written by older binaries
/// carry generation 0, which matches on pid alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunnerIdentity {
    pub pid: u32,
    pub generation: u64,
}

impl RunnerIdentity {
    /// Whether a registry record still describes this runner.
    pub fn matches_record(&self, pid: u32, generation: u64) -> bool {
        self.pid == pid
            && (self.generation == 0 || generation == 0 || self.generation == generation)
    }
}

/// Which code path is bringing a worker up. The UI treats both as
/// `Resuming`; capacity accounting only counts `Spawn`, because an attach
/// takes over a runner the registry already counts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeKind {
    Attach,
    Spawn,
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
        /// Teardown attempts already made under this epoch.
        attempts: u32,
        /// When this teardown was claimed, so one whose driver went away
        /// (a dropped request future) can be reclaimed by the retry pass.
        since: Instant,
    },
    TeardownRetry {
        identity: RunnerIdentity,
        attempts: u32,
    },
}

#[derive(Debug)]
struct Entry {
    epoch: u64,
    phase: Phase,
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
    /// The lease no longer names the session's current epoch.
    Stale,
    /// A stop arrived while the worker was coming up. The entry is now
    /// `Stopping` under this lease; the caller must tear the runner down
    /// exactly and then [`LifecycleTable::settle`].
    Cancelled { reason: String },
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

/// Process signalling and liveness, so teardown can be driven against a
/// fake in tests without spawning anything.
pub trait ProcessControl: Send + Sync + 'static {
    fn is_alive(&self, pid: u32) -> bool;
    fn terminate_group(&self, pid: u32);
    fn kill_group(&self, pid: u32);
}

pub struct SystemProcessControl;

impl ProcessControl for SystemProcessControl {
    /// pid 0 addresses the caller's own group and is never a runner.
    fn is_alive(&self, pid: u32) -> bool {
        pid != 0 && crate::process::worker::is_pid_alive_and_ours(pid)
    }

    fn terminate_group(&self, pid: u32) {
        if pid != 0 {
            crate::process::worker::terminate_process_group(pid);
        }
    }

    fn kill_group(&self, pid: u32) {
        if pid != 0 {
            crate::process::worker::kill_process_group(pid);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settlement {
    /// Process-group exit and registry cleanup were proven.
    Proven,
    /// The process survived escalation; keep ownership and retry.
    Unproven(RunnerIdentity),
}

/// Pending teardown a retry pass should drive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryClaim {
    pub lease: Lease,
    /// `None` for a reclaimed teardown whose driver never settled; the
    /// registry record then names the runner.
    pub identity: Option<RunnerIdentity>,
    pub attempts: u32,
}

pub struct LifecycleTable {
    entries: HashMap<String, Entry>,
    /// Also the generation stamped on the next spawned runner, so it must
    /// stay unique across daemon restarts; the supervisor seeds it from
    /// the wall clock.
    next_epoch: u64,
    /// Highest generation ever admitted or observed per session. Bounds
    /// restart-marker authority: a marker older than this is stale.
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

    /// Next epoch. Only a spawn or respawn stamps its epoch on a runner, so
    /// only those note it as a generation; an attach or an adopted stop
    /// records the generation it finds on the runner at install instead.
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

    /// Drop a stop kept for the next admission: the user is resuming the
    /// session on purpose, so the earlier stop no longer applies.
    pub fn forget_stale_cancel(&mut self, session_id: &str) {
        self.stale_cancels.remove(session_id);
    }

    /// Drop everything remembered for a session that no longer exists.
    pub fn forget(&mut self, session_id: &str) {
        self.entries.remove(session_id);
        self.last_generation.remove(session_id);
        self.stale_cancels.remove(session_id);
    }

    pub fn last_generation(&self, session_id: &str) -> u64 {
        self.last_generation.get(session_id).copied().unwrap_or(0)
    }

    /// Reserve the session for a spawn or attach. Only an absent session
    /// can be admitted; the new epoch is the generation a spawned runner
    /// must carry.
    pub fn admit(&mut self, session_id: &str, kind: ResumeKind) -> Result<Lease, AdmitError> {
        match self.entries.get(session_id).map(|e| &e.phase) {
            None => {}
            Some(Phase::Stopping { .. } | Phase::TeardownRetry { .. }) => {
                return Err(AdmitError::TeardownPending)
            }
            Some(_) => return Err(AdmitError::AlreadyPresent),
        }
        if let Some(reason) = self.stale_cancels.remove(session_id) {
            return Err(AdmitError::Cancelled(reason));
        }
        let epoch = self.mint(session_id, kind == ResumeKind::Spawn);
        self.entries.insert(
            session_id.to_string(),
            Entry {
                epoch,
                phase: Phase::Starting { kind, cancel: None },
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

    /// Give up a starting or respawning epoch that built nothing. Returns
    /// whether the lease was current; a stale lease is a no-op.
    pub fn abandon(&mut self, lease: &Lease) -> bool {
        let Some(entry) = self.current(lease) else {
            return false;
        };
        let cancel = match &entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel } => cancel.clone(),
            _ => return false,
        };
        self.entries.remove(&lease.session_id);
        if let Some(reason) = cancel {
            self.stale_cancels.insert(lease.session_id.clone(), reason);
        }
        true
    }

    /// Drop a running worker whose runner is left alive on disk (a
    /// rate-limit park, a burned budget). The registry, not this daemon,
    /// owns it from here.
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

    /// Ask for the session's worker to stop.
    pub fn begin_stop(&mut self, session_id: &str, reason: &str) -> StopDecision {
        let Some(entry) = self.entries.get_mut(session_id) else {
            return StopDecision::NotOwned;
        };
        let epoch = entry.epoch;
        match &mut entry.phase {
            Phase::Starting { cancel, .. } | Phase::Respawning { cancel } => {
                cancel.get_or_insert_with(|| reason.to_string());
                StopDecision::CancelRequested
            }
            Phase::Running { identity } => {
                let identity = *identity;
                entry.phase = Phase::Stopping {
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
        let epoch = self.mint(session_id, false);
        self.entries.insert(
            session_id.to_string(),
            Entry {
                epoch,
                phase: Phase::Stopping {
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
        let Phase::Stopping { attempts, .. } = entry.phase else {
            return;
        };
        match settlement {
            Settlement::Proven => {
                self.entries.remove(&lease.session_id);
            }
            Settlement::Unproven(identity) => {
                entry.phase = Phase::TeardownRetry {
                    identity,
                    attempts: attempts + 1,
                };
            }
        }
    }

    /// Move a running worker into its respawn epoch. The returned lease is
    /// the generation the replacement runner must carry; the identity is
    /// the runner being replaced, for a stop that lands before the launch.
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
        let epoch = self.mint(&session_id, true);
        let entry = self
            .entries
            .get_mut(&session_id)
            .expect("entry checked above");
        entry.epoch = epoch;
        entry.phase = Phase::Respawning { cancel: None };
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
        entry.phase = Phase::Stopping {
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
        let (identity, attempts) = match entry.phase {
            Phase::TeardownRetry { identity, attempts } => (Some(identity), attempts),
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
            None => WorkerPhase::Absent,
            Some(Phase::Starting { .. } | Phase::Respawning { .. }) => WorkerPhase::Resuming,
            Some(Phase::Running { .. }) => WorkerPhase::Running,
            Some(Phase::Stopping { .. } | Phase::TeardownRetry { .. }) => WorkerPhase::Stopping,
        }
    }

    pub fn snapshot(&self) -> HashMap<String, WorkerPhase> {
        self.entries
            .keys()
            .map(|id| (id.clone(), self.phase(id)))
            .collect()
    }

    /// Whether a worker is up or coming up. A stopping worker is owned
    /// but not running, so a prompt against it is refused.
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
        self.entries.clear();
    }
}

/// Scripted process control for lifecycle tests.
#[cfg(test)]
pub(crate) mod test_support {
    use super::ProcessControl;
    use std::collections::HashSet;
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct FakeProcessControl {
        alive: Mutex<HashSet<u32>>,
        /// Pids that ignore SIGTERM.
        stubborn: Mutex<HashSet<u32>>,
        /// Pids that survive SIGKILL too.
        immortal: Mutex<HashSet<u32>>,
        signals: Mutex<Vec<(u32, &'static str)>>,
    }

    impl FakeProcessControl {
        pub(crate) fn alive(&self, pid: u32) -> &Self {
            self.alive.lock().unwrap().insert(pid);
            self
        }

        pub(crate) fn stubborn(&self, pid: u32) -> &Self {
            self.alive(pid);
            self.stubborn.lock().unwrap().insert(pid);
            self
        }

        pub(crate) fn immortal(&self, pid: u32) -> &Self {
            self.stubborn(pid);
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
    }

    impl ProcessControl for FakeProcessControl {
        fn is_alive(&self, pid: u32) -> bool {
            self.alive.lock().unwrap().contains(&pid)
        }

        fn terminate_group(&self, pid: u32) {
            self.signals.lock().unwrap().push((pid, "TERM"));
            if !self.stubborn.lock().unwrap().contains(&pid) {
                self.alive.lock().unwrap().remove(&pid);
            }
        }

        fn kill_group(&self, pid: u32) {
            self.signals.lock().unwrap().push((pid, "KILL"));
            if !self.immortal.lock().unwrap().contains(&pid) {
                self.alive.lock().unwrap().remove(&pid);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "s-1";

    fn identity(pid: u32, generation: u64) -> RunnerIdentity {
        RunnerIdentity { pid, generation }
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
        table.settle(&lease, Settlement::Unproven(identity(9, 1)));
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
        table.settle(&orphan.lease, Settlement::Unproven(identity(9, 1)));
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
        table.settle(&lease, Settlement::Unproven(identity(3, 0)));
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
    fn a_stop_asked_of_an_abandoned_resume_refuses_the_next_admit_once() {
        let mut table = LifecycleTable::new(1);
        let lease = table.admit(ID, ResumeKind::Attach).unwrap();
        assert!(matches!(
            table.begin_stop(ID, "user_stopped"),
            StopDecision::CancelRequested
        ));
        assert!(table.abandon(&lease), "the attach failed before install");
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

    #[test]
    fn note_generation_only_moves_forward() {
        let mut table = LifecycleTable::new(50);
        table.note_generation(ID, 7);
        table.note_generation(ID, 3);
        assert_eq!(table.last_generation(ID), 7);
        let lease = table.admit(ID, ResumeKind::Spawn).unwrap();
        assert_eq!(table.last_generation(ID), 50);
        table.install(&lease, Some(identity(1, 2))).unwrap();
        assert_eq!(table.last_generation(ID), 50);
        table.clear();
        assert!(!table.is_owned(ID));
        assert_eq!(table.last_generation(ID), 50, "clear keeps marker bounds");
    }
}
