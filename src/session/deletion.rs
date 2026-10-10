//! Shared session deletion logic used by CLI, TUI, and web server.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;

use crate::containers::DockerContainer;
use crate::git::cleanup::remove_managed_worktree;
use crate::git::GitWorktree;
use crate::session::config::repo_config;
use crate::session::lifecycle_journal::LifecyclePhase;
use crate::session::storage::StorageFlock;
use crate::session::{Instance, LifecycleOperation, ReservationHeartbeat, Status, Storage};

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

pub enum PurgeReservation {
    Reserved(PurgeTransaction),
    Rejected(DeletionResult),
}

/// Owned purge transition.
pub struct PurgeTransaction {
    storage: Storage,
    request: DeletionRequest,
    was_trashed: bool,
    generation: u64,
    lifecycle_lock: Option<StorageFlock>,
    journal_path: Option<PathBuf>,
    journal_entry: Option<Box<crate::session::lifecycle_journal::LifecycleJournalEntry>>,
    status_before: Status,
    pre_teardown_error: Option<String>,
    active: bool,
}

/// A purge whose hooks are complete. The same lifecycle flock remains held while
/// irreversible sidecars are removed and the durable row is committed last.
#[must_use = "committed purge sidecars must be finished"]
pub struct CommittedPurge {
    storage: Storage,
    request: DeletionRequest,
    _lifecycle_lock: StorageFlock,
    journal_path: Option<PathBuf>,
    journal_entry: Option<Box<crate::session::lifecycle_journal::LifecycleJournalEntry>>,
    generation: u64,
    status_before: Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CompletionGate {
    Proceed,
    AlreadyGone,
    KeptRestored,
    Superseded,
}

impl PurgeTransaction {
    pub fn reserve_unwatched(request: DeletionRequest) -> Result<PurgeReservation> {
        let profile = request.instance.source_profile.clone();
        anyhow::ensure!(
            !profile.is_empty(),
            "session has no source profile; refusing to use the default profile"
        );
        let storage = Storage::open_unwatched(&profile)?;
        Self::reserve(storage, request)
    }

    pub fn reserve(storage: Storage, request: DeletionRequest) -> Result<PurgeReservation> {
        Self::reserve_with_transcript_option(storage, request, false)
    }

    pub(crate) fn reserve_with_acp_transcript(
        storage: Storage,
        request: DeletionRequest,
    ) -> Result<PurgeReservation> {
        Self::reserve_with_transcript_option(storage, request, true)
    }

    fn reserve_with_transcript_option(
        storage: Storage,
        mut request: DeletionRequest,
        purge_acp_transcript: bool,
    ) -> Result<PurgeReservation> {
        let id = request.session_id.clone();
        let was_trashed = request.instance.is_trashed();
        let expected_trashed_at = request.instance.trashed_at;
        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&id)
            .context("failed to acquire instance purge lock")?;
        let now = Utc::now();
        let mut reserved = None;
        let mut rejected = None;
        let mut journal_path = None;
        let mut journal_entry = None;
        let reserve_result = storage.update(|instances, _groups| {
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
            let status_before = stored.status;
            // Drop stale force permission before persisting the recovery intent.
            if was_trashed && stored.trashed_at != expected_trashed_at {
                request.force_delete = false;
            }
            let mut snapshot = stored.clone();
            snapshot.source_profile = storage.profile().to_string();
            let entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
                snapshot.clone(),
                status_before,
                storage.sessions_path().to_path_buf(),
                crate::session::lifecycle_journal::LifecycleDeletionOptions {
                    generation,
                    delete_worktree: request.delete_worktree,
                    delete_branch: request.delete_branch,
                    delete_sandbox: request.delete_sandbox,
                    force_delete: request.force_delete,
                    detach_hooks: request.detach_hooks,
                    keep_scratch: request.keep_scratch,
                    purge_acp_transcript,
                },
            );
            let path = crate::session::lifecycle_journal::record(&entry)
                .context("recording the durable lifecycle journal failed")?;
            stored.status = Status::Deleting;
            reserved = Some((generation, snapshot, status_before));
            journal_path = Some(path);
            journal_entry = Some(entry);
            Ok(())
        });
        if let Err(error) = reserve_result {
            if let Some(path) = journal_path.as_deref() {
                if let Err(cleanup_error) = crate::session::lifecycle_journal::consume(path) {
                    tracing::warn!(
                        target: "session.delete",
                        path = %path.display(),
                        error = %cleanup_error,
                        "failed reservation left a lifecycle journal that must be ignored"
                    );
                }
            }
            return Err(error);
        }

        if let Some((disposition, message, retained_instance)) = rejected {
            return Ok(PurgeReservation::Rejected(DeletionResult::rejected(
                id,
                disposition,
                message,
                retained_instance,
            )));
        }
        let (generation, snapshot, status_before) =
            reserved.ok_or_else(|| anyhow::anyhow!("purge reservation produced no outcome"))?;
        request.instance = snapshot;
        let mut transaction = Self {
            storage,
            request,
            was_trashed,
            generation,
            lifecycle_lock: Some(lifecycle_lock),
            journal_path,
            journal_entry: journal_entry.map(Box::new),
            status_before,
            pre_teardown_error: None,
            active: true,
        };
        if transaction.request.instance.is_structured() {
            if let Err(error) = crate::process::worker_registry::fence_for_purge(
                &transaction.request.session_id,
                transaction.storage.profile(),
                transaction.generation,
            ) {
                let _ = transaction.release_reservation();
                return Err(error.context("failed to fence the ACP runner before purge"));
            }
        }
        Ok(PurgeReservation::Reserved(transaction))
    }

    /// Run best-effort hooks without a lifecycle or storage flock held.
    pub fn run_hooks(self) -> Self {
        self.run_hooks_with(run_on_destroy_hooks)
    }

    fn run_hooks_with<F>(self, run_hooks: F) -> Self
    where
        F: FnOnce(&Instance, bool),
    {
        self.run_hooks_with_interval(run_hooks, Duration::from_secs(60))
    }

    fn run_hooks_with_interval<F>(mut self, run_hooks: F, interval: Duration) -> Self
    where
        F: FnOnce(&Instance, bool),
    {
        let heartbeat = match ReservationHeartbeat::start(
            &self.storage,
            &self.request.session_id,
            LifecycleOperation::Purge,
            self.generation,
            interval,
        ) {
            Ok(heartbeat) => heartbeat,
            Err(error) => {
                self.pre_teardown_error = Some(error.to_string());
                return self;
            }
        };
        if let Err(error) = self.mark_journal_phase(LifecyclePhase::HooksStarted) {
            heartbeat.stop();
            self.pre_teardown_error = Some(format!("failed to record hook start: {error:#}"));
            return self;
        }
        self.lifecycle_lock = None;
        run_hooks(&self.request.instance, self.request.detach_hooks);
        heartbeat.stop();
        if let Err(error) = self.mark_journal_phase(LifecyclePhase::HooksComplete) {
            self.pre_teardown_error = Some(format!("failed to record hook completion: {error:#}"));
        }
        self
    }

    fn update_journal(
        &mut self,
        update: impl FnOnce(
            &crate::session::lifecycle_journal::LifecycleJournalEntry,
        ) -> crate::session::lifecycle_journal::LifecycleJournalEntry,
    ) -> Result<()> {
        let (Some(path), Some(_entry)) = (&self.journal_path, &self.journal_entry) else {
            return Ok(());
        };
        let current = crate::session::lifecycle_journal::read_deletion(path)?
            .ok_or_else(|| anyhow::anyhow!("purge lifecycle journal was replaced"))?;
        anyhow::ensure!(
            current.session_id == self.request.session_id
                && current.source_profile == self.storage.profile()
                && current.generation == self.generation,
            "purge lifecycle journal is no longer owned by this generation"
        );
        let updated = update(&current);
        crate::session::lifecycle_journal::update(path, &updated)?;
        self.journal_entry = Some(Box::new(updated));
        Ok(())
    }

    fn mark_journal_phase(&mut self, phase: LifecyclePhase) -> Result<()> {
        self.update_journal(|entry| entry.with_phase(phase))
    }

    fn consume_journal(&mut self) {
        let Some(path) = self.journal_path.as_deref() else {
            return;
        };
        let matches_owner = crate::session::lifecycle_journal::read_deletion(path)
            .ok()
            .flatten()
            .is_some_and(|entry| {
                entry.session_id == self.request.session_id
                    && entry.source_profile == self.storage.profile()
                    && entry.generation == self.generation
            });
        if !matches_owner {
            self.journal_path = None;
            self.journal_entry = None;
            return;
        }
        if let Err(error) = self.clear_runner_fence() {
            tracing::warn!(
                target: "session.delete",
                path = %path.display(),
                error = %error,
                "purge journal retained so recovery can retry clearing its ACP runner fence"
            );
            return;
        }
        if let Err(error) = crate::session::lifecycle_journal::consume(path) {
            tracing::warn!(
                target: "session.delete",
                path = %path.display(),
                error = %error,
                "completed purge could not consume its lifecycle journal; startup will retry it"
            );
            return;
        }
        self.journal_path = None;
        self.journal_entry = None;
    }

    fn clear_runner_fence(&self) -> Result<()> {
        if !self.request.instance.is_structured() {
            return Ok(());
        }
        if let Err(error) = crate::process::worker_registry::clear_purge_fence_if_owned(
            &self.request.session_id,
            self.storage.profile(),
            self.generation,
        ) {
            tracing::warn!(
                target: "session.delete",
                session = %self.request.session_id,
                error = %error,
                "ACP runner purge fence could not be cleared"
            );
            return Err(error);
        }
        Ok(())
    }

    fn ensure_lifecycle_lock(&mut self) -> Result<()> {
        if self.lifecycle_lock.is_none() {
            self.lifecycle_lock = Some(
                self.storage
                    .acquire_instance_lifecycle_lock(&self.request.session_id)
                    .context("failed to reacquire instance purge lock after hooks")?,
            );
        }
        Ok(())
    }

    fn release_reservation(&mut self) -> Result<Option<Instance>> {
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let status_before = self.status_before;
        if let Err(error) = self.mark_journal_phase(LifecyclePhase::Abandoned) {
            tracing::warn!(
                target: "session.delete",
                session = %id,
                error = %error,
                "failed to mark rolled-back purge intent abandoned"
            );
        }
        let mut retained = None;
        self.storage.update(|instances, _groups| {
            if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                if stored.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation) {
                    if stored.status == Status::Deleting {
                        stored.status = status_before;
                    }
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                }
                retained = Some(stored.clone());
            }
            Ok(())
        })?;
        self.active = false;
        self.consume_journal();
        Ok(retained)
    }

    fn gate(&mut self) -> Result<(CompletionGate, Option<Instance>)> {
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let status_before = self.status_before;
        let mut outcome = None;
        self.storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) else {
                outcome = Some((CompletionGate::AlreadyGone, None));
                return Ok(());
            };
            let restored = crate::session::claim::purge_restored_row_must_be_kept(
                was_trashed,
                stored.is_trashed(),
            );
            let owns = stored.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation);
            let gate = if restored {
                CompletionGate::KeptRestored
            } else if !owns || stored.status != Status::Deleting {
                CompletionGate::Superseded
            } else {
                CompletionGate::Proceed
            };
            if !matches!(gate, CompletionGate::Proceed) && owns {
                if stored.status == Status::Deleting {
                    stored.status = status_before;
                }
                stored
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation);
            }
            outcome = Some((gate, Some(stored.clone())));
            Ok(())
        })?;
        let outcome = outcome.ok_or_else(|| anyhow::anyhow!("purge gate produced no outcome"))?;
        if !matches!(outcome.0, CompletionGate::Proceed) {
            self.active = false;
            if !matches!(outcome.0, CompletionGate::AlreadyGone) {
                self.consume_journal();
            }
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

    /// Atomically validate this reservation before ACP and sidecar teardown. The durable row stays
    /// present until teardown finishes so a crash always leaves a replayable session record.
    pub fn begin_irreversible(
        mut self,
    ) -> std::result::Result<CommittedPurge, Box<DeletionResult>> {
        if let Some(error) = self.pre_teardown_error.take() {
            let retained_instance = if self.ensure_lifecycle_lock().is_ok() {
                self.release_reservation().ok().flatten()
            } else {
                None
            };
            return Err(Box::new(DeletionResult::rejected(
                self.request.session_id.clone(),
                DeletionDisposition::Failed,
                format!("Failed to prepare session purge: {error}"),
                retained_instance,
            )));
        }
        if let Err(error) = self.ensure_lifecycle_lock() {
            return Err(Box::new(DeletionResult::rejected(
                self.request.session_id.clone(),
                DeletionDisposition::Failed,
                format!("Failed to resume reserved session purge: {error}"),
                None,
            )));
        }
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let status_before = self.status_before;
        let mut commit = None;
        if let Err(error) = self.storage.update(|instances, _groups| {
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
                if owns {
                    if instances[index].status == Status::Deleting {
                        instances[index].status = status_before;
                    }
                    instances[index].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                }
                commit = Some((CompletionGate::KeptRestored, Some(instances[index].clone())));
            } else if !owns || instances[index].status != Status::Deleting {
                if owns && instances[index].status == Status::Deleting {
                    instances[index].status = status_before;
                }
                if owns {
                    instances[index].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                }
                commit = Some((CompletionGate::Superseded, Some(instances[index].clone())));
            } else {
                commit = Some((CompletionGate::Proceed, Some(instances[index].clone())));
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

        let Some((gate, retained)) = commit else {
            return Err(Box::new(DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                "Irreversible purge commit produced no outcome",
                None,
            )));
        };
        if !matches!(gate, CompletionGate::Proceed) {
            self.active = false;
            if !matches!(gate, CompletionGate::AlreadyGone) {
                self.consume_journal();
            }
            return Err(Box::new(self.result_for_gate(gate, retained)));
        }
        if let Err(error) = self.mark_journal_phase(LifecyclePhase::TeardownStarted) {
            let retained_instance = self.release_reservation().ok().flatten();
            return Err(Box::new(DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                format!("Failed to record session teardown start: {error:#}"),
                retained_instance,
            )));
        }
        self.active = false;
        Ok(CommittedPurge {
            storage: self.storage.clone(),
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
            _lifecycle_lock: self
                .lifecycle_lock
                .take()
                .expect("active purge transaction must own its lifecycle lock"),
            journal_path: self.journal_path.take(),
            journal_entry: self.journal_entry.take(),
            generation,
            status_before,
        })
    }

    /// Reacquire and verify the token, then keep the lifecycle flock through
    /// teardown and the durable commit.
    fn complete_inner(
        mut self,
        after_teardown: impl FnOnce(&Instance) -> std::result::Result<(), String>,
        commit_on_teardown_failure: bool,
    ) -> DeletionResult {
        if let Some(error) = self.pre_teardown_error.take() {
            let retained_instance = if self.ensure_lifecycle_lock().is_ok() {
                self.release_reservation().ok().flatten()
            } else {
                None
            };
            return DeletionResult::rejected(
                self.request.session_id.clone(),
                DeletionDisposition::Failed,
                format!("Failed to prepare session purge: {error}"),
                retained_instance,
            );
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
        if let Err(error) = self.mark_journal_phase(LifecyclePhase::TeardownStarted) {
            let retained_instance = self.release_reservation().ok().flatten();
            return DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                format!("Failed to record session teardown start: {error:#}"),
                retained_instance,
            );
        }
        if let Err(error) = stop_acp_runner_for_purge(&self.request.instance, self.generation) {
            self.active = false;
            let mut result = DeletionResult::rejected(
                id,
                DeletionDisposition::Failed,
                format!(
                    "ACP runner exit could not be confirmed; purge resources were retained: {error:#}"
                ),
                self.storage.load().ok().and_then(|rows| {
                    rows.into_iter()
                        .find(|row| row.id == self.request.session_id)
                }),
            );
            result.teardown_started = false;
            return result;
        }
        let mut result = perform_deletion_teardown_lifecycle_locked(&self.request);
        if !result.success && !commit_on_teardown_failure {
            result.retained_instance = self.release_reservation().ok().flatten();
            result.disposition = DeletionDisposition::Failed;
            return result;
        }

        if let Err(error) = after_teardown(&self.request.instance) {
            result.success = false;
            result.errors.push(error);
            result.retained_instance = self.release_reservation().ok().flatten();
            result.disposition = DeletionDisposition::Failed;
            return result;
        }

        let kept_resources = kept_resources_from_messages(&self.request, &result.messages);
        if let Err(error) = self.update_journal(|entry| {
            entry
                .with_kept_resources(kept_resources.clone())
                .with_phase(LifecyclePhase::TeardownComplete)
        }) {
            result.success = false;
            result.errors.push(format!(
                "Session teardown completed, but its terminal journal checkpoint failed: {error:#}"
            ));
            result.retained_instance = self.release_reservation().ok().flatten();
            result.disposition = DeletionDisposition::Failed;
            return result;
        }

        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let status_before = self.status_before;
        let mut commit = None;
        let commit_result = self.storage.update(|instances, _groups| {
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
                if owns {
                    if instances[index].status == Status::Deleting {
                        instances[index].status = status_before;
                    }
                    instances[index].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                }
                commit = Some((CompletionGate::KeptRestored, Some(instances[index].clone())));
            } else if !owns || instances[index].status != Status::Deleting {
                if owns {
                    instances[index].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                }
                commit = Some((CompletionGate::Superseded, Some(instances[index].clone())));
            } else {
                instances.remove(index);
                commit = Some((CompletionGate::Proceed, None));
            }
            Ok(())
        });
        match commit_result {
            Err(error) => {
                self.active = false;
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
                        self.finalize_completed_journal(&kept_resources);
                        result
                    }
                    Some((gate, retained)) => {
                        self.consume_journal();
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

    fn finalize_completed_journal(&mut self, kept_resources: &[String]) {
        if let Err(error) = self.clear_runner_fence() {
            tracing::warn!(
                target: "session.delete",
                error = %error,
                "completed purge journal retained so recovery can retry clearing its ACP runner fence"
            );
            return;
        }
        let (Some(path), Some(entry)) = (&self.journal_path, &self.journal_entry) else {
            return;
        };
        let removed = entry.with_phase(LifecyclePhase::RowRemoved);
        if let Err(error) = crate::session::lifecycle_journal::update(path, &removed) {
            tracing::warn!(
                target: "session.delete",
                path = %path.display(),
                error = %error,
                "removed purge row but could not advance its lifecycle journal"
            );
            return;
        }
        self.journal_entry = Some(Box::new(removed.clone()));
        if kept_resources.is_empty() {
            self.consume_journal();
        } else {
            let kept = removed
                .with_kept_resources(kept_resources.to_vec())
                .with_phase(LifecyclePhase::Kept);
            if let Err(error) = crate::session::lifecycle_journal::update(path, &kept) {
                tracing::warn!(
                    target: "session.delete",
                    path = %path.display(),
                    error = %error,
                    "kept sidecar lifecycle journal could not be finalized"
                );
            } else {
                self.journal_entry = Some(Box::new(kept));
            }
        }
    }
    pub fn complete_with(
        self,
        after_teardown: impl FnOnce(&Instance) -> std::result::Result<(), String>,
    ) -> DeletionResult {
        self.complete_inner(after_teardown, false)
    }

    pub fn complete(self) -> DeletionResult {
        self.complete_inner(|_| Ok(()), false)
    }
}

impl CommittedPurge {
    /// Clean up transcripts and sidecars before removing the durable row.
    pub fn finish(self) -> DeletionResult {
        self.finish_with_transcript_cleanup(purge_acp_transcript)
    }

    pub(crate) fn finish_with_transcript_cleanup(
        mut self,
        purge_transcript: impl FnOnce(&Instance) -> Result<()>,
    ) -> DeletionResult {
        if let Err(error) = stop_acp_runner_for_purge(&self.request.instance, self.generation) {
            return DeletionResult::rejected(
                self.request.session_id.clone(),
                DeletionDisposition::Failed,
                format!(
                    "ACP runner exit could not be confirmed; purge resources were retained: {error:#}"
                ),
                self.storage.load().ok().and_then(|rows| {
                    rows.into_iter()
                        .find(|row| row.id == self.request.session_id)
                }),
            );
        }
        let mut result = perform_deletion_teardown_lifecycle_locked(&self.request);
        if result.success
            && self
                .journal_entry
                .as_ref()
                .is_some_and(|entry| entry.purge_acp_transcript)
        {
            if let Err(error) = purge_transcript(&self.request.instance) {
                result.success = false;
                result
                    .errors
                    .push(format!("ACP transcript cleanup failed: {error:#}"));
            }
        }
        if !result.success {
            let id = self.request.session_id.clone();
            let generation = self.generation;
            let status_before = self.status_before;
            if let Err(error) = self.update_journal(LifecyclePhase::Abandoned) {
                result.errors.push(format!(
                    "Failed to abandon the rolled-back purge journal: {error:#}"
                ));
            }
            let release_result = self.storage.update(|instances, _groups| {
                if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                    if stored.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation)
                    {
                        if stored.status == Status::Deleting {
                            stored.status = status_before;
                        }
                        stored.release_lifecycle_reservation_if_owned(
                            LifecycleOperation::Purge,
                            generation,
                        );
                    }
                }
                Ok(())
            });
            if let Err(error) = release_result {
                result.errors.push(format!(
                    "Failed to release the retained session row after teardown failure: {error}"
                ));
            } else {
                self.consume_journal();
            }
            result.disposition = DeletionDisposition::Failed;
            result.retained_instance = self
                .storage
                .load()
                .ok()
                .and_then(|instances| instances.into_iter().find(|instance| instance.id == id));
            return result;
        }
        let kept_resources = kept_resources_from_messages(&self.request, &result.messages);
        if let Err(error) = self.update_terminal_checkpoint(&kept_resources) {
            result.success = false;
            result.disposition = DeletionDisposition::Failed;
            result.errors.push(format!(
                "Session teardown completed, but its terminal journal checkpoint failed: {error:#}"
            ));
            self.abandon_and_release(&mut result);
            return result;
        }
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let was_trashed = self.request.instance.is_trashed();
        let status_before = self.status_before;
        let mut gate = None;
        let commit_result = self.storage.update(|instances, _groups| {
            let Some(index) = instances.iter().position(|instance| instance.id == id) else {
                gate = Some((CompletionGate::AlreadyGone, None));
                return Ok(());
            };
            let restored = crate::session::claim::purge_restored_row_must_be_kept(
                was_trashed,
                instances[index].is_trashed(),
            );
            let owns = instances[index]
                .lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation);
            if restored {
                if owns {
                    if instances[index].status == Status::Deleting {
                        instances[index].status = status_before;
                    }
                    instances[index].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                }
                gate = Some((CompletionGate::KeptRestored, Some(instances[index].clone())));
            } else if !owns || instances[index].status != Status::Deleting {
                if owns {
                    instances[index].release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                }
                gate = Some((CompletionGate::Superseded, Some(instances[index].clone())));
            } else {
                instances.remove(index);
                gate = Some((CompletionGate::Proceed, None));
            }
            Ok(())
        });
        match commit_result {
            Err(error) => {
                result.success = false;
                result.disposition = DeletionDisposition::Failed;
                result.errors.push(format!(
                    "Sidecar teardown completed, but sessions.json could not be updated: {error}"
                ));
            }
            Ok(()) => match gate {
                Some((CompletionGate::Proceed | CompletionGate::AlreadyGone, _)) => {
                    result.disposition = if result.success {
                        DeletionDisposition::Removed
                    } else {
                        DeletionDisposition::AlreadyGone
                    };
                    self.finalize_completed_journal(&kept_resources);
                }
                Some((other, retained)) => {
                    self.consume_journal();
                    let mut gated = DeletionResult::rejected(
                        id,
                        match other {
                            CompletionGate::KeptRestored => DeletionDisposition::KeptRestored,
                            CompletionGate::Superseded => DeletionDisposition::Busy,
                            _ => unreachable!(),
                        },
                        "Session changed lifecycle state before purge bookkeeping completed",
                        retained,
                    );
                    gated.teardown_started = true;
                    gated.messages = result.messages;
                    gated.errors.extend(result.errors);
                    return gated;
                }
                None => {
                    result.success = false;
                    result.disposition = DeletionDisposition::Failed;
                    result
                        .errors
                        .push("Purge commit produced no outcome".to_string());
                }
            },
        }
        result
    }

    fn update_journal(&mut self, phase: LifecyclePhase) -> Result<()> {
        let (Some(path), Some(_entry)) = (&self.journal_path, &self.journal_entry) else {
            return Ok(());
        };
        let current = crate::session::lifecycle_journal::read_deletion(path)?
            .ok_or_else(|| anyhow::anyhow!("purge lifecycle journal was replaced"))?;
        anyhow::ensure!(
            current.session_id == self.request.session_id
                && current.source_profile == self.request.instance.source_profile
                && current.generation == self.generation,
            "purge lifecycle journal is no longer owned by this generation"
        );
        let updated = current.with_phase(phase);
        crate::session::lifecycle_journal::update(path, &updated)?;
        self.journal_entry = Some(Box::new(updated));
        Ok(())
    }

    fn update_terminal_checkpoint(&mut self, kept_resources: &[String]) -> Result<()> {
        let (Some(path), Some(_entry)) = (&self.journal_path, &self.journal_entry) else {
            return Ok(());
        };
        let current = crate::session::lifecycle_journal::read_deletion(path)?
            .ok_or_else(|| anyhow::anyhow!("purge lifecycle journal was replaced"))?;
        anyhow::ensure!(
            current.session_id == self.request.session_id
                && current.source_profile == self.request.instance.source_profile
                && current.generation == self.generation,
            "purge lifecycle journal is no longer owned by this generation"
        );
        let updated = current
            .with_kept_resources(kept_resources.to_vec())
            .with_phase(LifecyclePhase::TeardownComplete);
        crate::session::lifecycle_journal::update(path, &updated)?;
        self.journal_entry = Some(Box::new(updated));
        Ok(())
    }

    fn finalize_completed_journal(&mut self, kept_resources: &[String]) {
        if let Err(error) = self.clear_runner_fence() {
            tracing::warn!(
                target: "session.delete",
                error = %error,
                "completed purge journal retained so recovery can retry clearing its ACP runner fence"
            );
            return;
        }
        let (Some(path), Some(entry)) = (&self.journal_path, &self.journal_entry) else {
            return;
        };
        let removed = entry.with_phase(LifecyclePhase::RowRemoved);
        if let Err(error) = crate::session::lifecycle_journal::update(path, &removed) {
            tracing::warn!(
                target: "session.delete",
                path = %path.display(),
                error = %error,
                "removed purge row but could not advance its lifecycle journal"
            );
            return;
        }
        self.journal_entry = Some(Box::new(removed.clone()));
        if kept_resources.is_empty() {
            self.consume_journal();
        } else {
            let kept = removed
                .with_kept_resources(kept_resources.to_vec())
                .with_phase(LifecyclePhase::Kept);
            if let Err(error) = crate::session::lifecycle_journal::update(path, &kept) {
                tracing::warn!(
                    target: "session.delete",
                    path = %path.display(),
                    error = %error,
                    "kept sidecar lifecycle journal could not be finalized"
                );
            } else {
                self.journal_entry = Some(Box::new(kept));
            }
        }
    }

    fn consume_journal(&mut self) {
        let Some(path) = self.journal_path.as_deref() else {
            return;
        };
        let matches_owner = crate::session::lifecycle_journal::read_deletion(path)
            .ok()
            .flatten()
            .is_some_and(|entry| {
                entry.session_id == self.request.session_id
                    && entry.source_profile == self.request.instance.source_profile
                    && entry.generation == self.generation
            });
        if matches_owner {
            if let Err(error) = self.clear_runner_fence() {
                tracing::warn!(
                    target: "session.delete",
                    path = %path.display(),
                    error = %error,
                    "purge journal retained so recovery can retry clearing its ACP runner fence"
                );
                return;
            }
            if let Err(error) = crate::session::lifecycle_journal::consume(path) {
                tracing::warn!(
                    target: "session.delete",
                    path = %path.display(),
                    error = %error,
                    "completed purge could not consume its lifecycle journal"
                );
            }
        } else {
            self.journal_path = None;
            self.journal_entry = None;
        }
    }

    fn clear_runner_fence(&self) -> Result<()> {
        if !self.request.instance.is_structured() {
            return Ok(());
        }
        if let Err(error) = crate::process::worker_registry::clear_purge_fence_if_owned(
            &self.request.session_id,
            &self.request.instance.source_profile,
            self.generation,
        ) {
            tracing::warn!(
                target: "session.delete",
                session = %self.request.session_id,
                error = %error,
                "ACP runner purge fence could not be cleared"
            );
            return Err(error);
        }
        Ok(())
    }

    fn abandon_and_release(&mut self, result: &mut DeletionResult) {
        let id = self.request.session_id.clone();
        if let Err(error) = self.update_journal(LifecyclePhase::Abandoned) {
            result.errors.push(format!(
                "Failed to abandon the rolled-back purge journal: {error:#}"
            ));
        }
        let release_result = self.storage.update(|instances, _groups| {
            if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                if stored.lifecycle_reservation_is_owned(LifecycleOperation::Purge, self.generation)
                {
                    if stored.status == Status::Deleting {
                        stored.status = self.status_before;
                    }
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        self.generation,
                    );
                }
            }
            Ok(())
        });
        if let Err(error) = release_result {
            result.errors.push(format!(
                "Failed to release the retained session row after teardown failure: {error}"
            ));
        } else {
            self.consume_journal();
        }
        result.retained_instance = self
            .storage
            .load()
            .ok()
            .and_then(|rows| rows.into_iter().find(|instance| instance.id == id));
    }
}

impl Drop for PurgeTransaction {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let profile = self.storage.profile().to_string();
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let status_before = self.status_before;
        let structured = self.request.instance.is_structured();
        let journal_path = self.journal_path.clone();
        let _ = std::thread::Builder::new()
            .name("aoe-purge-reservation-release".to_string())
            .spawn(move || {
                let Ok(storage) = Storage::open_unwatched(&profile) else {
                    return;
                };
                let Ok(_lifecycle_lock) = storage.acquire_instance_lifecycle_lock(&id) else {
                    return;
                };
                if let Some(path) = journal_path.as_deref() {
                    if let Ok(Some(entry)) = crate::session::lifecycle_journal::read_deletion(path)
                    {
                        if entry.session_id == id
                            && entry.source_profile == profile
                            && entry.generation == generation
                        {
                            let _ = crate::session::lifecycle_journal::update(
                                path,
                                &entry.with_phase(LifecyclePhase::Abandoned),
                            );
                        }
                    }
                }
                let update = storage.update(|instances, _groups| {
                    if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                        if stored
                            .lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation)
                        {
                            if stored.status == Status::Deleting {
                                stored.status = status_before;
                            }
                            stored.release_lifecycle_reservation_if_owned(
                                LifecycleOperation::Purge,
                                generation,
                            );
                        }
                    }
                    Ok(())
                });
                if update.is_ok() {
                    if let Some(path) = journal_path.as_deref() {
                        let owned = crate::session::lifecycle_journal::read_deletion(path)
                            .ok()
                            .flatten()
                            .is_some_and(|entry| {
                                entry.session_id == id
                                    && entry.source_profile == profile
                                    && entry.generation == generation
                            });
                        if owned {
                            if structured
                                && crate::process::worker_registry::clear_purge_fence_if_owned(
                                    &id, &profile, generation,
                                )
                                .is_err()
                            {
                                return;
                            }
                            let _ = crate::session::lifecycle_journal::consume(path);
                        }
                    }
                }
            });
    }
}

pub fn execute_deletion(request: DeletionRequest) -> DeletionResult {
    let id = request.session_id.clone();
    let recent_entry = crate::session::recent_project_entry_for(&request.instance);
    let result = match PurgeTransaction::reserve_unwatched(request) {
        Ok(PurgeReservation::Reserved(transaction)) => transaction.run_hooks().complete(),
        Ok(PurgeReservation::Rejected(result)) => result,
        Err(error) => DeletionResult::rejected(
            id,
            DeletionDisposition::Failed,
            format!("Could not reserve session deletion: {error}"),
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

pub fn recover_lifecycle_journals_once() -> Result<()> {
    use crate::session::lifecycle_journal::LifecycleJournalEntry;

    let profiles = crate::session::list_profiles()?;
    let mut storages = std::collections::HashMap::new();
    for profile in profiles {
        match Storage::open_unwatched(&profile) {
            Ok(storage) => {
                storages.insert(profile, storage);
            }
            Err(error) => tracing::warn!(
                target: "session.delete_recovery",
                profile = %profile,
                %error,
                "profile could not be opened while scanning lifecycle journals"
            ),
        }
    }

    let scan = crate::session::lifecycle_journal::scan(
        storages
            .values()
            .map(|storage| storage.sessions_path().to_path_buf()),
    );
    let mut blocked_dirs = std::collections::HashSet::new();
    for (dir, error) in scan.unreadable_dirs {
        blocked_dirs.insert(dir.clone());
        tracing::warn!(
            target: "session.delete_recovery",
            path = %dir.display(),
            %error,
            "lifecycle journal directory could not be read"
        );
    }

    let mut entries: Vec<(PathBuf, LifecycleJournalEntry)> = Vec::new();
    for (path, parsed) in scan.entries {
        match parsed {
            Ok(entry) => entries.push((path, entry)),
            Err(error) => {
                if let Some(dir) = path.parent() {
                    blocked_dirs.insert(dir.to_path_buf());
                }
                tracing::error!(
                    target: "session.delete_recovery",
                    path = %path.display(),
                    %error,
                    "unreadable lifecycle journal blocks recovery for this profile"
                );
            }
        }
    }
    entries.sort_by(|left, right| {
        left.1
            .source_profile
            .cmp(&right.1.source_profile)
            .then_with(|| right.1.generation.cmp(&left.1.generation))
            .then_with(|| right.1.created_at_epoch_ms.cmp(&left.1.created_at_epoch_ms))
    });

    let mut handled_sessions = std::collections::HashSet::new();
    for (path, entry) in entries {
        let Some(dir) = path.parent() else {
            continue;
        };
        if blocked_dirs.contains(dir) {
            continue;
        }
        let key = (entry.source_profile.clone(), entry.session_id.clone());
        if handled_sessions.contains(&key) {
            continue;
        }
        if entry.phase == LifecyclePhase::Kept {
            handled_sessions.insert(key);
            continue;
        }
        let Some(storage) = storages.get(&entry.source_profile) else {
            continue;
        };
        match recover_lifecycle_entry(&path, &entry, storage) {
            Ok(true) => {
                handled_sessions.insert(key);
                tracing::info!(
                    target: "session.delete_recovery",
                    profile = %entry.source_profile,
                    session_id = %entry.session_id,
                    "replayed interrupted session purge"
                );
            }
            Ok(false) => {}
            Err(error) => {
                handled_sessions.insert(key);
                tracing::warn!(
                    target: "session.delete_recovery",
                    path = %path.display(),
                    error = %error,
                    "interrupted session purge remains journaled for retry"
                );
            }
        }
    }
    Ok(())
}

pub fn start_lifecycle_recovery_worker() -> Result<()> {
    static STARTED: std::sync::OnceLock<std::result::Result<(), String>> =
        std::sync::OnceLock::new();
    match STARTED.get_or_init(|| {
        std::thread::Builder::new()
            .name("aoe-lifecycle-recovery".to_string())
            .spawn(|| loop {
                if let Err(error) = recover_lifecycle_journals_once() {
                    tracing::warn!(
                        target: "session.delete_recovery",
                        error = %error,
                        "lifecycle journal scan failed"
                    );
                }
                std::thread::sleep(Duration::from_secs(60));
            })
            .map(|handle| {
                drop(handle);
            })
            .map_err(|error| format!("failed to start lifecycle recovery worker: {error}"))
    }) {
        Ok(()) => Ok(()),
        Err(error) => Err(anyhow::anyhow!(error.clone())),
    }
}

fn recover_lifecycle_entry(
    path: &Path,
    scanned_entry: &crate::session::lifecycle_journal::LifecycleJournalEntry,
    storage: &Storage,
) -> Result<bool> {
    let Some(lifecycle_lock) =
        storage.try_acquire_instance_lifecycle_lock(&scanned_entry.session_id)?
    else {
        return Ok(false);
    };
    let mut lifecycle_lock = Some(lifecycle_lock);

    let Some(mut current_entry) = crate::session::lifecycle_journal::read_deletion(path)? else {
        return Ok(false);
    };
    anyhow::ensure!(
        current_entry.session_id == current_entry.instance.id,
        "journal session id does not match its instance snapshot"
    );
    anyhow::ensure!(
        current_entry.source_profile == storage.profile(),
        "journal profile does not match the opened profile"
    );
    if current_entry.session_id != scanned_entry.session_id
        || current_entry.generation != scanned_entry.generation
    {
        return Ok(false);
    }
    crate::session::validate_instance_id(&current_entry.session_id)
        .context("journal contains an invalid session id")?;
    anyhow::ensure!(
        paths_refer_to_same_sessions_file(&current_entry.sessions_path, storage.sessions_path()),
        "journal sessions path does not match its source profile"
    );
    if matches!(current_entry.phase, LifecyclePhase::Kept) {
        return Ok(false);
    }
    if current_entry.phase == LifecyclePhase::Abandoned {
        abandon_recovery_entry(path, &current_entry, storage)?;
        return Ok(false);
    }

    let id = current_entry.session_id.clone();
    let mut owners = session_owner_profiles(&id)?;
    let foreign_owner = owners.iter().any(|profile| profile != storage.profile());
    if foreign_owner
        || owners
            .iter()
            .filter(|profile| *profile == storage.profile())
            .count()
            > 1
    {
        clear_recovery_fence(&current_entry)?;
        consume_lifecycle_journal_if_owned(path, &current_entry)?;
        return Ok(false);
    }

    let mut row_present = owners.iter().any(|profile| profile == storage.profile());
    if !row_present && !current_entry.phase.teardown_started() {
        clear_recovery_fence(&current_entry)?;
        consume_lifecycle_journal_if_owned(path, &current_entry)?;
        return Ok(false);
    }

    let mut current_path = path.to_path_buf();
    if row_present {
        let stored = storage
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or_else(|| {
                anyhow::anyhow!("profile owner disappeared while purge lock was held")
            })?;
        if stored.status != Status::Deleting
            || crate::session::claim::purge_restored_row_must_be_kept(
                current_entry.instance.is_trashed(),
                stored.is_trashed(),
            )
            || !stored
                .lifecycle_reservation_is_owned(LifecycleOperation::Purge, current_entry.generation)
        {
            clear_recovery_fence(&current_entry)?;
            consume_lifecycle_journal_if_owned(path, &current_entry)?;
            return Ok(false);
        }
        if stored.has_fresh_lifecycle_reservation(Utc::now()) {
            return Ok(false);
        }

        let old_entry = current_entry.clone();
        let mut next_entry = current_entry.clone();
        let mut next_path = current_path.clone();
        let mut superseded = false;
        storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|row| row.id == id) else {
                superseded = true;
                return Ok(());
            };
            if stored.status != Status::Deleting
                || !stored
                    .lifecycle_reservation_is_owned(LifecycleOperation::Purge, old_entry.generation)
            {
                superseded = true;
                return Ok(());
            }
            let generation = stored.try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )?;
            next_entry = old_entry.with_generation(generation);
            next_entry.instance = stored.clone();
            next_path = crate::session::lifecycle_journal::record(&next_entry)?;
            Ok(())
        })?;
        if superseded {
            clear_recovery_fence(&old_entry)?;
            consume_lifecycle_journal_if_owned(path, &old_entry)?;
            return Ok(false);
        }
        current_entry = next_entry;
        current_path = next_path;
        if current_path != path {
            consume_lifecycle_journal_if_owned(path, &old_entry)?;
        }
    }

    if current_entry.instance.is_structured() {
        crate::process::worker_registry::fence_for_purge(
            &id,
            &current_entry.source_profile,
            current_entry.generation,
        )?;
    }

    stop_acp_runner_for_purge(&current_entry.instance, current_entry.generation)?;

    if !current_entry.phase.hooks_are_complete() {
        current_entry = current_entry.with_phase(LifecyclePhase::HooksStarted);
        crate::session::lifecycle_journal::update(&current_path, &current_entry)?;
        let generation = current_entry.generation;
        let heartbeat = if row_present {
            match ReservationHeartbeat::start(
                storage,
                &id,
                LifecycleOperation::Purge,
                generation,
                Duration::from_secs(60),
            ) {
                Ok(heartbeat) => Some(heartbeat),
                Err(error) => {
                    abandon_recovery_entry(&current_path, &current_entry, storage)?;
                    return Err(error);
                }
            }
        } else {
            None
        };
        if row_present {
            drop(lifecycle_lock.take());
        }
        let mut instance = current_entry.instance.clone();
        instance.source_profile = current_entry.source_profile.clone();
        run_on_destroy_hooks(&instance, true);
        if let Some(heartbeat) = heartbeat {
            heartbeat.stop();
        }
        if row_present {
            lifecycle_lock = storage.try_acquire_instance_lifecycle_lock(&id)?;
            if lifecycle_lock.is_none() {
                return Ok(false);
            }
        }
        let Some(latest) = crate::session::lifecycle_journal::read_deletion(&current_path)? else {
            return Ok(false);
        };
        if latest.session_id != current_entry.session_id
            || latest.source_profile != current_entry.source_profile
            || latest.generation != current_entry.generation
        {
            return Ok(false);
        }
        current_entry = latest;
        owners = session_owner_profiles(&id)?;
        if owners.iter().any(|profile| profile != storage.profile())
            || owners
                .iter()
                .filter(|profile| *profile == storage.profile())
                .count()
                > 1
        {
            clear_recovery_fence(&current_entry)?;
            consume_lifecycle_journal_if_owned(&current_path, &current_entry)?;
            return Ok(false);
        }
        row_present = owners.iter().any(|profile| profile == storage.profile());
        if row_present {
            let stored = storage
                .load()?
                .into_iter()
                .find(|row| row.id == id)
                .ok_or_else(|| anyhow::anyhow!("profile owner disappeared during hook replay"))?;
            if stored.status != Status::Deleting
                || crate::session::claim::purge_restored_row_must_be_kept(
                    current_entry.instance.is_trashed(),
                    stored.is_trashed(),
                )
                || !stored.lifecycle_reservation_is_owned(
                    LifecycleOperation::Purge,
                    current_entry.generation,
                )
            {
                clear_recovery_fence(&current_entry)?;
                consume_lifecycle_journal_if_owned(&current_path, &current_entry)?;
                return Ok(false);
            }
        }
        current_entry = current_entry.with_phase(LifecyclePhase::HooksComplete);
        crate::session::lifecycle_journal::update(&current_path, &current_entry)?;
    }

    owners = session_owner_profiles(&id)?;
    if owners.iter().any(|profile| profile != storage.profile())
        || owners
            .iter()
            .filter(|profile| *profile == storage.profile())
            .count()
            > 1
    {
        clear_recovery_fence(&current_entry)?;
        consume_lifecycle_journal_if_owned(&current_path, &current_entry)?;
        return Ok(false);
    }
    row_present = owners.iter().any(|profile| profile == storage.profile());
    if row_present {
        let stored = storage
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or_else(|| anyhow::anyhow!("profile owner disappeared before teardown"))?;
        if stored.status != Status::Deleting
            || crate::session::claim::purge_restored_row_must_be_kept(
                current_entry.instance.is_trashed(),
                stored.is_trashed(),
            )
            || !stored
                .lifecycle_reservation_is_owned(LifecycleOperation::Purge, current_entry.generation)
        {
            clear_recovery_fence(&current_entry)?;
            consume_lifecycle_journal_if_owned(&current_path, &current_entry)?;
            return Ok(false);
        }
    } else if !current_entry.phase.teardown_started() {
        clear_recovery_fence(&current_entry)?;
        consume_lifecycle_journal_if_owned(&current_path, &current_entry)?;
        return Ok(false);
    }

    if !current_entry.phase.teardown_complete() {
        current_entry = current_entry.with_phase(LifecyclePhase::TeardownStarted);
        crate::session::lifecycle_journal::update(&current_path, &current_entry)?;
        let mut request_instance = current_entry.instance.clone();
        request_instance.source_profile = current_entry.source_profile.clone();
        let request = DeletionRequest {
            session_id: id.clone(),
            instance: request_instance.clone(),
            delete_worktree: current_entry.delete_worktree,
            delete_branch: current_entry.delete_branch,
            delete_sandbox: current_entry.delete_sandbox,
            force_delete: current_entry.force_delete,
            detach_hooks: true,
            keep_scratch: current_entry.keep_scratch,
        };
        let result = perform_deletion_teardown_lifecycle_locked(&request);
        if !result.success {
            let error = result.errors.join("; ");
            abandon_recovery_entry(&current_path, &current_entry, storage)?;
            anyhow::bail!("lifecycle teardown failed: {error}");
        }
        if current_entry.purge_acp_transcript {
            if let Err(error) = purge_acp_transcript(&request_instance) {
                abandon_recovery_entry(&current_path, &current_entry, storage)?;
                anyhow::bail!("ACP transcript purge failed: {error:#}");
            }
        }
        let kept_resources = kept_resources_from_messages(&request, &result.messages);
        current_entry = current_entry
            .with_kept_resources(kept_resources)
            .with_phase(LifecyclePhase::TeardownComplete);
        if let Err(error) = crate::session::lifecycle_journal::update(&current_path, &current_entry)
        {
            abandon_recovery_entry(&current_path, &current_entry, storage)?;
            return Err(error);
        }
    }

    let latest = crate::session::lifecycle_journal::read_deletion(&current_path)?
        .ok_or_else(|| anyhow::anyhow!("purge lifecycle journal disappeared before row commit"))?;
    if latest.session_id != current_entry.session_id
        || latest.source_profile != current_entry.source_profile
        || latest.generation != current_entry.generation
        || !latest.phase.teardown_complete()
    {
        return Ok(false);
    }
    current_entry = latest;
    owners = session_owner_profiles(&id)?;
    if owners.iter().any(|profile| profile != storage.profile())
        || owners
            .iter()
            .filter(|profile| *profile == storage.profile())
            .count()
            > 1
    {
        clear_recovery_fence(&current_entry)?;
        consume_lifecycle_journal_if_owned(&current_path, &current_entry)?;
        return Ok(false);
    }
    row_present = owners.iter().any(|profile| profile == storage.profile());
    let mut row_superseded = false;
    if row_present {
        storage.update(|instances, _groups| {
            let Some(index) = instances.iter().position(|row| row.id == id) else {
                row_superseded = true;
                return Ok(());
            };
            if instances[index].status != Status::Deleting
                || !instances[index].lifecycle_reservation_is_owned(
                    LifecycleOperation::Purge,
                    current_entry.generation,
                )
                || crate::session::claim::purge_restored_row_must_be_kept(
                    current_entry.instance.is_trashed(),
                    instances[index].is_trashed(),
                )
            {
                row_superseded = true;
                return Ok(());
            }
            instances.remove(index);
            Ok(())
        })?;
    }
    if row_superseded {
        clear_recovery_fence(&current_entry)?;
        consume_lifecycle_journal_if_owned(&current_path, &current_entry)?;
        return Ok(false);
    }

    let removed = current_entry.with_phase(LifecyclePhase::RowRemoved);
    crate::session::lifecycle_journal::update(&current_path, &removed)?;
    clear_recovery_fence(&removed)?;
    if removed.kept_resources.is_empty() {
        consume_lifecycle_journal_if_owned(&current_path, &removed)?;
    } else {
        let kept = removed.with_phase(LifecyclePhase::Kept);
        crate::session::lifecycle_journal::update(&current_path, &kept)?;
    }
    drop(lifecycle_lock);
    Ok(true)
}

fn session_owner_profiles(session_id: &str) -> Result<Vec<String>> {
    let (profiles, storages) = all_profile_storages().map_err(|error| {
        anyhow::anyhow!("could not verify cross-profile session ownership: {error}")
    })?;
    let mut owners = Vec::new();
    for (profile, storage) in profiles.into_iter().zip(storages) {
        let rows = storage.load().with_context(|| {
            format!("failed to inspect profile '{profile}' for session ownership")
        })?;
        let matches = rows.iter().filter(|row| row.id == session_id).count();
        owners.extend(std::iter::repeat_n(profile, matches));
    }
    Ok(owners)
}

fn consume_lifecycle_journal_if_owned(
    path: &Path,
    expected: &crate::session::lifecycle_journal::LifecycleJournalEntry,
) -> Result<()> {
    let Some(current) = crate::session::lifecycle_journal::read_deletion(path)? else {
        return Ok(());
    };
    if current.session_id == expected.session_id
        && current.source_profile == expected.source_profile
        && current.generation == expected.generation
    {
        crate::session::lifecycle_journal::consume(path)?;
    }
    Ok(())
}

fn clear_recovery_fence(
    entry: &crate::session::lifecycle_journal::LifecycleJournalEntry,
) -> Result<()> {
    if entry.instance.is_structured() {
        crate::process::worker_registry::clear_purge_fence_if_owned(
            &entry.session_id,
            &entry.source_profile,
            entry.generation,
        )?;
    }
    Ok(())
}

fn abandon_recovery_entry(
    path: &Path,
    entry: &crate::session::lifecycle_journal::LifecycleJournalEntry,
    storage: &Storage,
) -> Result<()> {
    let Some(current) = crate::session::lifecycle_journal::read_deletion(path)? else {
        return Ok(());
    };
    if current.session_id != entry.session_id
        || current.source_profile != entry.source_profile
        || current.generation != entry.generation
    {
        return Ok(());
    }
    let abandoned = current.with_phase(LifecyclePhase::Abandoned);
    if let Err(update_error) = crate::session::lifecycle_journal::update(path, &abandoned) {
        tracing::warn!(
            target: "session.delete_recovery",
            path = %path.display(),
            error = %update_error,
            "journal abandonment checkpoint failed; retaining the existing intent for recovery"
        );
    }
    storage.update(|instances, _groups| {
        if let Some(stored) = instances.iter_mut().find(|row| row.id == entry.session_id) {
            if stored.lifecycle_reservation_is_owned(LifecycleOperation::Purge, entry.generation) {
                if stored.status == Status::Deleting {
                    stored.status = entry.status_before;
                }
                stored.release_lifecycle_reservation_if_owned(
                    LifecycleOperation::Purge,
                    entry.generation,
                );
            }
        }
        Ok(())
    })?;
    clear_recovery_fence(&abandoned)?;
    consume_lifecycle_journal_if_owned(path, &abandoned)?;
    Ok(())
}
fn paths_refer_to_same_sessions_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    let (Some(left_name), Some(right_name), Some(left_parent), Some(right_parent)) = (
        left.file_name(),
        right.file_name(),
        left.parent(),
        right.parent(),
    ) else {
        return false;
    };
    left_name == right_name
        && left_parent
            .canonicalize()
            .ok()
            .zip(right_parent.canonicalize().ok())
            .is_some_and(|(left, right)| left == right)
}

fn kept_resources_from_messages(request: &DeletionRequest, messages: &[String]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| {
            let lower = message.to_ascii_lowercase();
            lower.contains(" kept") || lower.starts_with("kept") || lower.contains("preserved")
        })
        .map(|message| {
            let workspace_path = request
                .instance
                .workspace_info
                .as_ref()
                .filter(|_| message.starts_with("Workspace directory kept:"))
                .map(|workspace| workspace.workspace_dir.as_str());
            workspace_path.map_or_else(|| message.clone(), |path| format!("{message} [{path}]"))
        })
        .collect()
}

pub(crate) fn purge_acp_transcript(instance: &Instance) -> Result<()> {
    let app_dir = crate::session::get_app_dir()
        .map_err(|error| anyhow::anyhow!("acp transcript purge: resolve app dir: {error}"))?;
    let db_path = app_dir.join("acp_events.db");
    if !db_path.exists() {
        return Ok(());
    }
    purge_acp_transcript_rows(&db_path, &instance.id)
}

fn stop_acp_runner_for_purge(instance: &Instance, generation: u64) -> Result<()> {
    if !instance.is_structured() {
        return Ok(());
    }
    anyhow::ensure!(
        !instance.source_profile.is_empty(),
        "structured session has no source profile"
    );
    crate::process::worker_registry::fence_for_purge(
        &instance.id,
        &instance.source_profile,
        generation,
    )?;
    crate::process::worker_registry::terminate_and_confirm_stopped(
        &instance.id,
        &instance.source_profile,
        generation,
    )
}

pub(crate) fn purge_acp_transcript_rows(db_path: &Path, session_id: &str) -> Result<()> {
    let mut conn = rusqlite::Connection::open(db_path)
        .map_err(|error| anyhow::anyhow!("acp transcript purge: open event store: {error}"))?;
    conn.busy_timeout(Duration::from_secs(5))
        .map_err(|error| anyhow::anyhow!("acp transcript purge: set busy_timeout: {error}"))?;
    let transaction = conn
        .transaction()
        .map_err(|error| anyhow::anyhow!("acp transcript purge: begin transaction: {error}"))?;
    let schema = crate::events::Schema::new("acp")
        .map_err(|error| anyhow::anyhow!("acp transcript purge: schema: {error}"))?;
    for table in [
        schema.events_table(),
        schema.attachments_table(),
        schema.pending_attachments_table(),
        schema.rate_limit_budgets_table(),
    ] {
        match transaction.execute(
            &format!("DELETE FROM {table} WHERE session_id = ?1"),
            rusqlite::params![session_id],
        ) {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(_, Some(message)))
                if message.contains("no such table") => {}
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "acp transcript purge: delete from {table}: {error}"
                ));
            }
        }
    }
    transaction
        .commit()
        .map_err(|error| anyhow::anyhow!("acp transcript purge: commit: {error}"))
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
            Ok(now) if now.iter().all(|profile| profiles.contains(profile)) => {
                scan_paths_in_use(&storages, &[except_id])
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

fn perform_deletion_teardown_lifecycle_locked(request: &DeletionRequest) -> DeletionResult {
    perform_deletion_core(request, true, |session_id| {
        DockerContainer::from_session_id(session_id).teardown(session_id)
    })
}

/// Core deletion routine, parameterized over how the sandbox container is torn down so the
/// container-removal contract can be exercised without a live runtime.
#[cfg(test)]
fn perform_deletion_with(
    request: &DeletionRequest,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
) -> DeletionResult {
    perform_deletion_core(request, false, teardown)
}

/// `lifecycle_locked` is the production path, which also keeps any worktree another session uses.
fn perform_deletion_core(
    request: &DeletionRequest,
    lifecycle_locked: bool,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
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

    // on_destroy hooks run in the transaction's unlocked hook phase, before
    // this lifecycle-locked resource teardown begins.

    // Stage 2: sever the live agent BEFORE we touch the working tree it may be writing to.
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

    // Host-side dirty check. The in-container preclean below destroys
    // worktree contents unconditionally, which would silently violate
    // the `force_delete=false` safety contract for users with untracked
    // or modified files. Walk every managed worktree we'd touch and
    // collect the dirty ones; preclean is skipped if anything is dirty
    // (the `find -delete` runs at the workspace root and can't easily
    // skip subpaths), and host-side worktree removal is skipped per
    // path that's dirty. Container, branch, and hook stages still run
    // per the user's flags.
    // Every repo the session works in, whether it was created multi-repo or
    // converted by `attach_project` (#3103): both end up in
    // `workspace_info.repos`, so one loop per stage covers them.
    //
    // `cleanup_on_delete` is the user's opt-out for the whole workspace, so it
    // gates the list rather than any individual repo.
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

    stage_cleanup_scratch(request, &mut errors, &mut messages);

    // Last, and only when nothing else failed: any error here rolls the purge back
    // (`PurgeTransaction::complete_inner`), and a session that survives its own purge must survive
    // with the store holding its login and history.
    if container_gone && errors.is_empty() {
        stage_remove_agent_stores(request, &mut messages);
    }

    // Stage 6: hook status cleanup
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
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) {
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
    use crate::containers::error::DockerError;
    use crate::containers::Teardown;
    use crate::session::test_support::{isolate_app_dir, isolate_app_dir_at};
    use crate::session::{SandboxInfo, View, WorkspaceInfo, WorkspaceRepo, WorktreeInfo};
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

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn acp_writer_helper() {
        use std::io::Write as _;
        use std::os::unix::net::UnixListener;

        let Some(socket_path) = std::env::var_os("AOE_TEST_ACP_WRITER_SOCKET") else {
            return;
        };
        let output_path =
            std::env::var_os("AOE_TEST_ACP_WRITER_OUTPUT").expect("writer output path");
        let listener = UnixListener::bind(socket_path).expect("bind ACP control socket");
        listener.set_nonblocking(true).unwrap();
        loop {
            match listener.accept() {
                Ok((stream, _)) => drop(stream),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("accept ACP control probe: {error}"),
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&output_path)
                .and_then(|mut file| file.write_all(b"x"))
                .expect("write ACP output");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    fn spawn_live_acp_writer(
        socket_path: &Path,
        output_path: &Path,
        reaped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> (u32, std::thread::JoinHandle<std::process::ExitStatus>) {
        use std::os::unix::process::CommandExt as _;

        let control_path = crate::process::worker::control_socket_sibling(socket_path);
        let executable = std::env::current_exe().unwrap();
        let mut child = std::process::Command::new(executable);
        child
            .args([
                "--exact",
                "session::deletion::tests::acp_writer_helper",
                "--nocapture",
            ])
            .env("AOE_TEST_ACP_WRITER_SOCKET", &control_path)
            .env("AOE_TEST_ACP_WRITER_OUTPUT", output_path)
            .process_group(0);
        let mut child = child.spawn().expect("spawn ACP writer stand-in");
        let pid = child.id();
        let waiter = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        reaped.store(true, std::sync::atomic::Ordering::Release);
                        return status;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        crate::process::worker::kill_process_group(pid);
                        let _ = child.kill();
                        let _ = child.wait();
                        panic!("wait for ACP writer: {error}");
                    }
                }
                if std::time::Instant::now() >= deadline {
                    crate::process::worker::kill_process_group(pid);
                    let _ = child.kill();
                    let status = child.wait().expect("reap timed-out ACP writer");
                    reaped.store(true, std::sync::atomic::Ordering::Release);
                    return status;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });

        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if crate::process::worker::peer_pid_from_socket(&control_path) == Some(pid) {
                return (pid, waiter);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("ACP control socket did not report its runner PID {pid}");
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
        instance
    }

    fn expired_deleting_row(mut snapshot: Instance) -> (Instance, Instance, u64) {
        let now = Utc::now();
        let mut row = snapshot.clone();
        let generation = row
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            )
            .unwrap();
        row.lifecycle_reservation
            .as_mut()
            .expect("purge reservation")
            .at = now - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1);
        row.status = Status::Deleting;
        snapshot.lifecycle_generation = row.lifecycle_generation;
        snapshot.lifecycle_reservation = row.lifecycle_reservation.clone();
        (snapshot, row, generation)
    }

    fn wait_for_reservation_renewal(
        storage: &Storage,
        session_id: &str,
        operation: LifecycleOperation,
        generation: u64,
        before: chrono::DateTime<Utc>,
    ) -> chrono::DateTime<Utc> {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let reservation = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|instance| instance.id == session_id)
                .and_then(|instance| instance.lifecycle_reservation);
            if let Some(reservation) = reservation.filter(|reservation| {
                reservation.op == operation
                    && reservation.generation == generation
                    && reservation.at > before
            }) {
                return reservation.at;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reservation heartbeat did not renew {operation:?} generation {generation}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn deletion_without_artifacts_succeeds_and_keeps_session_id() {
        let _app_guard = isolate_app_dir();
        for delete_worktree in [false, true] {
            let request = DeletionRequest {
                session_id: "custom-session-id-123".to_string(),
                delete_worktree,
                ..request(Instance::new("Test Session", "/tmp/test-project"))
            };
            let result = perform_deletion(&request);
            assert!(result.success && result.errors.is_empty());
            assert_eq!(result.session_id, "custom-session-id-123");
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
            let persisted = crate::session::lifecycle_journal::read_deletion(
                transaction.journal_path.as_deref().unwrap(),
            )
            .unwrap()
            .unwrap();
            assert_eq!(persisted.force_delete, same_lifecycle, "{profile}");
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
        );
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
        assert_eq!(retained[0].status, Status::Deleting);
        assert!(!retained[0].has_fresh_lifecycle_reservation(Utc::now()));

        let mut retry = retained.into_iter().next().unwrap();
        retry.source_profile = profile.to_string();
        let Ok(committed) = reserve(profile, retry).begin_irreversible() else {
            panic!("current purge reservation was rejected");
        };
        assert!(
            storage.load().unwrap().first().is_some_and(|instance| {
                instance.status == Status::Deleting && instance.lifecycle_reservation.is_some()
            }),
            "durable row must remain deleting and reserved until irreversible cleanup finishes"
        );
        let journal_before_finish =
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()]);
        assert_eq!(journal_before_finish.entries.len(), 1);
        assert_eq!(
            journal_before_finish.entries[0].1.as_ref().unwrap().phase,
            LifecyclePhase::TeardownStarted
        );
        assert_eq!(committed.finish().disposition, DeletionDisposition::Removed);
        assert!(
            storage.load().unwrap().is_empty(),
            "durable row must be removed after irreversible cleanup finishes"
        );
        assert!(
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                .entries
                .is_empty(),
            "completed purge must consume its lifecycle journal"
        );
    }

    #[test]
    #[serial]
    fn purge_hook_heartbeat_renews_the_durable_reservation() {
        let _guard = isolate_app_dir();
        let profile = "purge-heartbeat";
        let storage = Storage::new_unwatched(profile).unwrap();
        let instance = stored_instance(&storage, profile, "/tmp/test-project");
        let session_id = instance.id.clone();
        let transaction = reserve(profile, instance);
        let before = {
            let rows = storage.load().unwrap();
            rows[0]
                .lifecycle_reservation
                .as_ref()
                .expect("purge reservation")
                .at
        };
        let generation = transaction.generation;
        let (hook_started_tx, hook_started_rx) = std::sync::mpsc::channel();
        let (release_hook_tx, release_hook_rx) = std::sync::mpsc::channel();
        let hook = std::thread::spawn(move || {
            transaction.run_hooks_with_interval(
                move |_, _| {
                    hook_started_tx.send(()).unwrap();
                    release_hook_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("test must release the purge hook");
                },
                Duration::from_millis(20),
            )
        });
        hook_started_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("purge hook did not start");
        wait_for_reservation_renewal(
            &storage,
            &session_id,
            LifecycleOperation::Purge,
            generation,
            before,
        );
        release_hook_tx.send(()).unwrap();
        let transaction = hook.join().expect("purge hook thread panicked");
        assert_eq!(
            transaction.complete().disposition,
            DeletionDisposition::Removed
        );
    }

    #[test]
    #[serial]
    fn launch_heartbeat_renews_and_respects_generation_ownership() {
        let _guard = isolate_app_dir();
        let profile = "launch-heartbeat";
        let storage = Storage::new_unwatched(profile).unwrap();
        let instance = stored_instance(&storage, profile, "/tmp/test-project");
        let session_id = instance.id.clone();
        let generation = storage
            .update(|instances, _groups| {
                instances[0]
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            })
            .unwrap();
        let before = storage.load().unwrap()[0]
            .lifecycle_reservation
            .as_ref()
            .unwrap()
            .at;

        let heartbeat = crate::session::ReservationHeartbeat::start(
            &storage,
            &session_id,
            LifecycleOperation::Launch,
            generation,
            std::time::Duration::from_millis(20),
        )
        .unwrap();
        wait_for_reservation_renewal(
            &storage,
            &session_id,
            LifecycleOperation::Launch,
            generation,
            before,
        );
        heartbeat.stop();

        let reservation = storage.load().unwrap()[0]
            .lifecycle_reservation
            .clone()
            .unwrap();
        assert_eq!(reservation.op, LifecycleOperation::Launch);
        assert_eq!(reservation.generation, generation);
        assert!(
            reservation.at > before,
            "launch heartbeat did not renew the reservation"
        );

        let replacement_at =
            reservation.at + Instance::LIFECYCLE_RESERVATION_TTL + chrono::Duration::seconds(1);
        let replacement_generation = storage
            .update(|instances, _groups| {
                instances[0]
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Purge,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        replacement_at,
                    )
                    .map_err(|error| anyhow::anyhow!(error.to_string()))
            })
            .unwrap();
        let stale_renewal = storage
            .update(|instances, _groups| {
                let renewed = instances
                    .iter_mut()
                    .find(|instance| instance.id == session_id)
                    .is_some_and(|instance| {
                        instance.renew_lifecycle_reservation_if_owned(
                            LifecycleOperation::Launch,
                            generation,
                            Utc::now(),
                        )
                    });
                Ok(renewed)
            })
            .unwrap();
        assert!(!stale_renewal, "stale generation renewed its successor");

        let current = storage.load().unwrap()[0]
            .lifecycle_reservation
            .clone()
            .unwrap();
        assert_eq!(current.op, LifecycleOperation::Purge);
        assert_eq!(current.generation, replacement_generation);
        assert_eq!(
            current.at, replacement_at,
            "stale heartbeat changed its successor's lease"
        );
    }

    #[test]
    #[serial]
    fn kept_workspace_is_recorded_as_a_lifecycle_tombstone() {
        let _guard = isolate_app_dir();
        let profile = "kept-workspace-tombstone";
        let storage = Storage::new_unwatched(profile).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let main_repo = temp.path().join("frontend");
        let workspace = temp.path().join("workspace");
        let worktree = workspace.join("frontend");
        init_repo(&main_repo);
        std::fs::create_dir_all(&workspace).unwrap();
        git_in(
            &main_repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/kept-tombstone",
                worktree.to_str().unwrap(),
                "HEAD",
            ],
        );
        let stray = workspace.join("stray.txt");
        std::fs::write(&stray, "keep me").unwrap();

        let mut instance = Instance::new("Workspace", workspace.to_str().unwrap());
        instance.source_profile = profile.to_string();
        instance.workspace_info = Some(workspace_info(
            &workspace,
            vec![workspace_repo(
                &main_repo,
                &worktree,
                "feature/kept-tombstone",
            )],
        ));
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();

        let mut deletion = request(instance.clone());
        deletion.delete_worktree = true;
        deletion.delete_branch = true;
        let transaction = match PurgeTransaction::reserve(storage.clone(), deletion).unwrap() {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("purge was rejected: {result:?}"),
        };
        let result = transaction.run_hooks_with(|_, _| {}).complete();

        let workspace_path = workspace.to_string_lossy().to_string();
        assert!(result.success, "purge failed: {:?}", result.errors);
        assert!(result
            .messages
            .iter()
            .any(|message| message.contains(&workspace_path)));
        assert!(
            storage.load().unwrap().is_empty(),
            "successful purge must remove its row"
        );
        assert_eq!(std::fs::read_to_string(&stray).unwrap(), "keep me");
        assert!(workspace.exists());
        assert!(!worktree.exists());

        let journals =
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()]);
        assert_eq!(journals.entries.len(), 1);
        let tombstone = journals.entries[0].1.as_ref().unwrap();
        assert_eq!(tombstone.phase, LifecyclePhase::Kept);
        assert!(tombstone
            .kept_resources
            .iter()
            .any(|resource| resource.contains(&workspace_path)));

        recover_lifecycle_journals_once().unwrap();
        assert!(
            workspace.exists(),
            "startup recovery must leave an intentional tombstone alone"
        );
        assert_eq!(
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                .entries
                .len(),
            1
        );
    }

    #[test]
    #[serial]
    fn failed_teardown_abandons_the_purge_intent_for_explicit_retry() {
        let _guard = isolate_app_dir();
        let profile = "purge-failure-journal";
        let storage = Storage::new_unwatched(profile).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let mut instance = stored_instance(&storage, profile, temp.path().to_str().unwrap());
        instance.scratch = true;
        storage
            .update(|instances, _groups| {
                instances.clear();
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();

        let result = reserve(profile, instance).run_hooks().complete();
        assert_eq!(result.disposition, DeletionDisposition::Failed);
        assert!(!result.success);
        let rows = storage.load().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::Idle);
        assert!(rows[0].lifecycle_reservation.is_none());
        let journals =
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()]);
        assert!(journals.entries.is_empty());

        recover_lifecycle_journals_once().unwrap();
        let rows = storage.load().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::Idle);
        assert!(rows[0].lifecycle_reservation.is_none());
    }

    #[test]
    #[serial]
    fn failed_acp_transcript_cleanup_does_not_replay_the_user_visible_purge() {
        let _guard = isolate_app_dir();
        let profile = "purge-transcript-recovery";
        let storage = Storage::new_unwatched(profile).unwrap();
        let instance = stored_instance(&storage, profile, "/tmp/test-project");

        let transaction = match PurgeTransaction::reserve_with_acp_transcript(
            storage.clone(),
            request(instance.clone()),
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("purge was rejected: {result:?}"),
        };
        let result = transaction
            .run_hooks_with(|_, _| {})
            .complete_with(|_| Err("forced transcript store failure".to_string()));
        assert_eq!(result.disposition, DeletionDisposition::Failed);
        let rows = storage.load().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::Idle);
        assert!(rows[0].lifecycle_reservation.is_none());
        assert!(
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                .entries
                .is_empty()
        );

        let db_path = crate::session::get_app_dir().unwrap().join("acp_events.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_events (session_id TEXT, seq INTEGER, event_json TEXT);
             CREATE TABLE acp_attachments (session_id TEXT, attachment_id TEXT, data BLOB);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_events VALUES (?1, 0, '{}'), ('retained', 0, '{}')",
            rusqlite::params![instance.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_attachments VALUES (?1, 'a0', x'00'), ('retained', 'a1', x'01')",
            rusqlite::params![instance.id],
        )
        .unwrap();
        drop(conn);

        recover_lifecycle_journals_once().unwrap();
        assert_eq!(storage.load().unwrap().len(), 1);

        let conn = rusqlite::Connection::open(db_path).unwrap();
        let purged_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = ?1",
                rusqlite::params![instance.id],
                |row| row.get(0),
            )
            .unwrap();
        let retained_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 'retained'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let purged_attachments: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_attachments WHERE session_id = ?1",
                rusqlite::params![instance.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(purged_events, 1);
        assert_eq!(purged_attachments, 1);
        assert_eq!(retained_events, 1);
    }

    #[test]
    #[serial]
    fn committed_purge_removes_only_the_target_acp_transcript() {
        let _guard = isolate_app_dir();
        let profile = "committed-purge-acp-transcript";
        let storage = Storage::new_unwatched(profile).unwrap();
        let instance = stored_instance(&storage, profile, "/tmp/test-project");
        let db_path = crate::session::get_app_dir().unwrap().join("acp_events.db");
        let event_store = crate::acp::event_store::EventStore::open(&db_path, 100).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO acp_events (session_id, seq, event_json, created_at)
             VALUES (?1, 0, '{}', 0), ('retained', 0, '{}', 0)",
            rusqlite::params![instance.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_attachments
             (session_id, seq, attachment_id, kind, mime_type, data, created_at)
             VALUES (?1, 0, 'a0', 'file', 'text/plain', x'00', 0),
                    ('retained', 0, 'a1', 'file', 'text/plain', x'01', 0)",
            rusqlite::params![instance.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_pending_attachments
             (session_id, ref_id, attachment_id, kind, mime_type, data, created_at)
             VALUES (?1, 'r0', 'a0', 'file', 'text/plain', x'00', 0),
                    ('retained', 'r1', 'a1', 'file', 'text/plain', x'01', 0)",
            rusqlite::params![instance.id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO acp_rate_limit_budgets VALUES (?1, 2, 1), ('retained', 3, 1)",
            rusqlite::params![instance.id],
        )
        .unwrap();
        drop(conn);

        let transaction = match PurgeTransaction::reserve_with_acp_transcript(
            storage.clone(),
            request(instance.clone()),
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("purge was rejected: {result:?}"),
        };
        let committed = transaction
            .run_hooks_with(|_, _| {})
            .begin_irreversible()
            .expect("purge reservation should reach committed teardown");
        assert_eq!(storage.load().unwrap()[0].status, Status::Deleting);

        let result = committed.finish_with_transcript_cleanup(|instance| {
            event_store.delete_session_fallible(&instance.id)
        });

        assert!(result.success, "purge failed: {:?}", result.errors);
        assert!(storage.load().unwrap().is_empty());
        let conn = rusqlite::Connection::open(db_path).unwrap();
        let target_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = ?1",
                rusqlite::params![instance.id],
                |row| row.get(0),
            )
            .unwrap();
        let retained_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 'retained'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let target_attachments: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_attachments WHERE session_id = ?1",
                rusqlite::params![instance.id],
                |row| row.get(0),
            )
            .unwrap();
        let target_pending_attachments: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_pending_attachments WHERE session_id = ?1",
                rusqlite::params![instance.id],
                |row| row.get(0),
            )
            .unwrap();
        let target_budgets: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_rate_limit_budgets WHERE session_id = ?1",
                rusqlite::params![instance.id],
                |row| row.get(0),
            )
            .unwrap();
        let retained_pending_attachments: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_pending_attachments WHERE session_id = 'retained'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let retained_budgets: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_rate_limit_budgets WHERE session_id = 'retained'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target_events, 0);
        assert_eq!(target_attachments, 0);
        assert_eq!(target_pending_attachments, 0);
        assert_eq!(target_budgets, 0);
        assert_eq!(retained_events, 1);
        assert_eq!(retained_pending_attachments, 1);
        assert_eq!(retained_budgets, 1);
        assert!(
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                .entries
                .is_empty()
        );
    }

    #[test]
    #[serial]
    fn committed_purge_restores_the_row_when_acp_cleanup_fails() {
        let _guard = isolate_app_dir();
        let profile = "committed-purge-acp-failure";
        let storage = Storage::new_unwatched(profile).unwrap();
        let instance = stored_instance(&storage, profile, "/tmp/test-project");
        let db_path = crate::session::get_app_dir().unwrap().join("acp_events.db");
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE acp_events (unexpected TEXT);
             CREATE TABLE acp_attachments (unexpected TEXT);",
        )
        .unwrap();
        drop(conn);

        let transaction =
            match PurgeTransaction::reserve_with_acp_transcript(storage.clone(), request(instance))
                .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(result) => panic!("purge was rejected: {result:?}"),
            };
        let committed = transaction
            .run_hooks_with(|_, _| {})
            .begin_irreversible()
            .expect("purge reservation should reach committed teardown");

        let result = committed.finish();

        assert!(!result.success);
        assert_eq!(result.disposition, DeletionDisposition::Failed);
        assert!(result
            .errors
            .join("; ")
            .contains("ACP transcript cleanup failed"));
        let rows = storage.load().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::Idle);
        assert!(rows[0].lifecycle_reservation.is_none());
        let journals =
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()]);
        assert!(journals.entries.is_empty());

        recover_lifecycle_journals_once().unwrap();
        let rows = storage.load().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, Status::Idle);
        assert!(rows[0].lifecycle_reservation.is_none());
    }

    #[test]
    #[serial]
    fn recovery_preserves_the_row_owned_generation_when_a_newer_journal_is_orphaned() {
        let _guard = isolate_app_dir();
        let profile = "purge-recovery-generation-crash";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = Instance::new("generation-crash", "/tmp/generation-crash");
        instance.source_profile = profile.to_string();
        let (snapshot, deleting_row, generation) = expired_deleting_row(instance);
        storage
            .update(|instances, _groups| {
                instances.push(deleting_row);
                Ok(())
            })
            .unwrap();

        let entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            snapshot,
            Status::Idle,
            storage.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        );
        let owned_path = crate::session::lifecycle_journal::record(&entry).unwrap();
        let orphan_path =
            crate::session::lifecycle_journal::record(&entry.with_generation(generation + 1))
                .unwrap();
        assert_ne!(owned_path, orphan_path);

        recover_lifecycle_journals_once().unwrap();

        assert!(
            storage.load().unwrap().is_empty(),
            "recovery should follow the generation still owned by the row"
        );
        assert!(
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                .entries
                .is_empty()
        );
    }

    #[test]
    #[serial]
    fn startup_recovery_replays_a_purge_and_drops_superseded_restore_intent() {
        let _guard = isolate_app_dir();
        let profile = "purge-recovery";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = Instance::new("replay", "/tmp/replay-project");
        instance.source_profile = profile.to_string();
        let (snapshot, deleting_row, generation) = expired_deleting_row(instance.clone());
        storage
            .update(|instances, _groups| {
                instances.push(deleting_row.clone());
                Ok(())
            })
            .unwrap();
        let entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            snapshot,
            Status::Idle,
            storage.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        );
        crate::session::lifecycle_journal::record(&entry).unwrap();
        recover_lifecycle_journals_once().unwrap();
        assert!(storage.load().unwrap().is_empty());
        assert!(
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                .entries
                .is_empty()
        );

        let mut abandoned_row = Instance::new("abandoned", "/tmp/abandoned-project");
        abandoned_row.source_profile = profile.to_string();
        let abandoned_entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            abandoned_row,
            Status::Idle,
            storage.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation: 1,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        );
        crate::session::lifecycle_journal::record(&abandoned_entry).unwrap();
        recover_lifecycle_journals_once().unwrap();
        assert!(storage.load().unwrap().is_empty());
        assert!(
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                .entries
                .is_empty()
        );

        let mut restored_row = Instance::new("restored", "/tmp/restored-project");
        restored_row.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(restored_row.clone());
                Ok(())
            })
            .unwrap();
        let mut old_trash_snapshot = restored_row.clone();
        old_trash_snapshot.trash();
        let restore_race_entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            old_trash_snapshot,
            Status::Idle,
            storage.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation: 1,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        );
        crate::session::lifecycle_journal::record(&restore_race_entry).unwrap();
        recover_lifecycle_journals_once().unwrap();
        assert_eq!(storage.load().unwrap().len(), 1);
        assert!(!storage.load().unwrap()[0].is_trashed());
        assert!(
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()])
                .entries
                .is_empty()
        );
    }

    #[test]
    #[serial]
    fn recovery_does_not_replay_a_purge_after_the_session_moves_profiles() {
        let _guard = isolate_app_dir();
        let source_profile = "purge-moved-source";
        let destination_profile = "purge-moved-destination";
        let source = Storage::new_unwatched(source_profile).unwrap();
        let destination = Storage::new_unwatched(destination_profile).unwrap();
        let (_temp, main_repo, worktree_path, mut instance) =
            worktree_fixture("feature/moved-during-purge");
        instance.source_profile = source_profile.to_string();

        let mut moved_instance = instance.clone();
        moved_instance.source_profile = destination_profile.to_string();
        destination
            .update(|instances, _groups| {
                instances.push(moved_instance.clone());
                Ok(())
            })
            .unwrap();

        let entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            instance,
            Status::Idle,
            source.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation: 1,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        )
        .with_phase(LifecyclePhase::TeardownStarted);
        let journal_path = crate::session::lifecycle_journal::record(&entry).unwrap();

        recover_lifecycle_journals_once().unwrap();

        assert!(worktree_path.exists(), "moved session worktree was removed");
        assert!(main_repo.exists());
        assert!(branch_exists(&main_repo, "feature/moved-during-purge"));
        assert!(source.load().unwrap().is_empty());
        assert_eq!(destination.load().unwrap().len(), 1);
        assert!(!journal_path.exists());
    }

    #[test]
    #[serial]
    fn recovery_rechecks_the_journal_after_acquiring_the_lifecycle_lock() {
        let _guard = isolate_app_dir();
        let profile = "purge-stale-scan";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = Instance::new("stale-scan", "/tmp/stale-scan-project");
        instance.source_profile = profile.to_string();
        let (snapshot, deleting_row, generation) = expired_deleting_row(instance);
        storage
            .update(|instances, _groups| {
                instances.push(deleting_row.clone());
                Ok(())
            })
            .unwrap();
        let scanned_entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            snapshot,
            Status::Idle,
            storage.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        )
        .with_phase(LifecyclePhase::TeardownStarted);
        let path = crate::session::lifecycle_journal::record(&scanned_entry).unwrap();
        crate::session::lifecycle_journal::update(
            &path,
            &scanned_entry.with_generation(generation + 1),
        )
        .unwrap();

        assert!(!recover_lifecycle_entry(&path, &scanned_entry, &storage).unwrap());
        let row = storage.load().unwrap().remove(0);
        assert_eq!(row.status, Status::Deleting);
        assert!(row.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
        assert_eq!(
            crate::session::lifecycle_journal::read_deletion(&path)
                .unwrap()
                .unwrap()
                .generation,
            generation + 1
        );
    }

    #[test]
    #[serial]
    fn stale_purge_paths_preserve_successor_statuses() {
        let _guard = isolate_app_dir();
        let profile = "purge-successor-status";
        let storage = Storage::new_unwatched(profile).unwrap();

        let release_instance = stored_instance(&storage, profile, "/tmp/release-project");
        let mut release = reserve(profile, release_instance.clone());
        storage
            .update(|instances, _groups| {
                instances
                    .iter_mut()
                    .find(|row| row.id == release_instance.id)
                    .unwrap()
                    .status = Status::Running;
                Ok(())
            })
            .unwrap();
        let released = release.release_reservation().unwrap().unwrap();
        assert_eq!(released.status, Status::Running);
        assert!(released.lifecycle_reservation.is_none());

        let gate_instance = stored_instance(&storage, profile, "/tmp/gate-project");
        let mut gate = reserve(profile, gate_instance.clone());
        storage
            .update(|instances, _groups| {
                instances
                    .iter_mut()
                    .find(|row| row.id == gate_instance.id)
                    .unwrap()
                    .status = Status::Running;
                Ok(())
            })
            .unwrap();
        let (decision, gated) = gate.gate().unwrap();
        assert_eq!(decision, CompletionGate::Superseded);
        let gated = gated.unwrap();
        assert_eq!(gated.status, Status::Running);
        assert!(gated.lifecycle_reservation.is_none());

        let drop_instance = stored_instance(&storage, profile, "/tmp/drop-project");
        let dropped = reserve(profile, drop_instance.clone());
        storage
            .update(|instances, _groups| {
                instances
                    .iter_mut()
                    .find(|row| row.id == drop_instance.id)
                    .unwrap()
                    .status = Status::Running;
                Ok(())
            })
            .unwrap();
        drop(dropped);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let dropped_row = loop {
            let row = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == drop_instance.id)
                .unwrap();
            if row.lifecycle_reservation.is_none() {
                break row;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "dropped purge reservation was not released"
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(dropped_row.status, Status::Running);

        let begin_instance = stored_instance(&storage, profile, "/tmp/begin-project");
        let begin = reserve(profile, begin_instance.clone()).run_hooks_with(|_, _| {});
        let mut successor_generation = None;
        storage
            .update(|instances, _groups| {
                let row = instances
                    .iter_mut()
                    .find(|row| row.id == begin_instance.id)
                    .unwrap();
                row.lifecycle_reservation
                    .as_mut()
                    .expect("old reservation")
                    .at =
                    Utc::now() - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1);
                let generation = row
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Purge,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .unwrap();
                row.status = Status::Deleting;
                successor_generation = Some(generation);
                Ok(())
            })
            .unwrap();
        let Err(result) = begin.begin_irreversible() else {
            panic!("stale purge crossed the irreversible boundary");
        };
        assert_eq!(result.disposition, DeletionDisposition::Busy);
        let begun = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == begin_instance.id)
            .unwrap();
        assert_eq!(begun.status, Status::Deleting);
        assert!(begun.lifecycle_reservation_is_owned(
            LifecycleOperation::Purge,
            successor_generation.unwrap()
        ));
    }

    #[test]
    #[serial]
    fn startup_recovery_finishes_a_terminal_checkpoint_without_repeating_teardown() {
        let _guard = isolate_app_dir();
        let profile = "purge-terminal-checkpoint";
        let storage = Storage::new_unwatched(profile).unwrap();
        let mut instance = Instance::new("terminal", "/tmp/terminal-project");
        instance.source_profile = profile.to_string();
        instance.view = View::Structured;
        instance.scratch = true;
        let (snapshot, deleting_row, generation) = expired_deleting_row(instance.clone());
        storage
            .update(|instances, _groups| {
                instances.push(deleting_row.clone());
                Ok(())
            })
            .unwrap();

        let entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            snapshot,
            Status::Idle,
            storage.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: false,
                keep_scratch: false,
                purge_acp_transcript: false,
            },
        )
        .with_kept_resources(vec!["scratch workspace".to_string()])
        .with_phase(LifecyclePhase::TeardownComplete);
        crate::session::lifecycle_journal::record(&entry).unwrap();
        crate::process::worker_registry::fence_for_purge(
            &entry.session_id,
            &entry.source_profile,
            entry.generation,
        )
        .unwrap();

        recover_lifecycle_journals_once().unwrap();

        assert!(storage.load().unwrap().is_empty());
        assert!(!crate::process::worker_registry::is_purge_fenced(&entry.session_id).unwrap());
        let journals =
            crate::session::lifecycle_journal::scan([storage.sessions_path().to_path_buf()]);
        assert_eq!(journals.entries.len(), 1);
        let retained = journals.entries[0].1.as_ref().unwrap();
        assert_eq!(retained.phase, LifecyclePhase::Kept);
        assert_eq!(
            retained.kept_resources,
            vec!["scratch workspace".to_string()]
        );
    }

    #[test]
    #[serial]
    #[cfg(unix)]
    fn recovery_stops_a_live_acp_writer_before_removing_its_workspace_and_transcript() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let _guard = isolate_app_dir();
        let profile = "purge-live-acp-recovery";
        let storage = Storage::new_unwatched(profile).unwrap();
        let (_temp, main_repo, worktree_path, mut instance) =
            worktree_fixture("feature/lifecycle-recovery");
        instance.source_profile = profile.to_string();
        instance.view = View::Structured;

        let writer_path = worktree_path.join("live-writer-output");
        let reaped = Arc::new(AtomicBool::new(false));
        let socket_path = crate::process::worker_registry::socket_path_for(&instance.id).unwrap();
        let (writer_pid, waiter) =
            spawn_live_acp_writer(&socket_path, &writer_path, Arc::clone(&reaped));
        let writer_deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !writer_path.exists() {
            assert!(
                std::time::Instant::now() < writer_deadline,
                "ACP writer did not touch the worktree"
            );
            std::thread::sleep(Duration::from_millis(5));
        }

        let (snapshot, deleting_row, generation) = expired_deleting_row(instance.clone());
        storage
            .update(|instances, _groups| {
                instances.push(deleting_row.clone());
                Ok(())
            })
            .unwrap();
        let entry = crate::session::lifecycle_journal::LifecycleJournalEntry::deletion(
            snapshot,
            Status::Idle,
            storage.sessions_path().to_path_buf(),
            crate::session::lifecycle_journal::LifecycleDeletionOptions {
                generation,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: false,
                keep_scratch: false,
                purge_acp_transcript: true,
            },
        )
        .with_phase(LifecyclePhase::TeardownStarted);
        crate::session::lifecycle_journal::record(&entry).unwrap();

        let record = crate::process::worker_registry::WorkerRecord::new(
            instance.id.clone(),
            writer_pid,
            socket_path,
            "codex-acp".to_string(),
            "codex".to_string(),
            worktree_path.clone(),
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some(profile.to_string()),
        )
        .with_generation(1);
        crate::process::worker_registry::save(&record).unwrap();
        assert!(crate::process::worker_registry::is_record_live(&record));

        let db_path = crate::session::get_app_dir().unwrap().join("acp_events.db");
        let _event_store = crate::acp::event_store::EventStore::open(&db_path, 100).unwrap();
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.execute(
            "INSERT INTO acp_events (session_id, seq, event_json, created_at)
             VALUES (?1, 0, '{}', 0), ('retained', 0, '{}', 0)",
            rusqlite::params![instance.id],
        )
        .unwrap();
        drop(conn);

        let exited_before_worktree_cleanup = Arc::clone(&reaped);
        AFTER_PATHS_IN_USE_SCAN.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                assert!(
                    exited_before_worktree_cleanup.load(Ordering::Acquire),
                    "ACP writer must be reaped before managed worktree cleanup starts"
                );
            }));
        });

        recover_lifecycle_journals_once().unwrap();
        let writer_status = waiter.join().expect("ACP writer waiter panicked");
        assert!(!writer_status.success());
        assert!(!worktree_path.exists());
        assert!(storage.load().unwrap().is_empty());
        assert!(!crate::process::worker_registry::record_path(&instance.id)
            .unwrap()
            .exists());
        assert!(!crate::process::worker_registry::is_purge_fenced(&instance.id).unwrap());

        let conn = rusqlite::Connection::open(db_path).unwrap();
        let purged_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = ?1",
                rusqlite::params![instance.id],
                |row| row.get(0),
            )
            .unwrap();
        let retained_events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM acp_events WHERE session_id = 'retained'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(purged_events, 0);
        assert_eq!(retained_events, 1);
        assert!(main_repo.exists());
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

    /// #4107: a purge keeps a shared worktree when another profile cannot be read, and a session
    /// adopting the worktree after the ownership scan cannot see it removed.
    #[test]
    #[serial]
    fn purge_keeps_a_worktree_it_cannot_prove_unused() {
        for adopt_after_scan in [false, true] {
            let (tmp, main_repo, worktree, mut owner) = worktree_fixture("feature/shared");
            let _home = isolate_app_dir_at(&tmp.path().join("home"));
            let storage = Storage::new_unwatched("owner").unwrap();
            owner.source_profile = "owner".to_string();
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
                    ..request(owner)
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("purge reservation was refused"),
            };
            let result = transaction.complete();
            assert_eq!(result.disposition, DeletionDisposition::Removed);

            if let Some(writer) = writer {
                let adopted_while_present = writer.recv().unwrap().join().unwrap();
                assert!(
                    !adopted_while_present || worktree.exists(),
                    "a worktree adopted after the scan was removed"
                );
            } else {
                assert!(result.success, "{:?}", result.errors);
                assert!(
                    result
                        .messages
                        .iter()
                        .any(|m| m.contains("could not be checked")),
                    "{:?}",
                    result.messages
                );
                assert!(
                    worktree.exists(),
                    "worktree removed despite unreadable profile"
                );
                assert!(branch_exists(&main_repo, "feature/shared"));
            }
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
}
