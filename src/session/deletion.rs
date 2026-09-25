//! Shared session deletion logic used by CLI, TUI, and web server.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::containers::DockerContainer;
use crate::git::cleanup::{remove_managed_worktree, WorktreeCleanupOptions};
use crate::git::GitWorktree;
use crate::session::config::repo_config;
use crate::session::path_identity::CleanupProtection;
use crate::session::storage::StorageFlock;
use crate::session::{Instance, LifecycleOperation, SessionStore, Storage};
#[derive(Clone, Serialize, Deserialize)]
pub struct DeletionRequest {
    pub session_id: String,
    pub instance: Instance,
    pub delete_worktree: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    pub force_delete: bool,
    /// When `true`, on_destroy hooks run detached from the controlling
    /// terminal (TUI/web). When `false`, hooks inherit stdin/stdout so
    /// interactive prompts work (CLI).
    pub detach_hooks: bool,
    /// When `true` AND `instance.scratch` is `true`, the scratch directory
    /// is left on disk instead of being removed. The kept path is logged at
    /// info level and surfaced in the deletion result's messages. Has no
    /// effect on non-scratch sessions.
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
}

impl DeletionResult {
    fn rejected(
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
        }
    }
}

pub(crate) enum PurgeReservation<S: SessionStore + 'static> {
    Reserved(PurgeTransaction<S>),
    Rejected(DeletionResult),
}

/// Membership captured by a successful group-ungroup commit, before claiming purge.
pub(crate) struct PurgeSelection {
    pub profile: String,
    pub group_path: String,
    pub lifecycle_generation: u64,
}

impl PurgeSelection {
    fn matches_location(&self, row: &Instance, profile: &str) -> bool {
        self.profile == profile && self.group_path == row.group_path
    }
}

/// Owned purge transition. The durable reservation spans hooks, teardown, and
/// final commit. The lifecycle flock is deliberately released around hooks and
/// reacquired before any irreversible work.
pub(crate) struct PurgeTransaction<S: SessionStore + 'static> {
    store: Option<S>,
    request: Option<DeletionRequest>,
    selection: Option<PurgeSelection>,
    capture: Option<super::purge_owners::PurgeCapture>,
    was_trashed: bool,
    generation: u64,
    lifecycle_lock: Option<StorageFlock>,
    active: bool,
    additional_protection: Option<CleanupProtection>,
}

/// A purge whose durable row has already been removed. The same lifecycle
/// flock remains held while irreversible sidecars are removed.
#[must_use = "committed purge sidecars must be finished"]
pub(crate) struct CommittedPurge<S: SessionStore> {
    store: S,
    request: DeletionRequest,
    owner: super::purge_owners::PurgeOwner,
    additional_protection: Option<CleanupProtection>,
    _lifecycle_lock: Option<StorageFlock>,
    _identity_lock: Option<StorageFlock>,
}

#[derive(Clone, Copy)]
enum CompletionGate {
    Proceed,
    AlreadyGone,
    KeptRestored,
    Superseded,
}

impl CompletionGate {
    fn for_row(
        stored: &mut Instance,
        was_trashed: bool,
        generation: u64,
        structured: bool,
        selection: Option<&PurgeSelection>,
        profile: &str,
    ) -> Self {
        let gate = if crate::session::claim::purge_restored_row_must_be_kept(
            was_trashed,
            stored.is_trashed(),
        ) {
            Self::KeptRestored
        } else if stored.is_structured() != structured
            || !stored.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation)
            || selection.is_some_and(|selection| !selection.matches_location(stored, profile))
        {
            Self::Superseded
        } else {
            Self::Proceed
        };
        if !matches!(gate, Self::Proceed) {
            stored.release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation);
        }
        gate
    }
}

impl PurgeTransaction<Storage> {
    pub fn reserve_unwatched(request: DeletionRequest) -> Result<PurgeReservation<Storage>> {
        let profile = request.instance.source_profile.clone();
        anyhow::ensure!(
            !profile.is_empty(),
            "session has no source profile; refusing to use the default profile"
        );
        let storage = Storage::open_unwatched(&profile)?;
        Self::reserve(storage, request, None)
    }
}

impl<S: SessionStore + 'static> PurgeTransaction<S> {
    fn store(&self) -> &dyn SessionStore {
        self.store.as_ref().expect("active purge owns its backend")
    }

    fn request(&self) -> &DeletionRequest {
        self.request
            .as_ref()
            .expect("active purge owns its request")
    }

    pub(crate) fn instance(&self) -> &Instance {
        &self.request().instance
    }

    fn adopt_row(&mut self, mut row: Instance) {
        let request = self
            .request
            .as_mut()
            .expect("active purge owns its request");
        row.source_profile = std::mem::take(&mut request.instance.source_profile);
        request.instance = row;
    }

    pub fn reserve(
        backend: S,
        request: DeletionRequest,
        selection: Option<PurgeSelection>,
    ) -> Result<PurgeReservation<S>> {
        Self::reserve_inner(backend, request, selection, None)
    }

    /// Transfer only this creation's launch reservation into rollback ownership.
    pub(crate) fn reserve_failed_creation(
        backend: S,
        request: DeletionRequest,
        generation: u64,
    ) -> Result<PurgeReservation<S>> {
        Self::reserve_inner(backend, request, None, Some(generation))
    }

    fn reserve_inner(
        backend: S,
        mut request: DeletionRequest,
        selection: Option<PurgeSelection>,
        creation_generation: Option<u64>,
    ) -> Result<PurgeReservation<S>> {
        anyhow::ensure!(
            request.session_id == request.instance.id,
            "purge request identity does not match its selected row"
        );
        let store: &dyn SessionStore = &backend;
        let id = request.session_id.clone();
        let was_trashed = request.instance.is_trashed();
        let lifecycle_lock = store
            .storage()
            .acquire_instance_lifecycle_lock(&id)
            .context("failed to acquire instance purge lock")?;
        let now = Utc::now();
        let mut reserved = None;
        let mut rejected = None;
        store.update(|instances, _groups| {
            if let Some(stored) = instances.iter().find(|instance| instance.id == id) {
                if stored.is_structured() != request.instance.is_structured()
                    || selection.as_ref().is_some_and(|selection| {
                        !selection.matches_location(stored, store.storage().profile())
                            || selection.lifecycle_generation != stored.lifecycle_generation
                    })
                    || creation_generation
                        .is_some_and(|generation| !stored.creation_rollback_is_owned(generation))
                    || creation_generation.is_some_and(|_| {
                        stored.project_path != request.instance.project_path
                            || stored
                                .worktree_info
                                .as_ref()
                                .is_some_and(|stored_worktree| {
                                    request.instance.worktree_info.as_ref().is_none_or(|built| {
                                        stored_worktree.branch != built.branch
                                            || stored_worktree.main_repo_path
                                                != built.main_repo_path
                                    })
                                })
                            || stored.workspace_info.is_some()
                                != request.instance.workspace_info.is_some()
                            || stored.workspace_info.as_ref().is_some_and(|workspace| {
                                request
                                    .instance
                                    .workspace_info
                                    .as_ref()
                                    .is_none_or(|built| {
                                        workspace.workspace_dir != built.workspace_dir
                                            || workspace.repos.len() != built.repos.len()
                                            || workspace.repos.iter().zip(&built.repos).any(
                                                |(stored, built)| {
                                                    stored.worktree_path != built.worktree_path
                                                        || stored.main_repo_path
                                                            != built.main_repo_path
                                                        || stored.branch != built.branch
                                                },
                                            )
                                    })
                            })
                    })
                {
                    rejected = Some((
                        DeletionDisposition::Busy,
                        "Session selection changed before purge".to_owned(),
                        Some(stored.clone()),
                    ));
                    return Ok(());
                }
            }
            if let Some(generation) = creation_generation {
                if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Launch,
                        generation,
                    );
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
            let mut snapshot = stored.clone();
            snapshot.source_profile = store.storage().profile().to_string();
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
        if creation_generation.is_some() {
            request.instance.lifecycle_generation = snapshot.lifecycle_generation;
            request.instance.status = snapshot.status;
            request.instance.lifecycle_reservation = snapshot.lifecycle_reservation.clone();
            request.instance.source_profile = snapshot.source_profile;
        } else {
            request.instance = snapshot;
        }
        let mut transaction = Self {
            store: Some(backend),
            request: Some(request),
            selection,
            capture: None,
            was_trashed,
            generation,
            lifecycle_lock: Some(lifecycle_lock),
            active: true,
            additional_protection: None,
        };
        transaction.capture = Some(super::purge_owners::PurgeCapture::new(
            &transaction.request().instance,
        )?);
        Ok(PurgeReservation::Reserved(transaction))
    }
    pub(crate) fn with_additional_protection(mut self, protection: CleanupProtection) -> Self {
        self.additional_protection = Some(protection);
        self
    }

    /// Run best-effort hooks without a lifecycle or storage flock held.
    pub fn run_hooks(self) -> Result<Self> {
        let config = self
            .store()
            .configuration(Some(self.store().storage().profile()))?;
        Ok(self.run_hooks_with(|instance, detach| {
            run_on_destroy_hooks(instance, detach, &config.hooks.on_destroy)
        }))
    }

    fn run_hooks_with<F>(mut self, run_hooks: F) -> Self
    where
        F: FnOnce(&Instance, bool),
    {
        self.lifecycle_lock = None;
        run_hooks(&self.request().instance, self.request().detach_hooks);
        self
    }

    fn reacquire_cleanup_locks(&mut self) -> Result<StorageFlock> {
        self.lifecycle_lock = None;
        let identity = super::acquire_session_identity_lock()?;
        self.lifecycle_lock = Some(
            self.store()
                .storage()
                .acquire_instance_lifecycle_lock(&self.request().session_id)
                .context("failed to reacquire instance purge lock")?,
        );
        Ok(identity)
    }

    fn release_reservation(&mut self, errors: &[String]) -> Result<Option<Instance>> {
        let id = self.request().session_id.clone();
        let generation = self.generation;
        let mut retained = None;
        self.store().update(|instances, _groups| {
            if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                if stored.finish_lifecycle_status(
                    LifecycleOperation::Purge,
                    generation,
                    super::Status::Error,
                ) {
                    stored.last_error = Some(errors.join("; "));
                }
                retained = Some(stored.clone());
            }
            Ok(())
        })?;
        self.active = false;
        Ok(retained)
    }

    fn gate(&mut self) -> Result<(CompletionGate, Option<Instance>)> {
        let id = self.request().session_id.clone();
        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let structured = self.request().instance.is_structured();
        let mut outcome = None;
        self.store().update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) else {
                outcome = Some((CompletionGate::AlreadyGone, None));
                return Ok(());
            };
            let gate = CompletionGate::for_row(
                stored,
                was_trashed,
                generation,
                structured,
                self.selection.as_ref(),
                self.store().storage().profile(),
            );

            outcome = Some((gate, Some(stored.clone())));
            Ok(())
        })?;
        let outcome = outcome.ok_or_else(|| anyhow::anyhow!("purge gate produced no outcome"))?;
        if !matches!(outcome.0, CompletionGate::Proceed) {
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
                "Session changed while purge was pending, so it was retained",
            ),
            CompletionGate::Proceed => unreachable!("proceed is not a terminal result"),
        };
        DeletionResult::rejected(
            self.request().session_id.clone(),
            disposition,
            message,
            retained_instance,
        )
    }

    /// Persist ownership and remove the row before irreversible sidecar cleanup.
    pub fn begin_irreversible(
        mut self,
    ) -> std::result::Result<CommittedPurge<S>, Box<DeletionResult>> {
        let _identity = match self.reacquire_cleanup_locks() {
            Ok(identity) => identity,
            Err(error) => {
                return Err(Box::new(DeletionResult::rejected(
                    self.request().session_id.clone(),
                    DeletionDisposition::Failed,
                    format!("Failed to resume reserved session purge: {error}"),
                    None,
                )));
            }
        };
        let id = self.request().session_id.clone();
        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let structured = self.request().instance.is_structured();
        let mut commit = None;
        let mut owner = None;
        let mut capture = self.capture.take();
        let durable_request = self.request().clone();
        let durable_additional_protection = self.additional_protection.clone();
        if let Err(error) = self.store().update(|instances, _groups| {
            let Some(index) = instances.iter().position(|instance| instance.id == id) else {
                commit = Some((CompletionGate::AlreadyGone, None));
                return Ok(());
            };
            let gate = CompletionGate::for_row(
                &mut instances[index],
                was_trashed,
                generation,
                structured,
                self.selection.as_ref(),
                self.store().storage().profile(),
            );
            if matches!(gate, CompletionGate::Proceed) {
                owner = Some(super::purge_owners::PurgeOwner::record_plan(
                    self.store().storage(),
                    &instances[index],
                    generation,
                    Some(&durable_request),
                    durable_additional_protection.as_ref(),
                    capture
                        .take()
                        .expect("reserved purge has captured ownership"),
                )?);
                commit = Some((CompletionGate::Proceed, Some(instances.remove(index))));
            } else {
                commit = Some((gate, Some(instances[index].clone())));
            }
            Ok(())
        }) {
            return Err(Box::new(DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                format!("Failed to commit irreversible session purge: {error}"),
                None,
            )));
        }

        let Some((gate, row)) = commit else {
            return Err(Box::new(DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                "Irreversible purge commit produced no outcome",
                None,
            )));
        };
        self.active = false;
        if !matches!(gate, CompletionGate::Proceed) {
            return Err(Box::new(self.result_for_gate(gate, row)));
        }
        self.adopt_row(row.expect("committed purge owns the removed row"));
        Ok(CommittedPurge {
            owner: owner.expect("committed purge has durable ownership"),
            store: self.store.take().expect("committed purge owns its backend"),
            request: self
                .request
                .take()
                .expect("committed purge owns its request"),
            additional_protection: self.additional_protection.take(),
            _lifecycle_lock: Some(
                self.lifecycle_lock
                    .take()
                    .expect("active purge transaction must own its lifecycle lock"),
            ),
            _identity_lock: None,
        })
    }

    /// Verify the token under identity and lifecycle exclusion through commit.
    fn complete_inner(
        mut self,
        after_teardown: impl FnOnce(&Instance, &dyn SessionStore) -> std::result::Result<(), String>,
    ) -> DeletionResult {
        let _identity = match self.reacquire_cleanup_locks() {
            Ok(identity) => identity,
            Err(error) => {
                return DeletionResult::rejected(
                    self.request().session_id.clone(),
                    DeletionDisposition::Failed,
                    format!("Failed to resume reserved session purge: {error}"),
                    None,
                );
            }
        };
        let id = self.request().session_id.clone();
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
        self.adopt_row(retained.expect("accepted purge owns the validated row"));
        if let Err(error) = self
            .capture
            .as_ref()
            .expect("reserved purge has captured ownership")
            .ensure_captured_runner_stopped()
        {
            let error = error.to_string();
            let retained = self
                .release_reservation(std::slice::from_ref(&error))
                .ok()
                .flatten();
            return DeletionResult::rejected(id, DeletionDisposition::Failed, error, retained);
        }
        let mut result = perform_deletion_teardown_lifecycle_locked(
            self.request(),
            self.store(),
            None,
            self.additional_protection.as_ref(),
        );
        if !result.success {
            result.retained_instance = self.release_reservation(&result.errors).ok().flatten();
            result.disposition = DeletionDisposition::Failed;
            return result;
        }

        if let Err(error) = after_teardown(&self.request().instance, self.store()) {
            result.success = false;
            result.errors.push(error);
            result.retained_instance = self.release_reservation(&result.errors).ok().flatten();
            result.disposition = DeletionDisposition::Failed;
            return result;
        }

        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let structured = self.request().instance.is_structured();
        let mut commit = None;
        let commit_result = self.store().update(|instances, _groups| {
            let Some(index) = instances.iter().position(|instance| instance.id == id) else {
                commit = Some((CompletionGate::AlreadyGone, None));
                return Ok(());
            };
            let gate = CompletionGate::for_row(
                &mut instances[index],
                was_trashed,
                generation,
                structured,
                self.selection.as_ref(),
                self.store().storage().profile(),
            );
            if matches!(gate, CompletionGate::Proceed) {
                instances.remove(index);
                commit = Some((CompletionGate::Proceed, None));
            } else {
                commit = Some((gate, Some(instances[index].clone())));
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
        self.complete_inner(|instance, _| after_teardown(instance))
    }

    pub fn complete(self) -> DeletionResult {
        self.complete_inner(|_, _| Ok(()))
    }

    pub(crate) fn complete_creation_rollback<'a>(
        self,
        created: impl IntoIterator<Item = &'a super::builder::CreatedWorktree>,
    ) -> DeletionResult {
        let mut branches = created
            .into_iter()
            .filter(|worktree| !worktree.checkout_created && worktree.owned_branch.is_some())
            .peekable();
        if branches.peek().is_none() {
            return self.complete();
        }
        self.complete_inner(|instance, store| {
            (|| -> Result<()> {
                let live = store.storage().cleanup_protection(instance)?;
                let pending = super::purge_owners::protection(store.storage(), None)?;
                for worktree in branches {
                    super::builder::cleanup_unchecked_out_branch(
                        worktree,
                        std::iter::once(&live).chain(pending.iter()),
                    )?;
                }
                Ok(())
            })()
            .map_err(|error| format!("Creation branch rollback failed: {error:#}"))
        })
    }
}

impl<S: SessionStore> CommittedPurge<S> {
    pub fn finish(mut self) -> DeletionResult {
        drop(self._lifecycle_lock.take());
        let exclusion = (|| -> Result<_> {
            let identity = super::acquire_session_identity_lock()?;
            let lifecycle = self
                .store
                .storage()
                .acquire_instance_lifecycle_lock(&self.request.session_id)?;
            self.store.check_available()?;
            anyhow::ensure!(
                !self
                    .store
                    .load()?
                    .iter()
                    .any(|row| row.id == self.request.session_id),
                "session identity was reused after committed removal"
            );
            Ok((lifecycle, identity))
        })();
        let _exclusion = match exclusion {
            Ok(exclusion) => exclusion,
            Err(error) => {
                return DeletionResult::rejected(
                    self.request.session_id,
                    DeletionDisposition::Removed,
                    format!("Session removed, but cleanup authority is unavailable: {error}"),
                    None,
                );
            }
        };
        self.finish_with_exclusion()
    }

    pub(crate) fn finish_recovered(self) -> DeletionResult {
        if self._identity_lock.is_none() {
            return DeletionResult::rejected(
                self.request.session_id,
                DeletionDisposition::Removed,
                "Recovered purge lacks identity authority",
                None,
            );
        }
        self.finish_with_exclusion()
    }

    fn finish_with_exclusion(self) -> DeletionResult {
        if let Err(error) = self.owner.ensure_captured_runner_stopped() {
            return DeletionResult::rejected(
                self.request.session_id,
                DeletionDisposition::Removed,
                error.to_string(),
                None,
            );
        }
        let mut result = perform_deletion_teardown_lifecycle_locked(
            &self.request,
            &self.store,
            Some(self.owner.token()),
            self.additional_protection.as_ref(),
        );
        result.disposition = DeletionDisposition::Removed;
        if result.success {
            if let Err(error) = self.owner.release() {
                result.success = false;
                result.errors.push(format!(
                    "Resources cleaned, but purge ownership release failed: {error}"
                ));
            }
        }
        result
    }

    pub(crate) fn session_id(&self) -> &str {
        &self.request.session_id
    }

    pub(crate) fn is_structured(&self) -> bool {
        self.request.instance.is_structured()
    }

    /// Durable journal token of the owner backing this purge, so a caller can
    /// record that it already attempted this owner during a recovery pass.
    pub(crate) fn owner_token(&self) -> &str {
        self.owner.token()
    }
}

/// Rebuild the first durable committed purge whose row is still absent and
/// whose owner token is not in `attempted`. The caller retains the returned
/// identity and lifecycle locks across any structured shutdown and the
/// idempotent resource cleanup, and adds the selected token to `attempted` so
/// a failing owner is never reselected within the same pass.
pub(crate) fn recover_committed_purge(
    attempted: &mut HashSet<String>,
) -> Result<Option<CommittedPurge<Storage>>> {
    for plan in super::purge_owners::recovery_plans()? {
        if attempted.contains(&plan.token) {
            continue;
        }
        let owner_token = plan.token.clone();
        let profile_name = plan.profile.clone();
        let request_plan = plan.request.clone();
        let additional_protection = plan.additional_protection.clone();
        let attempt: Result<CommittedPurge<Storage>> = (|| {
            let request = request_plan
                .as_ref()
                .context("legacy pending purge owner has no executable cleanup plan")?;
            let store = Storage::open_unwatched(&profile_name)?;
            let identity = super::acquire_session_identity_lock()?;
            let lifecycle = store.acquire_instance_lifecycle_lock(&request.session_id)?;
            anyhow::ensure!(
                !store.load()?.iter().any(|row| row.id == request.session_id),
                "session identity was reused before pending purge recovery"
            );
            let owner = super::purge_owners::PurgeOwner::recover(&store, &owner_token)?;
            let profile = store.profile().to_owned();
            let mut request = request_plan
                .clone()
                .context("legacy pending purge owner has no executable cleanup plan")?;
            request.instance.source_profile = profile;
            Ok(CommittedPurge {
                store,
                request,
                owner,
                additional_protection,
                _lifecycle_lock: Some(lifecycle),
                _identity_lock: Some(identity),
            })
        })();
        match attempt {
            Ok(committed) => return Ok(Some(committed)),
            Err(error) => {
                attempted.insert(owner_token.clone());
                tracing::warn!(
                    target: "session.purge_recovery",
                    owner_token = %owner_token,
                    %error,
                    "pending purge owner is not currently recoverable; trying the next owner"
                );
            }
        }
    }
    Ok(None)
}

impl<S: SessionStore + 'static> Drop for PurgeTransaction<S> {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let (Some(store), Some(request)) = (self.store.take(), self.request.take()) {
            store.defer_lifecycle_release(
                request.session_id,
                LifecycleOperation::Purge,
                self.generation,
                self.lifecycle_lock.take(),
            );
        }
    }
}

pub fn execute_deletion(request: DeletionRequest) -> DeletionResult {
    let id = request.session_id.clone();
    let recent_entry = crate::session::recent_project_entry_for(&request.instance);
    let result =
        match PurgeTransaction::reserve_unwatched(request).and_then(|reservation| match reservation
        {
            PurgeReservation::Reserved(transaction) => Ok(transaction.run_hooks()?.complete()),
            PurgeReservation::Rejected(result) => Ok(result),
        }) {
            Ok(result) => result,
            Err(error) => DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                format!("Could not prepare session deletion: {error}"),
                None,
            ),
        };
    if result.disposition == DeletionDisposition::Removed {
        if let Some(entry) = recent_entry {
            if let Err(error) = crate::session::record_recent_project(entry) {
                tracing::warn!(
                    target: "session.delete",
                    "recording recent project after delete failed: {error}"
                );
            }
        }
    }
    result
}

/// Whether `workspace_dir` has the workspace layout AoE creates and may
/// therefore be removed once empty.
///
/// The workspace stage ends in a non-recursive removal of a path read from the
/// session record. The shape check remains a defense against future writers
/// setting `workspace_dir` to a session's own checkout, but it does not claim
/// to prove ownership by itself.
///
/// There must be at least one repo, and every repo's worktree must be a strict
/// descendant of `workspace_dir`. A `workspace_dir` that is one of the
/// worktrees, or that holds none of them, was not laid out by the workspace
/// builder. The final `remove_dir` succeeds only after the managed worktrees
/// have been removed and the directory is empty, so a corrupt ancestor path
/// cannot recursively remove unrelated user data.
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

/// Whether `branch` is one of the branches git states is `main_repo`'s default,
/// so its worktree must be preserved (#3215).
///
/// A repo aoe cannot open is a repo git cannot remove a worktree from either,
/// so a failure here is not treated as protection: the removal stage surfaces
/// its own error exactly as it did before this guard existed.
fn is_protected_default_branch(main_repo: &Path, branch: &str) -> bool {
    GitWorktree::new(main_repo.to_path_buf())
        .and_then(|git| git.protected_default_branch_names())
        .is_ok_and(|names| names.contains(branch))
}

/// Finish runtime cleanup after the session row is durably absent.
pub(crate) fn cleanup_abandoned_session(instance: Instance) {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        instance.kill_all_tmux_sessions_without_lifecycle_row()
    })) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(target: "session.delete", session_id = %instance.id, %error, "abandoned purge tmux teardown failed")
        }
        Err(panic) => {
            tracing::error!(target: "session.delete", session_id = %instance.id, ?panic, "abandoned purge tmux teardown panicked")
        }
    }
    if instance
        .sandbox_info
        .as_ref()
        .is_some_and(|sandbox| sandbox.enabled)
    {
        let container = DockerContainer::from_session_id(&instance.id);
        if let crate::containers::Teardown::Failed(error) = container.teardown(&instance.id) {
            tracing::warn!(target: "session.delete", session_id = %instance.id, %error,
                "abandoned purge container teardown failed");
        }
    }
}

/// Every path a session outside `except_ids` works in or will restore to.
fn other_sessions_paths(instances: &[Instance], except_ids: &[&str]) -> Vec<PathBuf> {
    instances
        .iter()
        .filter(|instance| !except_ids.contains(&instance.id.as_str()))
        .flat_map(|instance| {
            std::iter::once(instance.project_path.as_str())
                .chain(instance.pre_trash_project_path.as_deref())
                .chain(
                    instance
                        .all_repos()
                        .iter()
                        .map(|r| r.worktree_path.as_str()),
                )
                .map(PathBuf::from)
        })
        .collect()
}

/// The paths sessions outside a deletion use, across every profile.
pub(crate) enum PathsInUse {
    Known(Vec<PathBuf>),
    /// Some store could not be read, so every path must be assumed in use.
    Unknown(String),
}

impl PathsInUse {
    pub(crate) fn covers(&self, root: &Path) -> bool {
        match self {
            Self::Known(paths) => paths.iter().any(|path| path.starts_with(root)),
            Self::Unknown(_) => true,
        }
    }

    pub(crate) fn contains_exact_path(&self, target: &Path) -> bool {
        match self {
            Self::Known(paths) => paths.iter().any(|path| path == target),
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
    let profiles =
        crate::session::list_profiles().map_err(|error| format!("listing profiles: {error}"))?;
    let storages = profiles
        .iter()
        .map(|profile| {
            Storage::open_unwatched(profile)
                .map_err(|error| format!("opening profile '{profile}': {error}"))
        })
        .collect::<std::result::Result<_, _>>()?;
    Ok((profiles, storages))
}

fn scan_paths_in_use(storages: &[Storage], except_ids: &[&str]) -> PathsInUse {
    let mut paths = Vec::new();
    for storage in storages {
        match storage.load() {
            Ok(instances) => paths.extend(other_sessions_paths(&instances, except_ids)),
            Err(error) => {
                return PathsInUse::Unknown(format!(
                    "reading profile '{}': {error}",
                    storage.profile()
                ))
            }
        }
    }
    PathsInUse::Known(paths)
}

/// Unlocked snapshot of [`PathsInUse`], for a preflight that the teardown re-checks under lock.
pub(crate) fn paths_in_use_except(except_ids: &[&str]) -> PathsInUse {
    match all_profile_storages() {
        Ok((_, storages)) => scan_paths_in_use(&storages, except_ids),
        Err(reason) => PathsInUse::Unknown(reason),
    }
}

#[cfg(test)]
thread_local! {
    static AFTER_PATHS_IN_USE_SCAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with the paths other sessions use while every profile's storage lock is held, so no
/// session can adopt a path between the check and whatever `f` removes.
fn with_paths_in_use_locked<R>(except_id: &str, f: impl FnOnce(&PathsInUse) -> R) -> R {
    let (profiles, storages) = match all_profile_storages() {
        Ok(found) => found,
        Err(reason) => return f(&PathsInUse::Unknown(reason)),
    };
    let mut f = Some(f);
    let locked = crate::session::storage::with_storages_locked(&storages, || {
        let paths_in_use = match crate::session::list_profiles() {
            Ok(now)
                if now.len() == profiles.len()
                    && now.iter().all(|profile| profiles.contains(profile)) =>
            {
                scan_paths_in_use(&storages, &[except_id])
            }
            Ok(_) => {
                PathsInUse::Unknown("the profile catalog changed during the deletion".to_string())
            }
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
    let config = crate::session::config::profile_config::resolve_config_or_warn(
        &request.instance.effective_profile(),
    );
    run_on_destroy_hooks(
        &request.instance,
        request.detach_hooks,
        &config.hooks.on_destroy,
    );
    perform_deletion_core(
        request,
        false,
        |session_id| DockerContainer::from_session_id(session_id).teardown(session_id),
        &config,
        &[&CleanupProtection::default()],
    )
}

fn perform_deletion_teardown_lifecycle_locked(
    request: &DeletionRequest,
    store: &dyn SessionStore,
    except_owner: Option<&str>,
    additional_protection: Option<&CleanupProtection>,
) -> DeletionResult {
    let config = match store.configuration(Some(store.storage().profile())) {
        Ok(config) => config,
        Err(error) => {
            return DeletionResult::rejected(
                request.session_id.clone(),
                DeletionDisposition::Failed,
                format!("Purge configuration unavailable: {error}"),
                None,
            )
        }
    };
    let pending = match super::purge_owners::protection(store.storage(), except_owner) {
        Ok(pending) => pending,
        Err(error) => {
            return DeletionResult::rejected(
                request.session_id.clone(),
                DeletionDisposition::Failed,
                format!("Pending purge ownership unavailable: {error}"),
                None,
            )
        }
    };
    let live = match store.storage().cleanup_protection(&request.instance) {
        Ok(live) => live,
        Err(error) => {
            return DeletionResult::rejected(
                request.session_id.clone(),
                DeletionDisposition::Failed,
                format!("Resource ownership unavailable: {error}"),
                None,
            )
        }
    };
    let mut protections = Vec::with_capacity(pending.len() + 2);
    protections.push(&live);
    protections.extend(pending.iter());
    protections.extend(additional_protection);
    perform_deletion_core(
        request,
        true,
        |session_id| DockerContainer::from_session_id(session_id).teardown(session_id),
        &config,
        &protections,
    )
}

/// Core deletion routine, parameterized over how the sandbox container is torn
/// down so the container-removal contract can be exercised without a live
/// runtime.
///
/// NOTE: when the session is sandboxed and `delete_sandbox` is set, `teardown`
/// must be invoked unconditionally; it must not be gated behind a separate
/// existence probe, whose transient failure would skip removal and orphan a
/// live container.
#[cfg(test)]
fn perform_deletion_with(
    request: &DeletionRequest,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
) -> DeletionResult {
    let config = crate::session::config::profile_config::resolve_config_or_warn(
        &request.instance.effective_profile(),
    );
    perform_deletion_core(
        request,
        false,
        teardown,
        &config,
        &[&CleanupProtection::default()],
    )
}

/// `lifecycle_locked` is the production path, which also keeps any worktree another session uses.
fn perform_deletion_core(
    request: &DeletionRequest,
    lifecycle_locked: bool,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
    config: &crate::session::Config,
    protection: &[&CleanupProtection],
) -> DeletionResult {
    let mut errors = Vec::new();
    let mut messages = Vec::new();

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

    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "tmux_kill", "perform_deletion: stage");
    let stopped = if lifecycle_locked {
        request.instance.kill_all_tmux_sessions_locked()
    } else {
        request
            .instance
            .kill_all_tmux_sessions_without_lifecycle_row()
    };
    if let Err(error) = stopped {
        return DeletionResult {
            session_id: request.session_id.clone(),
            success: false,
            messages,
            errors: vec![format!("Tmux teardown failed; resources retained: {error}")],
            disposition: DeletionDisposition::Failed,
            teardown_started: true,
            retained_instance: None,
        };
    }

    let is_sandboxed = request
        .instance
        .sandbox_info
        .as_ref()
        .is_some_and(|s| s.enabled);
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

    let removes_managed_worktree = request.delete_worktree
        && (request
            .instance
            .worktree_info
            .as_ref()
            .is_some_and(|wt| wt.managed_by_aoe)
            || request.instance.workspace_info.is_some());
    let stage = |paths_in_use: &PathsInUse| {
        stage_teardown_worktrees(
            request,
            repos,
            is_sandboxed,
            paths_in_use,
            protection,
            teardown,
            &mut errors,
            &mut messages,
        )
    };
    let container_gone = if lifecycle_locked && removes_managed_worktree {
        with_paths_in_use_locked(&request.session_id, stage)
    } else {
        stage(&PathsInUse::Known(Vec::new()))
    };
    if !container_gone
        && errors
            .iter()
            .any(|error| error.starts_with("Container:") || error.starts_with("Sandbox preclean:"))
    {
        return DeletionResult {
            session_id: request.session_id.clone(),
            success: false,
            teardown_started: true,
            messages,
            errors,
            disposition: DeletionDisposition::Failed,
            retained_instance: None,
        };
    }

    stage_cleanup_scratch(request, protection, &mut errors, &mut messages);
    if container_gone && errors.is_empty() {
        stage_remove_agent_stores(request, config, &mut messages);
    }
    crate::hooks::cleanup_hook_status_dir(&request.instance.id);

    DeletionResult {
        session_id: request.session_id.clone(),
        success: errors.is_empty(),
        teardown_started: true,
        messages,
        errors,
        disposition: DeletionDisposition::Failed,
        retained_instance: None,
    }
}

fn preclean_blocks_container_removal(outcome: Option<crate::git::cleanup::SandboxCleanup>) -> bool {
    outcome == Some(crate::git::cleanup::SandboxCleanup::Blocked)
}

/// Container and worktree teardown, which destroys checkout contents and so must run inside the
/// same [`PathsInUse`] check that decides what to keep. Returns whether the container is gone.
#[expect(
    clippy::too_many_arguments,
    reason = "teardown stages workspace paths, protection and runtime results under one lock"
)]
fn stage_teardown_worktrees(
    request: &DeletionRequest,
    repos: &[super::WorkspaceRepo],
    is_sandboxed: bool,
    paths_in_use: &PathsInUse,
    protection: &[&CleanupProtection],
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) -> bool {
    let preserved_worktree_paths = stage_collect_preserved_worktrees(
        request,
        repos,
        paths_in_use,
        protection,
        errors,
        messages,
    );
    let root_cleanup_allowed = preserved_worktree_paths.is_empty()
        && !protection
            .iter()
            .any(|owner| owner.references_path(Path::new(&request.instance.project_path)))
        && !request
            .instance
            .workspace_info
            .as_ref()
            .is_some_and(|workspace| {
                protection
                    .iter()
                    .any(|owner| owner.references_path(Path::new(&workspace.workspace_dir)))
            });

    let preclean = if request.delete_worktree && is_sandboxed && root_cleanup_allowed {
        tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "sandbox_worktree_preclean", "perform_deletion: stage");
        Some(crate::git::cleanup::cleanup_sandbox_worktree(
            &request.instance,
        ))
    } else {
        None
    };
    let preclean_blocked = preclean_blocks_container_removal(preclean);
    let worktrees_removed_early = preclean_blocked && request.delete_sandbox;
    if worktrees_removed_early {
        // A stopped container cannot run the managed cleanup entrypoint. Try
        // host-side worktree removal first; only tear the container down after
        // that succeeds. A permission/root-owned failure leaves both intact.
        stage_remove_worktrees_and_branches(
            request,
            repos,
            &preserved_worktree_paths,
            root_cleanup_allowed,
            protection,
            errors,
            messages,
        );
        if !errors.is_empty() {
            return false;
        }
    }

    let mut container_gone = false;
    if request.delete_sandbox && is_sandboxed {
        tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "container_remove", "perform_deletion: stage");
        let outcome = teardown(&request.instance.id);
        let failed = matches!(&outcome, crate::containers::Teardown::Failed(_));
        container_gone = !failed;
        deletion_messages_for(outcome, messages, errors);
        if failed {
            return false;
        }
    }

    if !worktrees_removed_early {
        stage_remove_worktrees_and_branches(
            request,
            repos,
            &preserved_worktree_paths,
            root_cleanup_allowed,
            protection,
            errors,
            messages,
        );
    }
    container_gone
}

fn stage_collect_preserved_worktrees(
    request: &DeletionRequest,
    repos: &[super::WorkspaceRepo],
    paths_in_use: &PathsInUse,
    protection: &[&CleanupProtection],
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) -> std::collections::HashSet<PathBuf> {
    let mut preserved_worktree_paths: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();
    let root_referenced = request.delete_worktree
        && request
            .instance
            .workspace_info
            .as_ref()
            .is_some_and(|workspace| {
                protection
                    .iter()
                    .any(|owner| owner.references_exact_path(Path::new(&workspace.workspace_dir)))
            });

    if request.delete_worktree {
        let primary = request
            .instance
            .worktree_info
            .as_ref()
            .filter(|worktree| worktree.managed_by_aoe)
            .map(|_| request.instance.project_path.as_str());
        for path in primary.into_iter().chain(
            repos
                .iter()
                .filter(|repo| repo.managed_by_aoe)
                .map(|repo| repo.worktree_path.as_str()),
        ) {
            if root_referenced
                || protection
                    .iter()
                    .any(|owner| owner.references_path(Path::new(path)))
            {
                preserved_worktree_paths.insert(PathBuf::from(path));
                messages.push(format!("Worktree kept; another session references {path}"));
            }
        }
    }

    // Default branches remain protected even for forced deletion.
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
            let workspace_root = Path::new(&ws_info.workspace_dir);
            // A session attached to the workspace root uses every repo. A
            // session attached to one repo protects only that repo.
            let root_attached = paths_in_use.contains_exact_path(workspace_root);
            if in_use(workspace_root) {
                for repo in repos.iter().filter(|r| r.managed_by_aoe) {
                    if (root_attached || in_use(Path::new(&repo.worktree_path)))
                        && preserved_worktree_paths.insert(PathBuf::from(&repo.worktree_path))
                    {
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
                // A deliberately retained checkout cannot make the purge fail as dirty.
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
    root_cleanup_allowed: bool,
    protection: &[&CleanupProtection],
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) {
    // Worktree removal must precede deleting its checked-out branch.
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "worktree_remove", "perform_deletion: stage");
    let cleanup = WorktreeCleanupOptions {
        force: request.force_delete,
        allow_container_removal: request.delete_sandbox,
        allow_root_cleanup: root_cleanup_allowed,
    };
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

    // A branch is eligible only after its own checkout was removed.
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
                                cleanup,
                                protection,
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

    // An unshared repo remains eligible when another repo must survive.
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
                        cleanup,
                        protection,
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
            if ws_info.cleanup_on_delete && root_cleanup_allowed {
                let ws_path = PathBuf::from(&ws_info.workspace_dir);
                // A record whose shape is not aoe-owned should never occur: it
                // means workspace_dir was mis-written (e.g. set to the user's
                // own checkout). Unlike the benign non-empty case below, fail
                // loud with an error so a corrupt record is surfaced rather than
                // silently clearing the row over it.
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
                        // Normally unreachable: prune_empty_parent_dirs, run
                        // after each worktree removal, already deletes the
                        // emptied workspace dir. This is the fallback for the
                        // rare case where prune stopped early (hop cap, or a
                        // home / main-repo boundary) yet the dir is empty here.
                        Ok(()) => messages.push("Workspace directory removed".to_string()),
                        // A non-empty dir still holds something that is not one of
                        // the managed worktrees: unrelated content under a mislaid
                        // record, or files written at the workspace root, which is
                        // the session's own cwd. We cannot tell which, so we keep
                        // them. The removal is non-recursive, so this is a safe
                        // refusal, not a failure worth retrying: report it as a
                        // message so the purge still clears the row instead of
                        // retrying the same non-convergent refusal forever, as the
                        // default-branch guard above does (#3215).
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
        if protection
            .iter()
            .any(|owner| owner.references_branch(&main_repo, &branch))
        {
            messages.push(format!(
                "Branch '{branch}' kept; another session references it"
            ));
        } else if main_worktree_removed {
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
            // Branch ownership is independent from checkout ownership.
            if repo.branch_preexisting {
                // Silent for an unmanaged workspace repo before the merge; now it
                // says so, which matches what the attached path already reported
                // and is the same reason the worktree stage reports a preserve.
                messages.push(format!(
                    "Branch '{}' ({}) kept; aoe did not create it",
                    repo.branch, repo.name
                ));
                continue;
            }
            // Per-repo gate: only delete a repo's branch when that repo's
            // worktree was actually removed. A repo whose worktree was
            // preserved (or failed to remove) keeps its branch checked
            // out (#2532).
            if !removed_session_worktrees.contains(&PathBuf::from(&repo.worktree_path)) {
                messages.push(format!(
                    "Branch '{}' ({}) kept; its worktree was preserved",
                    repo.branch, repo.name
                ));
                continue;
            }
            let main_repo = PathBuf::from(&repo.main_repo_path);
            if protection
                .iter()
                .any(|owner| owner.references_branch(&main_repo, &repo.branch))
            {
                messages.push(format!(
                    "Branch '{}' ({}) kept; another session references it",
                    repo.branch, repo.name
                ));
                continue;
            }
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
    protection: &[&CleanupProtection],
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) {
    // Scratch cleanup is independent from worktree flags, but still reference-protected.
    if request.instance.scratch {
        let path = PathBuf::from(&request.instance.project_path);
        if protection.iter().any(|owner| owner.references_path(&path)) {
            messages.push(format!(
                "Scratch directory kept; another session references {}",
                path.display()
            ));
            return;
        }
        // keep_scratch + tampered project_path used to surface
        // "Scratch directory kept at: /etc" which implied AoE was
        // intentionally leaving a path it never owned. Gate the
        // keep-scratch message on the same `is_scratch_path` guard
        // the remove branch uses so the message only fires for
        // paths AoE actually controls.
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
            // Already gone (user removed it manually, FS hiccup, prior
            // partial cleanup). Nothing to do, and we must not reach the
            // guard branch: a canonicalized `is_scratch_path` rejects
            // missing paths and would otherwise surface this as a guard
            // refusal even though it is not a tampering case.
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
            // Tampered `project_path` (e.g. JSON edited by hand to claim
            // `scratch: true` while pointing outside the scratch root)
            // is the only path that reaches this branch in normal use.
            // The session record will still be deleted, so callers need
            // a visible signal that on-disk cleanup was skipped.
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

/// Final stage: the session's own agent stores. Each holds a copy of the
/// agent's credentials and is named by an instance id that stops resolving
/// with this purge, so leaving it behind strands a credential nothing will
/// ever open.
///
/// Runs with the container already removed, and only then: the store is bind
/// mounted into it, so a kept container keeps its store.
///
/// Reports rather than fails. An error here would roll the purge back and
/// keep the session, which is the outcome this stage exists to avoid; a store
/// left behind is the reclaim pass's job instead.
fn stage_remove_agent_stores(
    request: &DeletionRequest,
    config: &crate::session::Config,
    messages: &mut Vec<String>,
) {
    tracing::debug!(target: "session.delete", session_id = %request.session_id, stage = "agent_store_remove", "perform_deletion: stage");
    match crate::session::sandbox_store_reclaim::remove_stores_for(&request.instance, config) {
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

/// Map a container [`Teardown`](crate::containers::Teardown) outcome onto a
/// deletion's user-facing messages and errors.
///
/// A `Failed` outcome is recorded as an error so the caller keeps the session
/// record rather than dropping it and orphaning a live container; `AlreadyGone`
/// is a silent no-op (there was nothing to remove).
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

/// Hooks are best-effort; unapproved repository hooks never override host hooks.
fn run_on_destroy_hooks(instance: &Instance, detach: bool, configured_hooks: &[String]) {
    let project_path = Path::new(&instance.project_path);
    let mut resolved_on_destroy = std::borrow::Cow::Borrowed(configured_hooks);
    match repo_config::check_repo_trust(project_path) {
        Ok(trust) if trust.hooks.needs_trust() => {
            tracing::warn!(target: "session.delete",
                "Repo hooks changed since last trust approval; skipping repo on_destroy hooks"
            );
        }
        Ok(trust) => {
            if let Some(hooks) = trust.hooks.trusted() {
                if !hooks.on_destroy.is_empty() {
                    resolved_on_destroy = std::borrow::Cow::Owned(hooks.on_destroy);
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

    // The caller controls detachment: TUI/web pass detach=true to avoid
    // corrupting the rendered UI (see issue #901); CLI passes detach=false
    // so interactive prompts work.
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
    fn stopped_or_unreadable_sandbox_preclean_retains_container() {
        assert!(preclean_blocks_container_removal(Some(
            crate::git::cleanup::SandboxCleanup::Blocked
        )));
        assert!(!preclean_blocks_container_removal(Some(
            crate::git::cleanup::SandboxCleanup::Cleaned
        )));
        assert!(!preclean_blocks_container_removal(Some(
            crate::git::cleanup::SandboxCleanup::Absent
        )));
        assert!(!preclean_blocks_container_removal(None));
    }
    fn create_test_instance() -> Instance {
        Instance::new("Test Session", "/tmp/test-project")
    }

    fn test_git_in(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn test_branch_exists(repo: &Path, branch: &str) -> bool {
        !test_git_in(repo, &["branch", "--list", branch]).is_empty()
    }

    fn test_worktree_info(branch: &str, main_repo: &Path) -> crate::session::WorktreeInfo {
        crate::session::WorktreeInfo {
            branch: branch.to_string(),
            main_repo_path: main_repo.to_string_lossy().into_owned(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        }
    }

    fn test_worktree_fixture(branch: &str) -> (tempfile::TempDir, PathBuf, PathBuf, Instance) {
        let temp = tempfile::TempDir::new().unwrap();
        let main_repo = temp.path().join("main");
        let worktree = temp.path().join("worktree");
        std::fs::create_dir_all(&main_repo).unwrap();
        let repo = git2::Repository::init(&main_repo).unwrap();
        let signature = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
            .unwrap();
        test_git_in(
            &main_repo,
            &["worktree", "add", "-b", branch, worktree.to_str().unwrap()],
        );
        let mut instance = Instance::new("Test", worktree.to_str().unwrap());
        instance.worktree_info = Some(test_worktree_info(branch, &main_repo));
        (temp, main_repo, worktree, instance)
    }

    fn test_request(instance: Instance) -> DeletionRequest {
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

    #[test]
    fn test_deletion_result_success_when_no_worktree_or_sandbox() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let instance = create_test_instance();
        let request = DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };

        let result = perform_deletion(&request);

        assert!(result.success);
        assert!(result.errors.is_empty());
        assert_eq!(result.session_id, request.session_id);
    }

    #[test]
    #[serial_test::serial]
    fn purge_cleans_only_unshared_post_hook_scratch_paths() {
        for (structured, shared, replace) in [
            (false, false, false),
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (true, true, true),
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap())
                .unwrap();
            let profile = "purge-post-hook-path";
            let storage = Storage::new_unwatched(profile).unwrap();
            let mut instance = create_test_instance();
            instance.source_profile = profile.into();
            instance.scratch = true;
            if structured {
                instance.view = crate::session::View::Structured;
            }
            let old = crate::session::scratch::provision_scratch_dir(&instance.id).unwrap();
            let moved = old.with_file_name(format!("{}-moved", instance.id));
            instance.project_path = old.to_string_lossy().into_owned();
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let transaction = match PurgeTransaction::reserve(
                Storage::open_unwatched(profile).unwrap(),
                request,
                None,
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("initial purge rejected"),
            };
            let transaction = transaction.run_hooks_with(|_, _| {
                std::fs::rename(&old, &moved).unwrap();
                std::fs::write(moved.join("payload"), b"retained scratch data").unwrap();
                std::fs::create_dir(&old).unwrap();
                std::fs::write(old.join("peer"), b"peer").unwrap();
                storage
                    .update(|rows, _| {
                        rows[0].project_path = moved.to_string_lossy().into_owned();
                        Ok(())
                    })
                    .unwrap();
            });
            let mut transaction = Some(transaction);
            let committed =
                structured.then(|| transaction.take().unwrap().begin_irreversible().unwrap());
            let replacement = replace.then(|| {
                let mut row = committed.as_ref().unwrap().request.instance.clone();
                row.title = "replacement".into();
                row
            });
            let identity = shared.then(|| crate::session::acquire_session_identity_lock().unwrap());
            let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                ready_tx.send(()).unwrap();
                let result = match committed {
                    Some(committed) => committed.finish(),
                    None => transaction.unwrap().complete(),
                };
                result_tx.send(result).unwrap();
            });
            ready_rx.recv().unwrap();
            let premature = shared
                .then(|| {
                    result_rx
                        .recv_timeout(std::time::Duration::from_secs(2))
                        .ok()
                })
                .flatten();
            let peer_storage = if replace {
                Storage::open_unwatched(profile).unwrap()
            } else {
                Storage::new_unwatched("scratch-peer").unwrap()
            };
            if shared {
                let _lifecycle = replacement.as_ref().map(|row| {
                    peer_storage
                        .acquire_instance_lifecycle_lock(&row.id)
                        .unwrap()
                });
                peer_storage
                    .update(|rows, _| {
                        rows.push(
                            replacement
                                .unwrap_or_else(|| Instance::new("Peer", moved.to_str().unwrap())),
                        );
                        Ok(())
                    })
                    .unwrap();
            }
            drop(identity);
            let result = premature.unwrap_or_else(|| {
                result_rx
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap()
            });
            worker.join().unwrap();
            assert_eq!(result.disposition, DeletionDisposition::Removed);
            assert_eq!(result.success, !replace, "{:?}", result.errors);
            assert_eq!(
                std::fs::read(old.join("peer")).expect("purge touched the replaced pre-hook path"),
                b"peer"
            );
            if shared {
                assert_eq!(
                    std::fs::read(moved.join("payload")).unwrap(),
                    b"retained scratch data"
                );
            } else {
                assert!(!moved.exists(), "current scratch path was not cleaned");
            }
            let rows = storage.load().unwrap();
            if replace {
                assert_eq!(rows[0].title, "replacement");
                assert!(!result.teardown_started);
            } else {
                assert!(rows.is_empty());
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn purge_retains_resources_after_live_runner_record_disappears() {
        use std::os::unix::process::CommandExt;
        struct ChildGuard(std::process::Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        for irreversible in [true, false] {
            let _home = crate::session::test_support::isolate_app_dir();
            super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap())
                .unwrap();
            let storage = Storage::new_unwatched("purge-runner").unwrap();
            let mut instance = create_test_instance();
            instance.source_profile = "purge-runner".into();
            instance.view = crate::session::View::Structured;
            instance.scratch = true;
            let scratch = crate::session::scratch::provision_scratch_dir(&instance.id).unwrap();
            instance.project_path = scratch.to_string_lossy().into_owned();
            std::fs::write(scratch.join("payload"), b"owned by runner").unwrap();
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let child = ChildGuard(
                std::process::Command::new("sleep")
                    .arg("60")
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
            let record = crate::process::worker_registry::WorkerRecord::new(
                instance.id.clone(),
                child.0.id(),
                crate::process::worker_registry::socket_path_for(&instance.id).unwrap(),
                "test".into(),
                "test".into(),
                scratch.clone(),
                None,
                vec![],
                vec![],
                None,
                Some("purge-runner".into()),
            )
            .with_generation(9);
            crate::process::worker_registry::save(&record).unwrap();
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let PurgeReservation::Reserved(transaction) = PurgeTransaction::reserve(
                Storage::open_unwatched("purge-runner").unwrap(),
                request,
                None,
            )
            .unwrap() else {
                panic!("purge must be reserved")
            };
            let transaction = transaction.run_hooks_with(|_, _| {
                crate::process::worker_registry::delete(&record.session_id).unwrap();
            });
            let result = if irreversible {
                transaction.begin_irreversible().unwrap().finish()
            } else {
                transaction.complete()
            };
            assert!(
                !result.success,
                "registry disappearance does not prove runner exit"
            );
            assert_eq!(
                std::fs::read(scratch.join("payload")).unwrap(),
                b"owned by runner"
            );
            if irreversible {
                assert!(super::super::purge_owners::protection(&storage, None)
                    .unwrap()
                    .iter()
                    .any(|owner| owner.references_path(&scratch)));
            } else {
                assert!(storage
                    .load()
                    .unwrap()
                    .iter()
                    .any(|row| row.id == record.session_id));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn unresolvable_owner_path_prevents_scratch_cleanup() {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir().unwrap();
        super::super::purge_owners::initialize(&root).unwrap();
        let profile = "purge-unresolvable-owner";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = create_test_instance();
        instance.source_profile = profile.into();
        instance.scratch = true;
        let scratch = crate::session::scratch::provision_scratch_dir(&instance.id).unwrap();
        instance.project_path = scratch.to_string_lossy().into_owned();
        std::fs::write(scratch.join("payload"), b"keep").unwrap();
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let alias = root.join("unresolvable");
        std::os::unix::fs::symlink(&alias, &alias).unwrap();
        Storage::new_unwatched("unresolvable-peer")
            .unwrap()
            .update(|rows, _| {
                rows.push(Instance::new(
                    "peer",
                    alias.join("nested").to_str().unwrap(),
                ));
                Ok(())
            })
            .unwrap();
        let request = DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };
        let transaction = match PurgeTransaction::reserve(storage, request, None).unwrap() {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("initial purge rejected"),
        };
        let result = transaction.complete();
        assert_eq!(
            std::fs::read(scratch.join("payload"))
                .expect("unresolvable ownership authorized filesystem cleanup"),
            b"keep"
        );
        assert_eq!(result.disposition, DeletionDisposition::Failed);
        assert!(!result.success);
        assert_eq!(
            Storage::open_unwatched(profile).unwrap().load().unwrap()[0].id,
            result.session_id
        );
    }

    #[test]
    #[serial_test::serial]
    fn group_purge_rejects_changed_selection_without_following_it() {
        for change in [
            "group before claim",
            "generation before claim",
            "wrong profile",
            "group during hooks",
            "group after teardown",
            "unchanged",
        ] {
            let _home = crate::session::test_support::isolate_app_dir();
            super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap())
                .unwrap();
            let profile = "group-purge-selection";
            let storage = Storage::new_unwatched(profile).unwrap();
            let mut instance = create_test_instance();
            instance.source_profile = profile.into();
            let selection = PurgeSelection {
                profile: if change == "wrong profile" {
                    "another-profile"
                } else {
                    profile
                }
                .into(),
                group_path: instance.group_path.clone(),
                lifecycle_generation: instance.lifecycle_generation,
            };
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            storage
                .update(|rows, _| {
                    if change == "group before claim" {
                        rows[0].group_path = "peer-group".into();
                    }
                    if change == "generation before claim" {
                        rows[0].lifecycle_generation += 1;
                    }
                    Ok(())
                })
                .unwrap();
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = match PurgeTransaction::reserve(
                Storage::open_unwatched(profile).unwrap(),
                request,
                Some(selection),
            )
            .unwrap()
            {
                PurgeReservation::Rejected(result) => result,
                PurgeReservation::Reserved(transaction) => {
                    let transaction = transaction.run_hooks_with(|_, _| {
                        if change == "group during hooks" {
                            storage
                                .update(|rows, _| {
                                    rows[0].group_path = "peer-group".into();
                                    Ok(())
                                })
                                .unwrap();
                        }
                    });
                    if change == "group after teardown" {
                        transaction.complete_with(|_| {
                            storage
                                .update(|rows, _| {
                                    rows[0].group_path = "peer-group".into();
                                    Ok(())
                                })
                                .map_err(|error| error.to_string())
                        })
                    } else {
                        match transaction.begin_irreversible() {
                            Err(result) => *result,
                            Ok(committed) => {
                                assert_eq!(
                                    change, "unchanged",
                                    "purge followed a changed selection"
                                );
                                assert_eq!(
                                    committed.finish().disposition,
                                    DeletionDisposition::Removed
                                );
                                assert!(storage.load().unwrap().is_empty());
                                continue;
                            }
                        }
                    }
                }
            };
            assert_ne!(
                change, "unchanged",
                "valid selected generation was rejected after claiming"
            );
            assert_eq!(result.disposition, DeletionDisposition::Busy);
            assert_eq!(result.teardown_started, change == "group after teardown");
            let retained = storage.load().unwrap();
            assert_eq!(retained.len(), 1);
            assert!(!retained[0].has_fresh_lifecycle_reservation(Utc::now()));
            if change.starts_with("group ") {
                assert_eq!(retained[0].group_path, "peer-group");
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn purge_rejects_execution_mode_changes_after_hooks() {
        let _home = crate::session::test_support::isolate_app_dir();
        super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let profile = "purge-mode-after-hooks";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = create_test_instance();
        instance.source_profile = profile.into();
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let request = DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };
        let transaction = match PurgeTransaction::reserve(
            Storage::open_unwatched(profile).unwrap(),
            request,
            None,
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("initial purge rejected"),
        };
        let transaction = transaction.run_hooks_with(|_, _| {
            storage
                .update(|rows, _| {
                    rows[0].view = crate::session::View::Structured;
                    rows[0].status = crate::session::Status::Idle;
                    Ok(())
                })
                .unwrap();
        });
        let result = match transaction.begin_irreversible() {
            Err(result) => result,
            Ok(_) => panic!("purge removed a row whose execution mode changed during hooks"),
        };
        assert_eq!(result.disposition, DeletionDisposition::Busy);
        assert!(!result.teardown_started);
        let retained = storage.load().unwrap();
        assert_eq!(retained.len(), 1);
        assert!(retained[0].is_structured());
        assert_eq!(retained[0].status, crate::session::Status::Idle);
        assert!(!retained[0].has_fresh_lifecycle_reservation(Utc::now()));
    }

    #[test]
    #[serial_test::serial]
    fn recovery_skips_an_impossible_owner_and_finishes_the_next_one() {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir().unwrap();
        super::super::purge_owners::initialize(&root).unwrap();
        let profile = "recovery-head-of-line";
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _| {
                *instances = Vec::new();
                Ok(())
            })
            .unwrap();

        let mut blocked = create_test_instance();
        blocked.title = "blocked owner".into();
        blocked.project_path = "/tmp/blocked-owner".into();
        let _blocked_owner = super::super::purge_owners::PurgeOwner::record(
            &storage,
            &blocked,
            super::super::purge_owners::PurgeCapture::new(&blocked).unwrap(),
        )
        .unwrap();

        let mut recoverable = create_test_instance();
        recoverable.title = "recoverable owner".into();
        recoverable.project_path = "/tmp/recoverable-owner".into();
        let request = DeletionRequest {
            session_id: recoverable.id.clone(),
            instance: recoverable.clone(),
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: true,
            detach_hooks: true,
            keep_scratch: false,
        };
        let recoverable_id = recoverable.id.clone();
        let _recoverable_owner = super::super::purge_owners::PurgeOwner::record_plan(
            &storage,
            &recoverable,
            recoverable.lifecycle_generation,
            Some(&request),
            None,
            super::super::purge_owners::PurgeCapture::new(&recoverable).unwrap(),
        )
        .unwrap();

        let committed = recover_committed_purge(&mut HashSet::new())
            .unwrap()
            .expect("the owner after the impossible legacy plan must be recovered");
        assert_eq!(committed.session_id(), recoverable_id);
        let result = committed.finish_recovered();
        assert!(
            result.success,
            "the recoverable owner must finish: {result:?}"
        );
        assert!(recover_committed_purge(&mut HashSet::new())
            .unwrap()
            .is_none());
        assert_eq!(
            super::super::purge_owners::recovery_plans().unwrap().len(),
            1
        );
    }
    #[test]
    #[serial_test::serial]
    fn a_failed_owner_is_never_reselected_within_one_recovery_pass() {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir().unwrap();
        super::super::purge_owners::initialize(&root).unwrap();
        let profile = "recovery-single-attempt-per-pass";
        let storage = Storage::new_unwatched(profile).unwrap();
        storage
            .update(|instances, _| {
                *instances = Vec::new();
                Ok(())
            })
            .unwrap();

        let mut blocked = create_test_instance();
        blocked.title = "blocked owner".into();
        blocked.project_path = "/tmp/blocked-single-attempt".into();
        let _blocked_owner = super::super::purge_owners::PurgeOwner::record(
            &storage,
            &blocked,
            super::super::purge_owners::PurgeCapture::new(&blocked).unwrap(),
        )
        .unwrap();

        let mut pending = create_test_instance();
        pending.title = "pending owner".into();
        pending.project_path = "/tmp/pending-single-attempt".into();
        let request = DeletionRequest {
            session_id: pending.id.clone(),
            instance: pending.clone(),
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: true,
            detach_hooks: true,
            keep_scratch: false,
        };
        let _pending_owner = super::super::purge_owners::PurgeOwner::record_plan(
            &storage,
            &pending,
            pending.lifecycle_generation,
            Some(&request),
            None,
            super::super::purge_owners::PurgeCapture::new(&pending).unwrap(),
        )
        .unwrap();

        let mut attempted = HashSet::new();
        let committed = recover_committed_purge(&mut attempted)
            .unwrap()
            .expect("the recoverable owner is selected on the first pass");
        assert_eq!(committed.session_id(), request.session_id);
        let pending_token = committed.owner_token().to_owned();
        // Release the identity and lifecycle locks this selection took, so a
        // later pass can take them again.
        drop(committed);
        // A caller whose cleanup stage fails records the attempt, exactly as the
        // server recovery pass does, so the same owner is never reselected.
        attempted.insert(pending_token.clone());
        assert!(
            recover_committed_purge(&mut attempted).unwrap().is_none(),
            "an attempted owner must not be handed out again inside the same pass"
        );
        assert_eq!(
            attempted.len(),
            2,
            "both durable owners were attempted exactly once"
        );
        assert_eq!(
            super::super::purge_owners::recovery_plans().unwrap().len(),
            2,
            "a failed recovery attempt retains its durable owner"
        );
        // A later pass starts from a fresh attempt set and retries the owner.
        let retried = recover_committed_purge(&mut HashSet::new())
            .unwrap()
            .expect("a later pass retries the retained owner");
        assert_eq!(retried.owner_token(), pending_token);
    }
    #[test]
    #[serial_test::serial]
    fn irreversible_purge_retains_references_removed_by_destroy_hook() {
        let _home = crate::session::test_support::isolate_app_dir();
        let root = crate::session::get_app_dir().unwrap();
        super::super::purge_owners::initialize(&root).unwrap();
        let real = root.join("real-repository");
        let alias = root.join("repository-alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let profile = "purge-hook-alias-loss";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = create_test_instance();
        instance.source_profile = profile.into();
        instance.project_path = alias.join("checkout").to_string_lossy().into_owned();
        instance.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "retained-branch".into(),
            main_repo_path: alias.to_string_lossy().into_owned(),
            managed_by_aoe: false,
            created_at: Utc::now(),
            base_branch: None,
        });
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let session_id = instance.id.clone();
        let request = DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: false,
            keep_scratch: false,
        };
        let transaction = match PurgeTransaction::reserve(
            Storage::open_unwatched(profile).unwrap(),
            request,
            None,
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("initial purge rejected"),
        };
        let transaction = transaction.run_hooks_with(|_, _| {
            std::fs::remove_file(&alias).unwrap();
        });
        let committed = match transaction.begin_irreversible() {
            Ok(committed) => committed,
            Err(_) => panic!("irreversible purge rejected"),
        };
        drop(committed);
        assert!(storage.load().unwrap().is_empty());
        let protection = super::super::purge_owners::protection(&storage, None).unwrap();
        assert!(
            protection
                .iter()
                .any(|owner| owner.references_path(&real.join("checkout"))),
            "an interrupted purge lost its checkout ownership when the hook removed its alias"
        );
        assert!(
            protection
                .iter()
                .any(|owner| owner.references_branch(&real, "retained-branch")),
            "an interrupted purge lost its branch ownership when the hook removed its alias"
        );

        let mut reused = create_test_instance();
        reused.id = session_id;
        reused.source_profile = profile.into();
        storage
            .update(|rows, _| {
                rows.push(reused.clone());
                Ok(())
            })
            .unwrap();
        assert!(recover_committed_purge(&mut HashSet::new())
            .unwrap()
            .is_none());
        assert!(!super::super::purge_owners::protection(&storage, None)
            .unwrap()
            .is_empty());
        storage
            .update(|rows, _| {
                rows.retain(|row| row.id != reused.id);
                Ok(())
            })
            .unwrap();

        let recovered = recover_committed_purge(&mut HashSet::new())
            .unwrap()
            .expect("durable purge retry");
        let result = recovered.finish_recovered();
        assert!(result.success);
        assert!(super::super::purge_owners::protection(&storage, None)
            .unwrap()
            .is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn creation_rollback_keeps_post_provision_cleanup_flags() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "creation-provisioned-authority";
        let storage = Storage::new_unwatched(profile).unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let workspace = temp.path().join("workspace");
        let checkout = workspace.join("repo");
        let mut stored = Instance::new("creation provisioned", workspace.to_str().unwrap());
        stored.source_profile = profile.into();
        stored.status = crate::session::Status::Starting;
        stored.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "work".into(),
            workspace_dir: workspace.to_string_lossy().into_owned(),
            created_at: Utc::now(),
            cleanup_on_delete: false,
            repos: vec![crate::session::WorkspaceRepo {
                source_path: repo.to_string_lossy().into_owned(),
                name: "repo".into(),
                worktree_path: checkout.to_string_lossy().into_owned(),
                main_repo_path: repo.to_string_lossy().into_owned(),
                branch: "work".into(),
                managed_by_aoe: false,
                branch_preexisting: true,
                base_branch: None,
                base_branch_override: None,
            }],
        });
        let generation = stored
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )
            .unwrap();
        storage
            .update(|rows, _| {
                rows.push(stored.clone());
                Ok(())
            })
            .unwrap();
        let mut built = stored;
        built.workspace_info.as_mut().unwrap().cleanup_on_delete = true;
        built.workspace_info.as_mut().unwrap().repos[0].managed_by_aoe = true;
        built.workspace_info.as_mut().unwrap().repos[0].branch_preexisting = false;
        let request = DeletionRequest {
            session_id: built.id.clone(),
            instance: built,
            delete_worktree: true,
            delete_branch: true,
            delete_sandbox: false,
            force_delete: true,
            detach_hooks: true,
            keep_scratch: true,
        };

        let transaction = match PurgeTransaction::reserve_failed_creation(
            Storage::open_unwatched(profile).unwrap(),
            request,
            generation,
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("rollback rejected: {:?}", result.errors),
        };
        let workspace = transaction.instance().workspace_info.as_ref().unwrap();
        assert!(workspace.cleanup_on_delete);
        assert!(workspace.repos[0].managed_by_aoe);
        assert!(!workspace.repos[0].branch_preexisting);
    }

    #[test]
    #[serial_test::serial]
    fn creation_rollback_preserves_a_different_launch_owner() {
        let _guard = crate::session::test_support::isolate_app_dir();
        super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let profile = "creation-rollback";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = create_test_instance();
        instance.source_profile = profile.into();
        let scratch = crate::session::scratch::provision_scratch_dir(&instance.id).unwrap();
        instance.project_path = scratch.to_string_lossy().into_owned();
        instance.scratch = true;
        instance.status = crate::session::Status::Starting;
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )
            .unwrap();
        let marker = scratch.join("owned");
        std::fs::write(&marker, "creation resource").unwrap();
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let request = || DeletionRequest {
            session_id: instance.id.clone(),
            instance: instance.clone(),
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: true,
            detach_hooks: true,
            keep_scratch: false,
        };
        let rejected = PurgeTransaction::reserve_failed_creation(
            Storage::open_unwatched(profile).unwrap(),
            request(),
            generation + 1,
        )
        .unwrap();
        match rejected {
            PurgeReservation::Rejected(result) => {
                assert_eq!(result.disposition, DeletionDisposition::Busy)
            }
            PurgeReservation::Reserved(_) => panic!("foreign creation acquired rollback ownership"),
        }
        assert!(marker.exists());
        assert!(storage.load().unwrap()[0]
            .lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation));
        let owned = PurgeTransaction::reserve_failed_creation(
            Storage::open_unwatched(profile).unwrap(),
            request(),
            generation,
        )
        .unwrap();
        let result = match owned {
            PurgeReservation::Reserved(transaction) => transaction.complete(),
            PurgeReservation::Rejected(_) => {
                panic!("creation could not roll back its own resources")
            }
        };
        assert_eq!(result.disposition, DeletionDisposition::Removed);
        assert!(storage.load().unwrap().is_empty());
        assert!(!scratch.exists());
    }

    /// A post-launch failure releases the launch reservation before the
    /// rollback runs, so requiring reservation ownership would reject the
    /// rollback of the very row the creation published and strand every
    /// resource it provisioned.
    #[test]
    #[serial_test::serial]
    fn creation_rollback_survives_the_released_launch_reservation() {
        let _guard = crate::session::test_support::isolate_app_dir();
        super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let profile = "creation-rollback-post-launch";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = create_test_instance();
        instance.source_profile = profile.into();
        let scratch = crate::session::scratch::provision_scratch_dir(&instance.id).unwrap();
        instance.project_path = scratch.to_string_lossy().into_owned();
        instance.scratch = true;
        instance.status = crate::session::Status::Starting;
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )
            .unwrap();
        let marker = scratch.join("owned");
        std::fs::write(&marker, "creation resource").unwrap();
        let mut published = instance.clone();
        assert!(published
            .release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation));
        storage
            .update(|rows, _| {
                rows.push(published.clone());
                Ok(())
            })
            .unwrap();
        let result = match PurgeTransaction::reserve_failed_creation(
            Storage::open_unwatched(profile).unwrap(),
            DeletionRequest {
                session_id: published.id.clone(),
                instance: published,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
            },
            generation,
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction.complete(),
            PurgeReservation::Rejected(result) => panic!(
                "a released launch reservation must not block the rollback: {:?}",
                result.errors
            ),
        };
        assert_eq!(result.disposition, DeletionDisposition::Removed);
        assert!(storage.load().unwrap().is_empty());
        assert!(
            !scratch.exists(),
            "the failed creation left no scratch behind"
        );
    }

    /// The weaker proof must stay fail-closed: once another owner has taken
    /// the row under a newer generation, the failing creation owns nothing
    /// and its rollback is refused.
    #[test]
    #[serial_test::serial]
    fn creation_rollback_refuses_a_row_a_later_owner_took() {
        let _guard = crate::session::test_support::isolate_app_dir();
        super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let profile = "creation-rollback-later-owner";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = create_test_instance();
        instance.source_profile = profile.into();
        let scratch = crate::session::scratch::provision_scratch_dir(&instance.id).unwrap();
        instance.project_path = scratch.to_string_lossy().into_owned();
        instance.scratch = true;
        instance.status = crate::session::Status::Starting;
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )
            .unwrap();
        let marker = scratch.join("owned");
        std::fs::write(&marker, "creation resource").unwrap();
        let mut taken = instance.clone();
        taken.release_lifecycle_reservation_if_owned(LifecycleOperation::Launch, generation);
        let foreign = taken
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Stop,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )
            .unwrap();
        storage
            .update(|rows, _| {
                rows.push(taken.clone());
                Ok(())
            })
            .unwrap();
        let rejected = PurgeTransaction::reserve_failed_creation(
            Storage::open_unwatched(profile).unwrap(),
            DeletionRequest {
                session_id: taken.id.clone(),
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: true,
                instance,
                detach_hooks: true,
                keep_scratch: false,
            },
            generation,
        )
        .unwrap();
        match rejected {
            PurgeReservation::Rejected(result) => {
                assert_eq!(result.disposition, DeletionDisposition::Busy)
            }
            PurgeReservation::Reserved(_) => {
                panic!("a superseded creation took rollback ownership from generation {foreign}")
            }
        }
        assert!(marker.exists());
        assert!(storage.load().unwrap()[0]
            .lifecycle_reservation_is_owned(LifecycleOperation::Stop, foreign));
    }

    #[test]
    #[serial_test::serial]
    fn purge_transaction_generation_gate_and_durable_commit() {
        let _guard = crate::session::test_support::isolate_app_dir();
        super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let profile = "purge-generation-gate";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = create_test_instance();
        instance.source_profile = profile.to_string();
        let id = instance.id.clone();
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let request = DeletionRequest {
            session_id: id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };
        let transaction = match PurgeTransaction::reserve(
            Storage::open_unwatched(profile).unwrap(),
            request,
            None,
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("initial reservation was refused"),
        };
        storage
            .update(|instances, _groups| {
                instances[0].lifecycle_generation += 1;
                Ok(())
            })
            .unwrap();

        let result = match transaction.begin_irreversible() {
            Ok(_) => panic!("superseded purge crossed the irreversible boundary"),
            Err(result) => *result,
        };
        assert_eq!(result.disposition, DeletionDisposition::Busy);
        assert!(!result.teardown_started);
        let retained = storage.load().unwrap();
        assert_eq!(retained.len(), 1);
        assert!(!retained[0].has_fresh_lifecycle_reservation(Utc::now()));

        let mut retry = retained.into_iter().next().unwrap();
        retry.source_profile = profile.to_string();
        let retry_request = DeletionRequest {
            session_id: id,
            instance: retry,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };
        let retry = match PurgeTransaction::reserve(
            Storage::open_unwatched(profile).unwrap(),
            retry_request,
            None,
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("retry reservation was refused"),
        };
        let committed = match retry.begin_irreversible() {
            Ok(committed) => committed,
            Err(_) => panic!("current purge reservation was rejected"),
        };
        assert!(
            storage.load().unwrap().is_empty(),
            "durable row must be gone before irreversible cleanup starts"
        );
        let result = committed.finish();
        assert_eq!(result.disposition, DeletionDisposition::Removed);
    }

    #[test]
    #[serial_test::serial]
    fn on_destroy_hooks_run_without_the_instance_lifecycle_flock() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        super::super::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
        let profile = "purge-unlocked-hooks";
        let ready = temp.path().join("ready");
        let release = temp.path().join("release");

        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = Instance::new("purge unlocked hooks", temp.path().to_str().unwrap());
        instance.source_profile = profile.to_string();
        let id = instance.id.clone();
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let request = DeletionRequest {
            session_id: id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };
        let transaction = match PurgeTransaction::reserve(
            Storage::open_unwatched(profile).unwrap(),
            request,
            None,
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge reservation was refused"),
        };

        let (purge_tx, purge_rx) = std::sync::mpsc::channel();
        let ready_for_hook = ready.clone();
        let release_for_hook = release.clone();
        let purge = std::thread::spawn(move || {
            let after_hooks = transaction.run_hooks_with(|_, _| {
                std::fs::write(&ready_for_hook, b"ready").unwrap();
                while !release_for_hook.exists() {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
            });
            purge_tx.send(after_hooks.complete()).unwrap();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !ready.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready.exists(), "on_destroy hook did not start");

        let lock_storage = Storage::open_unwatched(profile).unwrap();
        let release_for_lock = release.clone();
        let (lock_tx, lock_rx) = std::sync::mpsc::channel();
        let lock = std::thread::spawn(move || {
            let identity = crate::session::acquire_session_identity_lock().unwrap();
            let guard = lock_storage.acquire_instance_lifecycle_lock(&id).unwrap();
            drop(guard);
            drop(identity);
            std::fs::write(release_for_lock, b"release").unwrap();
            lock_tx.send(()).unwrap();
        });
        let acquired = lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok();
        if !acquired {
            std::fs::write(&release, b"release").unwrap();
        }

        let result = purge_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        purge.join().unwrap();
        lock.join().unwrap();
        assert!(acquired, "on_destroy hook held the lifecycle flock");
        assert_eq!(result.disposition, DeletionDisposition::Removed);
        assert!(storage.load().unwrap().is_empty());
    }

    /// #4107: a purge keeps a shared worktree when another profile cannot be read, and a session
    /// adopting the worktree after the ownership scan cannot see it removed.
    #[test]
    #[serial_test::serial]
    fn purge_keeps_a_worktree_it_cannot_prove_unused() {
        for adopt_after_scan in [false, true] {
            let (tmp, main_repo, worktree, mut owner) = test_worktree_fixture("feature/shared");
            let _home = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("home"));
            let storage = Storage::new_unwatched("owner").unwrap();
            owner.source_profile = "owner".to_string();
            crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap())
                .unwrap();
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
                std::fs::write(other.sessions_path(), "not json").unwrap();
                None
            };

            let transaction = match PurgeTransaction::reserve(
                storage,
                DeletionRequest {
                    delete_worktree: true,
                    delete_branch: true,
                    ..test_request(owner)
                },
                None,
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("purge reservation was refused"),
            };
            let result = transaction.complete();
            if adopt_after_scan {
                assert_eq!(
                    result.disposition,
                    DeletionDisposition::Removed,
                    "{:?}",
                    result.errors
                );
                let writer = writer.expect("adoption race installs a writer");
                let adopted_while_present = writer.recv().unwrap().join().unwrap();
                assert!(
                    !adopted_while_present || worktree.exists(),
                    "a worktree adopted after the scan was removed"
                );
            } else {
                assert_eq!(result.disposition, DeletionDisposition::Failed);
                assert!(!result.success, "{:?}", result.errors);
                assert!(
                    worktree.exists(),
                    "worktree removed despite unreadable profile"
                );
                assert!(test_branch_exists(&main_repo, "feature/shared"));
            }
        }
    }

    #[test]
    fn test_deletion_result_success_even_with_delete_worktree_flag_when_no_worktree() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let instance = create_test_instance();
        let request = DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: true,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };

        let result = perform_deletion(&request);

        assert!(result.success);
        assert!(result.errors.is_empty());
    }

    fn workspace_info(
        workspace_dir: &str,
        worktree_paths: &[&str],
    ) -> crate::session::WorkspaceInfo {
        crate::session::WorkspaceInfo {
            branch: "feature/abc".to_string(),
            workspace_dir: workspace_dir.to_string(),
            repos: worktree_paths
                .iter()
                .enumerate()
                .map(|(i, wt)| crate::session::WorkspaceRepo {
                    name: format!("repo-{i}"),
                    source_path: format!("/src/repo-{i}"),
                    branch: "feature/abc".to_string(),
                    worktree_path: wt.to_string(),
                    main_repo_path: format!("/src/repo-{i}"),
                    managed_by_aoe: true,
                    branch_preexisting: false,
                    base_branch: None,
                    base_branch_override: None,
                })
                .collect(),
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        }
    }

    /// The layout `create_workspace` produces: every repo in a subdirectory of
    /// the workspace dir. Only this shape is eligible for removal.
    #[test]
    fn workspace_dir_owned_when_repos_sit_underneath_it() {
        assert!(workspace_dir_is_aoe_owned(&workspace_info(
            "/tmp/ws",
            &["/tmp/ws/backend", "/tmp/ws/frontend"]
        )));
    }

    /// The shape a synthesized `WorkspaceInfo` would have had for a session
    /// whose `project_path` is the user's own checkout: `workspace_dir` IS the
    /// repo worktree rather than a directory above it. Treating it as an
    /// aoe-owned workspace would target the user's checkout, so the guard has
    /// to refuse.
    #[test]
    fn workspace_dir_not_owned_when_it_is_itself_a_worktree() {
        assert!(!workspace_dir_is_aoe_owned(&workspace_info(
            "/home/u/backend",
            &["/home/u/backend"]
        )));
    }

    /// A workspace dir that does not actually contain one of its repos was not
    /// laid out by the builder, so its provenance is unknown.
    #[test]
    fn workspace_dir_not_owned_when_a_repo_lives_outside_it() {
        assert!(!workspace_dir_is_aoe_owned(&workspace_info(
            "/tmp/ws",
            &["/tmp/ws/backend", "/elsewhere/frontend"]
        )));
        // No repos at all proves nothing about the directory.
        assert!(!workspace_dir_is_aoe_owned(&workspace_info("/tmp/ws", &[])));
    }

    mod container_removal {
        use super::*;
        use crate::containers::error::DockerError;
        use crate::containers::Teardown;

        #[test]
        fn failure_is_recorded_as_error() {
            let mut messages = Vec::new();
            let mut errors = Vec::new();
            deletion_messages_for(
                Teardown::Failed(DockerError::RemoveFailed("daemon busy".into())),
                &mut messages,
                &mut errors,
            );
            assert_eq!(
                errors.len(),
                1,
                "a removal failure must be surfaced so the caller keeps the session record"
            );
            assert!(errors[0].contains("Container"));
            assert!(messages.is_empty());
        }

        #[test]
        fn removed_records_message() {
            let mut messages = Vec::new();
            let mut errors = Vec::new();
            deletion_messages_for(Teardown::Removed, &mut messages, &mut errors);
            assert_eq!(messages, vec!["Container removed".to_string()]);
            assert!(errors.is_empty());
        }

        #[test]
        fn already_gone_is_silent() {
            let mut messages = Vec::new();
            let mut errors = Vec::new();
            deletion_messages_for(Teardown::AlreadyGone, &mut messages, &mut errors);
            assert!(
                errors.is_empty(),
                "an already-gone container is idempotent, not a failure"
            );
            assert!(
                messages.is_empty(),
                "no spurious 'removed' message when nothing was removed"
            );
        }

        fn sandboxed_request() -> DeletionRequest {
            use crate::session::SandboxInfo;
            let mut instance = create_test_instance();
            instance.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image: "alpine".to_string(),
                container_name: "aoe-sandbox-calltest".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });
            DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: true,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            }
        }

        #[test]
        fn call_site_surfaces_teardown_failure() {
            // A failed container teardown must surface as an error so the
            // caller keeps the session record, not silently succeed. Pins the
            // `perform_deletion` call site, not just the mapping helper.
            let request = sandboxed_request();
            let result = perform_deletion_with(&request, |_id| {
                Teardown::Failed(DockerError::RemoveFailed("daemon busy".into()))
            });
            assert!(!result.success, "a teardown failure must fail the deletion");
            assert!(result.errors.iter().any(|e| e.contains("Container")));
        }

        #[test]
        fn call_site_invokes_teardown_unconditionally() {
            // Guards against re-introducing an existence-probe gate around the
            // teardown: it must run whenever the session is sandboxed and
            // delete_sandbox is set.
            use std::cell::Cell;
            let request = sandboxed_request();
            let called = Cell::new(false);
            let result = perform_deletion_with(&request, |_id| {
                called.set(true);
                Teardown::Removed
            });
            assert!(
                called.get(),
                "teardown must be invoked unconditionally, never gated behind a probe"
            );
            assert!(result.success);
        }
        #[test]
        #[serial_test::serial]
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
                let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
                let mut request = sandboxed_request();
                if case == Case::PreTransition {
                    request.instance.sandbox_store_generation = 0;
                }
                if case == Case::FailsAfterTeardown {
                    request.delete_worktree = true;
                    request.instance.worktree_info = Some(test_worktree_info(
                        "feature/x",
                        &temp.path().join("not-a-repo"),
                    ));
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

        /// The store holds the agent's credentials and is named by an id that
        /// stops resolving with the purge, so nothing would ever open it
        /// again and nothing else will find it.
        #[test]
        #[serial_test::serial]
        fn call_site_removes_the_session_agent_store() {
            let temp = tempfile::TempDir::new().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let request = sandboxed_request();
            let store = temp
                .path()
                .join(".claude")
                .join("sandbox-v2")
                .join(&request.instance.id);
            std::fs::create_dir_all(&store).unwrap();
            std::fs::write(store.join(".credentials.json"), b"token").unwrap();
            crate::migrations::v033_isolate_sandbox_content::certify_owned_test_root(
                &crate::session::get_app_dir().unwrap(),
                &request.instance.id,
                &store,
            )
            .unwrap();

            let result = perform_deletion_with(&request, |_id| Teardown::Removed);

            assert!(!store.exists(), "purge left the session's agent store");
            assert!(result.success, "{:?}", result.errors);
            assert!(result.messages.iter().any(|m| m.contains("Agent store")));
        }

        /// A session still on the shared legacy store owns no per-instance
        /// directory, and v027 may be publishing the one it will own.
        #[test]
        #[serial_test::serial]
        fn a_pre_transition_session_leaves_its_store_to_the_migration() {
            let temp = tempfile::TempDir::new().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let mut request = sandboxed_request();
            request.instance.sandbox_store_generation = 0;
            let store = temp
                .path()
                .join(".claude")
                .join("sandbox-v2")
                .join(&request.instance.id);
            std::fs::create_dir_all(&store).unwrap();

            perform_deletion_with(&request, |_id| Teardown::Removed);

            assert!(store.exists());
        }

        /// A teardown that failed leaves the container, and the purge is
        /// rolled back, so the session keeps running on the store it still
        /// has mounted.
        #[test]
        #[serial_test::serial]
        fn a_failed_teardown_leaves_the_store_for_the_reclaim_pass() {
            let temp = tempfile::TempDir::new().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let mut request = sandboxed_request();
            let scratch = crate::session::get_app_dir()
                .unwrap()
                .join("scratch")
                .join(&request.instance.id);
            std::fs::create_dir_all(&scratch).unwrap();
            std::fs::write(scratch.join("payload"), b"retained").unwrap();
            request.instance.project_path = scratch.to_string_lossy().into_owned();
            request.instance.scratch = true;
            let store = temp
                .path()
                .join(".claude")
                .join("sandbox-v2")
                .join(&request.instance.id);
            std::fs::create_dir_all(&store).unwrap();
            std::fs::write(store.join(".credentials.json"), b"token").unwrap();

            let result = perform_deletion_with(&request, |_id| {
                Teardown::Failed(crate::containers::error::DockerError::DaemonNotRunning)
            });

            assert!(store.exists(), "a failed teardown took the store with it");
            assert_eq!(
                std::fs::read(scratch.join("payload")).unwrap(),
                b"retained",
                "a failed container teardown must retain its mounted scratch files"
            );
            assert!(!result.success);
        }

        /// A purge that fails after the container came down is rolled back by
        /// `PurgeTransaction::complete_inner`, so the session survives. Its
        /// store must survive with it, or the session is left logged out with
        /// its history gone.
        #[test]
        #[serial_test::serial]
        fn a_purge_that_fails_after_teardown_leaves_the_store() {
            let temp = tempfile::TempDir::new().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let mut request = sandboxed_request();
            request.delete_worktree = true;
            request.instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feature/x".to_string(),
                main_repo_path: temp.path().join("not-a-repo").display().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
            let store = temp
                .path()
                .join(".claude")
                .join("sandbox-v2")
                .join(&request.instance.id);
            std::fs::create_dir_all(&store).unwrap();
            std::fs::write(store.join(".credentials.json"), b"token").unwrap();

            let result = perform_deletion_with(&request, |_id| Teardown::Removed);

            assert!(!result.success, "{:?}", result.messages);
            assert!(
                store.exists(),
                "a purge that will be rolled back took the store with it: {:?}",
                result.errors
            );
        }
    }

    mod ordering {
        use super::*;
        use crate::session::SandboxInfo;
        use std::sync::{Arc, Mutex};
        use tracing::field::{Field, Visit};
        use tracing::subscriber::with_default;
        use tracing::Subscriber;
        use tracing_subscriber::layer::{Context, SubscriberExt};
        use tracing_subscriber::registry::LookupSpan;
        use tracing_subscriber::Layer;

        /// tracing Layer that captures the `stage` field value of every
        /// `perform_deletion: stage` event in order of emission.
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
                struct V {
                    msg: Option<String>,
                    stage: Option<String>,
                }
                impl Visit for V {
                    fn record_str(&mut self, field: &Field, value: &str) {
                        match field.name() {
                            "stage" => self.stage = Some(value.to_string()),
                            "message" => self.msg = Some(value.to_string()),
                            _ => {}
                        }
                    }
                    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
                        let rendered = format!("{:?}", value);
                        let unquoted = rendered.trim_matches('"').to_string();
                        match field.name() {
                            "stage" => self.stage = Some(unquoted),
                            "message" => self.msg = Some(unquoted),
                            _ => {}
                        }
                    }
                }
                let mut v = V {
                    msg: None,
                    stage: None,
                };
                event.record(&mut v);
                if v.msg.as_deref() == Some("perform_deletion: stage") {
                    if let Some(stage) = v.stage {
                        self.stages.lock().unwrap().push(stage);
                    }
                }
            }
        }

        fn run_with_capture<F: Fn()>(f: F) -> Vec<String> {
            let stages = Arc::new(Mutex::new(Vec::new()));
            let layer = StageRecorder {
                stages: Arc::clone(&stages),
            };
            let subscriber = tracing_subscriber::registry().with(layer);
            with_default(subscriber, || {
                // Tracing's per-callsite `Interest` is cached globally on first
                // hit. `rebuild_interest_cache()` only re-evaluates callsites
                // that are *already* registered, so any stage callsite not yet
                // hit at this point can still lose a registration race to a
                // parallel test running `perform_deletion` without a subscriber
                // (the sibling tests at lines 335/354/372 do exactly this).
                // If they win, the callsite is cached as `Interest::never()`
                // and our subscriber never sees that one event, while the
                // other stages still come through. The fix is a two-pass run:
                //   1. Warmup pass: invoke f() once while we're the default,
                //      forcing the callsites to register under our subscriber
                //      (or be re-evaluated to Always if already registered).
                //   2. Clear captured stages, rebuild interest cache to fix up
                //      anything that lost a race during warmup, then run f()
                //      again as the measured pass.
                f();
                stages.lock().unwrap().clear();
                tracing::callsite::rebuild_interest_cache();
                f();
            });
            let g = stages.lock().unwrap();
            g.clone()
        }

        /// Index of the first occurrence of `needle` in `stages`. Panics
        /// with a descriptive message if absent so test failures point
        /// at the missing stage instead of an inscrutable `unwrap`.
        fn idx(stages: &[String], needle: &str) -> usize {
            stages
                .iter()
                .position(|s| s == needle)
                .unwrap_or_else(|| panic!("stage {:?} missing from {:?}", needle, stages))
        }

        /// Regression: sandboxed + worktree deletion must drop the
        /// container BEFORE touching the worktree directory. The old
        /// order (worktree first) raced the still-running in-container
        /// agent and produced flaky permission errors and dirty-tree
        /// failures.
        #[test]
        fn sandboxed_with_worktree_kills_tmux_and_container_before_worktree() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            // Exercise sandbox + worktree stage ordering with an explicitly
            // absent container. A nonexistent name alone is insufficient:
            // an unavailable runtime fails teardown and correctly prevents
            // worktree cleanup.
            let mut instance = Instance::new("Test", "/tmp/aoe-deletion-test-nonexistent");
            instance.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image: "alpine".to_string(),
                container_name: "aoe-sandbox-doesnotexist".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: true,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let stages = run_with_capture(|| {
                let result =
                    perform_deletion_with(&request, |_id| crate::containers::Teardown::AlreadyGone);
                assert!(result.success, "{:?}", result.errors);
            });

            // tmux_kill must precede container_remove must precede
            // worktree_remove must precede branch_delete.
            let i_tmux = idx(&stages, "tmux_kill");
            let i_container = idx(&stages, "container_remove");
            let i_worktree = idx(&stages, "worktree_remove");
            let i_branch = idx(&stages, "branch_delete");

            assert!(
                i_tmux < i_container,
                "tmux must be killed before container removal: stages={:?}",
                stages
            );
            assert!(
                i_container < i_worktree,
                "container must be removed before worktree cleanup: stages={:?}",
                stages
            );
            assert!(
                i_worktree < i_branch,
                "worktree must be cleaned before branch delete: stages={:?}",
                stages
            );

            // Sandboxed + delete_worktree: we should also see the
            // in-container preclean stage between tmux_kill and
            // container_remove.
            let i_preclean = idx(&stages, "sandbox_worktree_preclean");
            assert!(
                i_tmux < i_preclean && i_preclean < i_container,
                "preclean must run after tmux kill and before container remove: stages={:?}",
                stages
            );
        }

        /// End-to-end-on-disk: build a real git repo + worktree on the
        /// filesystem (no docker, no tmux session), call
        /// `perform_deletion(delete_worktree=true)`, and verify the
        /// worktree directory and `.git/worktrees/<name>` admin entry
        /// are gone afterwards. This is the closest we can get to an
        /// e2e test for the worktree-delete path without a real
        /// container runtime.
        #[test]
        fn e2e_real_worktree_is_removed_on_disk() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            // init main repo with one commit so branches can be created
            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = {
                let mut index = repo.index().unwrap();
                index.write_tree().unwrap()
            };
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            // create the worktree on a new branch via real `git` so the
            // admin files match what aoe creates in production
            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    "feature/delete-me",
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "git worktree add failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
            assert!(worktree_path.exists());
            assert!(
                main_repo.join(".git/worktrees/worktree").exists(),
                "worktree admin dir should exist before deletion"
            );

            // construct an Instance matching what builder would produce
            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feature/delete-me".to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let result = perform_deletion(&request);
            assert!(
                result.success,
                "perform_deletion failed: {:?}",
                result.errors
            );

            // worktree directory and its admin entry must be gone
            assert!(
                !worktree_path.exists(),
                "worktree dir should be removed after delete"
            );
            assert!(
                !main_repo.join(".git/worktrees/worktree").exists(),
                "worktree admin dir should be pruned after delete"
            );

            // branch should be deleted
            let branches_out = std::process::Command::new("git")
                .args(["branch", "--list", "feature/delete-me"])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&branches_out.stdout)
                    .trim()
                    .is_empty(),
                "branch should be deleted: stdout={}",
                String::from_utf8_lossy(&branches_out.stdout)
            );
        }

        /// Regression for #3215, in the shape that loses the most: the
        /// bare-repo layout, where the repo's default branch is checked out as
        /// a linked worktree that sibling tooling expects to stay put. Deleting
        /// the session used to remove that checkout and then delete the branch,
        /// leaving the bare repo's HEAD pointing at a ref that no longer
        /// existed. `force_delete` is set because that is what trash
        /// auto-purge and `empty-trash` pass, and it used to bypass every
        /// existing protection.
        #[test]
        fn default_branch_worktree_survives_a_forced_delete() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
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

            let out = std::process::Command::new("git")
                .args(["worktree", "add", worktree_path.to_str().unwrap(), "main"])
                .current_dir(&bare)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git worktree add failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(worktree_path.exists());

            let mut instance = Instance::new("Infra", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "main".to_string(),
                main_repo_path: bare.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });

            let result = perform_deletion(&DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
            });

            // Success matters as much as the preservation: a failure would keep
            // the row, and auto-purge would retry the same refusal every hour.
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
            assert!(
                worktree_path.exists(),
                "the default branch's checkout must survive"
            );

            let branches = std::process::Command::new("git")
                .args(["branch", "--list", "main"])
                .current_dir(&bare)
                .output()
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
                "the default branch itself must survive"
            );

            let head = std::process::Command::new("git")
                .args(["symbolic-ref", "HEAD"])
                .current_dir(&bare)
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8_lossy(&head.stdout).trim(),
                "refs/heads/main",
                "the bare repo's HEAD must still resolve"
            );
        }

        /// Init a repo with one commit so branches and worktrees can be made.
        fn init_repo(path: &std::path::Path) {
            std::fs::create_dir_all(path).unwrap();
            let repo = git2::Repository::init(path).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = {
                let mut index = repo.index().unwrap();
                index.write_tree().unwrap()
            };
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();
        }

        fn git_in(dir: &std::path::Path, args: &[&str]) {
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
        }
        #[test]
        #[serial_test::serial]
        fn purge_preserves_workspace_repos_referenced_by_surviving_sessions() {
            let _home = crate::session::test_support::isolate_app_dir();
            crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap())
                .unwrap();
            for reference in ["checkout", "branch", "workspace root"] {
                let tmp = tempfile::TempDir::new().unwrap();
                let workspace = tmp.path().join("workspace");
                std::fs::create_dir(&workspace).unwrap();
                let mut repos = Vec::new();
                for name in ["shared", "independent"] {
                    let main = tmp.path().join(format!("main-{name}"));
                    let worktree = workspace.join(name);
                    init_repo(&main);
                    git_in(
                        &main,
                        &["worktree", "add", "-b", "work", worktree.to_str().unwrap()],
                    );
                    repos.push(crate::session::WorkspaceRepo {
                        name: name.into(),
                        source_path: main.to_string_lossy().into_owned(),
                        branch: "work".into(),
                        worktree_path: worktree.to_string_lossy().into_owned(),
                        main_repo_path: main.to_string_lossy().into_owned(),
                        managed_by_aoe: true,
                        branch_preexisting: false,
                        base_branch: None,
                        base_branch_override: None,
                    });
                }
                let mut owner = Instance::new("owner", workspace.to_str().unwrap());
                owner.source_profile = "cleanup-owner".into();
                owner.workspace_info = Some(crate::session::WorkspaceInfo {
                    branch: "work".into(),
                    workspace_dir: workspace.to_string_lossy().into_owned(),
                    repos: repos.clone(),
                    created_at: Utc::now(),
                    cleanup_on_delete: true,
                });
                let mut peer = Instance::new(
                    "peer",
                    if reference == "checkout" {
                        &repos[0].worktree_path
                    } else {
                        workspace.to_str().unwrap()
                    },
                );
                if reference == "checkout" {
                    std::fs::write(
                        Path::new(&repos[0].worktree_path).join("peer-data"),
                        b"keep",
                    )
                    .unwrap();
                } else if reference == "branch" {
                    peer.project_path = tmp
                        .path()
                        .join("missing-peer-checkout")
                        .to_string_lossy()
                        .into_owned();
                    peer.worktree_info = Some(crate::session::WorktreeInfo {
                        branch: "work".into(),
                        main_repo_path: repos[0].main_repo_path.clone(),
                        managed_by_aoe: false,
                        created_at: Utc::now(),
                        base_branch: None,
                    });
                }
                let source = Storage::new_unwatched("cleanup-owner").unwrap();
                source
                    .update(|rows, _| {
                        rows.push(owner.clone());
                        Ok(())
                    })
                    .unwrap();
                let peer_store = Storage::new_unwatched("cleanup-peer").unwrap();
                peer_store
                    .update(|rows, _| {
                        rows.clear();
                        rows.push(peer);
                        Ok(())
                    })
                    .unwrap();
                let request = DeletionRequest {
                    session_id: owner.id.clone(),
                    instance: owner,
                    delete_worktree: true,
                    delete_branch: true,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: false,
                };
                let transaction = match PurgeTransaction::reserve(source, request, None).unwrap() {
                    PurgeReservation::Reserved(transaction) => transaction,
                    PurgeReservation::Rejected(_) => panic!("initial purge rejected"),
                };
                let result = transaction.complete();
                assert_eq!(
                    result.disposition,
                    DeletionDisposition::Removed,
                    "{reference}: {:?}",
                    result.errors
                );
                assert_eq!(
                    Path::new(&repos[1].worktree_path).exists(),
                    reference == "workspace root",
                    "reference={reference} errors={:?}",
                    result.errors
                );
                let second_branch = git2::Repository::open(&repos[1].main_repo_path)
                    .unwrap()
                    .find_branch("work", git2::BranchType::Local)
                    .is_ok();
                assert_eq!(second_branch, reference == "workspace root");
                if reference == "checkout" {
                    assert_eq!(
                        std::fs::read(Path::new(&repos[0].worktree_path).join("peer-data"))
                            .unwrap(),
                        b"keep"
                    );
                }
                assert_eq!(
                    Path::new(&repos[0].worktree_path).exists(),
                    reference != "branch"
                );
                assert!(git2::Repository::open(&repos[0].main_repo_path)
                    .unwrap()
                    .find_branch("work", git2::BranchType::Local)
                    .is_ok());
                if reference != "branch" {
                    assert!(workspace.is_dir());
                }
                assert!(Storage::open_unwatched("cleanup-owner")
                    .unwrap()
                    .load()
                    .unwrap()
                    .is_empty());
            }
        }

        /// Proves the ownership guard is actually consulted at the call site,
        /// not merely written: a `workspace_dir` pointing at a real checkout
        /// that is itself the repo worktree must survive, with the refusal
        /// surfaced as an error rather than silently skipped.
        #[test]
        fn e2e_workspace_dir_that_is_not_aoe_owned_is_refused() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let user_checkout = tmp.path().join("backend");
            init_repo(&user_checkout);
            let precious = user_checkout.join("uncommitted.txt");
            std::fs::write(&precious, "do not delete me").unwrap();

            let mut instance = Instance::new("Bad", user_checkout.to_str().unwrap());
            instance.workspace_info = Some(crate::session::WorkspaceInfo {
                branch: "feature/abc".to_string(),
                // The shape the guard exists to catch: the workspace dir IS the
                // repo worktree, so treating it as aoe-owned would target the
                // checkout.
                workspace_dir: user_checkout.to_string_lossy().to_string(),
                repos: vec![crate::session::WorkspaceRepo {
                    name: "backend".to_string(),
                    source_path: user_checkout.to_string_lossy().to_string(),
                    branch: "feature/abc".to_string(),
                    worktree_path: user_checkout.to_string_lossy().to_string(),
                    main_repo_path: user_checkout.to_string_lossy().to_string(),
                    // Not aoe-managed, so the dirty check never fires and the
                    // ownership guard is the only gate left.
                    managed_by_aoe: false,
                    branch_preexisting: false,
                    base_branch: None,
                    base_branch_override: None,
                }],
                created_at: chrono::Utc::now(),
                cleanup_on_delete: true,
            });

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&request);

            assert!(
                user_checkout.exists(),
                "a workspace dir aoe did not create must not be removed"
            );
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

        /// A strict-descendant layout alone does not prove ownership. A corrupt
        /// record naming a populated ancestor must never be wiped: the removal
        /// is non-recursive, so the ancestor is left in place and reported as a
        /// message rather than a hard error, and the trash row still clears.
        #[test]
        fn e2e_workspace_ancestor_with_unrelated_content_is_not_recursively_removed() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let user_checkout = tmp.path().join("backend");
            init_repo(&user_checkout);
            let precious = tmp.path().join("unrelated.txt");
            std::fs::write(&precious, "do not delete me").unwrap();

            let mut instance = Instance::new("Bad ancestor", user_checkout.to_str().unwrap());
            instance.workspace_info = Some(workspace_info(
                tmp.path().to_str().unwrap(),
                &[user_checkout.to_str().unwrap()],
            ));
            if let Some(repo) = instance
                .workspace_info
                .as_mut()
                .and_then(|workspace| workspace.repos.first_mut())
            {
                repo.source_path = user_checkout.to_string_lossy().into_owned();
                repo.main_repo_path = user_checkout.to_string_lossy().into_owned();
                repo.managed_by_aoe = false;
            }

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&request);

            assert!(precious.exists(), "unrelated ancestor content must survive");
            assert_eq!(
                std::fs::read_to_string(&precious).unwrap(),
                "do not delete me"
            );
            assert!(
                user_checkout.exists(),
                "the user's checkout under a corrupt ancestor must survive"
            );
            assert!(
                tmp.path().exists(),
                "the populated ancestor dir must be left in place, not wiped"
            );
            // A non-empty dir aoe cannot own is a safe refusal reported as a
            // message, not a hard error, so the purge still clears the row and
            // does not retry the same non-convergent refusal forever (#3215).
            assert!(
                result.success,
                "safe refusal must not fail the purge: {:?}",
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
        }

        // Checkout ownership does not imply branch ownership.
        #[test]
        fn e2e_workspace_repo_keeps_a_branch_aoe_did_not_create() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let workspace = tmp.path().join("ws");
            let main_repo = tmp.path().join("frontend");
            let worktree = workspace.join("frontend");
            init_repo(&main_repo);
            git_in(&main_repo, &["branch", "mine"]);
            let built = crate::session::builder::create_workspace(
                &crate::session::builder::WorkspaceRepoSpec {
                    path: main_repo.clone(),
                    base_branch: None,
                },
                &[],
                "mine",
                false,
                workspace.to_str().unwrap(),
                false,
            )
            .unwrap();
            let mut instance = Instance::new("Converted", workspace.to_str().unwrap());
            instance.workspace_info = Some(built.workspace_info);

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&request);
            assert!(
                result.success,
                "perform_deletion failed: {:?}",
                result.errors
            );

            assert!(!worktree.exists(), "worktree should be removed");
            let branches = std::process::Command::new("git")
                .args(["branch", "--list", "mine"])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                String::from_utf8_lossy(&branches.stdout).contains("mine"),
                "a branch aoe did not create must survive: stdout={}",
                String::from_utf8_lossy(&branches.stdout)
            );
        }

        // #2541: `concurrent_purge_reacquires_own_claim` relies on
        // `perform_deletion` being idempotent, because two purges of the same
        // row both (re)acquire the Purge claim and each runs the teardown. This
        // confirms the assumption rather than supposing it: a second
        // `perform_deletion` over an already-torn-down worktree still succeeds.
        #[test]
        fn perform_deletion_is_idempotent_on_worktree() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = {
                let mut index = repo.index().unwrap();
                index.write_tree().unwrap()
            };
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    "feature/delete-me",
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(status.status.success());

            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feature/delete-me".to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let first = perform_deletion(&request);
            assert!(first.success, "first purge failed: {:?}", first.errors);
            assert!(!worktree_path.exists());

            // Second purge over the already-gone artifacts must still succeed,
            // so a re-entrant purge (reacquired Purge claim) is safe.
            let second = perform_deletion(&request);
            assert!(
                second.success,
                "second purge over already-gone artifacts must succeed: {:?}",
                second.errors
            );
        }

        /// #2532 repro: requesting branch deletion while preserving the
        /// worktree (`delete_worktree=false, delete_branch=true`) must NOT
        /// attempt `git branch -d/-D` on the branch the preserved worktree
        /// still has checked out. Pre-fix this pushed a `Branch:` error and
        /// failed the deletion; post-fix the branch is kept with a message
        /// and the worktree + branch survive intact.
        #[test]
        fn e2e_preserved_worktree_keeps_its_branch() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    "feature/keep-me",
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "git worktree add failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );

            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feature/keep-me".to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let result = perform_deletion(&request);
            assert!(
                result.success,
                "preserving the worktree must not fail deletion: {:?}",
                result.errors
            );
            assert!(
                !result.errors.iter().any(|e| e.starts_with("Branch:")),
                "no branch-cleanup error expected: {:?}",
                result.errors
            );
            assert!(
                result.messages.iter().any(|m| m.contains("kept")),
                "a kept-branch message is expected: {:?}",
                result.messages
            );

            // Worktree directory and admin entry must survive.
            assert!(
                worktree_path.exists(),
                "preserved worktree dir must still exist"
            );
            assert!(
                main_repo.join(".git/worktrees/worktree").exists(),
                "preserved worktree admin dir must still exist"
            );

            // Branch must still be present.
            let branches_out = std::process::Command::new("git")
                .args(["branch", "--list", "feature/keep-me"])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                !String::from_utf8_lossy(&branches_out.stdout)
                    .trim()
                    .is_empty(),
                "branch should be preserved: stdout={}",
                String::from_utf8_lossy(&branches_out.stdout)
            );
        }

        /// Race-condition repro: the agent left untracked files in the
        /// worktree (this is what triggered the original
        /// "fatal: '<path>' contains modified or untracked files"
        /// failures). With `force_delete=true` the worktree must be
        /// removed cleanly even with untracked content, which is the
        /// fallback path the TUI takes when the user picks "force
        /// delete" after a normal delete failed.
        #[test]
        fn e2e_real_worktree_with_untracked_files_force_removed() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    "feature/race-repro",
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(status.status.success());

            // simulate what the in-container agent leaves behind: an
            // untracked log file, plus a modified-but-not-committed
            // file. Without force_delete, `git worktree remove` refuses
            // to delete a dirty tree.
            std::fs::write(worktree_path.join("agent.log"), "scratch").unwrap();
            std::fs::write(worktree_path.join("debug.json"), "{\"k\":1}").unwrap();

            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: "feature/race-repro".to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });

            // First: without force, deletion must fail and leave the
            // worktree intact so the user can decide.
            let req_no_force = DeletionRequest {
                session_id: instance.id.clone(),
                instance: instance.clone(),
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&req_no_force);
            assert!(
                !result.success,
                "dirty worktree must NOT be deleted without --force"
            );
            assert!(
                worktree_path.exists(),
                "dirty worktree must still exist after failed delete"
            );

            // Now retry with force: must succeed and clean everything.
            let req_force = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&req_force);
            assert!(
                result.success,
                "force delete should succeed: {:?}",
                result.errors
            );
            assert!(!worktree_path.exists());
            assert!(!main_repo.join(".git/worktrees/worktree").exists());
        }

        /// Builds a real on-disk worktree on a fresh branch, then
        /// returns a tuple of `(_tmp, main_repo, worktree_path, instance)`
        /// where the instance has `worktree_info` + `sandbox_info`
        /// pointing at a non-existent container (so container ops are
        /// no-ops in the test). The caller can drop untracked files
        /// into `worktree_path` before invoking `perform_deletion`.
        fn build_sandboxed_worktree(
            branch: &str,
        ) -> (
            tempfile::TempDir,
            std::path::PathBuf,
            std::path::PathBuf,
            Instance,
        ) {
            let tmp = tempfile::TempDir::new().unwrap();
            let main_repo = tmp.path().join("main");
            let worktree_path = tmp.path().join("worktree");
            std::fs::create_dir(&main_repo).unwrap();

            let repo = git2::Repository::init(&main_repo).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
                .unwrap();

            let status = std::process::Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    branch,
                    worktree_path.to_str().unwrap(),
                ])
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                status.status.success(),
                "git worktree add failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );

            let mut instance = Instance::new("Test", worktree_path.to_str().unwrap());
            instance.worktree_info = Some(crate::session::WorktreeInfo {
                branch: branch.to_string(),
                main_repo_path: main_repo.to_string_lossy().to_string(),
                managed_by_aoe: true,
                created_at: chrono::Utc::now(),
                base_branch: None,
            });
            instance.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image: "alpine".to_string(),
                container_name: "aoe-dirty-test-doesnotexist".to_string(),
                extra_env: None,
                custom_instruction: None,
                before_start_env: Vec::new(),
                container_workdir: None,
            });

            (tmp, main_repo, worktree_path, instance)
        }

        /// Regression for the silent-data-destruction bug introduced by
        /// the preclean stage (#1023): if the user has uncommitted
        /// changes in a sandboxed worktree and asks for a normal (non-
        /// force) delete, the in-container `find . -delete` would
        /// previously wipe those changes before any dirty check ever
        /// ran. With the host-side dirty check, preclean must be
        /// skipped, the worktree must survive, and the error must
        /// describe what's dirty so the user can choose to force.
        #[test]
        fn sandboxed_with_dirty_worktree_skips_preclean_and_preserves_changes() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let (_tmp, main_repo, worktree_path, instance) =
                build_sandboxed_worktree("feature/dirty-no-force");

            std::fs::write(worktree_path.join("uncommitted.log"), "important").unwrap();

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: true,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            // Stage assertions: preclean must not run when dirty.
            let stages = run_with_capture(|| {
                let _ = perform_deletion(&request);
            });
            assert!(
                !stages.iter().any(|s| s == "sandbox_worktree_preclean"),
                "preclean must be skipped when worktree is dirty: stages={:?}",
                stages
            );

            // After run_with_capture, deletion must have left the
            // worktree intact on both passes. Run once more to capture
            // the result + error message.
            let result = perform_deletion(&request);
            assert!(
                !result.success,
                "dirty worktree must not be deleted without --force"
            );
            assert!(
                !result.errors.is_empty(),
                "dirty deletion should surface errors"
            );
            let err = result.errors.join("; ");
            assert!(
                err.contains("modified or untracked"),
                "error should describe dirty state: {}",
                err
            );
            assert!(
                err.contains("uncommitted.log"),
                "error should list the dirty path: {}",
                err
            );
            assert!(
                worktree_path.exists(),
                "worktree dir must survive a refused dirty delete"
            );
            assert!(
                worktree_path.join("uncommitted.log").exists(),
                "uncommitted user data must survive a refused dirty delete"
            );
            assert!(
                main_repo.join(".git/worktrees/worktree").exists(),
                "worktree admin entry must still be present"
            );
        }

        /// Counterpart: with `force_delete=true` the user has explicitly
        /// opted into losing uncommitted changes, so preclean runs and
        /// the worktree is removed. Preclean is a docker no-op in this
        /// test (container does not exist), so the host-side path uses
        /// `git worktree remove --force` which correctly handles the
        /// untracked file.
        #[test]
        fn sandboxed_with_dirty_worktree_force_runs_preclean_and_removes() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let (_tmp, main_repo, worktree_path, instance) =
                build_sandboxed_worktree("feature/dirty-force");

            std::fs::write(worktree_path.join("uncommitted.log"), "scratch").unwrap();

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
            };

            let stages = run_with_capture(|| {
                let _ = perform_deletion(&request);
            });
            assert!(
                stages.iter().any(|s| s == "sandbox_worktree_preclean"),
                "preclean must run when force_delete=true: stages={:?}",
                stages
            );

            // run_with_capture invokes perform_deletion twice; the
            // first pass already removed the worktree, so the second
            // pass is a no-op. Both succeed.
            assert!(
                !worktree_path.exists(),
                "force delete must remove the worktree dir"
            );
            assert!(
                !main_repo.join(".git/worktrees/worktree").exists(),
                "force delete must prune the admin entry"
            );
        }

        /// Non-sandboxed deletion: no preclean stage is emitted, but
        /// tmux still gets killed before worktree work.
        #[test]
        fn unsandboxed_kills_tmux_before_worktree() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let instance = Instance::new("Test", "/tmp/aoe-deletion-test-nonexistent");
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let stages = run_with_capture(|| {
                let _ = perform_deletion(&request);
            });

            assert!(
                idx(&stages, "tmux_kill") < idx(&stages, "worktree_remove"),
                "tmux must be killed before worktree cleanup: stages={:?}",
                stages
            );
            assert!(
                !stages.iter().any(|s| s == "sandbox_worktree_preclean"),
                "unsandboxed deletion must not emit sandbox preclean stage: stages={:?}",
                stages
            );
        }

        /// A stray file under an otherwise aoe-owned workspace keeps the dir
        /// non-empty after the managed worktree is gone. The non-recursive
        /// removal must leave that file and report a kept-message rather than a
        /// failure, so the purge still clears the row instead of looping on a
        /// refusal that never converges (#3215).
        #[test]
        fn e2e_workspace_dir_with_stray_file_is_kept_not_failed() {
            let _app_guard = crate::session::test_support::isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let workspace = tmp.path().join("ws");
            let main_repo = tmp.path().join("frontend");
            let worktree = workspace.join("frontend");
            init_repo(&main_repo);
            std::fs::create_dir_all(&workspace).unwrap();
            git_in(
                &main_repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feature/ws-del",
                    worktree.to_str().unwrap(),
                    "HEAD",
                ],
            );
            let stray = workspace.join("stray.txt");
            std::fs::write(&stray, "keep me").unwrap();

            let mut instance = Instance::new("Workspace", workspace.to_str().unwrap());
            instance.workspace_info = Some(crate::session::WorkspaceInfo {
                branch: "feature/ws-del".to_string(),
                workspace_dir: workspace.to_string_lossy().to_string(),
                repos: vec![crate::session::WorkspaceRepo {
                    name: "frontend".to_string(),
                    source_path: main_repo.to_string_lossy().to_string(),
                    branch: "feature/ws-del".to_string(),
                    worktree_path: worktree.to_string_lossy().to_string(),
                    main_repo_path: main_repo.to_string_lossy().to_string(),
                    managed_by_aoe: true,
                    branch_preexisting: false,
                    base_branch: None,
                    base_branch_override: None,
                }],
                created_at: chrono::Utc::now(),
                cleanup_on_delete: true,
            });
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let result = perform_deletion(&request);
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
            assert!(!worktree.exists(), "managed worktree must be removed");
            assert!(stray.exists(), "the stray file must survive");
            assert!(workspace.exists(), "the kept workspace dir must remain");
        }
    }

    mod scratch_cleanup {
        use super::*;
        use crate::session::test_support::isolate_app_dir;
        use serial_test::serial;
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
        fn scratch_session_removes_dir() {
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };

            let result = perform_deletion(&request);
            assert!(result.success, "deletion errors: {:?}", result.errors);
            assert!(
                !dir.exists(),
                "scratch directory must be gone after perform_deletion"
            );
            assert!(
                result
                    .messages
                    .iter()
                    .any(|m| m.contains("Scratch directory removed")),
                "expected scratch-removed message, got {:?}",
                result.messages
            );
        }

        #[test]
        #[serial]
        fn scratch_session_with_missing_dir_still_succeeds() {
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
            // Simulate the "directory already gone" race (user deleted
            // manually, FS hiccup, etc.). Deletion must not fail.
            fs::remove_dir_all(&dir).unwrap();

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&request);
            assert!(
                result.success,
                "missing scratch dir must not fail deletion: {:?}",
                result.errors
            );
        }

        #[test]
        #[serial]
        fn tampered_project_path_does_not_get_removed() {
            // Defense against an edited or corrupted session JSON that
            // sets `scratch: true` while pointing project_path at something
            // the guard would reject. The directory must survive deletion.
            let _tmp = isolate_app_dir();
            let bystander =
                std::env::temp_dir().join(format!("important-data-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&bystander).expect("create bystander");
            fs::write(bystander.join("file.txt"), b"keep me").unwrap();

            let mut instance = Instance::new("Tampered", bystander.to_str().unwrap());
            instance.scratch = true;

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let result = perform_deletion(&request);

            assert!(
                bystander.exists(),
                "guard must refuse to remove a path outside the scratch root"
            );
            assert!(
                bystander.join("file.txt").exists(),
                "bystander contents must survive"
            );
            // The guard refusal must also surface as an error on the
            // deletion result, so callers can report the partial
            // cleanup instead of silently treating it as a clean
            // delete.
            assert!(
                result.errors.iter().any(|e| e.contains("scratch guard")),
                "guard refusal must be reported in result.errors, got: {:?}",
                result.errors
            );
            let _ = fs::remove_dir_all(&bystander);
        }

        #[test]
        #[serial]
        fn keep_scratch_leaves_dir_on_disk_and_reports_path() {
            // The --keep-scratch escape hatch. Session record still gets
            // removed (caller's responsibility), but the scratch directory
            // stays put and the deletion result calls out the kept path.
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: true,
            };

            let result = perform_deletion(&request);
            assert!(
                result.success,
                "keep-scratch deletion errors: {:?}",
                result.errors
            );
            assert!(
                dir.exists(),
                "keep-scratch must leave the directory on disk"
            );
            let kept_msg = result
                .messages
                .iter()
                .find(|m| m.contains("Scratch directory kept at:"));
            assert!(
                kept_msg.is_some(),
                "expected kept-path message, got {:?}",
                result.messages
            );
            assert!(
                kept_msg.unwrap().contains(dir.to_str().unwrap()),
                "kept-path message must include the actual path; got: {}",
                kept_msg.unwrap()
            );
            // Clean up the leftover dir so the next test starts clean.
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        #[serial]
        fn non_scratch_session_under_app_dir_is_untouched() {
            // A regular session whose project_path happens to live under
            // the app dir (e.g. a test fixture) must not be removed.
            let _tmp = isolate_app_dir();
            let dir = crate::session::get_app_dir()
                .unwrap()
                .join(format!("non-scratch-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).expect("create non-scratch test dir");

            let instance = Instance::new("Regular", dir.to_str().unwrap());
            // scratch is false by default.

            let request = DeletionRequest {
                session_id: instance.id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            };
            let _ = perform_deletion(&request);

            assert!(
                dir.exists(),
                "non-scratch session must never trip the scratch cleanup branch"
            );
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
