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
    active: bool,
    /// Paths other sessions work in, captured under the storage lock that
    /// admits teardown.
    paths_in_use: Vec<PathBuf>,
}

/// A purge whose durable row has already been removed. The same lifecycle
/// flock remains held while irreversible sidecars are removed.
#[must_use = "committed purge sidecars must be finished"]
pub struct CommittedPurge {
    request: DeletionRequest,
    paths_in_use: Vec<PathBuf>,
    _lifecycle_lock: StorageFlock,
}

#[derive(Clone, Copy)]
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

    pub fn reserve(storage: Storage, mut request: DeletionRequest) -> Result<PurgeReservation> {
        let id = request.session_id.clone();
        let was_trashed = request.instance.is_trashed();
        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&id)
            .context("failed to acquire instance purge lock")?;
        let now = Utc::now();
        let mut reserved = None;
        let mut rejected = None;
        storage.update(|instances, _groups| {
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
        Ok(PurgeReservation::Reserved(Self {
            storage,
            request,
            was_trashed,
            generation,
            lifecycle_lock: Some(lifecycle_lock),
            active: true,
            paths_in_use: Vec::new(),
        }))
    }

    /// Run best-effort hooks without a lifecycle or storage flock held.
    pub fn run_hooks(self) -> Self {
        self.run_hooks_with(run_on_destroy_hooks)
    }

    fn run_hooks_with<F>(mut self, run_hooks: F) -> Self
    where
        F: FnOnce(&Instance, bool),
    {
        self.lifecycle_lock = None;
        run_hooks(&self.request.instance, self.request.detach_hooks);
        self
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
        let mut retained = None;
        self.storage.update(|instances, _groups| {
            if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                stored
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation);
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
        let mut paths_in_use = Vec::new();
        self.storage.update(|instances, _groups| {
            let others = other_sessions_paths(instances, &id);
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
            } else if !owns {
                CompletionGate::Superseded
            } else {
                paths_in_use = others;
                CompletionGate::Proceed
            };
            if !matches!(gate, CompletionGate::Proceed) {
                stored
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation);
            }
            outcome = Some((gate, Some(stored.clone())));
            Ok(())
        })?;
        let outcome = outcome.ok_or_else(|| anyhow::anyhow!("purge gate produced no outcome"))?;
        if matches!(outcome.0, CompletionGate::Proceed) {
            paths_in_use.extend(other_profiles_paths(self.storage.profile(), &id));
            self.paths_in_use = paths_in_use;
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
        let mut commit = None;
        let mut paths_in_use = Vec::new();
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
                instances[index]
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation);
                commit = Some((CompletionGate::KeptRestored, Some(instances[index].clone())));
            } else if !owns {
                commit = Some((CompletionGate::Superseded, Some(instances[index].clone())));
            } else {
                instances.remove(index);
                paths_in_use = other_sessions_paths(instances, &id);
                commit = Some((CompletionGate::Proceed, None));
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
        self.active = false;
        if !matches!(gate, CompletionGate::Proceed) {
            return Err(Box::new(self.result_for_gate(gate, retained)));
        }
        paths_in_use.extend(other_profiles_paths(self.storage.profile(), &id));
        Ok(CommittedPurge {
            paths_in_use,
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
        })
    }

    /// Reacquire and verify the token, then keep the lifecycle flock through
    /// teardown and the durable commit.
    fn complete_inner(
        mut self,
        after_teardown: impl FnOnce(&Instance) -> std::result::Result<(), String>,
        commit_on_teardown_failure: bool,
    ) -> DeletionResult {
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
        let mut result =
            perform_deletion_teardown_lifecycle_locked(&self.request, &self.paths_in_use);
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

        let generation = self.generation;
        let was_trashed = self.was_trashed;
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
                instances[index]
                    .release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation);
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
        self.complete_inner(after_teardown, false)
    }

    pub fn complete(self) -> DeletionResult {
        self.complete_inner(|_| Ok(()), false)
    }
}

impl CommittedPurge {
    /// Clean up resources while retaining the lifecycle flock that covered the
    /// irreversible durable-row removal.
    pub fn finish(self) -> DeletionResult {
        let mut result =
            perform_deletion_teardown_lifecycle_locked(&self.request, &self.paths_in_use);
        result.disposition = DeletionDisposition::Removed;
        result
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
        let _ = std::thread::Builder::new()
            .name("aoe-purge-reservation-release".to_string())
            .spawn(move || {
                let Ok(storage) = Storage::open_unwatched(&profile) else {
                    return;
                };
                let Ok(_lifecycle_lock) = storage.acquire_instance_lifecycle_lock(&id) else {
                    return;
                };
                let _ = storage.update(|instances, _groups| {
                    if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
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

/// Every path a session other than `except_id` works in or will restore to.
fn other_sessions_paths(instances: &[Instance], except_id: &str) -> Vec<PathBuf> {
    instances
        .iter()
        .filter(|instance| instance.id != except_id)
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

/// [`other_sessions_paths`] across every profile but `profile`, read without
/// their storage locks. An unreadable profile is skipped with a warning.
fn other_profiles_paths(profile: &str, except_id: &str) -> Vec<PathBuf> {
    let profiles = match crate::session::list_profiles() {
        Ok(profiles) => profiles,
        Err(error) => {
            tracing::warn!(target: "session.delete", "listing profiles for shared worktrees failed: {error}");
            return Vec::new();
        }
    };
    profiles
        .iter()
        .filter(|other| other.as_str() != profile)
        .flat_map(
            |other| match Storage::open_unwatched(other).and_then(|storage| storage.load()) {
                Ok(instances) => other_sessions_paths(&instances, except_id),
                Err(error) => {
                    tracing::warn!(target: "session.delete", profile = %other,
                        "reading sessions for shared worktrees failed: {error}");
                    Vec::new()
                }
            },
        )
        .collect()
}

#[cfg(test)]
pub fn perform_deletion(request: &DeletionRequest) -> DeletionResult {
    run_on_destroy_hooks(&request.instance, request.detach_hooks);
    perform_deletion_with(request, |session_id| {
        DockerContainer::from_session_id(session_id).teardown(session_id)
    })
}

fn perform_deletion_teardown_lifecycle_locked(
    request: &DeletionRequest,
    paths_in_use: &[PathBuf],
) -> DeletionResult {
    perform_deletion_core(request, true, paths_in_use, |session_id| {
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
    perform_deletion_core(request, false, &[], teardown)
}

fn perform_deletion_core(
    request: &DeletionRequest,
    lifecycle_locked: bool,
    paths_in_use: &[PathBuf],
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

    let preserved_worktree_paths =
        stage_collect_preserved_worktrees(request, repos, paths_in_use, &mut errors, &mut messages);
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
        deletion_messages_for(outcome, &mut messages, &mut errors);
    }

    stage_remove_worktrees_and_branches(
        request,
        repos,
        &preserved_worktree_paths,
        any_preserved,
        &mut errors,
        &mut messages,
    );

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

fn stage_collect_preserved_worktrees(
    request: &DeletionRequest,
    repos: &[super::WorkspaceRepo],
    paths_in_use: &[PathBuf],
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

    // A worktree another session still works in is kept, and so its branch, whichever sessions
    // the caller named. Checked before the dirty gate so a kept worktree cannot fail the deletion.
    if request.delete_worktree {
        let in_use = |root: &Path| paths_in_use.iter().any(|path| path.starts_with(root));
        let still_used = "another session still uses it";
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
        assert!(!retained[0].has_fresh_lifecycle_reservation(Utc::now()));

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

        #[test]
        fn teardown_outcome_messages() {
            let failed = Teardown::Failed(DockerError::RemoveFailed("daemon busy".into()));
            for (teardown, want_messages, want_errors) in [
                (failed, 0, 1),
                (Teardown::Removed, 1, 0),
                (Teardown::AlreadyGone, 0, 0),
            ] {
                let (mut messages, mut errors) = (Vec::new(), Vec::new());
                deletion_messages_for(teardown, &mut messages, &mut errors);
                assert_eq!((messages.len(), errors.len()), (want_messages, want_errors));
                assert!(messages.iter().all(|m| m == "Container removed"));
                assert!(errors.iter().all(|e| e.contains("Container")));
            }
        }

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
        fn unsandboxed_kills_tmux_before_worktree() {
            let _app_guard = isolate_app_dir();
            let instance = Instance::new("Test", "/tmp/aoe-deletion-test-nonexistent");
            let (stages, _) = stages_of(&DeletionRequest {
                delete_worktree: true,
                ..request(instance)
            });
            assert!(idx(&stages, "tmux_kill") < idx(&stages, "worktree_remove"));
            assert!(!stages.iter().any(|s| s == "sandbox_worktree_preclean"));
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
        fn scratch_session_removes_dir_and_tolerates_missing_dir() {
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
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

        #[test]
        #[serial]
        fn tampered_project_path_does_not_get_removed() {
            let _tmp = isolate_app_dir();
            let bystander =
                std::env::temp_dir().join(format!("important-data-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&bystander).expect("create bystander");
            fs::write(bystander.join("file.txt"), b"keep me").unwrap();

            let mut instance = Instance::new("Tampered", bystander.to_str().unwrap());
            instance.scratch = true;
            let result = perform_deletion(&request(instance));

            let survived = bystander.join("file.txt").exists();
            let _ = fs::remove_dir_all(&bystander);
            assert!(
                survived,
                "guard must refuse a path outside the scratch root"
            );
            assert!(
                result.errors.iter().any(|e| e.contains("scratch guard")),
                "guard refusal must be reported, got: {:?}",
                result.errors
            );
        }

        #[test]
        #[serial]
        fn keep_scratch_leaves_dir_on_disk_and_reports_path() {
            let _tmp = isolate_app_dir();
            let (instance, dir) = scratch_instance();
            let result = perform_deletion(&DeletionRequest {
                keep_scratch: true,
                ..request(instance)
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
            let _ = fs::remove_dir_all(&dir);
        }

        #[test]
        #[serial]
        fn non_scratch_session_under_app_dir_is_untouched() {
            let _tmp = isolate_app_dir();
            let dir = crate::session::get_app_dir()
                .unwrap()
                .join(format!("non-scratch-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&dir).expect("create non-scratch test dir");
            let _ = perform_deletion(&request(Instance::new("Regular", dir.to_str().unwrap())));
            assert!(dir.exists());
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
