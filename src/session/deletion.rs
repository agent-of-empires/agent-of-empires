//! Shared session deletion logic used by CLI, TUI, and web server.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::containers::DockerContainer;
use crate::git::cleanup::remove_managed_worktree;
use crate::git::GitWorktree;
use crate::session::config::repo_config;
use crate::session::storage::StorageFlock;
use crate::session::{Instance, LifecycleOperation, Storage};

#[derive(Clone)]
pub(crate) struct PurgeControl {
    shared: std::sync::Arc<PurgeControlState>,
}

struct PurgeControlState {
    original: std::sync::Arc<crate::session::LaunchOrigin>,
    phase: std::sync::Mutex<PurgeProgress>,
    changed: tokio::sync::Notify,
}

struct PurgeProgress {
    phase: PurgePhase,
    force_keep_paths: bool,
    receipt: Option<std::sync::Arc<crate::session::runner_journal::OwnedStop>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PurgePhase {
    Queued,
    Native,
    HooksOrCommit,
    Finished,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForceIntent {
    Accepted,
    AlreadyRequested,
    TooLate,
    Closed,
}

pub(crate) struct PurgeOwner {
    control: PurgeControl,
}

impl PurgeOwner {
    pub(crate) fn issue(instance: &Instance) -> Result<(Self, PurgeControl)> {
        let control = PurgeControl {
            shared: std::sync::Arc::new(PurgeControlState {
                original: crate::session::LaunchOrigin::capture(instance)?,
                phase: std::sync::Mutex::new(PurgeProgress {
                    phase: PurgePhase::Queued,
                    force_keep_paths: false,
                    receipt: None,
                }),
                changed: tokio::sync::Notify::new(),
            }),
        };
        Ok((
            Self {
                control: control.clone(),
            },
            control,
        ))
    }

    fn bind(&self, receipt: &std::sync::Arc<crate::session::runner_journal::OwnedStop>) {
        let mut progress = self
            .control
            .shared
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(progress.phase == PurgePhase::Queued && progress.receipt.is_none());
        progress.receipt = Some(receipt.clone());
        progress.phase = PurgePhase::Native;
    }

    fn seal(&self) -> bool {
        let mut progress = self
            .control
            .shared
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        progress.phase = PurgePhase::HooksOrCommit;
        progress.force_keep_paths
    }
}

impl Drop for PurgeOwner {
    fn drop(&mut self) {
        self.control
            .shared
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .phase = PurgePhase::Finished;
        self.control.shared.changed.notify_one();
    }
}

impl PurgeControl {
    pub(crate) fn request_force(&self) -> ForceIntent {
        let mut progress = self
            .shared
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match progress.phase {
            PurgePhase::HooksOrCommit => ForceIntent::TooLate,
            PurgePhase::Finished => ForceIntent::Closed,
            PurgePhase::Queued | PurgePhase::Native if progress.force_keep_paths => {
                ForceIntent::AlreadyRequested
            }
            PurgePhase::Queued | PurgePhase::Native => {
                progress.force_keep_paths = true;
                self.shared.changed.notify_one();
                ForceIntent::Accepted
            }
        }
    }

    pub(crate) fn matches(&self, instance: &Instance) -> bool {
        let progress = self
            .shared
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let generation = instance.lifecycle_generation;
        if generation == self.shared.original.generation()
            && self
                .shared
                .original
                .validate_baseline_at(instance, generation)
                .is_ok()
        {
            return true;
        }
        progress.receipt.as_ref().is_some_and(|receipt| {
            generation == receipt.generation()
                && (self
                    .shared
                    .original
                    .validate_baseline_at(instance, generation)
                    .is_ok()
                    || receipt
                        .current_projection()
                        .validate_baseline_at(instance, generation)
                        .is_ok())
                && self
                    .shared
                    .original
                    .validate_native_history(instance)
                    .is_ok()
        })
    }

    pub(crate) fn force_requested(&self) -> bool {
        self.shared
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .force_keep_paths
    }

    pub(crate) async fn changed(&self) {
        self.shared.changed.notified().await;
    }
    pub(crate) fn owns_stop(
        &self,
        stop: &std::sync::Arc<crate::session::runner_journal::OwnedStop>,
    ) -> bool {
        let progress = self
            .shared
            .phase
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        progress.phase == PurgePhase::Native
            && progress
                .receipt
                .as_ref()
                .is_some_and(|receipt| std::sync::Arc::ptr_eq(receipt, stop))
    }
}
pub struct DeletionRequest {
    pub session_id: String,
    pub instance: Instance,
    pub delete_worktree: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    pub force_delete: bool,
    /// When `true`, on_destroy hooks run detached from the controlling terminal (TUI/web).
    pub detach_hooks: bool,
    /// When `true` AND `instance.scratch` is `true`, the scratch directory is left on disk instead
    /// of being removed.
    pub keep_scratch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionDisposition {
    Removed,
    KeptRestored,
    AlreadyGone,
    Busy,
    Failed,
}

#[derive(Debug)]
pub struct DeletionResult {
    pub session_id: String,
    pub success: bool,
    pub messages: Vec<String>,
    pub errors: Vec<String>,
    pub disposition: DeletionDisposition,
    pub teardown_started: bool,
    /// Latest durable row when the transaction deliberately kept it.
    pub retained_instance: Option<Instance>,
    pub(crate) retained_stop: Option<std::sync::Arc<crate::session::runner_journal::OwnedStop>>,
}

impl DeletionResult {
    pub(crate) fn rejected(
        session_id: String,
        disposition: DeletionDisposition,
        message: impl Into<String>,
        retained_instance: Option<Instance>,
    ) -> Self {
        Self {
            session_id,
            success: false,
            messages: Vec::new(),
            errors: vec![message.into()],
            disposition,
            retained_instance,
            teardown_started: false,
            retained_stop: None,
        }
    }

    pub(crate) fn retained_release_matches(&self, current: &Instance) -> bool {
        let (Some(stop), Some(retained)) = (&self.retained_stop, &self.retained_instance) else {
            return false;
        };
        let original = stop.original();
        let acknowledged = stop.current_projection();
        current.same_storage_origin(retained)
            && current.trashed_at == retained.trashed_at
            && (current.lifecycle_generation == original.generation()
                || current.lifecycle_generation == retained.lifecycle_generation)
            && (original
                .validate_baseline_at(current, current.lifecycle_generation)
                .is_ok()
                || acknowledged
                    .validate_baseline_at(current, current.lifecycle_generation)
                    .is_ok())
            && acknowledged
                .validate_baseline_at(retained, retained.lifecycle_generation)
                .is_ok()
            && original.validate_native_history(retained).is_ok()
    }
}

pub enum PurgeReservation {
    Reserved(PurgeTransaction),
    Rejected(DeletionResult),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PurgeCleanup {
    All,
    SidecarsOnly,
}

/// Owned purge transition.
pub struct PurgeTransaction {
    storage: Storage,
    original: std::sync::Arc<crate::session::LaunchOrigin>,
    native_stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
    control_owner: Option<PurgeOwner>,
    request: DeletionRequest,
    cleanup: PurgeCleanup,
    was_trashed: bool,
    generation: u64,
    lifecycle_lock: Option<StorageFlock>,
    workspace_claim_lock: Option<StorageFlock>,
    identity_lock: Option<StorageFlock>,
    active: bool,
    /// The ownership verdict, taken once before any hook runs. Re-scanning after
    /// the hooks would let a non-idempotent `on_destroy` play on a purge that is
    /// then refused, and would replay on the next retry.
    ownership_verdict: Option<Option<String>>,
}

/// A purge whose durable row has already been removed. The same lifecycle
/// flock remains held while irreversible sidecars are removed.
#[must_use = "committed purge sidecars must be finished"]
pub struct CommittedPurge {
    request: DeletionRequest,
    cleanup: PurgeCleanup,
    _control_owner: Option<PurgeOwner>,
    _lifecycle_lock: StorageFlock,
    _identity_lock: Option<StorageFlock>,
    _workspace_claim_lock: StorageFlock,
}

#[derive(Clone, Copy)]
enum CompletionGate {
    Proceed,
    AlreadyGone,
    KeptRestored,
    Superseded,
}

impl DeletionRequest {
    /// Whether this deletion can destroy a path another session owns. A session
    /// with no managed worktree, no workspace and no scratch has nothing to
    /// protect, so an inventory it cannot read must not refuse its deletion.
    fn needs_path_inventory(&self) -> bool {
        (self.delete_worktree
            && (self
                .instance
                .worktree_info
                .as_ref()
                .is_some_and(|wt| wt.managed_by_aoe)
                || self.instance.workspace_info.is_some()))
            || self.instance.scratch
    }
}

impl PurgeTransaction {
    pub fn reserve_unwatched(request: DeletionRequest) -> Result<PurgeReservation> {
        let storage = request.instance.original_storage()?;
        Self::reserve(storage.as_ref().clone(), request)
    }

    pub fn reserve(storage: Storage, request: DeletionRequest) -> Result<PurgeReservation> {
        Self::reserve_with_cleanup(storage, request, PurgeCleanup::All, None)
    }

    fn reserve_with_cleanup(
        storage: Storage,
        mut request: DeletionRequest,
        cleanup: PurgeCleanup,
        control_owner: Option<PurgeOwner>,
    ) -> Result<PurgeReservation> {
        let id = request.session_id.clone();
        let was_trashed = request.instance.is_trashed();
        let workspace_claim_lock = crate::session::acquire_session_workspace_claim_lock()?;
        let identity_lock = Some(crate::session::acquire_session_identity_lock()?);
        let origin = request.instance.original_storage()?;
        anyhow::ensure!(
            storage.same_origin_as(&origin),
            "purge request belongs to another physical profile"
        );
        origin.verify_profile_identity()?;
        let original = match &control_owner {
            Some(owner) => owner.control.shared.original.clone(),
            None => crate::session::LaunchOrigin::capture(&request.instance)?,
        };
        original.validate_baseline_at(&request.instance, original.generation())?;
        storage.verify_profile_identity()?;
        let expected_trashed_at = request.instance.trashed_at;
        let mut lifecycle_changed = false;
        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&id)
            .context("failed to acquire instance purge lock")?;
        let now = Utc::now();
        let mut reserved = None;
        let mut rejected = None;
        storage.update_under_workspace_claim_lock(|instances, _groups| {
            if let Some(stored) = instances.iter().find(|instance| instance.id == id) {
                if cleanup == PurgeCleanup::SidecarsOnly
                    && (expected_trashed_at.is_none() || stored.trashed_at != expected_trashed_at)
                {
                    rejected = Some((
                        DeletionDisposition::KeptRestored,
                        "The original trash lifecycle changed, so the session was kept".to_owned(),
                        Some(stored.clone()),
                    ));
                    return Ok(());
                }
                if original
                    .validate_baseline_at(stored, original.generation())
                    .is_err()
                    && original
                        .validate_restored_at(stored, original.generation())
                        .is_err()
                {
                    rejected = Some((
                        DeletionDisposition::Busy,
                        "Original purge scope was superseded; retry from the current instance"
                            .to_owned(),
                        None,
                    ));
                    return Ok(());
                }
            }
            let decision =
                crate::session::claim::decide_purge_claim(instances, &id, was_trashed, now)?;
            let generation = match decision {
                crate::session::claim::PurgeClaimDecision::Claimed(generation) => generation,
                crate::session::claim::PurgeClaimDecision::Restored => {
                    let retained = instances.iter().find(|instance| instance.id == id).cloned();
                    rejected = Some((
                        DeletionDisposition::KeptRestored,
                        "Session is being restored, so it was not purged".to_string(),
                        retained,
                    ));
                    return Ok(());
                }
                crate::session::claim::PurgeClaimDecision::Busy(holder) => {
                    let retained = instances.iter().find(|instance| instance.id == id).cloned();
                    rejected = Some((
                        DeletionDisposition::Busy,
                        format!("Session {}", holder.already_in_progress_reason()),
                        retained,
                    ));
                    return Ok(());
                }
                crate::session::claim::PurgeClaimDecision::AlreadyGone => {
                    rejected = Some((
                        DeletionDisposition::AlreadyGone,
                        "Session was already removed by another process".to_string(),
                        None,
                    ));
                    return Ok(());
                }
            };
            let stored = instances
                .iter_mut()
                .find(|instance| instance.id == id)
                .expect("reserved purge row must still exist");
            lifecycle_changed = was_trashed && stored.trashed_at != expected_trashed_at;
            let mut snapshot = stored.clone();
            snapshot.source_profile = storage.profile().to_string();
            reserved = Some((generation, snapshot));
            Ok(())
        })?;

        if let Some((disposition, message, retained_instance)) = rejected {
            return Ok(PurgeReservation::Rejected(DeletionResult::rejected(
                id,
                disposition,
                message,
                retained_instance,
            )));
        }
        let (generation, snapshot) =
            reserved.ok_or_else(|| anyhow::anyhow!("purge reservation produced no outcome"))?;
        request.instance = snapshot;
        // A force chosen against an earlier trash lifecycle (a peer restored and re-trashed
        // the row since) must not skip the dirty-worktree guard for the new one.
        if lifecycle_changed {
            request.force_delete = false;
        }
        let native_stop =
            crate::session::runner_journal::OwnedStop::from_purge(original.clone(), generation);
        if let Some(owner) = &control_owner {
            owner.bind(&native_stop);
        }
        Ok(PurgeReservation::Reserved(Self {
            storage,
            original,
            native_stop,
            control_owner,
            request,
            cleanup,
            was_trashed,
            workspace_claim_lock: Some(workspace_claim_lock),
            generation,
            lifecycle_lock: Some(lifecycle_lock),
            identity_lock,
            active: true,
            ownership_verdict: None,
        }))
    }
    /// The authoritative instance snapshot captured by the reservation.
    pub fn instance(&self) -> &Instance {
        &self.request.instance
    }

    pub(crate) fn native_stop_scope(
        &self,
    ) -> std::sync::Arc<crate::session::runner_journal::OwnedStop> {
        self.native_stop.clone()
    }

    /// Validate cross-profile ownership before hooks or irreversible row removal.
    pub fn preflight_ownership(mut self) -> std::result::Result<Self, Box<DeletionResult>> {
        let Some(message) = self.ownership_error() else {
            return Ok(self);
        };
        Err(Box::new(self.failed_after_release(message)))
    }

    fn seal_cleanup(&mut self) {
        if self.control_owner.as_ref().is_some_and(PurgeOwner::seal) {
            self.request.delete_worktree = false;
            self.request.delete_branch = false;
            self.request.keep_scratch = true;
            self.cleanup = PurgeCleanup::SidecarsOnly;
        }
    }

    fn ownership_error(&mut self) -> Option<String> {
        if self.cleanup == PurgeCleanup::SidecarsOnly || !self.request.needs_path_inventory() {
            return None;
        }
        if let Some(verdict) = &self.ownership_verdict {
            return verdict.clone();
        }
        let verdict = with_paths_in_use_locked(
            SessionPathOwner {
                profile: self.storage.profile(),
                session_id: &self.request.session_id,
            },
            self.identity_lock.is_some(),
            |paths| match paths {
                PathsInUse::Unknown(reason) => Some(format!(
                    "could not prove that session resources are unused: {reason}"
                )),
                PathsInUse::Known(_) => None,
            },
        );
        self.ownership_verdict = Some(verdict.clone());
        verdict
    }

    /// Run best-effort hooks without a lifecycle or storage flock held.
    pub fn run_hooks(self) -> Self {
        self.run_hooks_with(run_on_destroy_hooks)
    }

    /// Drop the global and per-instance flocks ahead of an await that must not
    /// block every other writer, keeping the durable reservation so a peer
    /// still sees this purge as claimed. The next phase restores the canonical
    /// lock order before the destructive effects.
    pub fn release_locks_for_teardown(mut self) -> Self {
        self.lifecycle_lock = None;
        self.workspace_claim_lock = None;
        self.identity_lock = None;
        self
    }

    fn run_hooks_with<F>(mut self, run_hooks: F) -> Self
    where
        F: FnOnce(&Instance, bool),
    {
        self.seal_cleanup();
        let ownership_failed = self.ownership_error().is_some();
        self.lifecycle_lock = None;
        self.workspace_claim_lock = None;
        self.identity_lock = None;
        if !ownership_failed && self.cleanup == PurgeCleanup::All {
            run_hooks(&self.request.instance, self.request.detach_hooks);
        }
        self
    }

    fn ensure_lifecycle_lock(&mut self) -> Result<()> {
        if self.workspace_claim_lock.is_none() {
            self.workspace_claim_lock = Some(
                crate::session::acquire_session_workspace_claim_lock()
                    .context("failed to reacquire workspace claim lock after hooks")?,
            );
        }
        if self.identity_lock.is_none() {
            self.identity_lock = Some(
                crate::session::acquire_session_identity_lock()
                    .context("failed to reacquire session identity lock after hooks")?,
            );
        }
        self.storage = self
            .storage
            .reopen_preserving_watch()
            .context("failed to reopen target profile after destroy hooks")?;
        if self.lifecycle_lock.is_none() {
            self.lifecycle_lock = Some(
                self.storage
                    .acquire_instance_lifecycle_lock(&self.request.session_id)
                    .context("failed to reacquire instance purge lock after hooks")?,
            );
        }
        // Hooks release ownership fences; the cached verdict cannot authorize
        // row removal after another profile inventory has changed.
        self.ownership_verdict = None;
        Ok(())
    }

    fn failed_after_release(&mut self, message: impl Into<String>) -> DeletionResult {
        let mut result = DeletionResult::rejected(
            self.request.session_id.clone(),
            DeletionDisposition::Failed,
            message,
            None,
        );
        self.acknowledge_retention(&mut result);
        result
    }

    fn acknowledge_retention(&mut self, result: &mut DeletionResult) {
        result.retained_instance = match self.release_reservation() {
            Ok(retained) => retained,
            Err(error) => {
                result
                    .errors
                    .push(format!("Failed to release purge reservation: {error}"));
                None
            }
        };
        result.retained_stop = result
            .retained_instance
            .as_ref()
            .map(|_| self.native_stop.clone());
    }

    fn release_reservation(&mut self) -> Result<Option<Instance>> {
        self.ensure_lifecycle_lock()?;
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let mut retained = None;
        self.storage
            .update_under_workspace_claim_lock(|instances, _groups| {
                if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                    self.native_stop
                        .current_projection()
                        .validate_baseline_at(stored, generation)?;
                    self.original.validate_native_history(stored)?;
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                    retained = Some(stored.clone());
                }
                Ok(())
            })?;
        self.active = false;
        Ok(retained)
    }

    fn gate(&mut self) -> Result<(CompletionGate, Option<Instance>)> {
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let mut outcome = None;
        self.storage
            .update_under_workspace_claim_lock(|instances, _groups| {
                let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) else {
                    outcome = Some((CompletionGate::AlreadyGone, None));
                    return Ok(());
                };
                let acknowledged = self.native_stop.current_projection();
                if self.original.validate_native_history(stored).is_err()
                    || (acknowledged
                        .validate_baseline_at(stored, generation)
                        .is_err()
                        && acknowledged
                            .validate_restored_at(stored, generation)
                            .is_err())
                {
                    outcome = Some((CompletionGate::Superseded, None));
                    return Ok(());
                }
                let restored = crate::session::claim::purge_restored_row_must_be_kept(
                    was_trashed,
                    stored.is_trashed(),
                ) || (self.cleanup == PurgeCleanup::SidecarsOnly
                    && stored.trashed_at != self.request.instance.trashed_at);
                let owns =
                    stored.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation);
                let gate = if restored {
                    CompletionGate::KeptRestored
                } else if !owns {
                    CompletionGate::Superseded
                } else {
                    CompletionGate::Proceed
                };
                if !matches!(gate, CompletionGate::Proceed) {
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                }
                outcome = Some((gate, Some(stored.clone())));
                Ok(())
            })?;
        let outcome = outcome.ok_or_else(|| anyhow::anyhow!("purge gate produced no outcome"))?;
        if matches!(outcome.0, CompletionGate::Proceed) {
            if let Some(current) = outcome.1.clone() {
                self.request.instance = current;
            }
        } else {
            self.active = false;
        }
        Ok(outcome)
    }

    fn result_for_gate(
        &self,
        gate: CompletionGate,
        retained_instance: Option<Instance>,
    ) -> DeletionResult {
        let (disposition, message) = match gate {
            CompletionGate::AlreadyGone => (
                DeletionDisposition::AlreadyGone,
                "Session was already removed by another process",
            ),
            CompletionGate::KeptRestored => (
                DeletionDisposition::KeptRestored,
                "Session was restored before teardown, so it was not purged",
            ),
            CompletionGate::Superseded => (
                DeletionDisposition::Busy,
                "Session changed lifecycle generation before teardown, so it was not purged",
            ),
            CompletionGate::Proceed => unreachable!("proceed is not a terminal result"),
        };
        DeletionResult::rejected(
            self.request.session_id.clone(),
            disposition,
            message,
            retained_instance,
        )
    }

    /// Atomically validate this reservation and remove its durable row before any irreversible
    /// external teardown.
    pub fn begin_irreversible(
        mut self,
    ) -> std::result::Result<CommittedPurge, Box<DeletionResult>> {
        self.seal_cleanup();
        if let Err(error) = self.ensure_lifecycle_lock() {
            return Err(Box::new(DeletionResult::rejected(
                self.request.session_id.clone(),
                DeletionDisposition::Failed,
                format!("Failed to resume reserved session purge: {error}"),
                None,
            )));
        }
        if let Some(message) = self.ownership_error() {
            return Err(Box::new(self.failed_after_release(message)));
        }
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let mut commit = None;
        if let Err(error) = self
            .storage
            .update_under_workspace_claim_lock(|instances, _groups| {
                let Some(index) = instances.iter().position(|instance| instance.id == id) else {
                    commit = Some((CompletionGate::AlreadyGone, None));
                    return Ok(());
                };
                let acknowledged = self.native_stop.current_projection();
                if self
                    .original
                    .validate_native_history(&instances[index])
                    .is_err()
                    || (acknowledged
                        .validate_baseline_at(&instances[index], generation)
                        .is_err()
                        && acknowledged
                            .validate_restored_at(&instances[index], generation)
                            .is_err())
                {
                    commit = Some((CompletionGate::Superseded, Some(instances[index].clone())));
                    return Ok(());
                }
                let restored = crate::session::claim::purge_restored_row_must_be_kept(
                    was_trashed,
                    instances[index].is_trashed(),
                ) || (self.cleanup == PurgeCleanup::SidecarsOnly
                    && instances[index].trashed_at != self.request.instance.trashed_at);
                let owns = instances[index]
                    .lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation);
                if restored {
                    instances[index].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                    commit = Some((CompletionGate::KeptRestored, Some(instances[index].clone())));
                } else if !owns {
                    commit = Some((CompletionGate::Superseded, Some(instances[index].clone())));
                } else {
                    instances.remove(index);
                    commit = Some((CompletionGate::Proceed, None));
                }
                Ok(())
            })
        {
            return Err(Box::new(DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                format!("Failed to commit irreversible session purge: {error}"),
                None,
            )));
        }

        let Some((gate, retained)) = commit else {
            return Err(Box::new(DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                "Irreversible purge commit produced no outcome",
                None,
            )));
        };
        self.active = false;
        if !matches!(gate, CompletionGate::Proceed) {
            return Err(Box::new(self.result_for_gate(gate, retained)));
        }
        // Same refresh `gate` performs, on the row captured at the commit: the
        // sidecars are torn down from here, after the row is gone, so a stale
        // `project_path` would have nothing left to correct it.
        if let Some(current) = retained {
            self.request.instance = current;
        }
        Ok(CommittedPurge {
            request: DeletionRequest {
                session_id: self.request.session_id.clone(),
                instance: self.request.instance.clone(),
                delete_worktree: self.request.delete_worktree,
                delete_branch: self.request.delete_branch,
                delete_sandbox: self.request.delete_sandbox,
                force_delete: self.request.force_delete,
                detach_hooks: self.request.detach_hooks,
                keep_scratch: self.request.keep_scratch,
            },
            cleanup: self.cleanup,
            _control_owner: self.control_owner.take(),
            _workspace_claim_lock: self
                .workspace_claim_lock
                .take()
                .expect("active purge transaction must own its workspace claim"),
            _identity_lock: self.identity_lock.take(),
            _lifecycle_lock: self
                .lifecycle_lock
                .take()
                .expect("active purge transaction must own its lifecycle lock"),
        })
    }

    /// Reacquire and verify the token, then keep the lifecycle flock through
    /// teardown and the durable commit.
    fn complete_inner(
        mut self,
        after_teardown: impl FnOnce(&Instance) -> std::result::Result<(), String>,
        commit_on_teardown_failure: bool,
        teardown: fn(&str) -> crate::containers::Teardown,
    ) -> DeletionResult {
        self.seal_cleanup();
        if self.cleanup == PurgeCleanup::SidecarsOnly {
            return match self.begin_irreversible() {
                Ok(committed) => committed.finish(),
                Err(result) => *result,
            };
        }
        if let Err(error) = self.ensure_lifecycle_lock() {
            return DeletionResult::rejected(
                self.request.session_id.clone(),
                DeletionDisposition::Failed,
                format!("Failed to resume reserved session purge: {error}"),
                None,
            );
        }
        let id = self.request.session_id.clone();
        let (gate, retained) = match self.gate() {
            Ok(outcome) => outcome,
            Err(error) => {
                return DeletionResult::rejected(
                    id,
                    DeletionDisposition::Failed,
                    format!("Failed to verify purge reservation: {error}"),
                    None,
                );
            }
        };
        if !matches!(gate, CompletionGate::Proceed) {
            return self.result_for_gate(gate, retained);
        }
        if let Some(message) = self.ownership_error() {
            return self.failed_after_release(message);
        }
        let mut result = perform_deletion_core(&self.request, true, true, teardown);
        if !result.success && !commit_on_teardown_failure {
            self.acknowledge_retention(&mut result);
            result.disposition = DeletionDisposition::Failed;
            return result;
        }
        if let Err(error) = after_teardown(&self.request.instance) {
            let mut failed = self.failed_after_release(error);
            failed.teardown_started = result.teardown_started;
            failed.messages = result.messages;
            return failed;
        }

        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let mut commit = None;
        let commit_result = self
            .storage
            .update_under_workspace_claim_lock(|instances, _groups| {
                let Some(index) = instances.iter().position(|instance| instance.id == id) else {
                    commit = Some((CompletionGate::AlreadyGone, None));
                    return Ok(());
                };
                let restored = crate::session::claim::purge_restored_row_must_be_kept(
                    was_trashed,
                    instances[index].is_trashed(),
                );
                let owns = instances[index]
                    .lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation);
                if restored {
                    instances[index].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                    commit = Some((CompletionGate::KeptRestored, Some(instances[index].clone())));
                } else if !owns {
                    commit = Some((CompletionGate::Superseded, Some(instances[index].clone())));
                } else {
                    instances.remove(index);
                    commit = Some((CompletionGate::Proceed, None));
                }
                Ok(())
            });
        self.lifecycle_lock = None;
        match commit_result {
            Err(error) => {
                result.success = false;
                result.disposition = DeletionDisposition::Failed;
                result.errors.push(format!(
                    "Session teardown completed, but sessions.json could not be updated: {error}"
                ));
                result
            }
            Ok(()) => {
                self.active = false;
                match commit {
                    Some((CompletionGate::Proceed, _)) => {
                        result.disposition = DeletionDisposition::Removed;
                        result
                    }
                    Some((gate, retained)) => {
                        let mut gated = self.result_for_gate(gate, retained);
                        gated.teardown_started = true;
                        gated.messages = result.messages;
                        gated.errors.extend(result.errors);
                        gated
                    }
                    None => DeletionResult::rejected(
                        id,
                        DeletionDisposition::Failed,
                        "Purge commit produced no outcome",
                        None,
                    ),
                }
            }
        }
    }
    pub fn complete_with(
        self,
        after_teardown: impl FnOnce(&Instance) -> std::result::Result<(), String>,
    ) -> DeletionResult {
        self.complete_inner(after_teardown, false, default_teardown)
    }
    #[cfg(test)]
    pub(crate) fn complete_with_test_teardown(
        self,
        after_teardown: impl FnOnce(&Instance) -> std::result::Result<(), String>,
        teardown: fn(&str) -> crate::containers::Teardown,
    ) -> DeletionResult {
        self.complete_inner(after_teardown, false, teardown)
    }

    pub fn complete(self) -> DeletionResult {
        self.complete_inner(|_| Ok(()), false, default_teardown)
    }
}

impl CommittedPurge {
    /// Clean up resources while retaining the lifecycle flock that covered the
    /// irreversible durable-row removal.
    pub fn finish(self) -> DeletionResult {
        let mut result = match self.cleanup {
            PurgeCleanup::All => perform_deletion_teardown_lifecycle_locked(&self.request, true),
            PurgeCleanup::SidecarsOnly => perform_deletion_sidecars(&self.request),
        };
        result.disposition = DeletionDisposition::Removed;
        result
    }
}

impl Drop for PurgeTransaction {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let stop = self.native_stop.clone();
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let _ = std::thread::Builder::new()
            .name("aoe-purge-reservation-release".to_string())
            .spawn(move || {
                let storage = stop.storage();
                let Ok(_workspace) = crate::session::acquire_session_workspace_claim_lock() else {
                    return;
                };
                let Ok(_identity) = crate::session::acquire_session_identity_lock() else {
                    return;
                };
                if storage.verify_profile_identity().is_err() {
                    return;
                }
                let Ok(_lifecycle_lock) = storage.acquire_instance_lifecycle_lock(&id) else {
                    return;
                };
                let _ = storage.update_under_workspace_claim_lock(|instances, _groups| {
                    if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                        stop.current_projection()
                            .validate_baseline_at(stored, generation)?;
                        stop.original().validate_native_history(stored)?;
                        stored.release_lifecycle_reservation_if_owned(
                            LifecycleOperation::Purge,
                            generation,
                        );
                    }
                    Ok(())
                });
            });
    }
}

/// Settle the stored execution journal before hooks or checkout destruction.
/// Registry absence and daemon memory are not execution coverage.
pub fn settle_runner_of(
    transaction: PurgeTransaction,
) -> impl std::future::Future<Output = Result<PurgeTransaction, Box<DeletionResult>>> + Send + 'static
{
    let id = transaction.request.session_id.clone();
    let driver = tokio::spawn(settle_runner_of_owned(transaction));
    async move {
        driver.await.unwrap_or_else(|error| {
            Err(Box::new(DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                format!("Owned purge settlement driver failed: {error}"),
                None,
            )))
        })
    }
}

async fn settle_runner_of_owned(
    transaction: PurgeTransaction,
) -> Result<PurgeTransaction, Box<DeletionResult>> {
    let mut released = transaction.release_locks_for_teardown();
    let scope = released.native_stop_scope();
    let outcome = match &released.control_owner {
        Some(owner) => {
            crate::session::runner_journal::settle_purge(scope, owner.control.clone()).await
        }
        None => crate::session::runner_journal::settle(scope).await,
    };
    match outcome {
        Ok(()) => Ok(released),
        Err(error) => {
            let failed = released.failed_after_release(format!(
                "The agent for this session is not proven dead, so nothing was removed: \
                 {error}. Retry once it exits."
            ));
            released.active = false;
            Err(Box::new(failed))
        }
    }
}

pub async fn execute_deletion(request: DeletionRequest) -> DeletionResult {
    execute_deletion_with_cleanup(request, PurgeCleanup::All, None).await
}

pub(crate) async fn execute_owned_deletion(
    request: DeletionRequest,
    owner: PurgeOwner,
) -> DeletionResult {
    execute_deletion_with_cleanup(request, PurgeCleanup::All, Some(owner)).await
}

pub(crate) async fn execute_drop(instance: Instance, owner: Option<PurgeOwner>) -> DeletionResult {
    let request = DeletionRequest {
        session_id: instance.id.clone(),
        instance,
        delete_worktree: false,
        delete_branch: false,
        delete_sandbox: true,
        force_delete: true,
        detach_hooks: true,
        keep_scratch: true,
    };
    execute_deletion_with_cleanup(request, PurgeCleanup::SidecarsOnly, owner).await
}

async fn execute_deletion_with_cleanup(
    request: DeletionRequest,
    cleanup: PurgeCleanup,
    owner: Option<PurgeOwner>,
) -> DeletionResult {
    let id = request.session_id.clone();
    let driver = tokio::spawn(execute_deletion_owned(request, cleanup, owner));
    driver.await.unwrap_or_else(|error| {
        DeletionResult::rejected(
            id,
            DeletionDisposition::Failed,
            format!("Owned deletion driver failed: {error}"),
            None,
        )
    })
}

async fn execute_deletion_owned(
    request: DeletionRequest,
    cleanup: PurgeCleanup,
    owner: Option<PurgeOwner>,
) -> DeletionResult {
    let id = request.session_id.clone();
    let recent_entry = crate::session::recent_project_entry_for(&request.instance);
    let reserved = tokio::task::spawn_blocking(move || {
        let storage = request.instance.original_storage()?;
        PurgeTransaction::reserve_with_cleanup(storage.as_ref().clone(), request, cleanup, owner)
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(|result| result);
    let settled = match reserved {
        Ok(PurgeReservation::Reserved(transaction)) => settle_runner_of(transaction).await,
        Ok(PurgeReservation::Rejected(result)) => Err(Box::new(result)),
        Err(error) => Err(Box::new(DeletionResult::rejected(
            id,
            DeletionDisposition::Failed,
            format!("Could not reserve session deletion: {error}"),
            None,
        ))),
    };
    let result = match settled {
        Ok(transaction) if cleanup == PurgeCleanup::SidecarsOnly => {
            match transaction.begin_irreversible() {
                Ok(committed) => committed.finish(),
                Err(result) => *result,
            }
        }
        Ok(transaction) => transaction.run_hooks().complete(),
        Err(result) => *result,
    };
    if result.disposition == DeletionDisposition::Removed {
        if let Some(entry) = recent_entry {
            if let Err(error) = crate::session::record_recent_project(entry) {
                tracing::warn!(target: "session.delete", "recording recent project after delete failed: {error}");
            }
        }
    }
    result
}

fn perform_deletion_sidecars(request: &DeletionRequest) -> DeletionResult {
    request.instance.kill_all_tmux_sessions_locked();
    let mut messages = Vec::new();
    let mut errors = Vec::new();
    if request.delete_sandbox
        && request
            .instance
            .sandbox_info
            .as_ref()
            .is_some_and(|s| s.enabled)
    {
        deletion_messages_for(
            default_teardown(&request.session_id),
            &mut messages,
            &mut errors,
        );
    }
    DeletionResult {
        session_id: request.session_id.clone(),
        success: errors.is_empty(),
        messages,
        errors,
        disposition: DeletionDisposition::Removed,
        teardown_started: true,
        retained_instance: None,
        retained_stop: None,
    }
}

/// Whether `workspace_dir` has the workspace layout AoE creates and may therefore be removed once
/// empty.
fn workspace_dir_is_aoe_owned(ws_info: &crate::session::WorkspaceInfo) -> bool {
    let ws_path = Path::new(&ws_info.workspace_dir);
    if ws_info.repos.is_empty() {
        return false;
    }
    ws_info.repos.iter().all(|repo| {
        let worktree = Path::new(&repo.worktree_path);
        worktree != ws_path && worktree.starts_with(ws_path)
    })
}

/// Whether `branch` is one of the branches git states is `main_repo`'s default, so its worktree
/// must be preserved.
fn is_protected_default_branch(main_repo: &Path, branch: &str) -> bool {
    GitWorktree::new(main_repo.to_path_buf())
        .and_then(|git| git.protected_default_branch_names())
        .is_ok_and(|names| names.contains(branch))
}

#[derive(Clone, Copy)]
pub(crate) struct SessionPathOwner<'a> {
    pub profile: &'a str,
    pub session_id: &'a str,
}

/// Resolve missing claims only through components proven absent, never broken aliases.
pub(crate) fn resolve_claim_path(path: &Path) -> Option<PathBuf> {
    let mut current = path;
    let mut suffix = Vec::new();
    loop {
        match current.canonicalize() {
            Ok(mut resolved) => {
                for component in suffix.into_iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return None,
        }
        match std::fs::symlink_metadata(current) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            _ => return None,
        }
        let component = match current.components().next_back()? {
            std::path::Component::Normal(component) => component,
            _ => return None,
        };
        suffix.push(component);
        current = current.parent()?;
        if current.as_os_str().is_empty() {
            current = Path::new(".");
        }
    }
}

pub(crate) fn paths_overlap_destructive(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (resolve_claim_path(left), resolve_claim_path(right)) {
        (Some(left), Some(right)) => left.starts_with(&right) || right.starts_with(&left),
        _ => true,
    }
}

#[derive(Default)]
pub(crate) struct WorktreePathInventory {
    pub(crate) established: Vec<PathBuf>,
    pub(crate) pending: Vec<PathBuf>,
}

impl WorktreePathInventory {
    fn iter(&self) -> impl Iterator<Item = &PathBuf> {
        self.established.iter().chain(&self.pending)
    }
}

/// The paths sessions outside a deletion use, across every profile.
pub(crate) enum PathsInUse {
    Known(WorktreePathInventory),
    /// Some store could not be read, so every path must be assumed in use.
    Unknown(String),
}

impl PathsInUse {
    pub(crate) fn covers(&self, root: &Path) -> bool {
        match self {
            Self::Known(paths) => paths
                .iter()
                .any(|path| paths_overlap_destructive(path, root)),
            Self::Unknown(_) => true,
        }
    }

    fn reason(&self) -> String {
        match self {
            Self::Known(_) => "another session still uses it".to_string(),
            Self::Unknown(reason) => format!("other sessions could not be checked ({reason})"),
        }
    }
}

fn all_profile_storages() -> std::result::Result<(Vec<String>, Vec<Storage>), String> {
    let profiles = crate::session::list_profiles_for_worktree_inventory()
        .map_err(|error| format!("listing profiles: {error}"))?;
    let storages = profiles
        .iter()
        .map(|profile| {
            Storage::open_unwatched(profile)
                .map_err(|error| format!("opening profile '{profile}': {error}"))
        })
        .collect::<std::result::Result<_, _>>()?;
    Ok((profiles, storages))
}

fn scan_paths_in_use(storages: &[Storage], except: &[SessionPathOwner<'_>]) -> PathsInUse {
    let mut owners = Vec::with_capacity(except.len());
    for owner in except {
        if owner.profile.is_empty() || owner.session_id.is_empty() {
            return PathsInUse::Unknown("session ownership has no explicit profile or id".into());
        }
        let identity = crate::session::get_profile_dir_path(owner.profile)
            .and_then(|dir| std::fs::metadata(dir).map_err(Into::into));
        let identity = match identity {
            Ok(identity) if identity.is_dir() => identity,
            Ok(_) => return PathsInUse::Unknown("owner profile is not a directory".into()),
            Err(error) => return PathsInUse::Unknown(format!("resolving owner profile: {error}")),
        };
        owners.push((identity, owner.session_id, false, false));
    }
    let mut paths = WorktreePathInventory::default();
    for storage in storages {
        if !owners.is_empty() {
            let identity = storage
                .sessions_path()
                .parent()
                .and_then(|dir| std::fs::metadata(dir).ok());
            let Some(identity) = identity else {
                return PathsInUse::Unknown(format!("resolving profile '{}'", storage.profile()));
            };
            for (owner_identity, _, seen, matches) in &mut owners {
                *matches =
                    crate::session::storage::same_filesystem_identity(owner_identity, &identity);
                *seen |= *matches;
            }
        }
        match storage.load_path_owners_locked() {
            Ok(document) => {
                for (_, id, _, matches) in &owners {
                    if *matches
                        && document
                            .owners
                            .get(*id)
                            .is_some_and(|slot| slot.count != 1 || slot.ambiguous)
                    {
                        return PathsInUse::Unknown(format!(
                            "ambiguous path owner {id} cannot be excluded"
                        ));
                    }
                }
                for row in document.rows {
                    if row.ids.iter().any(|id| {
                        owners
                            .iter()
                            .any(|(_, excluded, _, matches)| *matches && *excluded == id)
                    }) {
                        continue;
                    }
                    paths.established.extend(row.paths);
                    match row.pending {
                        super::WorktreePathClaims::Pending(pending)
                        | super::WorktreePathClaims::Unknown(Some(pending)) => {
                            paths.pending.extend(pending)
                        }
                        super::WorktreePathClaims::Unknown(None) => {
                            return PathsInUse::Unknown(format!(
                                "profile '{}' has unknown filesystem intent",
                                storage.profile()
                            ))
                        }
                        super::WorktreePathClaims::None => {}
                    }
                }
            }
            Err(error) => {
                return PathsInUse::Unknown(format!(
                    "reading profile '{}': {error}",
                    storage.profile()
                ))
            }
        }
    }
    let retained = match super::retained_intents::load_owners() {
        Ok(retained) => retained,
        Err(error) => {
            return PathsInUse::Unknown(format!("reading permanent retained owners: {error}"))
        }
    };
    for row in retained.rows {
        paths.established.extend(row.paths);
        match row.pending {
            super::WorktreePathClaims::Pending(pending)
            | super::WorktreePathClaims::Unknown(Some(pending)) => paths.pending.extend(pending),
            super::WorktreePathClaims::Unknown(None) => {
                return PathsInUse::Unknown(
                    "permanent retained owner has unknown filesystem intent".into(),
                )
            }
            super::WorktreePathClaims::None => {}
        }
    }
    if owners.iter().any(|(_, _, seen, _)| !seen) {
        return PathsInUse::Unknown("owner profile is missing from the ownership inventory".into());
    }
    PathsInUse::Known(paths)
}

/// Unlocked snapshot; destructive callers recheck under the ownership locks.
pub(crate) fn paths_in_use_except(except: &[SessionPathOwner<'_>]) -> PathsInUse {
    match all_profile_storages() {
        Ok((_, storages)) => scan_paths_in_use(&storages, except),
        Err(reason) => PathsInUse::Unknown(reason),
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ClaimOwner {
    profile: Option<usize>,
    id: std::sync::Arc<str>,
    exclusive: bool,
}

pub(crate) struct PathOwnerEnvelope {
    pub(crate) ids: Vec<String>,
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) pending: super::WorktreePathClaims,
}

pub(crate) struct WorktreeOwnerDocument {
    pub(crate) owners: std::collections::HashMap<String, super::raw_document::OwnerSlot>,
    pub(crate) rows: Vec<PathOwnerEnvelope>,
}

impl WorktreeOwnerDocument {
    pub(crate) fn project(raw: super::raw_document::RawDocument) -> Result<Self> {
        use super::raw_document::RawObject;
        let owners = raw.owners("id");
        let mut rows = Vec::with_capacity(raw.rows.len());
        for raw in &raw.rows {
            let object = RawObject::parse(raw).context("unreadable path owner envelope")?;
            let ids = object
                .values("id")
                .map(|id| serde_json::from_str(id.get()))
                .collect::<std::result::Result<Vec<String>, _>>()?;
            anyhow::ensure!(!ids.is_empty(), "path owner has no identity");
            let mut paths = vec![serde_json::from_str(
                object
                    .unique("project_path")?
                    .context("path owner has no project path")?
                    .get(),
            )?];
            if let Some(path) = object.unique("pre_trash_project_path")? {
                if let Some(path) = serde_json::from_str::<Option<PathBuf>>(path.get())? {
                    paths.push(path);
                }
            }
            if let Some(workspace) = object
                .unique("workspace_info")?
                .filter(|workspace| workspace.get() != "null")
            {
                let workspace = RawObject::parse(workspace)?;
                paths.push(serde_json::from_str(
                    workspace
                        .unique("workspace_dir")?
                        .context("workspace owner has no directory")?
                        .get(),
                )?);
                let repos: Vec<&serde_json::value::RawValue> = serde_json::from_str(
                    workspace
                        .unique("repos")?
                        .context("workspace owner has no repository inventory")?
                        .get(),
                )?;
                for repo in repos {
                    let repo = RawObject::parse(repo)?;
                    paths.push(serde_json::from_str(
                        repo.unique("worktree_path")?
                            .context("workspace repository has no worktree path")?
                            .get(),
                    )?);
                }
            }
            let pending = if let Some(lease) = object
                .unique("lifecycle_reservation")?
                .filter(|lease| lease.get() != "null")
            {
                let lease = RawObject::parse(lease)?;
                let _: LifecycleOperation = serde_json::from_str(
                    lease
                        .unique("op")?
                        .context("filesystem intent has no operation")?
                        .get(),
                )?;
                let claims = lease
                    .unique("path_claims")?
                    .context("filesystem intent has no path claims")?;
                let fields = RawObject::parse(claims)?;
                fields.unique("state")?;
                fields.unique("paths")?;
                serde_json::from_str(claims.get())?
            } else {
                super::WorktreePathClaims::None
            };
            rows.push(PathOwnerEnvelope {
                ids,
                paths,
                pending,
            });
        }
        Ok(Self { owners, rows })
    }
}

/// A strict, physically keyed ownership snapshot for one fenced reconciliation pass.
pub(crate) struct PathClaimIndex {
    claims: std::collections::BTreeMap<PathBuf, Vec<ClaimOwner>>,
    owned_paths: Vec<std::collections::HashMap<std::sync::Arc<str>, Vec<(PathBuf, bool)>>>,
    ambiguous_owners: Vec<std::collections::HashSet<String>>,
    targets: Vec<(usize, usize, Vec<Instance>)>,
    profile_identities: Vec<std::fs::Metadata>,
    valid: bool,
}

impl PathClaimIndex {
    pub(crate) fn load(targets: &[Storage]) -> anyhow::Result<Self> {
        Self::load_snapshot(targets, true)
    }

    pub(crate) fn load_for_writer(targets: &[Storage]) -> anyhow::Result<Self> {
        Self::load_snapshot(targets, false)
    }

    fn load_snapshot(targets: &[Storage], lock_stores: bool) -> anyhow::Result<Self> {
        let target_identities = targets
            .iter()
            .map(|target| {
                target.verify_profile_identity()?;
                Ok(std::fs::metadata(target.sessions_path().parent().unwrap())?)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let (_, mut storages) = all_profile_storages().map_err(anyhow::Error::msg)?;
        for target in targets {
            if !storages
                .iter()
                .any(|candidate| candidate.same_origin_as(target))
            {
                storages.push(target.clone());
            }
        }
        let scan = || -> anyhow::Result<Self> {
            let mut result = Self {
                claims: Default::default(),
                owned_paths: Vec::new(),
                ambiguous_owners: Vec::new(),
                targets: Vec::new(),
                profile_identities: Vec::new(),
                valid: true,
            };
            let mut profiles: Vec<std::fs::Metadata> = Vec::new();
            for storage in &storages {
                storage.verify_profile_identity()?;
                let identity = std::fs::metadata(storage.sessions_path().parent().unwrap())?;
                if profiles.iter().any(|previous| {
                    crate::session::storage::same_filesystem_identity(previous, &identity)
                }) {
                    continue;
                }
                let profile = profiles.len();
                result.owned_paths.push(Default::default());
                let document = storage.load_path_owners_locked()?;
                result.ambiguous_owners.push(
                    document
                        .owners
                        .into_iter()
                        .filter_map(|(id, slot)| (slot.count != 1 || slot.ambiguous).then_some(id))
                        .collect(),
                );
                for row in &document.rows {
                    let pending = match &row.pending {
                        super::WorktreePathClaims::Pending(paths)
                        | super::WorktreePathClaims::Unknown(Some(paths)) => paths.as_slice(),
                        super::WorktreePathClaims::Unknown(None) => {
                            result.valid = false;
                            &[]
                        }
                        super::WorktreePathClaims::None => &[],
                    };
                    for id in &row.ids {
                        result.add_claims(
                            profile,
                            id,
                            row.paths
                                .iter()
                                .map(|path| (path.as_path(), false))
                                .chain(pending.iter().map(|path| (path.as_path(), true))),
                        );
                    }
                }
                if let Some(target) = target_identities.iter().position(|target| {
                    crate::session::storage::same_filesystem_identity(target, &identity)
                }) {
                    if lock_stores {
                        result.targets.push((
                            target,
                            profile,
                            storage.load_strict_for_worktree_ownership_locked()?,
                        ));
                    }
                }
                profiles.push(identity);
            }
            let retained = super::retained_intents::load_owners()?;
            for row in retained.rows {
                let pending = match &row.pending {
                    super::WorktreePathClaims::Pending(paths)
                    | super::WorktreePathClaims::Unknown(Some(paths)) => paths.as_slice(),
                    super::WorktreePathClaims::Unknown(None) => {
                        result.valid = false;
                        &[]
                    }
                    super::WorktreePathClaims::None => &[],
                };
                for id in &row.ids {
                    let id: std::sync::Arc<str> = std::sync::Arc::from(id.as_str());
                    for (path, exclusive) in row
                        .paths
                        .iter()
                        .map(|path| (path, false))
                        .chain(pending.iter().map(|path| (path, true)))
                    {
                        let Some(path) = resolve_claim_path(path) else {
                            result.valid = false;
                            continue;
                        };
                        result.claims.entry(path).or_default().push(ClaimOwner {
                            profile: None,
                            id: std::sync::Arc::clone(&id),
                            exclusive,
                        });
                    }
                }
            }
            for (target, identity) in targets.iter().zip(&target_identities) {
                anyhow::ensure!(
                    profiles.iter().any(|profile| {
                        crate::session::storage::same_filesystem_identity(profile, identity)
                    }),
                    "original profile is absent from ownership inventory"
                );
                target.verify_profile_identity()?;
            }
            result.profile_identities = profiles;
            Ok(result)
        };
        if lock_stores {
            crate::session::storage::with_storages_locked(&storages, scan)?
        } else {
            scan()
        }
    }

    pub(crate) fn take_targets(&mut self) -> Vec<(usize, usize, Vec<Instance>)> {
        std::mem::take(&mut self.targets)
    }

    pub(crate) fn writer_profile(&self, storage: &Storage) -> anyhow::Result<usize> {
        anyhow::ensure!(self.valid, "path ownership inventory is uncertain");
        storage.verify_profile_identity()?;
        let identity = std::fs::metadata(storage.sessions_path().parent().unwrap())?;
        self.profile_identities
            .iter()
            .position(|original| {
                crate::session::storage::same_filesystem_identity(original, &identity)
            })
            .ok_or_else(|| {
                anyhow::anyhow!("writer physical profile is absent from the fenced inventory")
            })
    }

    fn add_claims<'a>(
        &mut self,
        profile: usize,
        id: &str,
        paths: impl Iterator<Item = (&'a Path, bool)>,
    ) {
        let id = self.owned_paths[profile]
            .get_key_value(id)
            .map(|(id, _)| std::sync::Arc::clone(id))
            .unwrap_or_else(|| std::sync::Arc::from(id));
        let previous = self.owned_paths[profile]
            .entry(std::sync::Arc::clone(&id))
            .or_default();
        for (path, exclusive) in paths {
            let Some(path) = resolve_claim_path(path) else {
                self.valid = false;
                continue;
            };
            if previous
                .iter()
                .any(|(known, held)| known == &path && *held == exclusive)
            {
                continue;
            }
            self.claims
                .entry(path.clone())
                .or_default()
                .push(ClaimOwner {
                    profile: Some(profile),
                    id: std::sync::Arc::clone(&id),
                    exclusive,
                });
            previous.push((path, exclusive));
        }
    }

    pub(crate) fn update(&mut self, profile: usize, row: &Instance) {
        if self.ambiguous_owners[profile].contains(row.id.as_str()) {
            self.valid = false;
            return;
        }
        let id = self.owned_paths[profile]
            .get_key_value(row.id.as_str())
            .map(|(id, _)| std::sync::Arc::clone(id))
            .unwrap_or_else(|| std::sync::Arc::from(row.id.as_str()));
        let owner = ClaimOwner {
            profile: Some(profile),
            id: std::sync::Arc::clone(&id),
            exclusive: false,
        };
        let previous = self.owned_paths[profile].entry(id).or_default();
        let mut retained = 0;
        let pending = match row
            .lifecycle_reservation
            .as_ref()
            .map(|lease| &lease.path_claims)
        {
            Some(crate::session::WorktreePathClaims::Pending(paths))
            | Some(crate::session::WorktreePathClaims::Unknown(Some(paths))) => paths.as_slice(),
            Some(crate::session::WorktreePathClaims::Unknown(None)) => {
                self.valid = false;
                return;
            }
            _ => &[],
        };
        for (path, exclusive) in row
            .durable_worktree_paths()
            .map(|path| (path, false))
            .chain(pending.iter().map(|path| (path.as_path(), true)))
        {
            let Some(path) = resolve_claim_path(path) else {
                self.valid = false;
                continue;
            };
            let path = (path, exclusive);
            if previous[..retained].contains(&path) {
                continue;
            }
            if let Some(offset) = previous[retained..].iter().position(|old| old == &path) {
                previous.swap(retained, retained + offset);
            } else {
                self.claims
                    .entry(path.0.clone())
                    .or_default()
                    .push(ClaimOwner {
                        exclusive,
                        ..owner.clone()
                    });
                previous.push(path);
                let last = previous.len() - 1;
                previous.swap(retained, last);
            }
            retained += 1;
        }
        for (path, exclusive) in previous.drain(retained..) {
            if let Some(owners) = self.claims.get_mut(&path) {
                owners.retain(|claim| {
                    claim
                        != &ClaimOwner {
                            exclusive,
                            ..owner.clone()
                        }
                });
            }
            if self.claims.get(&path).is_some_and(Vec::is_empty) {
                self.claims.remove(&path);
            }
        }
    }

    pub(crate) fn ensure_unclaimed(
        &self,
        profile: usize,
        id: &str,
        candidates: &[PathBuf],
    ) -> anyhow::Result<()> {
        self.ensure_claims_unclaimed(
            Some((profile, id)),
            candidates.iter().map(PathBuf::as_path),
            false,
        )
    }

    pub(crate) fn ensure_pending_unclaimed(
        &self,
        profile: usize,
        id: &str,
        candidates: &[PathBuf],
    ) -> anyhow::Result<()> {
        self.ensure_claims_unclaimed(
            Some((profile, id)),
            candidates.iter().map(PathBuf::as_path),
            true,
        )
    }

    pub(crate) fn ensure_pending_writes_unclaimed(
        &self,
        candidates: &[&Path],
    ) -> anyhow::Result<()> {
        self.ensure_claims_unclaimed(None, candidates.iter().copied(), true)
    }

    pub(crate) fn ensure_writes_unclaimed(&self, candidates: &[&Path]) -> anyhow::Result<()> {
        self.ensure_claims_unclaimed(None, candidates.iter().copied(), false)
    }

    fn ensure_claims_unclaimed<'a>(
        &self,
        excluded_owner: Option<(usize, &str)>,
        candidates: impl IntoIterator<Item = &'a Path>,
        pending_only: bool,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(self.valid, "path ownership inventory is uncertain");
        if let Some((profile, id)) = excluded_owner {
            anyhow::ensure!(
                !self.ambiguous_owners[profile].contains(id),
                "ambiguous path owner cannot be excluded"
            );
        }
        let is_peer = |owners: &[ClaimOwner]| {
            owners.iter().any(|owner| {
                (!pending_only || owner.exclusive)
                    && excluded_owner.is_none_or(|(profile, id)| {
                        owner.profile != Some(profile) || owner.id.as_ref() != id
                    })
            })
        };
        for candidate in candidates {
            let candidate = resolve_claim_path(candidate)
                .ok_or_else(|| anyhow::anyhow!("candidate path cannot be resolved safely"))?;
            for ancestor in candidate.ancestors() {
                anyhow::ensure!(
                    !self
                        .claims
                        .get(ancestor)
                        .is_some_and(|owners| is_peer(owners)),
                    "another session claims a candidate ancestor"
                );
            }
            for (path, owners) in self.claims.range::<Path, _>((
                std::ops::Bound::Included(candidate.as_path()),
                std::ops::Bound::Unbounded,
            )) {
                if !path.starts_with(&candidate) {
                    break;
                }
                anyhow::ensure!(
                    !is_peer(owners),
                    "another session claims a candidate descendant"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn invalidate(&mut self) {
        self.valid = false;
    }
}

pub(crate) fn ensure_unclaimed_paths(
    owner: SessionPathOwner<'_>,
    candidates: &[PathBuf],
) -> std::result::Result<(), String> {
    let paths = paths_in_use_except(&[owner]);
    match paths {
        PathsInUse::Unknown(reason) => Err(reason),
        PathsInUse::Known(paths)
            if paths.iter().any(|path| {
                candidates
                    .iter()
                    .any(|candidate| paths_overlap_destructive(Path::new(path), candidate))
            }) =>
        {
            Err("another session already claims one of the candidate paths".to_string())
        }
        PathsInUse::Known(_) => Ok(()),
    }
}

#[cfg(test)]
thread_local! {
    static AFTER_PATHS_IN_USE_SCAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with the paths other sessions use while every profile's storage lock is held, so no
/// session can adopt a path between the check and whatever `f` removes.
fn with_paths_in_use_locked<R>(
    owner: SessionPathOwner<'_>,
    identity_lock_held: bool,
    f: impl FnOnce(&PathsInUse) -> R,
) -> R {
    let identity_lock = if identity_lock_held {
        None
    } else {
        crate::session::acquire_session_identity_lock().ok()
    };
    if !identity_lock_held && identity_lock.is_none() {
        return f(&PathsInUse::Unknown(
            "could not acquire session identity lock".to_string(),
        ));
    }
    let _identity_lock = identity_lock;
    let (profiles, storages) = match all_profile_storages() {
        Ok(found) => found,
        Err(reason) => return f(&PathsInUse::Unknown(reason)),
    };
    let mut f = Some(f);
    let locked = crate::session::storage::with_storages_locked(&storages, || {
        let paths_in_use = match crate::session::list_profiles_for_worktree_inventory() {
            Ok(now) if now.iter().all(|profile| profiles.contains(profile)) => {
                scan_paths_in_use(&storages, &[owner])
            }
            Ok(_) => PathsInUse::Unknown("a profile was created during the deletion".to_string()),
            Err(error) => PathsInUse::Unknown(format!("listing profiles: {error}")),
        };
        #[cfg(test)]
        if let Some(hook) = AFTER_PATHS_IN_USE_SCAN.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        f.take().expect("called once")(&paths_in_use)
    });
    match locked {
        Ok(result) => result,
        Err(error) => f.take().expect("called once")(&PathsInUse::Unknown(format!(
            "locking session stores: {error}"
        ))),
    }
}

#[cfg(test)]
pub fn perform_deletion(request: &DeletionRequest) -> DeletionResult {
    run_on_destroy_hooks(&request.instance, request.detach_hooks);
    perform_deletion_with(request, |session_id| {
        DockerContainer::from_session_id(session_id).teardown(session_id)
    })
}
fn default_teardown(session_id: &str) -> crate::containers::Teardown {
    DockerContainer::from_session_id(session_id).teardown(session_id)
}

fn perform_deletion_teardown_lifecycle_locked(
    request: &DeletionRequest,
    identity_lock_held: bool,
) -> DeletionResult {
    perform_deletion_core(request, true, identity_lock_held, default_teardown)
}

/// Core deletion routine, parameterized over how the sandbox container is torn down so the
/// container-removal contract can be exercised without a live runtime.
#[cfg(test)]
fn perform_deletion_with(
    request: &DeletionRequest,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
) -> DeletionResult {
    perform_deletion_core(request, false, false, teardown)
}

#[cfg(test)]
fn perform_deletion_with_lifecycle_locked(
    request: &DeletionRequest,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
) -> DeletionResult {
    perform_deletion_core(request, true, false, teardown)
}

/// `lifecycle_locked` is the production path, which also keeps any worktree another session uses.
fn perform_deletion_core(
    request: &DeletionRequest,
    lifecycle_locked: bool,
    identity_lock_held: bool,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
) -> DeletionResult {
    tracing::debug!(target: "session.delete",
        session_id = %request.session_id,
        title = %request.instance.title,
        delete_worktree = request.delete_worktree,
        delete_branch = request.delete_branch,
        delete_sandbox = request.delete_sandbox,
        force_delete = request.force_delete,
        worktree_branch = request.instance.worktree_info.as_ref().map(|w| w.branch.as_str()).unwrap_or("<none>"),
        worktree_managed = request.instance.worktree_info.as_ref().map(|w| w.managed_by_aoe).unwrap_or(false),
        worktree_main_repo = request.instance.worktree_info.as_ref().map(|w| w.main_repo_path.as_str()).unwrap_or("<none>"),
        workspace_repos = request.instance.workspace_info.as_ref().map(|w| w.repos.len()).unwrap_or(0),
        "perform_deletion: starting"
    );

    // Keep the profile and identity locks around the complete teardown, not just
    // the ownership scan. A peer must not claim a path between the scan and any
    // destructive stage.
    let repos: &[super::WorkspaceRepo] = if request
        .instance
        .workspace_info
        .as_ref()
        .is_some_and(|w| w.cleanup_on_delete)
    {
        request.instance.all_repos()
    } else {
        &[]
    };
    if lifecycle_locked && request.needs_path_inventory() {
        return with_paths_in_use_locked(
            SessionPathOwner {
                profile: &request.instance.source_profile,
                session_id: &request.session_id,
            },
            identity_lock_held,
            |paths| {
                perform_deletion_teardown_under_ownership_guard(
                    request, repos, true, paths, teardown,
                )
            },
        );
    }
    perform_deletion_teardown_under_ownership_guard(
        request,
        repos,
        lifecycle_locked,
        &PathsInUse::Known(WorktreePathInventory::default()),
        teardown,
    )
}

fn perform_deletion_teardown_under_ownership_guard(
    request: &DeletionRequest,
    repos: &[super::WorkspaceRepo],
    lifecycle_locked: bool,
    paths_in_use: &PathsInUse,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
) -> DeletionResult {
    let mut errors = Vec::new();
    let mut messages = Vec::new();
    if let PathsInUse::Unknown(reason) = paths_in_use {
        return DeletionResult {
            session_id: request.session_id.clone(),
            success: false,
            teardown_started: false,
            messages,
            errors: vec![format!(
                "could not prove that session resources are unused: {reason}"
            )],
            disposition: DeletionDisposition::Failed,
            retained_instance: None,
            retained_stop: None,
        };
    }

    // on_destroy hooks run in the transaction's unlocked hook phase, before
    // this lifecycle-locked resource teardown begins.
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "tmux_kill", "perform_deletion: stage");
    if lifecycle_locked {
        request.instance.kill_all_tmux_sessions_locked();
    } else {
        request.instance.kill_all_tmux_sessions();
    }

    let is_sandboxed = request
        .instance
        .sandbox_info
        .as_ref()
        .is_some_and(|s| s.enabled);
    let container_gone = stage_teardown_worktrees(
        request,
        repos,
        is_sandboxed,
        paths_in_use,
        teardown,
        &mut errors,
        &mut messages,
    );
    let scratch_preserved = if request.instance.scratch {
        paths_in_use.covers(Path::new(&request.instance.project_path))
    } else {
        false
    };
    stage_cleanup_scratch(request, scratch_preserved, &mut errors, &mut messages);

    if container_gone && errors.is_empty() {
        stage_remove_agent_stores(request, &mut messages);
    }

    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "hook_status_cleanup", "perform_deletion: stage");
    crate::hooks::cleanup_hook_status_dir(&request.instance.id);

    if !errors.is_empty() {
        tracing::debug!(target: "session.delete",
            session_id = %request.session_id,
            error_count = errors.len(),
            errors = ?errors,
            "perform_deletion: completed with errors"
        );
    } else {
        tracing::debug!(target: "session.delete", session_id = %request.session_id, "perform_deletion: completed successfully");
    }

    DeletionResult {
        session_id: request.session_id.clone(),
        success: errors.is_empty(),
        teardown_started: true,
        messages,
        errors,
        disposition: DeletionDisposition::Failed,
        retained_instance: None,
        retained_stop: None,
    }
}

/// Container and worktree teardown, which destroys checkout contents and so must run inside the
/// same [`PathsInUse`] check that decides what to keep. Returns whether the container is gone.
fn stage_teardown_worktrees(
    request: &DeletionRequest,
    repos: &[super::WorkspaceRepo],
    is_sandboxed: bool,
    paths_in_use: &PathsInUse,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) -> bool {
    let preserved_worktree_paths =
        stage_collect_preserved_worktrees(request, repos, paths_in_use, errors, messages);
    // Any preserved worktree, dirty or default-branch, blocks the in-container preclean (a
    // recursive `find. -delete` that would reach through and destroy the contents we just decided
    // to keep) and the host workspace-dir removal alike: with a worktree preserved under it the
    // directory is not ours to remove, so we skip it rather than surface a spurious failure.
    let any_preserved = !preserved_worktree_paths.is_empty();

    if request.delete_worktree && is_sandboxed && !any_preserved {
        tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "sandbox_worktree_preclean", "perform_deletion: stage");
        let _ = crate::git::cleanup::cleanup_sandbox_worktree(&request.instance);
    }

    // Stage 3: container removal.
    let mut container_gone = false;
    if request.delete_sandbox && is_sandboxed {
        tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "container_remove", "perform_deletion: stage");
        let outcome = teardown(&request.instance.id);
        // A failed teardown can leave the container live with the store still bind mounted, so the
        // store may only go once the container is provably gone.
        container_gone = !matches!(outcome, crate::containers::Teardown::Failed(_));
        deletion_messages_for(outcome, messages, errors);
    }

    stage_remove_worktrees_and_branches(
        request,
        repos,
        &preserved_worktree_paths,
        any_preserved,
        errors,
        messages,
    );

    container_gone
}

fn stage_collect_preserved_worktrees(
    request: &DeletionRequest,
    repos: &[super::WorkspaceRepo],
    paths_in_use: &PathsInUse,
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) -> std::collections::HashSet<PathBuf> {
    let mut preserved_worktree_paths: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();

    // Default-branch guard, deliberately NOT behind the `!force_delete` gate below.
    if request.delete_worktree {
        if let Some(wt_info) = &request.instance.worktree_info {
            if wt_info.managed_by_aoe
                && is_protected_default_branch(Path::new(&wt_info.main_repo_path), &wt_info.branch)
            {
                let path = PathBuf::from(&request.instance.project_path);
                tracing::warn!(target: "session.delete",
                    session_id = %request.session_id,
                    branch = %wt_info.branch,
                    path = %path.display(),
                    "perform_deletion: preserving the worktree of a default branch"
                );
                messages.push(format!(
                    "Worktree preserved; '{}' is a default branch of its repository",
                    wt_info.branch
                ));
                preserved_worktree_paths.insert(path);
            }
        }
        for repo in repos.iter().filter(|r| r.managed_by_aoe) {
            if is_protected_default_branch(Path::new(&repo.main_repo_path), &repo.branch) {
                tracing::warn!(target: "session.delete",
                    session_id = %request.session_id,
                    repo = %repo.name,
                    branch = %repo.branch,
                    path = %repo.worktree_path,
                    "perform_deletion: preserving the worktree of a default branch"
                );
                messages.push(format!(
                    "Workspace ({}) worktree preserved; '{}' is a default branch of its repository",
                    repo.name, repo.branch
                ));
                preserved_worktree_paths.insert(PathBuf::from(&repo.worktree_path));
            }
        }
    }

    // A worktree another session still works in, or may, is kept, and so its branch, whichever
    // sessions the caller named. Checked before the dirty gate so a kept worktree cannot fail the
    // deletion.
    if request.delete_worktree {
        let in_use = |root: &Path| paths_in_use.covers(root);
        let still_used = paths_in_use.reason();
        if let Some(wt_info) = &request.instance.worktree_info {
            let path = PathBuf::from(&request.instance.project_path);
            if wt_info.managed_by_aoe && !preserved_worktree_paths.contains(&path) && in_use(&path)
            {
                messages.push(format!("Worktree kept; {still_used}"));
                preserved_worktree_paths.insert(path);
            }
        }
        if let Some(ws_info) = &request.instance.workspace_info {
            // Sessions attached to a workspace work in its root, so any use under it keeps every
            // repo worktree.
            if in_use(Path::new(&ws_info.workspace_dir)) {
                for repo in repos.iter().filter(|r| r.managed_by_aoe) {
                    if preserved_worktree_paths.insert(PathBuf::from(&repo.worktree_path)) {
                        messages.push(format!(
                            "Workspace ({}) worktree kept; {still_used}",
                            repo.name
                        ));
                    }
                }
            }
        }
    }

    if request.delete_worktree && !request.force_delete {
        if let Some(wt_info) = &request.instance.worktree_info {
            if wt_info.managed_by_aoe {
                let path = PathBuf::from(&request.instance.project_path);
                // A path the guard above already preserved must not also report dirty: that error
                // would fail the deletion and strand the row in the trash, which is what the guard
                // exists to avoid.
                if !preserved_worktree_paths.contains(&path) {
                    if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                        tracing::debug!(target: "session.delete",
                            session_id = %request.session_id,
                            path = %path.display(),
                            "perform_deletion: dirty worktree, skipping preclean + host remove"
                        );
                        errors.push(format!("Worktree: {}", msg));
                        preserved_worktree_paths.insert(path);
                    }
                }
            }
        }
        for repo in repos.iter().filter(|r| r.managed_by_aoe) {
            let path = PathBuf::from(&repo.worktree_path);
            if preserved_worktree_paths.contains(&path) {
                continue;
            }
            if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                tracing::debug!(target: "session.delete",
                    session_id = %request.session_id,
                    repo = %repo.name,
                    path = %path.display(),
                    "perform_deletion: dirty session repo, skipping preclean + host remove"
                );
                errors.push(format!("Workspace ({}): {}", repo.name, msg));
                preserved_worktree_paths.insert(path);
            }
        }
    }

    preserved_worktree_paths
}

fn stage_remove_worktrees_and_branches(
    request: &DeletionRequest,
    repos: &[super::WorkspaceRepo],
    preserved_worktree_paths: &std::collections::HashSet<PathBuf>,
    any_preserved: bool,
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) {
    // Stage 4: worktree cleanup.
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "worktree_remove", "perform_deletion: stage");
    let branch_to_delete = if request.delete_branch {
        request
            .instance
            .worktree_info
            .as_ref()
            .filter(|wt| wt.managed_by_aoe)
            .map(|wt| (wt.branch.clone(), PathBuf::from(&wt.main_repo_path)))
    } else {
        None
    };
    if let Some((b, r)) = branch_to_delete.as_ref() {
        tracing::debug!(target: "session.delete", branch = %b, main_repo = %r.display(), "perform_deletion: branch_to_delete resolved");
    }

    // Branch cleanup is gated on the worktree actually being removed, not on
    // `request.delete_worktree`.
    let mut main_worktree_removed = false;
    // Keyed by worktree path, not repo name: two workspace repos can share a
    // name, and the path is what uniquely identifies the removed worktree.
    let mut removed_session_worktrees: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();

    if request.delete_worktree {
        if let Some(wt_info) = &request.instance.worktree_info {
            if wt_info.managed_by_aoe {
                let worktree_path = PathBuf::from(&request.instance.project_path);
                if !preserved_worktree_paths.contains(&worktree_path) {
                    let main_repo = PathBuf::from(&wt_info.main_repo_path);

                    match GitWorktree::new(main_repo.clone()) {
                        Ok(git_wt) => {
                            if let Err(errs) = remove_managed_worktree(
                                &git_wt,
                                &worktree_path,
                                &main_repo,
                                &request.instance,
                                request.force_delete,
                                request.delete_sandbox,
                            ) {
                                errors.extend(errs);
                            } else {
                                messages.push("Worktree removed".to_string());
                                main_worktree_removed = true;
                            }
                        }
                        Err(e) => {
                            errors.push(format!("Worktree: {}", e));
                        }
                    }
                }
            }
        }
    }

    // Per-repo worktree cleanup, for both creation-time workspace repos and repos attached later.
    if request.delete_worktree {
        for repo in repos {
            if !repo.managed_by_aoe {
                messages.push(format!(
                    "Workspace ({}) worktree preserved; aoe did not create it",
                    repo.name
                ));
                continue;
            }
            let worktree_path = PathBuf::from(&repo.worktree_path);
            if preserved_worktree_paths.contains(&worktree_path) {
                continue;
            }
            let main_repo = PathBuf::from(&repo.main_repo_path);
            match GitWorktree::new(main_repo.clone()) {
                Ok(git_wt) => {
                    match remove_managed_worktree(
                        &git_wt,
                        &worktree_path,
                        &main_repo,
                        &request.instance,
                        request.force_delete,
                        request.delete_sandbox,
                    ) {
                        Ok(()) => {
                            messages.push(format!("Workspace ({}) worktree removed", repo.name));
                            removed_session_worktrees.insert(worktree_path.clone());
                        }
                        Err(errs) => {
                            errors.extend(
                                errs.into_iter()
                                    .map(|e| format!("Workspace ({}): {}", repo.name, e)),
                            );
                        }
                    }
                }
                Err(e) => {
                    errors.push(format!("Workspace ({}): {}", repo.name, e));
                }
            }
        }

        if let Some(ws_info) = &request.instance.workspace_info {
            // Remove workspace parent directory only when no repo under it was preserved; otherwise
            // we'd nuke the user's uncommitted changes, or a default-branch checkout, through the
            // back door.
            if ws_info.cleanup_on_delete && !any_preserved {
                let ws_path = PathBuf::from(&ws_info.workspace_dir);
                // A record whose shape is not aoe-owned should never occur: it means workspace_dir
                // was mis-written (e.g. set to the user's own checkout).
                if !workspace_dir_is_aoe_owned(ws_info) {
                    tracing::warn!(target: "session.delete",
                        session_id = %request.session_id,
                        path = %ws_path.display(),
                        "perform_deletion: refusing to remove workspace dir, not aoe-owned"
                    );
                    errors.push(format!(
                        "Workspace dir: refusing to remove {}, it does not look like a \
                         directory aoe created",
                        ws_path.display()
                    ));
                } else if ws_path.exists() {
                    match std::fs::remove_dir(&ws_path) {
                        // Normally unreachable: prune_empty_parent_dirs, run after each worktree
                        // removal, already deletes the emptied workspace dir.
                        Ok(()) => messages.push("Workspace directory removed".to_string()),
                        // A non-empty dir still holds something that is not one of the managed
                        // worktrees: unrelated content under a mislaid record, or files written at
                        // the workspace root, which is the session's own cwd.
                        Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                            messages.push(format!(
                                "Workspace directory kept: {} is not empty, so it was not removed",
                                ws_path.display()
                            ));
                        }
                        Err(e) => errors.push(format!("Workspace dir: {}", e)),
                    }
                }
            }
        }
    }

    // Stage 5: branch cleanup (if user opted to delete it and worktree
    // was successfully removed).
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "branch_delete", "perform_deletion: stage");
    if let Some((branch, main_repo)) = branch_to_delete {
        tracing::debug!(target: "session.delete", branch = %branch, main_repo = %main_repo.display(), main_worktree_removed, "perform_deletion: attempting branch deletion");
        if main_worktree_removed {
            match GitWorktree::new(main_repo.clone()) {
                Ok(git_wt) => {
                    if let Err(e) = git_wt.delete_branch(&branch) {
                        tracing::debug!(target: "session.delete", branch = %branch, error = %e, "perform_deletion: delete_branch returned error");
                        errors.push(format!("Branch: {}", e));
                    } else {
                        messages.push(format!("Branch '{}' deleted", branch));
                    }
                }
                Err(e) => {
                    tracing::debug!(target: "session.delete", main_repo = %main_repo.display(), error = %e, "perform_deletion: GitWorktree::new failed");
                    errors.push(format!("Branch: {}", e));
                }
            }
        } else {
            tracing::debug!(target: "session.delete",
                "perform_deletion: skipping branch deletion (worktree preserved or not removed)"
            );
            messages.push(format!(
                "Branch '{}' kept; its worktree was preserved",
                branch
            ));
        }
    }

    if request.delete_branch {
        for repo in repos {
            // Branch ownership is tracked separately from worktree ownership: for a creation-time
            // workspace repo the two coincide, because the builder makes both, but attaching a repo
            // on a branch the user already had records `branch_preexisting = true`, and that branch
            // is not ours to delete however the worktree around it was created.
            if repo.branch_preexisting {
                // Silent for an unmanaged workspace repo before the merge; now it says so, which
                // matches what the attached path already reported and is the same reason the
                // worktree stage reports a preserve.
                messages.push(format!(
                    "Branch '{}' ({}) kept; aoe did not create it",
                    repo.branch, repo.name
                ));
                continue;
            }
            // Per-repo gate: only delete a repo's branch when that repo's worktree was actually
            // removed.
            if !removed_session_worktrees.contains(&PathBuf::from(&repo.worktree_path)) {
                messages.push(format!(
                    "Branch '{}' ({}) kept; its worktree was preserved",
                    repo.branch, repo.name
                ));
                continue;
            }
            let main_repo = PathBuf::from(&repo.main_repo_path);
            if let Ok(git_wt) = GitWorktree::new(main_repo) {
                if let Err(e) = git_wt.delete_branch(&repo.branch) {
                    errors.push(format!("Branch ({}): {}", repo.name, e));
                } else {
                    messages.push(format!("Branch '{}' ({}) deleted", repo.branch, repo.name));
                }
            }
        }
    }
}

fn stage_cleanup_scratch(
    request: &DeletionRequest,
    preserved_by_peer: bool,
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) {
    if preserved_by_peer {
        messages.push("Scratch directory kept; another session still uses it".to_string());
        return;
    }
    // Scratch directory cleanup.
    if request.instance.scratch {
        let path = PathBuf::from(&request.instance.project_path);
        // keep_scratch + tampered project_path used to surface "Scratch directory kept at: /etc"
        // which implied AoE was intentionally leaving a path it never owned.
        let guard_ok = path.exists() && super::scratch::is_scratch_path(&path);
        if request.keep_scratch && guard_ok {
            tracing::info!(
                target: "session.delete",
                session_id = %request.session_id,
                path = %path.display(),
                "keep-scratch opted in; leaving scratch directory on disk"
            );
            messages.push(format!("Scratch directory kept at: {}", path.display()));
        } else if request.keep_scratch {
            // Tampered or missing path with keep_scratch on: still nothing
            // to remove, but we cannot claim ownership of the path either.
            tracing::warn!(
                target: "session.delete",
                session_id = %request.session_id,
                path = %path.display(),
                "keep-scratch requested but project_path failed the guard or is missing"
            );
        } else if !path.exists() {
            // Already gone (user removed it manually, FS hiccup, prior partial cleanup).
            tracing::debug!(
                target: "session.delete",
                session_id = %request.session_id,
                path = %path.display(),
                "scratch dir already gone before deletion ran"
            );
        } else if super::scratch::is_scratch_path(&path) {
            tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "scratch_remove", "perform_deletion: stage");
            match std::fs::remove_dir_all(&path) {
                Ok(()) => messages.push("Scratch directory removed".to_string()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(target: "session.delete",
                        session_id = %request.session_id,
                        path = %path.display(),
                        "perform_deletion: scratch dir already gone, treating as success"
                    );
                }
                Err(e) => {
                    errors.push(format!("Scratch directory: {}", e));
                }
            }
        } else {
            // Tampered `project_path` (e.g. JSON edited by hand to claim `scratch: true` while
            // pointing outside the scratch root) is the only path that reaches this branch in
            // normal use.
            tracing::warn!(
                target: "session.delete",
                session_id = %request.session_id,
                path = %path.display(),
                "scratch flag set but project_path failed the guard; refusing to remove"
            );
            errors.push(format!(
                "Scratch directory: refused to remove {} (path failed scratch guard)",
                path.display()
            ));
        }
    }
}

/// Final stage: the session's own agent stores.
fn stage_remove_agent_stores(request: &DeletionRequest, messages: &mut Vec<String>) {
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "agent_store_remove", "perform_deletion: stage");
    match crate::session::sandbox_store_reclaim::remove_stores_for(&request.instance) {
        Ok((removed, _)) if removed.is_empty() => {}
        Ok((_, freed)) => messages.push(format!(
            "Agent store removed ({})",
            crate::migrations::progress::format_bytes(freed)
        )),
        Err(error) => {
            tracing::warn!(target: "session.store",
                "leaving the agent store of {}: {error}", request.session_id);
            messages.push(format!(
                "Agent store kept ({error}); `aoe sandbox reclaim` removes it later"
            ));
        }
    }
}

/// Map a container [`Teardown`](crate::containers::Teardown) outcome onto a deletion's user-facing
/// messages and errors.
fn deletion_messages_for(
    outcome: crate::containers::Teardown,
    messages: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    use crate::containers::Teardown;
    match outcome {
        Teardown::Removed => messages.push("Container removed".to_string()),
        Teardown::AlreadyGone => {}
        Teardown::Failed(e) => errors.push(format!("Container: {}", e)),
    }
}

/// Run on_destroy hooks for an instance.
fn run_on_destroy_hooks(instance: &Instance, detach: bool) {
    let profile = crate::session::config::effective_profile(&instance.source_profile);

    let project_path = Path::new(&instance.project_path);

    // Start with global+profile on_destroy hooks (implicitly trusted).
    let mut resolved_on_destroy =
        crate::session::config::profile_config::resolve_config_or_warn(&profile)
            .hooks
            .on_destroy;

    // Check if repo has trusted hooks that override. Only the hooks surface
    // matters here; untrusted project MCP must not suppress trusted hooks.
    match repo_config::check_repo_trust(project_path) {
        Ok(trust) if trust.hooks.needs_trust() => {
            tracing::warn!(target: "session.delete",
                "Repo hooks changed since last trust approval; skipping repo on_destroy hooks"
            );
        }
        Ok(trust) => {
            if let Some(hooks) = trust.hooks.trusted() {
                if !hooks.on_destroy.is_empty() {
                    resolved_on_destroy = hooks.on_destroy;
                }
            }
        }
        Err(_) => {}
    }

    if resolved_on_destroy.is_empty() {
        return;
    }

    tracing::info!(target: "session.delete", "Running on_destroy hooks for session {}", instance.id);

    let is_sandboxed = instance.sandbox_info.as_ref().is_some_and(|s| s.enabled);
    let hook_env = repo_config::lifecycle_env_vars(instance);

    // The caller controls detachment: TUI/web pass detach=true to avoid corrupting the rendered UI
    // (see issue); CLI passes detach=false so interactive prompts work.
    let errors = if is_sandboxed {
        if let Some(ref sandbox) = instance.sandbox_info {
            let workdir = instance.container_workdir();
            repo_config::execute_hooks_in_container_best_effort(
                &resolved_on_destroy,
                &sandbox.container_name,
                &workdir,
                detach,
                &hook_env,
            )
        } else {
            vec![]
        }
    } else {
        repo_config::execute_hooks_best_effort(
            &resolved_on_destroy,
            project_path,
            detach,
            &hook_env,
        )
    };

    if !errors.is_empty() {
        tracing::warn!(target: "session.delete",
            "on_destroy hooks had {} failure(s) for session {}",
            errors.len(),
            instance.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn scoped_unknown_preserves_every_path_without_blocking_unrelated_paths() -> Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = tempfile::tempdir()?;
        let peer = Storage::new_unwatched("uncertain-peer")?;
        let target = Storage::new_unwatched("target")?;
        let first = root.path().join("first");
        let second = root.path().join("second");
        let unrelated = root.path().join("unrelated");
        let mut row = Instance::new("peer", root.path().join("current").to_str().unwrap());
        row.try_acquire_lifecycle_reservation(
            LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        row.lifecycle_reservation.as_mut().unwrap().path_claims =
            crate::session::WorktreePathClaims::Unknown(Some(vec![first.clone(), second.clone()]));
        std::fs::write(
            peer.sessions_path(),
            serde_json::to_vec(&vec![row.clone()])?,
        )?;
        let claims = PathClaimIndex::load_for_writer(std::slice::from_ref(&target))?;
        let profile = claims.writer_profile(&target)?;
        claims.ensure_unclaimed(profile, "new-owner", std::slice::from_ref(&unrelated))?;
        for blocked in [first, second, root.path().to_path_buf()] {
            assert!(claims
                .ensure_unclaimed(profile, "new-owner", std::slice::from_ref(&blocked))
                .is_err());
        }
        let PathsInUse::Known(inventory) = paths_in_use_except(&[]) else {
            panic!("complete scoped Unknown has a usable exclusion inventory")
        };
        assert_eq!(inventory.pending.len(), 2);
        row.lifecycle_reservation.as_mut().unwrap().path_claims =
            crate::session::WorktreePathClaims::Unknown(None);
        std::fs::write(peer.sessions_path(), serde_json::to_vec(&vec![row])?)?;
        let claims = PathClaimIndex::load_for_writer(std::slice::from_ref(&target))?;
        let profile = claims.writer_profile(&target)?;
        assert!(claims
            .ensure_unclaimed(profile, "new-owner", &[unrelated])
            .is_err());
        assert!(matches!(paths_in_use_except(&[]), PathsInUse::Unknown(_)));
        Ok(())
    }

    use crate::containers::error::DockerError;
    use crate::containers::Teardown;
    use crate::session::test_support::{isolate_app_dir, isolate_app_dir_at};
    use crate::session::{SandboxInfo, WorkspaceInfo, WorkspaceRepo, WorktreeInfo};
    use serial_test::serial;

    fn request(instance: Instance) -> DeletionRequest {
        DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        }
    }

    fn sandbox_info(container_name: &str) -> SandboxInfo {
        SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "alpine".to_string(),
            container_name: container_name.to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        }
    }

    fn worktree_info(branch: &str, main_repo: &Path) -> WorktreeInfo {
        WorktreeInfo {
            branch: branch.to_string(),
            main_repo_path: main_repo.to_string_lossy().to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        }
    }

    fn workspace_repo(main_repo: &Path, worktree: &Path, branch: &str) -> WorkspaceRepo {
        WorkspaceRepo {
            name: main_repo
                .file_name()
                .map_or("repo".to_string(), |n| n.to_string_lossy().to_string()),
            source_path: main_repo.to_string_lossy().to_string(),
            branch: branch.to_string(),
            worktree_path: worktree.to_string_lossy().to_string(),
            main_repo_path: main_repo.to_string_lossy().to_string(),
            managed_by_aoe: true,
            branch_preexisting: false,
            base_branch: None,
            base_branch_override: None,
        }
    }

    fn workspace_info(workspace_dir: &Path, repos: Vec<WorkspaceRepo>) -> WorkspaceInfo {
        WorkspaceInfo {
            branch: repos
                .first()
                .map_or("feature/abc".to_string(), |repo| repo.branch.clone()),
            workspace_dir: workspace_dir.to_string_lossy().to_string(),
            repos,
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        }
    }

    #[test]
    #[serial_test::serial]
    fn fenced_geometry_writes_refuse_pending_peers_and_invalidated_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let target = Storage::new_unwatched("target").unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let original = Instance::new("target", temp.path().join("original").to_str().unwrap());
        target
            .update(|rows, _| {
                rows.push(original.clone());
                Ok(())
            })
            .unwrap();
        let mut prepared =
            Instance::new("pending peer", temp.path().join("future").to_str().unwrap());
        let _intent =
            crate::session::builder::CreationIntent::reserve(&peer, &mut prepared).unwrap();
        let peer_before = std::fs::read(peer.sessions_path()).unwrap();
        let _workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        let _identity = crate::session::acquire_session_identity_lock().unwrap();
        let mut index = PathClaimIndex::load(std::slice::from_ref(&target)).unwrap();
        assert!(target
            .update_with_claim_index_under_workspace_lock(&index, |rows, _| {
                rows[0].project_path = prepared.project_path.clone();
                Ok(())
            })
            .is_err());
        index.invalidate();
        assert!(target
            .update_with_claim_index_under_workspace_lock(&index, |rows, _| {
                rows[0].project_path = temp.path().join("unrelated").to_string_lossy().into_owned();
                Ok(())
            })
            .is_err());
        let persisted = target.load().unwrap();
        assert_eq!(persisted[0].id, original.id);
        assert_eq!(persisted[0].created_at, original.created_at);
        assert_eq!(persisted[0].project_path, original.project_path);
        assert_eq!(std::fs::read(peer.sessions_path()).unwrap(), peer_before);
    }

    #[test]
    #[serial_test::serial]
    fn raw_path_inventory_unions_duplicate_owners_without_decoding_metadata() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let _home = isolate_app_dir_at(temp.path());
        let target = Storage::new_unwatched("healthy")?;
        let peer = Storage::new_unwatched("opaque-peer")?;
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        let future = temp.path().join("future");
        let peer_document = serde_json::json!([
            {"id":"duplicate", "title":17,"project_path":first,"lifecycle_reservation":{"op":"launch","generation":"broken","at":false,"path_claims":{"state":"none"},"custodian":null}},
            {"id":"duplicate", "project_path":second,"lifecycle_reservation":{"op":"attach","path_claims":{"state":"pending","paths":[future]},"custodian":null}}
        ]);
        std::fs::write(peer.sessions_path(), serde_json::to_vec(&peer_document)?)?;
        let index = PathClaimIndex::load_for_writer(std::slice::from_ref(&target))?;
        let profile = index.writer_profile(&target)?;
        for path in [&first, &second, &future] {
            assert!(index
                .ensure_unclaimed(profile, "new-owner", std::slice::from_ref(path))
                .is_err());
        }
        let peer_profile = index.writer_profile(&peer)?;
        assert!(index
            .ensure_unclaimed(peer_profile, "duplicate", &[])
            .is_err());
        assert!(matches!(
            paths_in_use_except(&[SessionPathOwner {
                profile: "opaque-peer",
                session_id: "duplicate"
            }]),
            PathsInUse::Unknown(_)
        ));
        assert!(PathClaimIndex::load(std::slice::from_ref(&peer)).is_err());
        let mut prepared =
            Instance::new("healthy", temp.path().join("independent").to_str().unwrap());
        let original_peer = std::fs::read(peer.sessions_path())?;
        let intent = crate::session::builder::CreationIntent::reserve(&target, &mut prepared)?;
        assert_eq!(intent.session_id(), prepared.id);
        assert_eq!(
            target
                .load()?
                .into_iter()
                .find(|row| row.id == prepared.id)
                .unwrap()
                .lifecycle_reservation
                .unwrap()
                .op,
            LifecycleOperation::Create
        );
        assert_eq!(std::fs::read(peer.sessions_path())?, original_peer);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn indexed_claims_track_persisted_paths_by_physical_owner() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let target = Storage::new_unwatched("target").unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let mut row = Instance::new("target", temp.path().join("checkout").to_str().unwrap());
        let mut peer_row = row.clone();
        peer_row.project_path = temp
            .path()
            .join("peer-checkout")
            .to_string_lossy()
            .into_owned();
        target
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        peer.update(|rows, _| {
            rows.push(peer_row.clone());
            Ok(())
        })
        .unwrap();
        let _workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        let _identity = crate::session::acquire_session_identity_lock().unwrap();
        let mut index = PathClaimIndex::load(std::slice::from_ref(&target)).unwrap();
        let (_, profile, rows) = index.take_targets().pop().unwrap();
        assert_eq!(rows.len(), 1);
        index
            .ensure_unclaimed(profile, &row.id, &[PathBuf::from(&row.project_path)])
            .unwrap();
        assert!(
            index
                .ensure_unclaimed(profile, &row.id, &[PathBuf::from(&peer_row.project_path)])
                .is_err(),
            "same id in another physical profile remains a peer"
        );
        let previous = row.project_path.clone();
        row.project_path = temp.path().join("relocated").to_string_lossy().into_owned();
        row.pre_trash_project_path = Some(previous.clone());
        target
            .update_under_workspace_claim_lock(|rows, _| {
                rows[0] = row.clone();
                Ok(())
            })
            .unwrap();
        index.update(profile, &row);
        assert!(index
            .ensure_unclaimed(profile, "another", &[PathBuf::from(&row.project_path)])
            .is_err());
        assert!(index
            .ensure_unclaimed(profile, "another", &[PathBuf::from(previous)])
            .is_err());
        index.invalidate();
        assert!(index
            .ensure_unclaimed(profile, &row.id, &[temp.path().join("unused")])
            .is_err());
    }

    #[test]
    #[serial_test::serial]
    fn native_legacy_claim_guards_use_strict_symmetric_resolution() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let target = Storage::new_unwatched("target").unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let row = Instance::new("target", temp.path().join("owned").to_str().unwrap());
        target
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let parent = temp.path().join("parent");
        let child = parent.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&parent, &alias).unwrap();
        let dangling = temp.path().join("dangling");
        std::os::unix::fs::symlink(&parent, &dangling).unwrap();
        let looped = temp.path().join("loop");
        std::os::unix::fs::symlink(&parent, &looped).unwrap();
        let disappearing = temp.path().join("absent");
        std::fs::create_dir(&disappearing).unwrap();
        let _workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        let _identity = crate::session::acquire_session_identity_lock().unwrap();
        let owner = SessionPathOwner {
            profile: target.profile(),
            session_id: &row.id,
        };
        for (claim, candidate, conflict) in [
            (parent.clone(), child.clone(), true),
            (child.clone(), parent.clone(), true),
            (alias.clone(), child.clone(), true),
            (temp.path().join("missing/child"), child.clone(), false),
            (dangling.join("child"), child.clone(), true),
            (looped.clone(), child.clone(), true),
            (temp.path().join("absent/../other"), child, true),
        ] {
            peer.update_under_workspace_claim_lock(|rows, _| {
                *rows = vec![Instance::new("peer", claim.to_str().unwrap())];
                Ok(())
            })
            .unwrap();
            if claim.starts_with(&dangling) {
                std::fs::remove_file(&dangling).unwrap();
                std::os::unix::fs::symlink(temp.path().join("missing-target"), &dangling).unwrap();
            } else if claim == looped {
                std::fs::remove_file(&looped).unwrap();
                std::os::unix::fs::symlink(&looped, &looped).unwrap();
            } else if claim.starts_with(&disappearing) {
                std::fs::remove_dir(&disappearing).unwrap();
            }
            assert_eq!(
                ensure_unclaimed_paths(owner, std::slice::from_ref(&candidate)).is_err(),
                conflict,
                "claim {} candidate {}",
                claim.display(),
                candidate.display()
            );
            assert_eq!(paths_in_use_except(&[owner]).covers(&candidate), conflict);
            if claim.starts_with(&dangling) || claim == looped {
                let alias = if claim == looped { &looped } else { &dangling };
                std::fs::remove_file(alias).unwrap();
                std::os::unix::fs::symlink(&parent, alias).unwrap();
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn indexed_claim_deltas_keep_common_repos_and_release_removed_paths() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = Storage::new_unwatched("target").unwrap();
        let primary = temp.path().join("primary");
        let repository = temp.path().join("repository");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&repository).unwrap();
        let mut row = Instance::new("target", primary.to_str().unwrap());
        row.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "feature".into(),
            workspace_dir: primary.to_string_lossy().into_owned(),
            repos: vec![crate::session::WorkspaceRepo {
                name: "repository".into(),
                source_path: repository.to_string_lossy().into_owned(),
                branch: "feature".into(),
                worktree_path: repository.to_string_lossy().into_owned(),
                main_repo_path: repository.to_string_lossy().into_owned(),
                managed_by_aoe: false,
                branch_preexisting: true,
                base_branch: None,
                base_branch_override: None,
            }],
            created_at: chrono::Utc::now(),
            cleanup_on_delete: false,
        });
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let _workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        let _identity = crate::session::acquire_session_identity_lock().unwrap();
        let mut index = PathClaimIndex::load(std::slice::from_ref(&storage)).unwrap();
        let (_, profile, _) = index.take_targets().pop().unwrap();
        let relocated = temp.path().join("relocated");
        row.project_path = relocated.to_string_lossy().into_owned();
        row.workspace_info.as_mut().unwrap().workspace_dir = row.project_path.clone();
        row.pre_trash_project_path = Some(primary.to_string_lossy().into_owned());
        storage
            .update_under_workspace_claim_lock(|rows, _| {
                rows[0] = row.clone();
                Ok(())
            })
            .unwrap();
        index.update(profile, &row);
        for path in [&primary, &repository, &relocated] {
            assert!(index
                .ensure_unclaimed(profile, "peer", std::slice::from_ref(path))
                .is_err());
        }
        row.project_path = temp.path().join("current").to_string_lossy().into_owned();
        row.workspace_info.as_mut().unwrap().workspace_dir = row.project_path.clone();
        row.pre_trash_project_path = None;
        storage
            .update_under_workspace_claim_lock(|rows, _| {
                rows[0] = row.clone();
                Ok(())
            })
            .unwrap();
        index.update(profile, &row);
        for path in [&primary, &relocated] {
            index
                .ensure_unclaimed(profile, "peer", std::slice::from_ref(path))
                .unwrap();
        }
        assert!(index
            .ensure_unclaimed(profile, "peer", std::slice::from_ref(&repository))
            .is_err());
        assert!(index
            .ensure_unclaimed(profile, "peer", &[PathBuf::from(&row.project_path)])
            .is_err());
    }

    #[test]
    fn destructive_claims_resolve_absence_but_refuse_broken_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        std::fs::create_dir(&root).unwrap();
        let missing = temp.path().join("missing/child");
        assert!(!paths_overlap_destructive(&missing, &root));
        assert!(!paths_overlap_destructive(&root, &missing));
        assert!(paths_overlap_destructive(&root.join("missing"), &root));
        assert!(paths_overlap_destructive(&root, &root.join("missing")));
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&root, &alias).unwrap();
        assert!(paths_overlap_destructive(&alias.join("missing"), &root));
        let dangling = temp.path().join("dangling");
        std::os::unix::fs::symlink(temp.path().join("absent"), &dangling).unwrap();
        assert!(paths_overlap_destructive(&dangling.join("child"), &root));
        assert!(paths_overlap_destructive(
            &temp.path().join("absent/../other"),
            &root
        ));
        let looped = temp.path().join("loop");
        std::os::unix::fs::symlink(&looped, &looped).unwrap();
        assert!(paths_overlap_destructive(&looped, &root));
    }

    /// The mirror case: a candidate that does not exist yet still has to be
    /// caught lexically when it would land under a live peer parent, otherwise
    /// the post-create check would wave it through.
    #[test]
    fn missing_candidate_under_an_existing_peer_parent_still_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("peer");
        std::fs::create_dir_all(&parent).unwrap();
        let candidate = parent.join("not-created-yet");

        assert!(!candidate.exists());
        assert!(paths_overlap_destructive(&parent, &candidate));
    }

    /// A peer recorded through a symlink names the same directory as its
    /// resolved form. Testing only the recorded spelling let a candidate under
    /// the real path through, which is the normal macOS layout.
    #[test]
    fn missing_candidate_under_a_symlinked_peer_parent_still_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let peer = real.join("peer");
        std::fs::create_dir_all(&peer).unwrap();
        std::os::unix::fs::symlink(&real, temp.path().join("link")).unwrap();

        let recorded = temp.path().join("link").join("peer");
        let candidate = peer.join("not-created-yet");
        assert!(!candidate.exists());

        assert!(
            paths_overlap_destructive(&recorded, &candidate),
            "the recorded spelling and the candidate name different parents"
        );
    }

    /// The macOS shape: the candidate's own parent is reached through an alias,
    /// `/var` being `/private/var` there. Resolving only the peer cannot see it.
    #[test]
    fn missing_candidate_behind_its_own_alias_still_conflicts() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("private");
        let peer = base.join("real/peer");
        std::fs::create_dir_all(&peer).unwrap();
        std::os::unix::fs::symlink(&base, temp.path().join("var")).unwrap();

        let candidate = temp.path().join("var/real/peer/not-created-yet");
        assert!(!candidate.exists());

        assert!(
            paths_overlap_destructive(&peer, &candidate),
            "the candidate's parent is an alias of the peer's"
        );
    }

    /// Two profiles holding the same session id used to make the whole
    /// cross-profile inventory `Unknown`, so every path in every profile read
    /// as claimed. The id is not an ownership claim: only the recorded paths
    /// are, and they must still be enforced.
    #[test]
    #[serial_test::serial]
    fn duplicate_session_ids_do_not_poison_every_ownership_check() {
        let _app_guard = isolate_app_dir();
        for profile in ["dup-a", "dup-b"] {
            crate::session::create_profile(profile).unwrap();
            let storage = Storage::open_unwatched(profile).unwrap();
            let mut instance = Instance::new("Dup", &format!("/tmp/{profile}"));
            instance.id = "dup-shared-id".to_string();
            instance.source_profile = profile.to_string();
            storage
                .update(|instances, _groups| {
                    instances.push(instance);
                    Ok(())
                })
                .unwrap();
        }
        let caller = SessionPathOwner {
            profile: "dup-a",
            session_id: "unrelated-caller",
        };
        assert!(
            ensure_unclaimed_paths(caller, &[PathBuf::from("/tmp/dup-a")]).is_err(),
            "a candidate colliding with a recorded peer path is still refused"
        );
        assert!(
            ensure_unclaimed_paths(caller, &[PathBuf::from("/tmp/dup-b")]).is_err(),
            "the second profile's recorded path is enforced too"
        );
        assert!(
            ensure_unclaimed_paths(caller, &[PathBuf::from("/tmp/unrelated")]).is_ok(),
            "a duplicate id elsewhere must not make unrelated candidates look claimed"
        );
    }

    #[test]
    #[serial_test::serial]
    fn same_id_peer_in_another_profile_keeps_ownership() {
        let _home = isolate_app_dir();
        for profile in ["owner", "peer"] {
            let storage = Storage::new_unwatched(profile).unwrap();
            let mut instance = Instance::new(profile, &format!("/tmp/{profile}-same-id"));
            instance.id = "same-id".into();
            instance.source_profile = profile.into();
            instance.pre_trash_project_path = Some(format!("/tmp/{profile}-restore"));
            storage
                .update(|instances, _| {
                    instances.push(instance);
                    Ok(())
                })
                .unwrap();
        }
        std::os::unix::fs::symlink(
            "owner",
            crate::session::get_app_dir()
                .unwrap()
                .join("profiles/owner-alias"),
        )
        .unwrap();
        for profile in ["owner", "owner-alias"] {
            let caller = SessionPathOwner {
                profile,
                session_id: "same-id",
            };
            for claimed in ["/tmp/peer-same-id", "/tmp/peer-restore"] {
                assert!(
                    ensure_unclaimed_paths(caller, &[PathBuf::from(claimed)]).is_err(),
                    "peer retained: {profile}/{claimed}"
                );
            }
            for unclaimed in ["/tmp/owner-same-id", "/tmp/owner-restore", "/tmp/unrelated"] {
                assert!(
                    ensure_unclaimed_paths(caller, &[PathBuf::from(unclaimed)]).is_ok(),
                    "own/unrelated admitted: {profile}/{unclaimed}"
                );
            }
        }
        for profile in ["", "missing"] {
            assert!(ensure_unclaimed_paths(
                SessionPathOwner {
                    profile,
                    session_id: "same-id"
                },
                &[PathBuf::from("/tmp/unrelated")]
            )
            .is_err());
        }
    }

    /// The cs/cxa pattern: `profiles/<alias>` is a symlink to another profile.
    /// The inventory resolves it to the store it already scans, so an aliased
    /// installation can still delete and the aliased store's peers still claim
    /// their recorded paths.
    #[test]
    #[serial_test::serial]
    fn symlinked_profile_alias_does_not_poison_every_ownership_check() {
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(temp.path());
        crate::session::create_profile("default").unwrap();
        crate::session::create_profile("personal").unwrap();
        // The cs/cxa pattern: an alias symlink pointing at `default`.
        std::os::unix::fs::symlink(
            "default",
            crate::session::get_app_dir()
                .unwrap()
                .join("profiles/forit-work"),
        )
        .unwrap();
        store_peer_session("personal", "/tmp/aliased-peer");

        assert!(
            ensure_unclaimed_paths(
                SessionPathOwner {
                    profile: "forit-work",
                    session_id: "caller"
                },
                &[PathBuf::from("/tmp/aliased-peer")]
            )
            .is_err(),
            "a peer session in another profile still claims its path"
        );
        assert!(
            ensure_unclaimed_paths(
                SessionPathOwner {
                    profile: "forit-work",
                    session_id: "caller"
                },
                &[PathBuf::from("/tmp/unrelated")]
            )
            .is_ok(),
            "an alias for a profile already in the inventory must not make every path look claimed"
        );
    }

    #[test]
    #[serial_test::serial]
    fn external_profile_alias_preserves_peer_claims() {
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(temp.path());
        crate::session::create_profile("personal").unwrap();
        let profiles_dir = crate::session::get_app_dir().unwrap().join("profiles");
        std::os::unix::fs::symlink("no-such-profile", profiles_dir.join("dangling")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), profiles_dir.join("elsewhere")).unwrap();
        let claimed = temp.path().join("peer-checkout");
        store_peer_session("elsewhere", claimed.to_str().unwrap());
        let owner = SessionPathOwner {
            profile: "personal",
            session_id: "caller",
        };
        assert!(
            ensure_unclaimed_paths(owner, &[claimed]).is_err(),
            "a readable external store still owns its checkout"
        );
        let unrelated = temp.path().join("unrelated");
        assert!(ensure_unclaimed_paths(owner, std::slice::from_ref(&unrelated)).is_ok());
        let looped = profiles_dir.join("looped");
        std::os::unix::fs::symlink("looped", &looped).unwrap();
        assert!(
            ensure_unclaimed_paths(owner, std::slice::from_ref(&unrelated)).is_err(),
            "an unreadable alias cannot prove absence of peers"
        );
        std::fs::remove_file(looped).unwrap();
        std::fs::write(outside.path().join("sessions.json"), "{ invalid json").unwrap();
        assert!(
            ensure_unclaimed_paths(owner, &[unrelated]).is_err(),
            "corrupt external ownership must remain unknown"
        );
    }

    /// A session with no managed worktree, no workspace and no scratch destroys
    /// no path another session can own, so an inventory it cannot read must not
    /// refuse its deletion.
    #[test]
    #[serial_test::serial]
    fn a_session_with_nothing_to_protect_is_not_refused_on_an_unknown_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        let mut plain = Instance::new("Plain", temp.path().join("plain").to_str().unwrap());
        plain.source_profile = "owner".to_string();
        plain.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        storage
            .update(|instances, _groups| {
                instances.push(plain.clone());
                Ok(())
            })
            .unwrap();

        // A neighbouring profile the inventory cannot read.
        let other = Storage::new_unwatched("other").unwrap();
        other.update(|_, _| Ok(())).unwrap();
        std::fs::write(other.sessions_path(), "[{\"id\":1}]").unwrap();

        let transaction = match PurgeTransaction::reserve(
            storage,
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(plain)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("reservation refused: {result:?}"),
        };
        let result = transaction.complete();

        assert!(
            result.success,
            "nothing here can be owned, so an unreadable inventory must not refuse: {:?}",
            result.errors
        );
    }

    /// An unreadable profile owns a session, so the inventory cannot be known.
    /// `Path::exists()` answers `false` for a permission error just as it does
    /// for a missing file, which silently emptied the inventory and let a purge
    /// delete a peer's worktree.
    #[test]
    #[serial_test::serial]
    fn unreadable_profile_inventory_is_unknown_not_empty() {
        // Permission bits do not apply to root, so the setup could not
        // reproduce anything there.
        if nix::unistd::geteuid().is_root() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(temp.path());
        crate::session::create_profile("personal").unwrap();
        store_peer_session("personal", "/tmp/unreadable-peer");
        // Open before the profile goes unreadable, so the failure under test is
        // the inventory read and not opening the store.
        let storage = Storage::open_unwatched("personal").unwrap();
        let profile_dir = crate::session::get_app_dir()
            .unwrap()
            .join("profiles/personal");
        let mut perms = std::fs::metadata(&profile_dir).unwrap().permissions();
        perms.set_mode(0o000);
        std::fs::set_permissions(&profile_dir, perms).unwrap();

        let outcome = storage.load_strict_for_worktree_ownership_locked();
        let _ = std::fs::set_permissions(&profile_dir, std::fs::Permissions::from_mode(0o755));

        assert!(
            outcome.is_err(),
            "an unreadable inventory must fail closed rather than read as owning nothing"
        );
    }

    fn store_peer_session(profile: &str, project_path: &str) {
        let storage = Storage::open_unwatched(profile).unwrap();
        let mut instance = Instance::new("Peer", project_path);
        instance.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(instance);
                Ok(())
            })
            .unwrap();
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let repo = git2::Repository::init(path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        !git_in(repo, &["branch", "--list", branch]).is_empty()
    }

    /// A repo at `<tmp>/main` with a managed worktree for `branch` at `<tmp>/worktree`.
    fn worktree_fixture(branch: &str) -> (tempfile::TempDir, PathBuf, PathBuf, Instance) {
        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        let worktree_path = tmp.path().join("worktree");
        init_repo(&main_repo);
        let worktree = worktree_path.to_str().unwrap();
        git_in(&main_repo, &["worktree", "add", "-b", branch, worktree]);
        let mut instance = Instance::new("Test", worktree);
        instance.worktree_info = Some(worktree_info(branch, &main_repo));
        (tmp, main_repo, worktree_path, instance)
    }

    fn reserve(profile: &str, instance: Instance) -> PurgeTransaction {
        let storage = Storage::open_unwatched(profile).unwrap();
        match PurgeTransaction::reserve(storage, request(instance)).unwrap() {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge reservation was refused"),
        }
    }

    fn stored_instance(storage: &Storage, profile: &str, project: &str) -> Instance {
        let mut instance = Instance::new("purge", project);
        instance.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == instance.id)
            .unwrap()
    }

    #[test]
    #[serial]
    fn failed_purge_reports_release_failure_and_preserves_replacement() {
        let _guard = isolate_app_dir();
        let storage = Storage::new_unwatched("retention").unwrap();
        let instance = stored_instance(&storage, "retention", "/tmp/purge-retention");
        let mut transaction = reserve("retention", instance).release_locks_for_teardown();
        let profile = crate::session::get_profile_dir_path("retention").unwrap();
        let displaced = profile.with_file_name("displaced-retention");
        std::fs::rename(&profile, &displaced).unwrap();
        let replacement = Storage::new_unwatched("retention").unwrap();
        let replacement_row = stored_instance(&replacement, "retention", "/tmp/replacement");
        let before = serde_json::to_value(replacement.load().unwrap()).unwrap();

        let result = transaction.failed_after_release("original teardown refused");

        assert_eq!(result.disposition, DeletionDisposition::Failed);
        assert!(!result.success);
        assert!(result.retained_instance.is_none());
        assert!(result.retained_stop.is_none());
        assert!(result
            .errors
            .iter()
            .any(|error| error == "original teardown refused"));
        assert!(result
            .errors
            .iter()
            .any(|error| error.contains("failed to reopen target profile after destroy hooks")));
        assert_eq!(
            serde_json::to_value(replacement.load().unwrap()).unwrap(),
            before
        );
        assert_eq!(replacement.load().unwrap()[0].id, replacement_row.id);
    }

    #[tokio::test]
    #[serial]
    async fn drop_retains_unknown_history_and_changed_trash_lifecycles() {
        let _guard = isolate_app_dir();
        let artifact = tempfile::TempDir::new().unwrap();
        let sentinel = artifact.path().join("retained");
        std::fs::write(&sentinel, b"original artifact").unwrap();
        for (profile, change, expected) in [
            ("drop-unknown", 0, DeletionDisposition::Failed),
            ("drop-restored", 1, DeletionDisposition::KeptRestored),
            ("drop-retrashed", 2, DeletionDisposition::KeptRestored),
        ] {
            let storage = Storage::new_unwatched(profile).unwrap();
            let original = stored_instance(&storage, profile, artifact.path().to_str().unwrap());
            storage
                .update(|instances, _| {
                    instances[0].trash();
                    instances[0].runner_journal = Default::default();
                    Ok(())
                })
                .unwrap();
            let requested = storage.load().unwrap().remove(0);
            storage
                .update(|instances, _| {
                    match change {
                        1 => instances[0].trashed_at = None,
                        2 => {
                            instances[0].trashed_at = requested
                                .trashed_at
                                .map(|at| at + chrono::Duration::microseconds(1))
                        }
                        _ => {}
                    }
                    Ok(())
                })
                .unwrap();
            let before = storage.load().unwrap().remove(0);
            let result = execute_drop(requested, None).await;
            assert_eq!(result.disposition, expected, "{profile}");
            if change == 0 {
                assert!(result.retained_release_matches(&before));
                assert!(result.retained_release_matches(result.retained_instance.as_ref().unwrap()));
                for field in ["dob", "plan", "trash", "generation", "profile"] {
                    let mut replaced = before.clone();
                    match field {
                        "dob" => replaced.created_at += chrono::Duration::microseconds(1),
                        "plan" => replaced.title.push_str(" changed"),
                        "trash" => {
                            replaced.trashed_at = replaced
                                .trashed_at
                                .map(|at| at + chrono::Duration::microseconds(1))
                        }
                        "generation" => {
                            replaced.lifecycle_generation = result
                                .retained_instance
                                .as_ref()
                                .unwrap()
                                .lifecycle_generation
                                + 1
                        }
                        "profile" => {
                            replaced.storage_origin = Some(std::sync::Arc::new(
                                Storage::new_unwatched("replacement-root").unwrap(),
                            ))
                        }
                        _ => unreachable!(),
                    }
                    assert!(!result.retained_release_matches(&replaced), "{field}");
                }
            }
            let retained = storage.load().unwrap().remove(0);
            assert_eq!(
                (retained.id, retained.created_at),
                (original.id, original.created_at)
            );
            assert_eq!(retained.trashed_at, before.trashed_at);
            assert!(retained.lifecycle_reservation.is_none());
            assert_eq!(std::fs::read(&sentinel).unwrap(), b"original artifact");
        }
    }

    /// A force chosen against an earlier trash lifecycle is dropped at the claim, so a row
    /// a peer restored and re-trashed keeps its dirty-worktree guard.
    #[test]
    #[serial]
    fn purge_claim_drops_force_from_a_stale_trash_lifecycle() {
        let _guard = isolate_app_dir();
        let earlier = chrono::Utc::now() - chrono::Duration::minutes(5);
        // (profile, lifecycle the request was chosen against matches the stored one)
        for (profile, same_lifecycle) in [("purge-stale-force", false), ("purge-same-force", true)]
        {
            let storage = Storage::new_unwatched(profile).unwrap();
            let mut requested = stored_instance(&storage, profile, "/tmp/test-project");
            let trashed_at = chrono::Utc::now();
            storage
                .update(|instances, _groups| {
                    instances[0].trashed_at = Some(trashed_at);
                    Ok(())
                })
                .unwrap();
            requested.trashed_at = Some(if same_lifecycle { trashed_at } else { earlier });
            let transaction = match PurgeTransaction::reserve(
                storage,
                DeletionRequest {
                    force_delete: true,
                    ..request(requested)
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("purge reservation was refused"),
            };
            assert_eq!(
                transaction.request.force_delete, same_lifecycle,
                "{profile}"
            );
        }
    }

    #[test]
    #[serial]
    fn purge_transaction_generation_gate_and_durable_commit() {
        let _guard = isolate_app_dir();
        let profile = "purge-generation-gate";
        let storage = Storage::new_unwatched(profile).unwrap();
        let transaction = reserve(
            profile,
            stored_instance(&storage, profile, "/tmp/test-project"),
        )
        .release_locks_for_teardown();
        storage
            .update(|instances, _groups| {
                instances[0].lifecycle_generation += 1;
                Ok(())
            })
            .unwrap();

        let Err(result) = transaction.begin_irreversible() else {
            panic!("superseded purge crossed the irreversible boundary");
        };
        assert_eq!(result.disposition, DeletionDisposition::Busy);
        assert!(!result.teardown_started);
        let retained = storage.load().unwrap();
        assert_eq!(retained.len(), 1);
        assert!(!retained[0].has_active_lifecycle_reservation(Utc::now()));

        let mut retry = retained.into_iter().next().unwrap();
        retry.source_profile = profile.to_string();
        let Ok(committed) = reserve(profile, retry).begin_irreversible() else {
            panic!("current purge reservation was rejected");
        };
        assert!(
            storage.load().unwrap().is_empty(),
            "durable row must be gone before irreversible cleanup starts"
        );
        assert_eq!(committed.finish().disposition, DeletionDisposition::Removed);
    }

    #[test]
    #[serial]
    fn on_destroy_hooks_run_without_the_instance_lifecycle_flock() {
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(temp.path());
        let profile = "purge-unlocked-hooks";
        let storage = Storage::new_unwatched(profile).unwrap();
        let instance = stored_instance(&storage, profile, temp.path().to_str().unwrap());
        let id = instance.id.clone();
        let transaction = reserve(profile, instance);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let purge = std::thread::spawn(move || {
            transaction
                .run_hooks_with(|_, _| {
                    ready_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                })
                .complete()
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("on_destroy hook did not start");

        let (lock_tx, lock_rx) = std::sync::mpsc::channel();
        let lock = std::thread::spawn(move || {
            let storage = Storage::open_unwatched(profile).unwrap();
            drop(storage.acquire_instance_lifecycle_lock(&id).unwrap());
            lock_tx.send(()).unwrap();
        });
        let acquired = lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok();
        release_tx.send(()).unwrap();

        let result = purge.join().unwrap();
        lock.join().unwrap();
        assert!(acquired, "on_destroy hook held the lifecycle flock");
        assert_eq!(result.disposition, DeletionDisposition::Removed);
        assert!(storage.load().unwrap().is_empty());
    }
    #[test]
    #[serial]
    fn purge_rechecks_an_inventory_corrupted_while_hooks_are_unlocked() {
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(temp.path());
        let checkout = temp.path().join("checkout");
        std::fs::create_dir(&checkout).unwrap();
        let sentinel = checkout.join("keep");
        std::fs::write(&sentinel, "uncommitted content").unwrap();
        let storage = Storage::new_unwatched("owner").unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        peer.update(|_, _| Ok(())).unwrap();
        let mut instance = Instance::new("never-launched", checkout.to_str().unwrap());
        instance.source_profile = "owner".into();
        instance.worktree_info = Some(worktree_info("feature/rescan", temp.path()));
        let id = instance.id.clone();
        storage
            .update(|rows, _| {
                rows.push(instance);
                Ok(())
            })
            .unwrap();
        let instance = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                ..request(instance)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction.preflight_ownership().unwrap(),
            PurgeReservation::Rejected(_) => {
                panic!("never-launched row was refused before preflight")
            }
        };
        let transaction = transaction.run_hooks_with(|_, _| {
            std::fs::write(peer.sessions_path(), "not valid JSON").unwrap();
        });
        let Err(result) = transaction.begin_irreversible() else {
            panic!("unreadable peer inventory crossed the row-removal boundary");
        };
        assert_eq!(result.disposition, DeletionDisposition::Failed);
        assert!(!result.teardown_started);
        assert!(storage.load().unwrap().iter().any(|row| row.id == id));
        assert_eq!(
            std::fs::read_to_string(sentinel).unwrap(),
            "uncommitted content"
        );
    }

    #[test]
    #[serial]
    fn purge_rescans_a_peer_changed_while_waiting_for_its_storage_flock() {
        let (tmp, main_repo, worktree, mut owner) = worktree_fixture("feature/rescan");
        let _home = isolate_app_dir_at(&tmp.path().join("home"));
        owner.source_profile = "owner".into();
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let initial_path = tmp.path().join("initial-peer");
        std::fs::create_dir(&initial_path).unwrap();
        let mut adopter = Instance::new("adopter", initial_path.to_str().unwrap());
        peer.update(|rows, _| {
            rows.push(adopter.clone());
            Ok(())
        })
        .unwrap();
        let sentinel = worktree.join("peer-evidence");
        std::fs::write(&sentinel, "keep").unwrap();
        let (committed_tx, committed_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let purge = std::thread::spawn(move || {
            let transaction = match PurgeTransaction::reserve(
                storage,
                DeletionRequest {
                    delete_worktree: true,
                    delete_branch: true,
                    force_delete: true,
                    ..request(owner)
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("initial inventory must allow purge"),
            };
            let transaction = transaction.begin_irreversible().unwrap();
            committed_tx.send(()).unwrap();
            continue_rx.recv().unwrap();
            let _observer = crate::session::storage::observe_lock_contention_for_test(event_tx);
            transaction.finish()
        });
        committed_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("purge commits before the peer lock is held");
        let held = crate::session::storage::acquire_storage_flock(
            peer.sessions_path().parent().unwrap(),
            crate::session::storage::STORAGE_LOCK_FILENAME,
        )
        .unwrap();
        continue_tx.send(()).unwrap();
        let contended = event_rx.recv_timeout(std::time::Duration::from_secs(3));
        adopter.project_path = worktree.to_str().unwrap().into();
        crate::session::storage::atomic_write(
            peer.sessions_path(),
            &serde_json::to_vec(&[adopter]).unwrap(),
        )
        .unwrap();
        drop(held);
        let result = purge.join().unwrap();
        assert!(result.success, "{:?}", result.errors);
        contended.expect("purge must actually contend before the peer commits its new path");
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "keep");
        assert!(branch_exists(&main_repo, "feature/rescan"));
        assert_eq!(
            peer.load().unwrap()[0].project_path,
            worktree.to_str().unwrap()
        );
    }

    /// A purge retains a checkout when ownership is unknown or claimed after the scan.
    #[test]
    #[serial]
    fn purge_keeps_a_worktree_it_cannot_prove_unused() {
        for adopt_after_scan in [false, true] {
            let (tmp, main_repo, worktree, mut owner) = worktree_fixture("feature/shared");
            let _home = isolate_app_dir_at(&tmp.path().join("home"));
            let storage = Storage::new_unwatched("owner").unwrap();
            owner.source_profile = "owner".to_string();
            owner.storage_origin = Some(std::sync::Arc::new(storage.clone()));
            storage
                .update(|instances, _groups| {
                    instances.push(owner.clone());
                    Ok(())
                })
                .unwrap();
            let other = Storage::new_unwatched("other").unwrap();
            other.update(|_, _| Ok(())).unwrap();
            let adopter = Instance::new("adopter", worktree.to_str().unwrap());

            let writer = if adopt_after_scan {
                let (event_tx, event_rx) = std::sync::mpsc::channel();
                let (writer_tx, writer_rx) = std::sync::mpsc::channel();
                let worktree = worktree.clone();
                AFTER_PATHS_IN_USE_SCAN.with(|slot| {
                    *slot.borrow_mut() = Some(Box::new(move || {
                        writer_tx
                            .send(std::thread::spawn(move || {
                                let _observer =
                                    crate::session::storage::observe_lock_contention_for_test(
                                        event_tx.clone(),
                                    );
                                let mut present = false;
                                other
                                    .update(|instances, _groups| {
                                        present = worktree.exists();
                                        instances.push(adopter);
                                        Ok(())
                                    })
                                    .unwrap();
                                let _ = event_tx.send(PathBuf::new());
                                present
                            }))
                            .unwrap();
                        // Resume once the adoption either committed or is blocked on its lock.
                        event_rx.recv().unwrap();
                    }));
                });
                Some(writer_rx)
            } else {
                other
                    .update(|instances, _groups| {
                        instances.push(adopter);
                        Ok(())
                    })
                    .unwrap();
                std::fs::write(other.sessions_path(), "[{\"id\":1}]").unwrap();
                None
            };

            let transaction = match PurgeTransaction::reserve(
                storage,
                DeletionRequest {
                    delete_worktree: true,
                    delete_branch: true,
                    ..request(owner)
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("purge reservation was refused"),
            };
            let result = transaction.complete();
            if writer.is_some() {
                assert_eq!(result.disposition, DeletionDisposition::Removed);
            } else {
                assert_eq!(result.disposition, DeletionDisposition::Failed);
                assert!(!result.success);
            }

            if let Some(writer) = writer {
                let adopted_while_present = writer.recv().unwrap().join().unwrap();
                assert!(
                    !adopted_while_present || worktree.exists(),
                    "a worktree adopted after the scan was removed"
                );
            } else {
                assert!(
                    worktree.exists(),
                    "worktree removed despite unreadable profile"
                );
                assert!(branch_exists(&main_repo, "feature/shared"));
            }
        }
    }

    #[test]
    #[serial]
    fn deletion_unknown_ownership_stops_before_teardown() {
        let (tmp, main_repo, worktree, mut owner) = worktree_fixture("feature/unknown-owner");
        let _home = isolate_app_dir_at(&tmp.path().join("home"));
        let owner_storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".to_string();
        owner_storage
            .update(|instances, _groups| {
                instances.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let other = Storage::new_unwatched("other").unwrap();
        other
            .update(|instances, _groups| {
                instances.push(owner.clone());
                Ok(())
            })
            .unwrap();
        // The inventory is unverifiable because a peer profile cannot be read
        // at all, which is the surviving way to reach an `Unknown` verdict.
        std::fs::write(other.sessions_path(), b"{ not json").unwrap();
        let hook_dir = crate::hooks::hook_status_dir(&owner.id).unwrap();
        std::fs::create_dir_all(&hook_dir).unwrap();
        std::fs::write(hook_dir.join("sentinel"), b"keep").unwrap();

        let called = std::cell::Cell::new(false);
        let result = perform_deletion_with_lifecycle_locked(
            &DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: true,
                ..request(owner.clone())
            },
            |_id| {
                called.set(true);
                crate::containers::Teardown::Removed
            },
        );
        assert!(!result.success);
        assert!(!result.teardown_started);
        assert!(!called.get(), "container teardown must not start");
        assert!(worktree.exists());
        assert!(branch_exists(&main_repo, "feature/unknown-owner"));
        assert!(owner_storage
            .load()
            .unwrap()
            .iter()
            .any(|row| row.id == owner.id));
        assert!(
            hook_dir.exists(),
            "unknown ownership must skip hook status cleanup"
        );
        crate::hooks::cleanup_hook_status_dir(&owner.id);
    }

    #[test]
    #[serial]
    fn purge_preflight_rejects_unknown_inventory_before_hooks() {
        let (tmp, _main_repo, _worktree, mut owner) = worktree_fixture("feature/preflight");
        let _home = isolate_app_dir_at(&tmp.path().join("home"));
        let owner_storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".to_string();
        owner.storage_origin = Some(std::sync::Arc::new(owner_storage.clone()));
        owner_storage
            .update(|instances, _groups| {
                instances.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let other = Storage::new_unwatched("other").unwrap();
        other
            .update(|instances, _groups| {
                instances.push(owner.clone());
                Ok(())
            })
            .unwrap();
        // The inventory is unverifiable because a peer profile cannot be read
        // at all, which is the surviving way to reach an `Unknown` verdict.
        std::fs::write(other.sessions_path(), b"{ not json").unwrap();
        let transaction = match PurgeTransaction::reserve(
            owner_storage,
            DeletionRequest {
                delete_worktree: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("unexpected rejection: {result:?}"),
        };
        let Err(result) = transaction.preflight_ownership() else {
            panic!("unknown inventory must refuse the purge before hooks");
        };
        assert!(!result.success);
        assert!(result
            .errors
            .iter()
            .any(|error| error.contains("could not prove")));
        let stored = Storage::open_unwatched("owner").unwrap().load().unwrap();
        assert_eq!(stored.len(), 1);
        assert!(stored[0].lifecycle_reservation.is_none());
    }

    #[test]
    #[serial]
    fn profile_creation_publication_waits_for_each_inventory_fence() {
        let _home = isolate_app_dir();
        for identity in [true, false] {
            let name = if identity {
                "identity-peer"
            } else {
                "namespace-peer"
            };
            let path = crate::session::get_profile_dir_path(name).unwrap();
            let held = if identity {
                crate::session::acquire_session_identity_lock().unwrap()
            } else {
                crate::session::storage::acquire_profile_namespace_lock().unwrap()
            };
            let (event_tx, event_rx) = std::sync::mpsc::channel();
            let create = std::thread::spawn(move || {
                let _observer =
                    crate::session::storage::observe_lock_contention_for_test(event_tx.clone());
                let result = crate::session::create_profile(name);
                let _ = event_tx.send(PathBuf::new());
                result
            });
            let observation = event_rx.recv_timeout(std::time::Duration::from_secs(3));
            let absent_while_fenced = !path.exists();
            drop(held);
            create.join().unwrap().unwrap();
            observation.expect("creation must reach a held fence or finish within the deadline");
            assert!(
                absent_while_fenced,
                "a profile appeared while its inventory fence was held"
            );
            assert!(path.is_dir());
        }
    }

    #[test]
    fn workspace_dir_ownership() {
        let owned = |dir: &str, worktrees: &[&str]| {
            let repos = worktrees
                .iter()
                .map(|wt| workspace_repo(Path::new("/src/repo"), Path::new(wt), "feature/abc"))
                .collect();
            workspace_dir_is_aoe_owned(&workspace_info(Path::new(dir), repos))
        };
        assert!(owned("/tmp/ws", &["/tmp/ws/backend", "/tmp/ws/frontend"]));
        // `workspace_dir` IS the user's checkout rather than a directory above it.
        assert!(!owned("/home/u/backend", &["/home/u/backend"]));
        assert!(!owned(
            "/tmp/ws",
            &["/tmp/ws/backend", "/elsewhere/frontend"]
        ));
        assert!(!owned("/tmp/ws", &[]));
    }

    mod container_removal {
        use super::*;

        fn sandboxed_request() -> DeletionRequest {
            let mut instance = Instance::new("Test Session", "/tmp/test-project");
            instance.sandbox_info = Some(sandbox_info("aoe-sandbox-calltest"));
            DeletionRequest {
                delete_sandbox: true,
                ..request(instance)
            }
        }

        #[test]
        fn call_site_always_invokes_teardown_and_surfaces_failure() {
            let request = sandboxed_request();
            let called = std::cell::Cell::new(false);
            let result = perform_deletion_with(&request, |_id| {
                called.set(true);
                Teardown::Removed
            });
            assert!(called.get(), "teardown must never be gated behind a probe");
            assert!(result.success);

            let result = perform_deletion_with(&request, |_id| {
                Teardown::Failed(DockerError::RemoveFailed("daemon busy".into()))
            });
            assert!(!result.success, "a teardown failure must fail the deletion");
            assert!(result.errors.iter().any(|e| e.contains("Container")));
        }

        /// The agent store goes only when a current session's purge fully succeeds.
        #[test]
        #[serial]
        fn agent_store_removal() {
            #[derive(Clone, Copy, Debug, PartialEq)]
            enum Case {
                Removed,
                PreTransition,
                FailedTeardown,
                FailsAfterTeardown,
            }
            for case in [
                Case::Removed,
                Case::PreTransition,
                Case::FailedTeardown,
                Case::FailsAfterTeardown,
            ] {
                let temp = tempfile::TempDir::new().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let mut request = sandboxed_request();
                if case == Case::PreTransition {
                    request.instance.sandbox_store_generation = 0;
                }
                if case == Case::FailsAfterTeardown {
                    request.delete_worktree = true;
                    request.instance.worktree_info =
                        Some(worktree_info("feature/x", &temp.path().join("not-a-repo")));
                }
                let store = temp
                    .path()
                    .join(".claude/sandbox-v2")
                    .join(&request.instance.id);
                std::fs::create_dir_all(&store).unwrap();
                std::fs::write(store.join(".credentials.json"), b"token").unwrap();
                if case != Case::PreTransition {
                    crate::migrations::v033_isolate_sandbox_content::certify_owned_test_root(
                        &crate::session::get_app_dir().unwrap(),
                        &request.instance.id,
                        &store,
                    )
                    .unwrap();
                }

                let result = perform_deletion_with(&request, |_id| match case {
                    Case::FailedTeardown => Teardown::Failed(DockerError::DaemonNotRunning),
                    _ => Teardown::Removed,
                });

                if case == Case::Removed {
                    assert!(!store.exists(), "purge left the session's agent store");
                    assert!(result.success, "{:?}", result.errors);
                    assert!(result.messages.iter().any(|m| m.contains("Agent store")));
                } else {
                    assert!(store.exists(), "{case:?}: {:?}", result.errors);
                    assert!(case == Case::PreTransition || !result.success, "{case:?}");
                }
            }
        }
        fn fail(_: &str) -> Teardown {
            Teardown::Failed(DockerError::RemoveFailed("busy".into()))
        }
        fn ok(_: &str) -> Teardown {
            Teardown::Removed
        }
        #[test]
        #[serial]
        fn transcript_callback_runs_only_after_successful_teardown() {
            for (teardown, expect_purged) in [
                (fail as fn(&str) -> Teardown, false),
                (ok as fn(&str) -> Teardown, true),
            ] {
                let temp = tempfile::TempDir::new().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let profile = "purge-transcript-ordering";
                let storage = Storage::new_unwatched(profile).unwrap();
                let mut instance = stored_instance(&storage, profile, "/tmp/test-project");
                instance.sandbox_info = Some(sandbox_info("aoe-sandbox-ordering"));
                storage
                    .update(|instances, _| {
                        *instances = vec![instance.clone()];
                        Ok(())
                    })
                    .unwrap();
                let storage = Storage::open_unwatched(profile).unwrap();
                let transaction = match PurgeTransaction::reserve(
                    storage,
                    DeletionRequest {
                        delete_sandbox: true,
                        ..request(instance.clone())
                    },
                )
                .unwrap()
                {
                    PurgeReservation::Reserved(transaction) => transaction,
                    PurgeReservation::Rejected(result) => {
                        panic!("purge reservation was refused: {:?}", result.errors)
                    }
                };
                let purged = std::cell::Cell::new(false);
                let result = transaction.complete_with_test_teardown(
                    |_| {
                        purged.set(true);
                        Ok(())
                    },
                    teardown,
                );
                assert_eq!(purged.get(), expect_purged, "result={result:?}");
            }
        }
    }

    mod ordering {
        use super::*;
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::subscriber::with_default;
        use tracing::Subscriber;
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;
        use tracing_subscriber::Layer;

        struct StageRecorder {
            stages: Arc<Mutex<Vec<String>>>,
        }

        impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for StageRecorder {
            fn register_callsite(
                &self,
                _meta: &'static tracing::Metadata<'static>,
            ) -> tracing::subscriber::Interest {
                tracing::subscriber::Interest::always()
            }

            fn enabled(&self, _meta: &tracing::Metadata<'_>, _ctx: Context<'_, S>) -> bool {
                true
            }

            fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
                Some(tracing::level_filters::LevelFilter::TRACE)
            }

            fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
                #[derive(Default)]
                struct V {
                    msg: Option<String>,
                    stage: Option<String>,
                }
                impl Visit for V {
                    fn record_str(&mut self, field: &Field, value: &str) {
                        self.record_debug(field, &value);
                    }
                    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                        let value = format!("{value:?}").trim_matches('"').to_string();
                        match field.name() {
                            "stage" => self.stage = Some(value),
                            "message" => self.msg = Some(value),
                            _ => {}
                        }
                    }
                }
                let mut v = V::default();
                event.record(&mut v);
                if v.msg.as_deref() == Some("perform_deletion: stage") {
                    self.stages.lock().unwrap().extend(v.stage);
                }
            }
        }

        fn stages_of(request: &DeletionRequest) -> (Vec<String>, DeletionResult) {
            let stages = Arc::new(Mutex::new(Vec::new()));
            let subscriber = tracing_subscriber::registry().with(StageRecorder {
                stages: Arc::clone(&stages),
            });
            let result = with_default(subscriber, || {
                tracing::callsite::rebuild_interest_cache();
                perform_deletion(request)
            });
            let stages = stages.lock().unwrap().clone();
            (stages, result)
        }

        fn idx(stages: &[String], needle: &str) -> usize {
            stages
                .iter()
                .position(|s| s == needle)
                .unwrap_or_else(|| panic!("stage {needle:?} missing from {stages:?}"))
        }

        // Regression: the container must be dropped before the worktree directory is touched.
        #[test]
        fn sandboxed_with_worktree_kills_tmux_and_container_before_worktree() {
            let _app_guard = isolate_app_dir();
            let mut instance = Instance::new("Test", "/tmp/aoe-deletion-test-nonexistent");
            instance.sandbox_info = Some(sandbox_info("aoe-sandbox-doesnotexist"));
            let (stages, _) = stages_of(&DeletionRequest {
                delete_worktree: true,
                delete_sandbox: true,
                ..request(instance)
            });
            let order = [
                "tmux_kill",
                "sandbox_worktree_preclean",
                "container_remove",
                "worktree_remove",
                "branch_delete",
            ]
            .map(|stage| idx(&stages, stage));
            assert!(order.is_sorted(), "stages={stages:?}");
        }

        #[test]
        fn real_worktree_and_branch_are_removed_idempotently() {
            let _app_guard = isolate_app_dir();
            let (_tmp, main_repo, worktree_path, instance) = worktree_fixture("feature/delete-me");
            let request = DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(instance)
            };
            for _ in 0..2 {
                let result = perform_deletion(&request);
                assert!(
                    result.success,
                    "perform_deletion failed: {:?}",
                    result.errors
                );
                assert!(!worktree_path.exists());
                assert!(!main_repo.join(".git/worktrees/worktree").exists());
                assert!(!branch_exists(&main_repo, "feature/delete-me"));
            }
        }

        // Regression: the bare-repo layout checks out the default branch as a linked worktree.
        #[test]
        fn default_branch_worktree_survives_a_forced_delete() {
            let _app_guard = isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let bare = tmp.path().join("project/.bare");
            let worktree_path = tmp.path().join("project/main");
            std::fs::create_dir_all(&bare).unwrap();

            let repo = git2::Repository::init_bare(&bare).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = {
                let blob = repo.blob(b"hello").unwrap();
                let mut tb = repo.treebuilder(None).unwrap();
                tb.insert("file.txt", blob, 0o100644).unwrap();
                tb.write().unwrap()
            };
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
                .unwrap();
            repo.set_head("refs/heads/main").unwrap();
            let worktree = worktree_path.to_str().unwrap();
            git_in(&bare, &["worktree", "add", worktree, "main"]);

            let mut instance = Instance::new("Infra", worktree);
            instance.worktree_info = Some(worktree_info("main", &bare));
            let result = perform_deletion(&DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                force_delete: true,
                ..request(instance)
            });

            assert!(
                result.success,
                "deletion must still succeed: {:?}",
                result.errors
            );
            assert!(
                result
                    .messages
                    .iter()
                    .any(|m| m.contains("default branch of its repository")),
                "preservation must be reported: {:?}",
                result.messages
            );
            assert!(worktree_path.exists());
            assert!(branch_exists(&bare, "main"));
            assert_eq!(git_in(&bare, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        }

        // The ownership guard must be consulted at the call site and its refusal surfaced.
        #[test]
        fn workspace_dir_that_is_not_aoe_owned_is_refused() {
            let _app_guard = isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let user_checkout = tmp.path().join("backend");
            init_repo(&user_checkout);
            let precious = user_checkout.join("uncommitted.txt");
            std::fs::write(&precious, "do not delete me").unwrap();

            let mut instance = Instance::new("Bad", user_checkout.to_str().unwrap());
            let mut repo = workspace_repo(&user_checkout, &user_checkout, "feature/abc");
            repo.managed_by_aoe = false;
            instance.workspace_info = Some(workspace_info(&user_checkout, vec![repo]));
            let result = perform_deletion(&DeletionRequest {
                delete_worktree: true,
                ..request(instance)
            });

            assert_eq!(
                std::fs::read_to_string(&precious).unwrap(),
                "do not delete me"
            );
            assert!(
                result
                    .errors
                    .iter()
                    .any(|e| e.contains("does not look like a directory aoe created")),
                "the refusal should be surfaced, not silent: {:?}",
                result.errors
            );
        }

        /// A workspace dir holding content aoe did not put there is kept, not wiped, and the
        /// purge still succeeds.
        #[test]
        fn workspace_dir_with_foreign_content_is_kept_not_failed() {
            let _app_guard = isolate_app_dir();
            for corrupt_ancestor in [true, false] {
                let tmp = tempfile::TempDir::new().unwrap();
                let main_repo = tmp.path().join("frontend");
                init_repo(&main_repo);
                let (workspace, worktree) = if corrupt_ancestor {
                    (tmp.path().to_path_buf(), main_repo.clone())
                } else {
                    let workspace = tmp.path().join("ws");
                    let worktree = workspace.join("frontend");
                    std::fs::create_dir_all(&workspace).unwrap();
                    let path = worktree.to_str().unwrap();
                    git_in(
                        &main_repo,
                        &["worktree", "add", "-b", "feature/ws-del", path, "HEAD"],
                    );
                    (workspace, worktree)
                };
                let stray = workspace.join("stray.txt");
                std::fs::write(&stray, "keep me").unwrap();

                let mut instance = Instance::new("Workspace", workspace.to_str().unwrap());
                let mut repo = workspace_repo(&main_repo, &worktree, "feature/ws-del");
                repo.managed_by_aoe = !corrupt_ancestor;
                instance.workspace_info = Some(workspace_info(&workspace, vec![repo]));
                let result = perform_deletion(&DeletionRequest {
                    delete_worktree: true,
                    delete_branch: !corrupt_ancestor,
                    ..request(instance)
                });

                assert!(
                    result.success,
                    "a stray file must not wedge the purge: {:?}",
                    result.errors
                );
                assert!(
                    result
                        .messages
                        .iter()
                        .any(|m| m.starts_with("Workspace directory kept:")),
                    "expected a 'kept' message: {:?}",
                    result.messages
                );
                assert_eq!(std::fs::read_to_string(&stray).unwrap(), "keep me");
                assert!(workspace.exists());
                assert_eq!(
                    worktree.exists(),
                    corrupt_ancestor,
                    "only a managed worktree goes"
                );
            }
        }

        #[test]
        fn workspace_repo_keeps_a_branch_aoe_did_not_create() {
            let _app_guard = isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let workspace = tmp.path().join("ws");
            let main_repo = tmp.path().join("frontend");
            let worktree = workspace.join("frontend");
            init_repo(&main_repo);
            git_in(&main_repo, &["branch", "mine"]);
            std::fs::create_dir_all(&workspace).unwrap();
            git_in(
                &main_repo,
                &["worktree", "add", worktree.to_str().unwrap(), "mine"],
            );

            let mut instance = Instance::new("Converted", workspace.to_str().unwrap());
            let mut repo = workspace_repo(&main_repo, &worktree, "mine");
            repo.branch_preexisting = true;
            instance.workspace_info = Some(workspace_info(&workspace, vec![repo]));
            let result = perform_deletion(&DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(instance)
            });

            assert!(
                result.success,
                "perform_deletion failed: {:?}",
                result.errors
            );
            assert!(!worktree.exists(), "worktree should be removed");
            assert!(branch_exists(&main_repo, "mine"));
        }

        #[test]
        fn preserved_worktree_keeps_its_branch() {
            let _app_guard = isolate_app_dir();
            let (_tmp, main_repo, worktree_path, instance) = worktree_fixture("feature/keep-me");
            let result = perform_deletion(&DeletionRequest {
                delete_branch: true,
                ..request(instance)
            });

            assert!(result.success, "{:?}", result.errors);
            assert!(!result.errors.iter().any(|e| e.starts_with("Branch:")));
            assert!(
                result.messages.iter().any(|m| m.contains("kept")),
                "a kept-branch message is expected: {:?}",
                result.messages
            );
            assert!(worktree_path.exists());
            assert!(main_repo.join(".git/worktrees/worktree").exists());
            assert!(branch_exists(&main_repo, "feature/keep-me"));
        }

        /// A dirty worktree survives a normal delete (the sandbox preclean is skipped too, or it
        /// would wipe the changes first) and is removed by a forced one.
        #[test]
        fn dirty_worktree_requires_force() {
            let _app_guard = isolate_app_dir();
            for sandboxed in [false, true] {
                let (_tmp, main_repo, worktree_path, mut instance) =
                    worktree_fixture("feature/dirty");
                if sandboxed {
                    instance.sandbox_info = Some(sandbox_info("aoe-dirty-test-doesnotexist"));
                }
                std::fs::write(worktree_path.join("uncommitted.log"), "important").unwrap();
                let request = DeletionRequest {
                    delete_worktree: true,
                    delete_branch: true,
                    delete_sandbox: sandboxed,
                    ..request(instance)
                };

                let (stages, result) = stages_of(&request);
                assert!(!result.success, "dirty worktree deleted without --force");
                if sandboxed {
                    let err = result.errors.join("; ");
                    assert!(err.contains("modified or untracked"), "{err}");
                    assert!(err.contains("uncommitted.log"), "{err}");
                }
                assert!(worktree_path.join("uncommitted.log").exists());
                assert!(main_repo.join(".git/worktrees/worktree").exists());
                assert!(!stages.iter().any(|s| s == "sandbox_worktree_preclean"));

                let (stages, result) = stages_of(&DeletionRequest {
                    force_delete: true,
                    delete_sandbox: false,
                    ..request
                });
                assert!(
                    result.success,
                    "force delete should succeed: {:?}",
                    result.errors
                );
                assert_eq!(
                    stages.iter().any(|s| s == "sandbox_worktree_preclean"),
                    sandboxed
                );
                assert!(!worktree_path.exists());
                assert!(!main_repo.join(".git/worktrees/worktree").exists());
            }
        }
    }

    mod scratch_cleanup {
        use super::*;
        use std::fs;

        fn scratch_instance() -> (Instance, PathBuf) {
            let id = format!("delete-test-{}", uuid::Uuid::new_v4());
            let dir = crate::session::scratch::provision_scratch_dir(&id)
                .expect("provision scratch dir for test");
            let mut instance = Instance::new("Scratch", dir.to_str().unwrap());
            instance.scratch = true;
            (instance, dir)
        }

        #[test]
        #[serial]
        fn scratch_session_is_kept_on_request_then_removed_and_tolerates_missing_dir() {
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
            let result = perform_deletion(&DeletionRequest {
                keep_scratch: true,
                ..request(instance.clone())
            });
            assert!(result.success, "{:?}", result.errors);
            assert!(dir.exists());
            assert!(
                result.messages.iter().any(|m| {
                    m.contains("Scratch directory kept at:") && m.contains(dir.to_str().unwrap())
                }),
                "expected kept-path message, got {:?}",
                result.messages
            );

            let request = request(instance);
            let result = perform_deletion(&request);
            assert!(result.success, "deletion errors: {:?}", result.errors);
            assert!(!dir.exists());
            assert!(
                result
                    .messages
                    .iter()
                    .any(|m| m.contains("Scratch directory removed")),
                "{:?}",
                result.messages
            );

            let result = perform_deletion(&request);
            assert!(
                result.success,
                "missing scratch dir must not fail: {:?}",
                result.errors
            );
        }

        /// The scratch guard refuses a scratch row pointing outside the scratch root, and a
        /// non-scratch row under the app dir is never treated as scratch.
        #[test]
        #[serial]
        fn scratch_cleanup_leaves_paths_outside_its_root_alone() {
            let _tmp = isolate_app_dir();
            let app_dir = crate::session::get_app_dir().unwrap();
            for (scratch, parent) in [(true, std::env::temp_dir()), (false, app_dir)] {
                let dir = parent.join(format!("aoe-scratch-guard-{}", uuid::Uuid::new_v4()));
                fs::create_dir(&dir).unwrap();
                fs::write(dir.join("file.txt"), b"keep me").unwrap();

                let mut instance = Instance::new("Guarded", dir.to_str().unwrap());
                instance.scratch = scratch;
                let result = perform_deletion(&request(instance));

                let survived = dir.join("file.txt").exists();
                let _ = fs::remove_dir_all(&dir);
                assert!(survived, "scratch={scratch}: {dir:?} must survive");
                assert_eq!(
                    result.errors.iter().any(|e| e.contains("scratch guard")),
                    scratch,
                    "scratch={scratch}: {:?}",
                    result.errors
                );
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn purge_keeps_unknown_history_and_live_groups_before_hooks() {
        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let sentinel = project.join("keep");
        std::fs::write(&sentinel, "checkout content").unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "read -r ignored || :"])
            .stdin(std::process::Stdio::piped());
        crate::process::configure_process_group(&mut command);
        let mut child = Reap(command.spawn().unwrap());
        let pid = child.0.id();
        let incarnation = crate::process::process_incarnation(pid).unwrap().unwrap();
        let boot = *uuid::Uuid::parse_str(&crate::process::boot_id().unwrap())
            .unwrap()
            .as_bytes();
        let live = serde_json::from_value(serde_json::json!({
            "coverage": "complete", "preparations": [], "launches": [{
                "nonce": *uuid::Uuid::new_v4().as_bytes(), "boot": boot,
                "generation": 0, "incarnation": incarnation,
            }],
        }))
        .unwrap();
        let storage = Storage::new_unwatched("owner").unwrap();
        let mut instance = Instance::new("Protected", project.to_str().unwrap());
        instance.id = "protected-purge".into();
        instance.source_profile = storage.profile().into();
        instance.view = crate::session::View::Structured;
        instance.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        for journal in [
            crate::session::runner_journal::RunnerExecutionJournal::default(),
            live,
        ] {
            instance.runner_journal = journal.clone();
            storage
                .update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == instance.id)
                        .unwrap()
                        .runner_journal = journal.clone();
                    Ok(())
                })
                .unwrap();
            assert!(crate::process::worker::is_process_group_alive(pid));
            assert!(crate::process::worker_registry::load_strict(&instance.id)
                .unwrap()
                .is_none());
            let transaction =
                match PurgeTransaction::reserve_unwatched(request(instance.clone())).unwrap() {
                    PurgeReservation::Reserved(transaction) => transaction,
                    PurgeReservation::Rejected(result) => panic!("reservation failed: {result:?}"),
                };
            let mut hooks = 0;
            let result = match settle_runner_of(transaction).await {
                Ok(transaction) => transaction
                    .run_hooks_with(|_, _| {
                        hooks += 1;
                    })
                    .complete_with(|_| Ok(())),
                Err(result) => *result,
            };
            assert_eq!(result.disposition, DeletionDisposition::Failed);
            assert_eq!(hooks, 0);
            assert_eq!(
                std::fs::read_to_string(&sentinel).unwrap(),
                "checkout content"
            );
            assert!(storage
                .load()
                .unwrap()
                .iter()
                .any(|row| row.id == instance.id && row.lifecycle_reservation.is_none()));
            assert!(crate::process::worker::is_process_group_alive(pid));
            instance = result
                .retained_instance
                .expect("failed original transaction returns its own released CAS projection");
        }
        drop(child.0.stdin.take());
        assert!(child.0.wait().unwrap().success());
    }
}
