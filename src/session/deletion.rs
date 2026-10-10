//! Shared session deletion logic used by CLI, TUI, and web server.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;

use crate::containers::DockerContainer;
use crate::git::cleanup::remove_managed_worktree;
use crate::git::GitWorktree;
use crate::session::config::repo_config;
use crate::session::storage::{acquire_ownership_lock, OwnershipGuard, StorageFlock};
use crate::session::{Instance, LifecycleOperation, Storage};

#[derive(Clone)]
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
    ownership: Option<OwnershipGuard>,
    active: bool,
}

/// A rowless purge retaining shared ownership until runtime shutdown completes.
#[must_use = "committed purge sidecars must be finished"]
pub struct CommittedPurge {
    request: DeletionRequest,
    storage: Storage,
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
        let expected_trashed_at = request.instance.trashed_at;
        let mut lifecycle_changed = false;
        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&id)
            .context("failed to acquire instance purge lock")?;
        storage.load_strict_for_worktree_ownership()?;
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
        Ok(PurgeReservation::Reserved(Self {
            storage,
            request,
            was_trashed,
            generation,
            lifecycle_lock: Some(lifecycle_lock),
            ownership: None,
            active: true,
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
        self.ownership = None;
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

    fn acquire_exclusive(&mut self) -> Result<()> {
        self.lifecycle_lock = None;
        self.ownership = None;
        let ownership = acquire_ownership_lock()?;
        let lifecycle = self
            .storage
            .acquire_instance_lifecycle_lock_with_ownership(&ownership, &self.request.session_id)?;
        self.storage.load_strict_for_worktree_ownership()?;
        self.ownership = Some(ownership);
        self.lifecycle_lock = Some(lifecycle);
        Ok(())
    }

    fn release_reservation(&mut self) -> Result<Option<Instance>> {
        let id = self.request.session_id.clone();
        let generation = self.generation;
        self.storage.load_strict_for_worktree_ownership()?;
        let mut retained = None;
        self.storage.update_with_ownership(
            self.ownership.as_ref().expect("exclusive purge"),
            |instances, _groups| {
                if let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) {
                    stored.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Purge,
                        generation,
                    );
                    retained = Some(stored.clone());
                }
                Ok(())
            },
        )?;
        self.active = false;
        Ok(retained)
    }

    fn gate(&mut self) -> Result<(CompletionGate, Option<Instance>)> {
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let was_trashed = self.was_trashed;
        let mut outcome = None;
        self.storage.update_with_ownership(
            self.ownership.as_ref().expect("exclusive purge"),
            |instances, _groups| {
                let Some(stored) = instances.iter_mut().find(|instance| instance.id == id) else {
                    outcome = Some((CompletionGate::AlreadyGone, None));
                    return Ok(());
                };
                let restored = crate::session::claim::purge_restored_row_must_be_kept(
                    was_trashed,
                    stored.is_trashed(),
                );
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
            },
        )?;
        let outcome = outcome.ok_or_else(|| anyhow::anyhow!("purge gate produced no outcome"))?;
        if !matches!(outcome.0, CompletionGate::Proceed) {
            self.active = false;
        }
        if matches!(outcome.0, CompletionGate::Proceed) {
            if let Some(mut current) = outcome.1.clone() {
                current.source_profile = self.storage.profile().to_string();
                self.request.instance = current;
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

    /// Atomically validate this reservation and remove its durable row before any irreversible
    /// external teardown.
    pub fn begin_irreversible(
        mut self,
    ) -> std::result::Result<CommittedPurge, Box<DeletionResult>> {
        if let Err(error) = self.ensure_lifecycle_lock().and_then(|()| {
            self.storage
                .load_strict_for_worktree_ownership()
                .map(|_| ())
        }) {
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
                let mut snapshot = instances.remove(index);
                snapshot.source_profile = self.storage.profile().to_string();
                self.request.instance = snapshot;
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
        })
    }

    /// Reacquire and verify the token, then keep the lifecycle flock through
    /// teardown and the durable commit.
    fn complete_inner(
        mut self,
        after_teardown: impl FnOnce(&Instance) -> std::result::Result<(), String>,
        commit_on_teardown_failure: bool,
    ) -> DeletionResult {
        if let Err(error) = self.acquire_exclusive() {
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
        let mut result = perform_deletion_teardown_lifecycle_locked(
            &self.request,
            self.ownership.as_ref().expect("exclusive purge"),
        );
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
        let commit_result = self.storage.update_with_ownership(
            self.ownership.as_ref().expect("exclusive purge"),
            |instances, _groups| {
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
            },
        );
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
    /// Revalidate profile and path ownership after runtime shutdown, before Git cleanup.
    pub fn finish(self) -> DeletionResult {
        let Self {
            request,
            storage,
            _lifecycle_lock,
        } = self;
        drop(_lifecycle_lock);
        let guarded = (|| -> Result<(OwnershipGuard, StorageFlock)> {
            let ownership = acquire_ownership_lock()?;
            let lifecycle = storage
                .acquire_instance_lifecycle_lock_with_ownership(&ownership, &request.session_id)?;
            let instances = storage.load_strict_for_worktree_ownership()?;
            anyhow::ensure!(
                !instances.iter().any(|row| row.id == request.session_id),
                "purged session identity was reused"
            );
            Ok((ownership, lifecycle))
        })();
        let (ownership, _lifecycle) = match guarded {
            Ok(guards) => guards,
            Err(error) => {
                return DeletionResult::rejected(
                    request.session_id,
                    DeletionDisposition::Failed,
                    format!("Failed to resume committed purge: {error}"),
                    None,
                )
            }
        };
        let mut result = perform_deletion_teardown_lifecycle_locked(&request, &ownership);
        result.disposition = DeletionDisposition::Removed;
        result
    }
}

impl Drop for PurgeTransaction {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let storage = self.storage.clone();
        let id = self.request.session_id.clone();
        let generation = self.generation;
        let _ = std::thread::Builder::new()
            .name("aoe-purge-reservation-release".to_string())
            .spawn(move || {
                let Ok(_lifecycle_lock) = storage.acquire_instance_lifecycle_lock(&id) else {
                    return;
                };
                if storage.load_strict_for_worktree_ownership().is_err() {
                    return;
                }
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

/// Resolve absent tails only through components proven missing, never broken aliases.
pub(crate) fn resolve_claim_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            std::path::Component::RootDir => resolved.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::Normal(name) => {
                resolved.push(name);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => resolved = resolved.canonicalize().ok()?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
        }
    }
    Some(resolved)
}

pub(crate) fn paths_overlap_destructive(left: &Path, right: &Path) -> bool {
    match (resolve_claim_path(left), resolve_claim_path(right)) {
        (Some(left), Some(right)) => left.starts_with(&right) || right.starts_with(&left),
        _ => true,
    }
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
            Self::Known(paths) => {
                resolve_claim_path(root).is_none()
                    || paths
                        .iter()
                        .any(|path| paths_overlap_destructive(path, root))
            }
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

fn all_profile_storages_with_ownership(
    ownership: &OwnershipGuard,
) -> std::result::Result<(Vec<String>, Vec<Storage>), String> {
    let profiles = crate::session::list_profiles_for_worktree_inventory()
        .map_err(|error| format!("listing profiles: {error}"))?;
    let mut storages = Vec::new();
    let mut identities = std::collections::HashSet::new();
    for profile in &profiles {
        let storage = Storage::open_unwatched_with_ownership(profile, ownership)
            .map_err(|error| format!("opening profile {profile}: {error}"))?;
        let identity = storage
            .physical_profile_identity()
            .map_err(|error| format!("resolving profile {profile}: {error}"))?;
        if identities.insert(identity) {
            storages.push(storage);
        }
    }
    Ok((profiles, storages))
}

fn scan_paths_in_use_qualified(
    storages: &[Storage],
    profile: &str,
    except_ids: &[&str],
) -> PathsInUse {
    let owner = if except_ids.is_empty() {
        None
    } else {
        // Resolve the caller's physical directory, not its alias spelling.
        let identity = match crate::session::get_profile_dir_path(profile)
            .and_then(|path| std::fs::metadata(path).map_err(Into::into))
        {
            Ok(metadata) if !profile.is_empty() && metadata.is_dir() => {
                crate::session::storage::filesystem_identity(&metadata)
            }
            _ => return PathsInUse::Unknown("owner profile cannot be resolved".into()),
        };
        Some(identity)
    };
    let mut owner_seen = owner.is_none();
    let mut paths = Vec::new();
    for storage in storages {
        let identity = match storage.physical_profile_identity() {
            Ok(identity) => identity,
            Err(error) => return PathsInUse::Unknown(format!("resolving profile: {error}")),
        };
        let excluded = owner == Some(identity);
        owner_seen |= excluded;
        let instances = match storage.load_strict_for_worktree_ownership() {
            Ok(instances) => instances,
            Err(error) => {
                return PathsInUse::Unknown(format!(
                    "reading profile {}: {error}",
                    storage.profile()
                ))
            }
        };
        for path in other_sessions_paths(&instances, if excluded { except_ids } else { &[] }) {
            let Some(path) = resolve_claim_path(&path) else {
                return PathsInUse::Unknown("a session ownership path cannot be resolved".into());
            };
            paths.push(path);
        }
    }
    if !owner_seen {
        return PathsInUse::Unknown("owner profile is missing from ownership inventory".into());
    }
    PathsInUse::Known(paths)
}

pub(crate) fn paths_in_use_except_with_ownership(
    ownership: &OwnershipGuard,
    profile: &str,
    except_ids: &[&str],
) -> PathsInUse {
    let (_, storages) = match all_profile_storages_with_ownership(ownership) {
        Ok(found) => found,
        Err(reason) => return PathsInUse::Unknown(reason),
    };
    crate::session::storage::with_storages_locked_with_ownership(ownership, &storages, || {
        scan_paths_in_use_qualified(&storages, profile, except_ids)
    })
    .unwrap_or_else(|error| PathsInUse::Unknown(format!("locking session stores: {error}")))
}

pub(crate) fn branch_in_use_with_ownership(
    ownership: &OwnershipGuard,
    profile: &str,
    except_ids: &[&str],
    main_repo: &Path,
    branch: &str,
) -> Result<bool> {
    let (_, storages) =
        all_profile_storages_with_ownership(ownership).map_err(anyhow::Error::msg)?;
    let main_repo = resolve_claim_path(main_repo)
        .ok_or_else(|| anyhow::anyhow!("repository ownership path cannot be resolved"))?;
    let owner_identity = crate::session::storage::filesystem_identity(&std::fs::metadata(
        crate::session::get_profile_dir_path(profile)?,
    )?);
    crate::session::storage::with_storages_locked_with_ownership(
        ownership,
        &storages,
        || -> Result<bool> {
            for storage in &storages {
                for row in storage.load_strict_for_worktree_ownership()? {
                    if storage.physical_profile_identity()? == owner_identity
                        && except_ids.contains(&row.id.as_str())
                    {
                        continue;
                    }
                    if let Some(wt) = &row.worktree_info {
                        if wt.branch == branch
                            && resolve_claim_path(Path::new(&wt.main_repo_path)).ok_or_else(
                                || anyhow::anyhow!("repository ownership path cannot be resolved"),
                            )? == main_repo
                        {
                            return Ok(true);
                        }
                    }
                    for repo in row.all_repos() {
                        if repo.branch == branch
                            && resolve_claim_path(Path::new(&repo.main_repo_path)).ok_or_else(
                                || anyhow::anyhow!("repository ownership path cannot be resolved"),
                            )? == main_repo
                        {
                            return Ok(true);
                        }
                    }
                }
            }
            Ok(false)
        },
    )?
}

#[cfg(test)]
thread_local! {
    static AFTER_PATHS_IN_USE_SCAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run `f` with the paths other sessions use while every profile's storage lock is held, so no
/// session can adopt a path between the check and whatever `f` removes.
fn with_paths_in_use_locked<R>(
    ownership: &OwnershipGuard,
    profile: &str,
    except_id: &str,
    f: impl FnOnce(&PathsInUse) -> R,
) -> R {
    let (profiles, storages) = match all_profile_storages_with_ownership(ownership) {
        Ok(found) => found,
        Err(reason) => return f(&PathsInUse::Unknown(reason)),
    };
    let mut f = Some(f);
    let locked =
        crate::session::storage::with_storages_locked_with_ownership(ownership, &storages, || {
            let paths_in_use = match crate::session::list_profiles_for_worktree_inventory() {
                Ok(now) if now.iter().all(|profile| profiles.contains(profile)) => {
                    scan_paths_in_use_qualified(&storages, profile, &[except_id])
                }
                Ok(_) => {
                    PathsInUse::Unknown("a profile was created during the deletion".to_string())
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
    run_on_destroy_hooks(&request.instance, request.detach_hooks);
    perform_deletion_with(request, |session_id| {
        DockerContainer::from_session_id(session_id).teardown(session_id)
    })
}

fn perform_deletion_teardown_lifecycle_locked(
    request: &DeletionRequest,
    ownership: &OwnershipGuard,
) -> DeletionResult {
    let mut guarded_request = None;
    if request.delete_branch {
        let used = request
            .instance
            .worktree_info
            .as_ref()
            .map(|wt| (wt.main_repo_path.as_str(), wt.branch.as_str()))
            .into_iter()
            .chain(
                request
                    .instance
                    .all_repos()
                    .iter()
                    .map(|repo| (repo.main_repo_path.as_str(), repo.branch.as_str())),
            )
            .any(|(repo, branch)| {
                branch_in_use_with_ownership(
                    ownership,
                    &request.instance.source_profile,
                    &[&request.session_id],
                    Path::new(repo),
                    branch,
                )
                .unwrap_or(true)
            });
        if used {
            let mut safe = request.clone();
            safe.delete_branch = false;
            guarded_request = Some(safe);
        }
    }
    let request = guarded_request.as_ref().unwrap_or(request);
    perform_deletion_core(request, Some(ownership), |session_id| {
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
    perform_deletion_core(request, None, teardown)
}

/// `lifecycle_locked` is the production path, which also keeps any worktree another session uses.
fn perform_deletion_core(
    request: &DeletionRequest,
    ownership: Option<&OwnershipGuard>,
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
    if ownership.is_some() {
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
    let container_gone = if let Some(ownership) = ownership.filter(|_| removes_managed_worktree) {
        with_paths_in_use_locked(
            ownership,
            &request.instance.source_profile,
            &request.session_id,
            stage,
        )
    } else {
        stage(&PathsInUse::Known(Vec::new()))
    };

    if let Some(ownership) = ownership.filter(|_| request.instance.scratch && !request.keep_scratch)
    {
        let paths = paths_in_use_except_with_ownership(
            ownership,
            &request.instance.source_profile,
            &[&request.session_id],
        );
        if paths.covers(Path::new(&request.instance.project_path)) {
            messages.push(format!("Scratch directory kept: {}", paths.reason()));
        } else {
            stage_cleanup_scratch(request, &mut errors, &mut messages);
        }
    } else {
        stage_cleanup_scratch(request, &mut errors, &mut messages);
    }

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
        for repo in repos.iter().filter(|repo| repo.managed_by_aoe) {
            let path = PathBuf::from(&repo.worktree_path);
            if in_use(&path) && preserved_worktree_paths.insert(path) {
                messages.push(format!(
                    "Workspace ({}) worktree kept; {still_used}",
                    repo.name
                ));
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

    #[test]
    #[cfg(unix)]
    #[serial]
    fn strict_ownership_reader_never_treats_unreadable_or_broken_store_as_absent() {
        let _home = isolate_app_dir();
        let storage = Storage::new_unwatched("strict-io").unwrap();
        assert!(storage
            .load_strict_for_worktree_ownership()
            .unwrap()
            .is_empty());
        std::fs::create_dir(storage.sessions_path()).unwrap();
        assert!(storage.load_strict_for_worktree_ownership().is_err());
        std::fs::remove_dir(storage.sessions_path()).unwrap();
        std::os::unix::fs::symlink("missing.json", storage.sessions_path()).unwrap();
        assert!(storage.load_strict_for_worktree_ownership().is_err());
        let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
        assert!(matches!(
            paths_in_use_except_with_ownership(&ownership, "strict-io", &[]),
            PathsInUse::Unknown(_)
        ));
    }

    #[test]
    #[serial]
    fn ownership_inventory_rejects_invalid_and_ambiguous_rows_without_quarantine() {
        let _home = isolate_app_dir();
        let storage = Storage::new_unwatched("strict").unwrap();
        let valid = Instance::new("owner", "/claimed");
        for rows in [
            serde_json::json!([valid, {"id": "broken", "project_path": "/precious"}]),
            serde_json::json!([valid, valid]),
        ] {
            std::fs::write(storage.sessions_path(), serde_json::to_vec(&rows).unwrap()).unwrap();
            let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
            assert!(storage.load_strict_for_worktree_ownership().is_err());
            assert!(matches!(
                paths_in_use_except_with_ownership(&ownership, "strict", &[&valid.id]),
                PathsInUse::Unknown(_)
            ));
            assert!(!storage
                .sessions_path()
                .with_file_name("sessions.corrupt.jsonl")
                .exists());
        }
        // The forgiving UI reader remains deliberately available.
        std::fs::write(
            storage.sessions_path(),
            serde_json::to_vec(&serde_json::json!([valid, {"id": "broken"}])).unwrap(),
        )
        .unwrap();
        assert_eq!(storage.load().unwrap().len(), 1);
    }

    #[test]
    #[cfg(unix)]
    #[serial]
    fn external_profile_aliases_exclude_only_the_same_physical_owner() {
        let _home = isolate_app_dir();
        let external = tempfile::tempdir().unwrap();
        let profiles = crate::session::get_app_dir().unwrap().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        for alias in ["external-a", "external-b"] {
            std::os::unix::fs::symlink(external.path(), profiles.join(alias)).unwrap();
        }
        let mut owner = Instance::new("external owner", "/external-owned");
        owner.id = "same-id".into();
        std::fs::write(
            external.path().join("sessions.json"),
            serde_json::to_vec(&vec![owner.clone()]).unwrap(),
        )
        .unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let mut peer_row = owner.clone();
        peer_row.project_path = "/peer-owned".into();
        peer.update(|rows, _| {
            rows.push(peer_row);
            Ok(())
        })
        .unwrap();
        assert_eq!(crate::session::list_profiles().unwrap(), vec!["peer"]);
        let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
        for alias in ["external-a", "external-b"] {
            let paths = paths_in_use_except_with_ownership(&ownership, alias, &["same-id"]);
            assert!(!paths.covers(Path::new("/external-owned")));
            assert!(paths.covers(Path::new("/peer-owned")));
        }
        assert_eq!(
            all_profile_storages_with_ownership(&ownership)
                .unwrap()
                .1
                .len(),
            2
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial]
    fn raw_alias_ancestor_missing_tail_and_broken_alias_claims_are_protected() {
        let _home = isolate_app_dir();
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        peer.update(|rows, _| {
            rows.push(Instance::new("peer", alias.to_str().unwrap()));
            Ok(())
        })
        .unwrap();
        let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
        let paths = paths_in_use_except_with_ownership(&ownership, "peer", &[]);
        assert!(paths.covers(&real));
        assert!(paths.covers(&real.join("missing/child")));
        assert!(paths.covers(temp.path()));
        assert!(!paths.covers(&temp.path().join("unrelated")));
        assert_eq!(
            resolve_claim_path(&alias.join("missing/../child")),
            Some(real.canonicalize().unwrap().join("child"))
        );
        std::os::unix::fs::symlink(temp.path().join("gone"), temp.path().join("broken")).unwrap();
        assert!(paths.covers(&temp.path().join("broken/tail")));
        let profiles = crate::session::get_app_dir().unwrap().join("profiles");
        std::os::unix::fs::symlink(temp.path().join("gone"), profiles.join("broken-profile"))
            .unwrap();
        assert!(matches!(
            paths_in_use_except_with_ownership(&ownership, "peer", &[]),
            PathsInUse::Unknown(_)
        ));
    }

    #[test]
    fn late_profile_process_helper() {
        use std::io::Write;
        let Some(home) = std::env::var_os("AOE_LATE_PROFILE_HOME") else {
            return;
        };
        let _home = isolate_app_dir_at(Path::new(&home));
        let app = crate::session::get_app_dir().unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(app.join(".workspace-claim.lock"))
            .unwrap();
        assert_eq!(
            fs2::FileExt::try_lock_shared(&file).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        println!("OWNERSHIP_BLOCKED");
        std::io::stdout().flush().unwrap();
        let storage = Storage::new_unwatched("late-b").unwrap();
        let target = PathBuf::from(std::env::var_os("AOE_LATE_PROFILE_TARGET_DIR").unwrap());
        assert!(
            !target.exists(),
            "profile creation crossed the destructive ownership interval"
        );
        storage
            .update(|rows, _| {
                rows.push(Instance::new("B", "/unrelated"));
                Ok(())
            })
            .unwrap();
    }

    #[test]
    #[serial]
    fn late_profile_after_scan_waits_for_real_git_removal() {
        use std::io::{BufRead, BufReader};
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/late-b");
        let home = temp.path().join("home");
        let _home = isolate_app_dir_at(&home);
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".to_string();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let (child_tx, child_rx) = std::sync::mpsc::channel();
        let target = worktree.clone();
        AFTER_PATHS_IN_USE_SCAN.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "session::deletion::tests::late_profile_process_helper",
                        "--nocapture",
                    ])
                    .env("AOE_LATE_PROFILE_HOME", &home)
                    .env("AOE_LATE_PROFILE_TARGET_DIR", &target)
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap();
                let stdout = child.stdout.take().unwrap();
                let (ready_tx, ready_rx) = std::sync::mpsc::channel();
                let reader = std::thread::spawn(move || {
                    for line in BufReader::new(stdout).lines() {
                        if line.unwrap().contains("OWNERSHIP_BLOCKED") {
                            ready_tx.send(()).unwrap();
                        }
                    }
                });
                ready_rx
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .expect("child did not observe ownership exclusion");
                child_tx.send((child, reader)).unwrap();
            }))
        });
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
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let result = transaction.complete();
        assert!(result.success, "{:?}", result.errors);
        assert!(!worktree.exists());
        assert!(!branch_exists(&repo, "feature/late-b"));
        let (mut child, reader) = child_rx.recv().unwrap();
        assert!(child.wait().unwrap().success());
        reader.join().unwrap();
        assert_eq!(
            Storage::open_unwatched("late-b")
                .unwrap()
                .load()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    #[serial]
    fn purge_refreshes_checkout_after_hooks_before_real_git_removal() {
        let (temp, repo, old_path, mut owner) = worktree_fixture("feature/hook-refresh");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let moved = temp.path().join("moved");
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let result = transaction
            .run_hooks_with(|_, _| {
                git_in(
                    &repo,
                    &[
                        "worktree",
                        "move",
                        old_path.to_str().unwrap(),
                        moved.to_str().unwrap(),
                    ],
                );
                std::fs::create_dir(&old_path).unwrap();
                std::fs::write(old_path.join("keep"), "unrelated").unwrap();
                storage
                    .update(|rows, _| {
                        rows[0].project_path = moved.to_string_lossy().into_owned();
                        Ok(())
                    })
                    .unwrap();
            })
            .complete();
        assert!(result.success, "{:?}", result.errors);
        assert!(!moved.exists());
        assert_eq!(
            std::fs::read_to_string(old_path.join("keep")).unwrap(),
            "unrelated"
        );
        assert!(storage.load().unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn committed_purge_allows_sdk_metadata_and_rechecks_new_claim_before_git() {
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/committed");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        let other = Instance::new("SDK peer", "/unrelated");
        let other_id = other.id.clone();
        storage
            .update(|rows, _| {
                rows.extend([owner.clone(), other]);
                Ok(())
            })
            .unwrap();
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let committed = transaction
            .begin_irreversible()
            .unwrap_or_else(|_| panic!("purge commit rejected"));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer_storage = storage.clone();
        let adopted_path = worktree.clone();
        let writer = std::thread::spawn(move || {
            writer_storage
                .update(|rows, _| {
                    let row = rows.iter_mut().find(|row| row.id == other_id).unwrap();
                    row.title = "SDK metadata committed".into();
                    row.project_path = adopted_path.to_string_lossy().into_owned();
                    Ok(())
                })
                .unwrap();
            done_tx.send(()).unwrap();
        });
        let published_before_finish = done_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .is_ok();
        let result = committed.finish();
        writer.join().unwrap();
        assert!(
            published_before_finish,
            "committed purge blocked unrelated SDK metadata"
        );
        assert!(result.success, "{:?}", result.errors);
        assert!(
            worktree.exists(),
            "rowless cleanup missed a claim published during the SDK await"
        );
        assert!(branch_exists(&repo, "feature/committed"));
    }

    #[test]
    #[serial]
    fn purge_admission_preserves_unreadable_owner_rows_before_metadata_mutation() {
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/strict-admission");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        let mut unreadable = serde_json::to_value(&owner).unwrap();
        unreadable["id"] = serde_json::json!("unreadable-peer");
        unreadable["status"] = serde_json::json!("invalid-status");
        let original = serde_json::to_vec(&serde_json::json!([owner.clone(), unreadable])).unwrap();
        std::fs::write(storage.sessions_path(), &original).unwrap();
        let result = PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                session_id: owner.id.clone(),
                instance: owner,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: false,
                keep_scratch: false,
            },
        );
        assert!(
            result.is_err(),
            "purge admission rewrote an incomplete inventory"
        );
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), original);
        assert!(worktree.is_dir());
        assert!(branch_exists(&repo, "feature/strict-admission"));
    }

    #[test]
    #[serial]
    fn purge_exclusion_is_profile_qualified_for_paths_and_branches() {
        for branch_only in [false, true] {
            let (temp, repo, worktree, mut owner) = worktree_fixture("feature/profile-owner");
            let _home = isolate_app_dir_at(&temp.path().join("home"));
            let storage = Storage::new_unwatched("owner").unwrap();
            owner.source_profile = "owner".into();
            storage
                .update(|rows, _| {
                    rows.push(owner.clone());
                    Ok(())
                })
                .unwrap();
            let other = Storage::new_unwatched("other").unwrap();
            let mut alias = owner.clone();
            alias.source_profile = "other".into();
            if branch_only {
                alias.project_path = repo.to_string_lossy().into_owned();
            }
            other
                .update(|rows, _| {
                    rows.push(alias);
                    Ok(())
                })
                .unwrap();
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
                PurgeReservation::Rejected(_) => panic!("purge rejected"),
            };
            let result = transaction.complete();
            assert!(result.success, "{:?}", result.errors);
            assert_eq!(worktree.exists(), !branch_only);
            assert!(branch_exists(&repo, "feature/profile-owner"));
            assert_eq!(other.load().unwrap().len(), 1);
        }
    }

    #[test]
    #[serial]
    fn purge_rejects_replaced_original_profile_after_hooks() {
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/profile-refresh");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let result = transaction
            .run_hooks_with(|_, _| {
                let reserved = storage.load().unwrap();
                crate::session::rename_profile("owner", "original-owner").unwrap();
                let replacement = Storage::new_unwatched("owner").unwrap();
                replacement
                    .update(|rows, _| {
                        rows.extend(reserved);
                        Ok(())
                    })
                    .unwrap();
            })
            .complete();
        assert_eq!(result.disposition, DeletionDisposition::Failed);
        assert!(!result.teardown_started);
        assert!(worktree.exists());
        assert!(branch_exists(&repo, "feature/profile-refresh"));
    }
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
