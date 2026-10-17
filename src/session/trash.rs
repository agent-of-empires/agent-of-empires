//! Trash retention helpers.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::claim::ResourceReservation;
use super::move_journal::{
    EffectObservation, IntentOwner, ProfileAuthority, ResourceEffect, ResourceIntent,
};
use super::LifecycleOperation;

use chrono::{DateTime, Utc};

use crate::git::GitWorktree;
#[cfg(test)]
use crate::session::storage::acquire_ownership_lock;
use crate::session::storage::{acquire_session_identity_lock_with_ownership, OwnershipGuard};
use crate::session::worktree_edit::ensure_sandbox_container_released;
use crate::session::Instance;

/// Hidden, product-owned holding directory for trashed worktrees.
const TRASH_DIR_NAME: &str = ".aoe-trash";

/// Where a trashed session's worktree is parked. `None` when `original` has no
/// parent (a filesystem root), in which case relocation is skipped.
pub fn trash_holding_path(original: &Path, session_id: &str) -> Option<PathBuf> {
    Some(original.parent()?.join(TRASH_DIR_NAME).join(session_id))
}

/// True when `path` is already a holding path for this session, i.e. its leaf is the session id
/// sitting directly under a `.aoe-trash` dir.
fn is_holding_path(path: &Path, session_id: &str) -> bool {
    path.file_name()
        .is_some_and(|leaf| leaf == std::ffi::OsStr::new(session_id))
        && path
            .parent()
            .and_then(|p| p.file_name())
            .is_some_and(|name| name == std::ffi::OsStr::new(TRASH_DIR_NAME))
}

/// Result of attempting to relocate a trashed session's worktree.
#[derive(Debug)]
pub enum RelocateOutcome {
    /// The worktree was moved into the holding area and `project_path` was
    /// repointed; `pre_trash_project_path` now holds the original location.
    Relocated { from: PathBuf, to: PathBuf },
    /// Nothing to do: not a managed single-repo worktree, or already
    /// relocated. `project_path` is untouched.
    Skipped,
    /// The move could not run safely (sandbox container still mounting the dir, locked,
    /// cross-device, git error).
    Failed { reason: String },
}

fn is_managed_single_worktree(inst: &Instance) -> bool {
    !inst.scratch
        && inst
            .worktree_info
            .as_ref()
            .is_some_and(|w| w.managed_by_aoe)
}

/// Whether the session's branch is one git states is the repo's default, so its checkout must be
/// left where it is.
fn is_protected_default_branch(inst: &Instance) -> bool {
    is_protected_default_branch_cached(inst, &mut ProtectedBranchCache::default())
}

/// One sweep's worth of `protected_default_branch_names` results, keyed by main repo path.
#[derive(Default)]
struct ProtectedBranchCache(std::collections::HashMap<String, std::collections::HashSet<String>>);

fn is_protected_default_branch_cached(inst: &Instance, cache: &mut ProtectedBranchCache) -> bool {
    let Some(wt) = inst.worktree_info.as_ref() else {
        return false;
    };
    if let Some(names) = cache.0.get(&wt.main_repo_path) {
        return names.contains(&wt.branch);
    }
    let Ok(names) = GitWorktree::new(PathBuf::from(&wt.main_repo_path))
        .and_then(|git| git.protected_default_branch_names())
    else {
        return false;
    };
    let hit = names.contains(&wt.branch);
    cache.0.insert(wt.main_repo_path.clone(), names);
    hit
}

/// Whether a managed worktree's directory has outlived its registration, so `git worktree move` can
/// only ever answer "not a working tree".
fn is_stranded_checkout(worktree: &Path) -> bool {
    let link = worktree.join(".git");
    let metadata = match std::fs::symlink_metadata(&link) {
        Ok(metadata) => metadata,
        Err(error) => return error.kind() == std::io::ErrorKind::NotFound,
    };
    if metadata.is_dir() {
        // A repo of its own, not a linked worktree; nothing to strand.
        return false;
    }
    // `Path::exists` reports false for every error, so a permission or I/O blip on the admin dir
    // would read a live checkout as stranded.
    match crate::git::cleanup::read_linked_worktree_gitdir(worktree) {
        Some(admin) => matches!(admin.try_exists(), Ok(false)),
        None => false,
    }
}

fn is_sandboxed(inst: &Instance) -> bool {
    inst.sandbox_info.as_ref().is_some_and(|s| s.enabled)
}

/// All-profile, profile-qualified exclusion: a move invalidates claims at either end.
fn ensure_move_unclaimed(
    inst: &Instance,
    ownership: &OwnershipGuard,
    source: &Path,
    destination: &Path,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !inst.source_profile.is_empty(),
        "session has no source profile; refusing worktree move"
    );
    let paths = crate::session::deletion::paths_in_use_except_with_ownership(
        ownership,
        &inst.source_profile,
        &[&inst.id],
    );
    anyhow::ensure!(
        !paths.covers(source) && !paths.covers(destination),
        "worktree ownership is shared or unknown; refusing to move {} to {}",
        source.display(),
        destination.display(),
    );
    Ok(())
}

pub struct TrashRequest {
    pub storage: crate::session::Storage,
    pub session_id: String,
    pub instance: Instance,
    pub generation: u64,
    transaction: Arc<ResourceReservation>,
    effect_count: usize,
    relocate_warning: Option<String>,
    started: AtomicBool,
}

impl TrashRequest {
    pub(crate) fn cancel(&self) -> anyhow::Result<()> {
        self.started.store(true, Ordering::Release);
        self.transaction.rollback()
    }
}

impl Drop for TrashRequest {
    fn drop(&mut self) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        let transaction = Arc::clone(&self.transaction);
        if let Err(error) = std::thread::Builder::new()
            .name("aoe-trash-cancel".into())
            .spawn(move || {
                if let Err(error) = transaction.rollback() {
                    tracing::warn!(target: "session.trash", "queued trash cancellation retained its intent: {error:#}");
                }
            })
        {
            tracing::warn!(target: "session.trash", "could not schedule queued trash cancellation: {error}");
        }
    }
}

/// Bind the durable Trash epoch to its full effect plan before queueing work.
pub fn prepare_trash(
    storage: &crate::session::Storage,
    baseline: &Instance,
    generation: u64,
) -> anyhow::Result<TrashRequest> {
    let mut cleanup = super::claim::ReservationCleanup::new(
        storage,
        baseline,
        LifecycleOperation::Trash,
        generation,
    );
    let root = storage.acquire_ownership_lock_bound()?;
    let _identity = acquire_session_identity_lock_with_ownership(&root)?;
    let _lifecycle = storage.acquire_instance_lifecycle_lock_with_ownership(&root, &baseline.id)?;
    let request = prepare_trash_with_ownership(storage, baseline, generation, &root)?;
    cleanup.disarm();
    Ok(request)
}

fn prepare_trash_with_ownership(
    storage: &crate::session::Storage,
    baseline: &Instance,
    generation: u64,
    root: &OwnershipGuard,
) -> anyhow::Result<TrashRequest> {
    let before = storage
        .load_strict_for_worktree_ownership()?
        .into_iter()
        .find(|row| row.id == baseline.id && row.created_at == baseline.created_at)
        .ok_or_else(|| anyhow::anyhow!("trash incarnation disappeared"))?;
    anyhow::ensure!(
        before.is_trashed()
            && restore_inputs_unchanged(baseline, &before)
            && before.lifecycle_reservation_is_owned(LifecycleOperation::Trash, generation),
        "trash lifecycle reservation or relocation plan was superseded"
    );
    let mut after = before.clone();
    after.lifecycle_reservation = None;
    let mut effects = vec![ResourceEffect::Callback {
        name: "trash-teardown".into(),
        directory: PathBuf::from(&before.project_path),
        checkouts: Vec::new(),
    }];
    if is_sandboxed(&before) {
        effects.push(ResourceEffect::ReleaseContainer);
    }
    let mut relocate_warning = None;
    if is_managed_single_worktree(&before)
        && before.pre_trash_project_path.is_none()
        && !is_protected_default_branch(&before)
    {
        let plan = (|| -> anyhow::Result<_> {
            let source = PathBuf::from(&before.project_path);
            let target = trash_holding_path(&source, &before.id)
                .ok_or_else(|| anyhow::anyhow!("worktree path has no parent directory"))?;
            anyhow::ensure!(
                !target.try_exists()?,
                "trash holding path {} already exists",
                target.display()
            );
            ensure_move_unclaimed(&before, root, &source, &target)?;
            let info = before.worktree_info.as_ref().expect("managed worktree");
            let effect = ResourceEffect::MoveWorktree {
                repository: PathBuf::from(&info.main_repo_path),
                from: source,
                to: target.clone(),
            };
            Ok((target, effect))
        })();
        match plan {
            Ok((target, effect)) => {
                after.pre_trash_project_path = Some(before.project_path.clone());
                after.project_path = target.to_string_lossy().into_owned();
                effects.push(effect);
            }
            Err(error) => relocate_warning = Some(error.to_string()),
        }
    }
    let effect_count = effects.len();
    let authority = ProfileAuthority::capture(storage)?;
    let intent = ResourceIntent::new(
        IntentOwner {
            profile: authority.clone(),
            id: before.id.clone(),
            created_at: before.created_at,
            generation,
        },
        authority,
        LifecycleOperation::Trash,
        Some(before.clone()),
        Some(after),
        effects,
        Vec::new(),
    )?;
    let admitted = ResourceReservation::admit_with_ownership(storage, root, intent, before)?;
    Ok(TrashRequest {
        storage: storage.clone(),
        session_id: admitted.instance.id.clone(),
        instance: admitted.instance,
        generation,
        transaction: admitted.transaction,
        effect_count,
        relocate_warning,
        started: AtomicBool::new(false),
    })
}

pub(crate) fn try_prepare_trash(
    storage: &crate::session::Storage,
    baseline: &Instance,
) -> anyhow::Result<Option<TrashRequest>> {
    let Some(root) = super::storage::try_acquire_ownership_lock()? else {
        return Ok(None);
    };
    let Some(_identity) = super::storage::try_acquire_session_identity_lock_with_ownership(&root)?
    else {
        return Ok(None);
    };
    let Some(_lifecycle) =
        storage.try_acquire_instance_lifecycle_lock_with_ownership(&root, &baseline.id)?
    else {
        return Ok(None);
    };
    let Some(reserved) = storage.try_update_with_ownership(&root, |rows, _| {
        let row = rows
            .iter_mut()
            .find(|row| row.id == baseline.id && row.created_at == baseline.created_at)
            .ok_or_else(|| anyhow::anyhow!("session disappeared before trash"))?;
        row.try_acquire_lifecycle_reservation(
            LifecycleOperation::Trash,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        row.trash();
        Ok(row.clone())
    })?
    else {
        return Ok(None);
    };
    let result =
        prepare_trash_with_ownership(storage, &reserved, reserved.lifecycle_generation, &root);
    if result.is_err() {
        let cleanup = storage.update_with_ownership(&root, |rows, _| {
            if let Some(row) = rows
                .iter_mut()
                .find(|row| row.id == reserved.id && row.created_at == reserved.created_at)
            {
                if row
                    .lifecycle_reservation
                    .as_ref()
                    .is_some_and(|reservation| reservation.resource_intent.is_none())
                {
                    row.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Trash,
                        reserved.lifecycle_generation,
                    );
                }
            }
            Ok(())
        });
        if let Err(error) = cleanup {
            tracing::warn!(target: "session.trash", session = %reserved.id, "trash admission cleanup could not persist: {error:#}");
        }
    }
    result.map(Some)
}

fn run_transition_effect(
    transaction: &ResourceReservation,
    instance: &Instance,
    index: usize,
) -> anyhow::Result<()> {
    let effect = transaction.effect(index)?;
    transaction.run_effect(index, || {
        if matches!(effect, ResourceEffect::ReleaseContainer) {
            anyhow::ensure!(
                !ensure_sandbox_container_released(&instance.id, is_sandboxed(instance)),
                "sandbox container still holds the worktree; stop the session first"
            );
            Ok((
                (),
                Some(EffectObservation {
                    container_released: Some(true),
                    ..EffectObservation::default()
                }),
            ))
        } else {
            for warning in effect.execute()? {
                tracing::warn!(target: "session.trash", session = %instance.id, "{warning}");
            }
            Ok(((), None))
        }
    })??;
    Ok(())
}

fn rollback_transition(transaction: &ResourceReservation, error: anyhow::Error) -> anyhow::Error {
    match transaction.rollback() {
        Ok(()) => error,
        Err(rollback) => error.context(format!(
            "transition retained its recovery intent: {rollback:#}"
        )),
    }
}

#[derive(Debug, Clone)]
pub struct TrashRelocation {
    pub new_project_path: String,
    pub pre_trash_project_path: Option<String>,
}

#[derive(Debug)]
pub struct TrashResult {
    pub session_id: String,
    pub source_profile: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub generation: u64,
    pub trash_lifecycle_id: Option<String>,
    pub relocation: Option<TrashRelocation>,
    pub relocate_warning: Option<String>,
}
/// Reserve, stop without metadata flocks, then revalidate before relocation.
pub fn perform_trash(request: &TrashRequest) -> TrashResult {
    match trash_transition(request, Instance::kill_all_tmux_sessions_locked) {
        Ok(outcome) => TrashResult {
            session_id: request.session_id.clone(),
            source_profile: request.instance.source_profile.clone(),
            created_at: request.instance.created_at,
            generation: request.generation,
            trash_lifecycle_id: request.instance.trash_lifecycle_id.clone(),
            relocation: match &outcome {
                RelocateOutcome::Relocated { from, to } => Some(TrashRelocation {
                    new_project_path: to.to_string_lossy().into_owned(),
                    pre_trash_project_path: Some(from.to_string_lossy().into_owned()),
                }),
                _ => None,
            },
            relocate_warning: match outcome {
                RelocateOutcome::Failed { reason } => Some(reason),
                _ => None,
            },
        },
        Err(error) => TrashResult {
            session_id: request.session_id.clone(),
            source_profile: request.instance.source_profile.clone(),
            created_at: request.instance.created_at,
            generation: request.generation,
            trash_lifecycle_id: request.instance.trash_lifecycle_id.clone(),
            relocation: None,
            relocate_warning: Some(error.to_string()),
        },
    }
}

pub(crate) fn trash_transition(
    request: &TrashRequest,
    teardown: impl FnOnce(&Instance),
) -> anyhow::Result<RelocateOutcome> {
    anyhow::ensure!(
        !request.started.swap(true, Ordering::AcqRel),
        "trash request has already started or was cancelled"
    );
    let result = (|| -> anyhow::Result<_> {
        request.transaction.run_effect(0, || {
            teardown(&request.instance);
            if let Err(error) = crate::session::worktree_edit::stop_sandbox_container(
                &request.instance.id,
                is_sandboxed(&request.instance),
            ) {
                tracing::warn!(target: "session.trash", session = %request.instance.id, "stopping sandbox container before trash relocation failed: {error}");
            }
            Ok(((), None))
        })??;
        for index in 1..request.effect_count {
            run_transition_effect(&request.transaction, &request.instance, index)?;
        }
        let root = request.storage.acquire_ownership_lock_bound()?;
        let _identity = acquire_session_identity_lock_with_ownership(&root)?;
        let _lifecycle = request
            .storage
            .acquire_instance_lifecycle_lock_with_ownership(&root, &request.session_id)?;
        request
            .transaction
            .publish_with_ownership(&root, |_, _| Ok(()))?
            .ok_or_else(|| anyhow::anyhow!("trash publication lost its incarnation"))
    })();
    let committed = result.map_err(|error| rollback_transition(&request.transaction, error))?;
    if committed.project_path != request.instance.project_path {
        Ok(RelocateOutcome::Relocated {
            from: PathBuf::from(&request.instance.project_path),
            to: PathBuf::from(&committed.project_path),
        })
    } else if let Some(reason) = &request.relocate_warning {
        Ok(RelocateOutcome::Failed {
            reason: reason.clone(),
        })
    } else {
        Ok(RelocateOutcome::Skipped)
    }
}

#[derive(Debug)]
pub(crate) enum RestoreError {
    NotFound,
    Busy(String),
    Worktree(String),
    Persist(anyhow::Error),
}

fn restore_inputs_unchanged(before: &Instance, after: &Instance) -> bool {
    before.id == after.id
        && before.created_at == after.created_at
        && before.source_profile == after.source_profile
        && before.lifecycle_generation == after.lifecycle_generation
        && before.trashed_at == after.trashed_at
        && before.trash_lifecycle_id == after.trash_lifecycle_id
        && before.project_path == after.project_path
        && before.pre_trash_project_path == after.pre_trash_project_path
        && before.worktree_info == after.worktree_info
        && before
            .workspace_info
            .as_ref()
            .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
            == after
                .workspace_info
                .as_ref()
                .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
        && before.scratch == after.scratch
        && is_sandboxed(before) == is_sandboxed(after)
}

/// Admit once, wait for Docker and Git without fences, then publish the owned delta.
pub(crate) fn restore_transition(
    storage: &crate::session::Storage,
    baseline: &Instance,
    owned_trash_generation: Option<u64>,
) -> Result<Instance, RestoreError> {
    use crate::session::claim::RestoreClaimDecision;
    let (transaction, reserved, effect_count) = {
        let root = storage
            .acquire_ownership_lock_bound()
            .map_err(RestoreError::Persist)?;
        let _identity =
            acquire_session_identity_lock_with_ownership(&root).map_err(RestoreError::Persist)?;
        let _lifecycle = storage
            .acquire_instance_lifecycle_lock_with_ownership(&root, &baseline.id)
            .map_err(RestoreError::Persist)?;
        let mut rows = storage
            .load_strict_for_worktree_ownership()
            .map_err(RestoreError::Persist)?;
        let current = rows
            .iter()
            .find(|row| row.id == baseline.id)
            .ok_or(RestoreError::NotFound)?;
        if !restore_inputs_unchanged(baseline, current) {
            return Err(RestoreError::Busy(
                crate::session::NEWER_GENERATION_BUSY_REASON.into(),
            ));
        }
        if let Some(lease) = current
            .lifecycle_reservation
            .as_ref()
            .filter(|lease| lease.resource_intent.is_some())
        {
            return Err(RestoreError::Busy(lease.op.busy_reason()));
        }
        let before = current.clone();
        let decision = match owned_trash_generation {
            Some(generation) => crate::session::claim::decide_restore_claim_after_trash(
                &mut rows,
                &baseline.id,
                generation,
                Utc::now(),
            ),
            None => {
                crate::session::claim::decide_restore_claim(&mut rows, &baseline.id, Utc::now())
            }
        }
        .map_err(|error| RestoreError::Persist(error.into()))?;
        let generation = match decision {
            RestoreClaimDecision::AlreadyGone => return Err(RestoreError::NotFound),
            RestoreClaimDecision::Busy(holder) => {
                return Err(RestoreError::Busy(holder.busy_reason()))
            }
            RestoreClaimDecision::Claimed(generation) => generation,
        };
        let reserved = rows
            .into_iter()
            .find(|row| row.id == baseline.id)
            .ok_or(RestoreError::NotFound)?;
        let mut after = reserved.clone();
        after.untrash();
        after.lifecycle_reservation = None;
        let mut effects = Vec::new();
        if let Some(original) = &reserved.pre_trash_project_path {
            if *original != reserved.project_path {
                let source = PathBuf::from(&reserved.project_path);
                let destination = PathBuf::from(original);
                if destination
                    .try_exists()
                    .map_err(|error| RestoreError::Persist(error.into()))?
                {
                    return Err(RestoreError::Worktree(format!(
                        "original worktree path {} is occupied; move or remove it first",
                        destination.display(),
                    )));
                }
                ensure_move_unclaimed(&reserved, &root, &source, &destination)
                    .map_err(|error| RestoreError::Worktree(error.to_string()))?;
                let info = reserved.worktree_info.as_ref().ok_or_else(|| {
                    RestoreError::Worktree("relocated checkout has no repository".into())
                })?;
                if is_sandboxed(&reserved) {
                    effects.push(ResourceEffect::ReleaseContainer);
                }
                effects.push(ResourceEffect::MoveWorktree {
                    repository: PathBuf::from(&info.main_repo_path),
                    from: source,
                    to: destination,
                });
            }
            after.project_path.clone_from(original);
            after.pre_trash_project_path = None;
        }
        let count = effects.len();
        let authority = ProfileAuthority::capture(storage).map_err(RestoreError::Persist)?;
        let intent = ResourceIntent::new(
            IntentOwner {
                profile: authority.clone(),
                id: reserved.id.clone(),
                created_at: reserved.created_at,
                generation,
            },
            authority,
            LifecycleOperation::Restore,
            Some(before),
            Some(after),
            effects,
            Vec::new(),
        )
        .map_err(RestoreError::Persist)?;
        let admitted = ResourceReservation::admit_with_ownership(storage, &root, intent, reserved)
            .map_err(RestoreError::Persist)?;
        (admitted.transaction, admitted.instance, count)
    };
    #[cfg(test)]
    if let Some(hook) = BEFORE_RESTORE_PREFLIGHT.with(|slot| slot.borrow_mut().take()) {
        hook(&reserved);
    }
    for index in 0..effect_count {
        if let Err(error) = run_transition_effect(&transaction, &reserved, index) {
            return Err(RestoreError::Worktree(
                rollback_transition(&transaction, error).to_string(),
            ));
        }
    }
    let publish = (|| -> anyhow::Result<_> {
        let root = storage.acquire_ownership_lock_bound()?;
        let _identity = acquire_session_identity_lock_with_ownership(&root)?;
        let _lifecycle =
            storage.acquire_instance_lifecycle_lock_with_ownership(&root, &reserved.id)?;
        transaction
            .publish_with_ownership(&root, |_, _| Ok(()))?
            .ok_or_else(|| anyhow::anyhow!("restore incarnation disappeared"))
    })();
    publish.map_err(|error| RestoreError::Persist(rollback_transition(&transaction, error)))
}

#[cfg(test)]
type RestorePreflightHook = Box<dyn FnOnce(&Instance)>;

#[cfg(test)]
thread_local! {
    static BEFORE_RESTORE_PREFLIGHT: std::cell::RefCell<Option<RestorePreflightHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn with_restore_preflight_for_test<T>(
    hook: impl FnOnce(&Instance) + 'static,
    run: impl FnOnce() -> T,
) -> T {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            BEFORE_RESTORE_PREFLIGHT.with(|slot| slot.borrow_mut().take());
        }
    }
    BEFORE_RESTORE_PREFLIGHT.with(|slot| {
        assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
    });
    let _reset = Reset;
    run()
}

/// What a load-time reconcile would do to one trashed row.
#[derive(Debug, PartialEq, Eq)]
enum ReconcilePlan {
    /// The row is consistent, or is not one this pass owns.
    Nothing,
    /// Move a protected default branch's checkout back out of the holding area.
    RestoreDefaultBranch,
    /// Legacy backfill: relocate a worktree still sitting in the active dir.
    Relocate,
    /// The worktree is in the holding area but the pointer persist was lost.
    PointAtHolding { holding: PathBuf, original: PathBuf },
    /// The holding move never took (or was undone); point back at the original.
    PointAtOriginal(PathBuf),
}

fn plan_trashed_reconcile(inst: &Instance) -> ReconcilePlan {
    plan_trashed_reconcile_cached(inst, &mut ProtectedBranchCache::default())
}

fn plan_trashed_reconcile_cached(
    inst: &Instance,
    cache: &mut ProtectedBranchCache,
) -> ReconcilePlan {
    if !inst.is_trashed() || !is_managed_single_worktree(inst) {
        return ReconcilePlan::Nothing;
    }

    // Upgrade path for: a default branch's checkout that an earlier version relocated is still
    // sitting in the holding area, and the purge now refuses to remove it, so clearing the row
    // would leave that checkout there with nothing pointing at it.
    if inst.pre_trash_project_path.is_some() && is_protected_default_branch_cached(inst, cache) {
        return ReconcilePlan::RestoreDefaultBranch;
    }

    let current = PathBuf::from(&inst.project_path);
    // The pre-trash location: the recorded marker if we have one, else the
    // current path (an un-relocated legacy row points at its own original).
    let original = inst
        .pre_trash_project_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| current.clone());
    let Some(holding) = trash_holding_path(&original, &inst.id) else {
        return ReconcilePlan::Nothing;
    };

    if current.exists() {
        // Legacy backfill: a trashed managed worktree still sitting in the active dir with no
        // marker gets relocated now.
        if inst.pre_trash_project_path.is_some()
            || current == holding
            || is_holding_path(&current, &inst.id)
        {
            return ReconcilePlan::Nothing;
        }
        // Crash case: the worktree was already moved to `holding` but the marker/pointer persist
        // was lost and something was recreated at the original path.
        if holding.exists() {
            return ReconcilePlan::PointAtHolding { holding, original };
        }
        // Terminal state for a relocation that can never succeed.
        if is_stranded_checkout(&current) {
            tracing::warn!(
                target: "session.trash",
                session = %inst.id,
                path = %current.display(),
                "trashed worktree is no longer registered with its repo; leaving it in place"
            );
            return ReconcilePlan::Nothing;
        }
        // A default branch's checkout is never relocated, so planning the move would reserve the
        // row, take its flock, and write twice on every sweep for a relocation that always answers
        // Skipped.
        if is_protected_default_branch_cached(inst, cache) {
            return ReconcilePlan::Nothing;
        }
        return ReconcilePlan::Relocate;
    }

    // The recorded path is gone. Heal the pointer toward wherever the worktree
    // actually landed.
    if holding.exists() {
        return ReconcilePlan::PointAtHolding { holding, original };
    }
    if original.exists() && original != current {
        return ReconcilePlan::PointAtOriginal(original);
    }
    ReconcilePlan::Nothing
}

/// Reconcile every trashed row in one profile, batched.
pub fn reconcile_trashed_profile(profile: &str) -> anyhow::Result<Vec<Instance>> {
    let storage = crate::session::Storage::open_unwatched(profile)?;
    reconcile_trashed_storage(&storage)
}

/// Sweep the logical profile selected when background work was requested.
pub(crate) fn reconcile_trashed_storage(
    storage: &crate::session::Storage,
) -> anyhow::Result<Vec<Instance>> {
    let ownership = storage.acquire_ownership_read_bound()?;
    storage.verify_profile_available()?;
    let mut cache = ProtectedBranchCache::default();
    let mut candidates: Vec<Instance> = storage
        .load_strict_for_worktree_ownership()?
        .into_iter()
        .filter(|inst| plan_trashed_reconcile_cached(inst, &mut cache) != ReconcilePlan::Nothing)
        .collect();
    candidates.sort_by(|a, b| a.id.cmp(&b.id));
    drop(ownership);

    let mut healed = Vec::new();
    for batch in candidates.chunks(RECONCILE_BATCH) {
        // Each row retains its own reservation only across its preflight and commit.
        match reconcile_trashed_batch(storage, batch) {
            Ok(batch_healed) => healed.extend(batch_healed),
            Err(error) => tracing::warn!(
                target: "session.trash",
                rows = batch.len(),
                "trash reconciliation batch skipped: {error}"
            ),
        }
    }
    Ok(healed)
}

/// Limit each scan chunk without retaining fences across Docker.
const RECONCILE_BATCH: usize = 8;

fn reconcile_trashed_batch(
    storage: &crate::session::Storage,
    batch: &[Instance],
) -> anyhow::Result<Vec<Instance>> {
    let mut healed = Vec::new();
    for snapshot in batch {
        let mut instance = snapshot.clone();
        match reconcile_trashed_transition(storage, &mut instance) {
            Ok(true) => healed.push(instance),
            Ok(false) => {}
            Err(error) => tracing::debug!(
                target: "session.trash", session = %snapshot.id,
                "trash reconciliation deferred: {error}"
            ),
        }
    }
    Ok(healed)
}

/// Repair a trashed row through the same logical intent as interactive moves.
pub fn reconcile_trashed_transition(
    storage: &crate::session::Storage,
    inst: &mut Instance,
) -> anyhow::Result<bool> {
    let plan = plan_trashed_reconcile(inst);
    if plan == ReconcilePlan::Nothing {
        return Ok(false);
    }
    let (transaction, reserved, effect_count) = {
        let root = storage.acquire_ownership_lock_bound()?;
        let _identity = acquire_session_identity_lock_with_ownership(&root)?;
        let _lifecycle = storage.acquire_instance_lifecycle_lock_with_ownership(&root, &inst.id)?;
        let before = storage
            .load_strict_for_worktree_ownership()?
            .into_iter()
            .find(|row| row.id == inst.id && row.created_at == inst.created_at)
            .ok_or_else(|| anyhow::anyhow!("trash reconciliation incarnation disappeared"))?;
        anyhow::ensure!(
            restore_inputs_unchanged(inst, &before) && plan_trashed_reconcile(&before) == plan,
            "trash reconciliation snapshot or plan was superseded"
        );
        let mut reserved = before.clone();
        let generation = reserved.try_acquire_lifecycle_reservation(
            LifecycleOperation::Trash,
            Instance::LIFECYCLE_RESERVATION_TTL,
            Utc::now(),
        )?;
        let mut after = reserved.clone();
        after.lifecycle_reservation = None;
        let destination = match &plan {
            ReconcilePlan::Nothing => unreachable!(),
            ReconcilePlan::RestoreDefaultBranch => {
                let original = before
                    .pre_trash_project_path
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("default checkout has no original path"))?;
                after.project_path.clone_from(original);
                after.pre_trash_project_path = None;
                Some(PathBuf::from(original))
            }
            ReconcilePlan::Relocate => {
                let holding = trash_holding_path(Path::new(&before.project_path), &before.id)
                    .ok_or_else(|| anyhow::anyhow!("worktree path has no parent directory"))?;
                after.pre_trash_project_path = Some(before.project_path.clone());
                after.project_path = holding.to_string_lossy().into_owned();
                Some(holding)
            }
            ReconcilePlan::PointAtHolding { holding, original } => {
                after.project_path = holding.to_string_lossy().into_owned();
                after.pre_trash_project_path = Some(original.to_string_lossy().into_owned());
                None
            }
            ReconcilePlan::PointAtOriginal(original) => {
                after.project_path = original.to_string_lossy().into_owned();
                after.pre_trash_project_path = None;
                None
            }
        };
        let mut effects = Vec::new();
        if let Some(destination) =
            destination.filter(|path| *path != Path::new(&before.project_path))
        {
            let source = PathBuf::from(&before.project_path);
            anyhow::ensure!(
                !destination.try_exists()?,
                "worktree destination {} is occupied",
                destination.display()
            );
            ensure_move_unclaimed(&before, &root, &source, &destination)?;
            if is_sandboxed(&before) {
                effects.push(ResourceEffect::ReleaseContainer);
            }
            effects.push(ResourceEffect::MoveWorktree {
                repository: PathBuf::from(
                    &before
                        .worktree_info
                        .as_ref()
                        .expect("managed worktree")
                        .main_repo_path,
                ),
                from: source,
                to: destination,
            });
        }
        let count = effects.len();
        let authority = ProfileAuthority::capture(storage)?;
        let intent = ResourceIntent::new(
            IntentOwner {
                profile: authority.clone(),
                id: before.id.clone(),
                created_at: before.created_at,
                generation,
            },
            authority,
            LifecycleOperation::Trash,
            Some(before),
            Some(after),
            effects,
            Vec::new(),
        )?;
        let admitted = ResourceReservation::admit_with_ownership(storage, &root, intent, reserved)?;
        (admitted.transaction, admitted.instance, count)
    };
    #[cfg(test)]
    if let Some(hook) = BEFORE_RESTORE_PREFLIGHT.with(|slot| slot.borrow_mut().take()) {
        hook(&reserved);
    }
    let publish = (|| -> anyhow::Result<_> {
        for index in 0..effect_count {
            run_transition_effect(&transaction, &reserved, index)?;
        }
        let root = storage.acquire_ownership_lock_bound()?;
        let _identity = acquire_session_identity_lock_with_ownership(&root)?;
        let _lifecycle =
            storage.acquire_instance_lifecycle_lock_with_ownership(&root, &reserved.id)?;
        transaction
            .publish_with_ownership(&root, |_, _| Ok(()))?
            .ok_or_else(|| anyhow::anyhow!("trash reconciliation incarnation disappeared"))
    })();
    *inst = publish.map_err(|error| rollback_transition(&transaction, error))?;
    Ok(true)
}

/// True when a trashed session is past its retention window and should be auto-purged.
pub fn is_expired(instance: &Instance, retention_minutes: u32, now: DateTime<Utc>) -> bool {
    if retention_minutes == 0 {
        return false;
    }
    match instance.trashed_at {
        Some(trashed_at) => {
            now >= trashed_at + chrono::Duration::minutes(i64::from(retention_minutes))
        }
        None => false,
    }
}

/// Shortest wait between daemon retention sweeps, and how often the daemon
/// re-reads the windows so a shortened one applies within a minute.
pub const SWEEP_RECHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// Wait between daemon retention sweeps: a tenth of the shortest nonzero
/// window in minutes, clamped to [`SWEEP_RECHECK`] through one hour, so a
/// purge lags its window by at most that much.
pub fn sweep_interval(retention_minutes: impl IntoIterator<Item = u32>) -> std::time::Duration {
    const MAX_SECS: u64 = 60 * 60;
    let shortest = retention_minutes.into_iter().filter(|m| *m > 0).min();
    let secs = shortest.map_or(MAX_SECS, |minutes| u64::from(minutes) * 6);
    std::time::Duration::from_secs(secs.clamp(SWEEP_RECHECK.as_secs(), MAX_SECS))
}

/// Ids of every trashed session whose retention window has elapsed, in the order they appear in
/// `instances`.
pub fn expired_trashed_ids(
    instances: &[Instance],
    retention_minutes: u32,
    now: DateTime<Utc>,
) -> Vec<String> {
    instances
        .iter()
        .filter(|i| is_expired(i, retention_minutes, now))
        .map(|i| i.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persist_fixture(instance: &mut Instance) -> anyhow::Result<crate::session::Storage> {
        let profile = if instance.source_profile.is_empty() {
            "default".to_owned()
        } else {
            instance.source_profile.clone()
        };
        let storage = crate::session::Storage::open_unwatched(&profile)?;
        instance.source_profile = profile;
        storage.update(|rows, _| {
            if let Some(current) = rows.iter_mut().find(|row| row.id == instance.id) {
                *current = instance.clone();
            } else {
                rows.push(instance.clone());
            }
            Ok(())
        })?;
        Ok(storage)
    }

    fn relocate_fixture(instance: &mut Instance) -> RelocateOutcome {
        let from = PathBuf::from(&instance.project_path);
        let moved = persist_fixture(instance)
            .and_then(|storage| reconcile_trashed_transition(&storage, instance));
        match moved {
            Ok(true) if from != Path::new(&instance.project_path) => RelocateOutcome::Relocated {
                from,
                to: PathBuf::from(&instance.project_path),
            },
            Ok(_) => RelocateOutcome::Skipped,
            Err(error) => RelocateOutcome::Failed {
                reason: error.to_string(),
            },
        }
    }

    fn restore_fixture(instance: &mut Instance) -> Result<(), RestoreError> {
        let storage = persist_fixture(instance).map_err(RestoreError::Persist)?;
        *instance = restore_transition(&storage, instance, None)?;
        Ok(())
    }

    fn reconcile_fixture(instance: &mut Instance) -> bool {
        persist_fixture(instance)
            .and_then(|storage| reconcile_trashed_transition(&storage, instance))
            .unwrap_or(false)
    }

    fn trashed_days_ago(days: i64) -> Instance {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.trashed_at = Some(Utc::now() - chrono::Duration::days(days));
        inst
    }

    #[test]
    fn is_expired_cases() {
        let now = Utc::now();
        const DAY: u32 = 24 * 60;
        // (case, trashed minutes ago, retention minutes, expected)
        let cases = [
            ("retention 0 keeps forever", Some(9999 * 1440), 0, false),
            ("never trashed", None, 30 * DAY, false),
            ("at the retention window", Some(30 * 1440), 30 * DAY, true),
            (
                "one day inside the window",
                Some(29 * 1440),
                30 * DAY,
                false,
            ),
            ("past a sub-hour window", Some(16), 15, true),
            ("inside a sub-hour window", Some(14), 15, false),
            ("past a two-hour window", Some(121), 120, true),
        ];
        for (case, trashed_minutes, retention, expected) in cases {
            let mut inst = Instance::new("s", "/tmp/x");
            inst.trashed_at =
                trashed_minutes.map(|minutes| now - chrono::Duration::minutes(minutes));
            assert_eq!(is_expired(&inst, retention, now), expected, "{case}");
        }
        let fresh = trashed_days_ago(1);
        let old_a = trashed_days_ago(40);
        let live = Instance::new("s", "/tmp/x");
        let old_b = trashed_days_ago(31);
        let instances = vec![fresh, old_a.clone(), live, old_b.clone()];
        assert_eq!(
            expired_trashed_ids(&instances, 30 * DAY, now),
            vec![old_a.id, old_b.id],
            "filters and preserves order"
        );
    }

    #[test]
    fn sweep_interval_tracks_the_shortest_window() {
        use std::time::Duration;
        // (case, windows in minutes, expected seconds)
        let cases: [(&str, &[u32], u64); 6] = [
            ("no profiles", &[], 3600),
            ("keep forever only", &[0, 0], 3600),
            ("30 days", &[43200], 3600),
            ("two hours", &[43200, 120], 720),
            ("sub-hour window", &[0, 30], 180),
            ("floor of a minute", &[5], 60),
        ];
        for (case, windows, secs) in cases {
            assert_eq!(
                sweep_interval(windows.iter().copied()),
                Duration::from_secs(secs),
                "{case}"
            );
        }
    }

    #[test]
    fn holding_path_is_namespaced_sibling() {
        let p = trash_holding_path(Path::new("/repo-worktrees/feature"), "abc123").unwrap();
        assert_eq!(p, PathBuf::from("/repo-worktrees/.aoe-trash/abc123"));
        assert!(trash_holding_path(Path::new("/"), "abc123").is_none());
    }

    fn real_worktree_instance() -> (tempfile::TempDir, Instance) {
        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        let worktree_path = tmp.path().join("wt").join("feature");
        std::fs::create_dir_all(&main_repo).unwrap();
        std::fs::create_dir_all(worktree_path.parent().unwrap()).unwrap();

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
                "feature/relocate-me",
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

        let mut inst = Instance::new("WT", worktree_path.to_str().unwrap());
        inst.source_profile = "default".into();
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "feature/relocate-me".to_string(),
            main_repo_path: main_repo.to_string_lossy().to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        (tmp, inst)
    }

    fn stored_restore_fixture() -> (tempfile::TempDir, crate::session::Storage, Instance) {
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let (tmp, mut instance) = real_worktree_instance();
        instance.status = crate::session::Status::Stopped;
        instance.trash();
        assert!(matches!(
            relocate_fixture(&mut instance),
            RelocateOutcome::Relocated { .. }
        ));
        (tmp, storage, instance)
    }

    #[test]
    #[serial_test::serial]
    fn restore_preflight_releases_all_fences_and_preserves_annotations() {
        let _app = crate::session::test_support::isolate_app_dir();
        let (_tmp, storage, baseline) = stored_restore_fixture();
        let peer = storage.clone();
        let restored = with_restore_preflight_for_test(
            move |reserved| {
                let root = acquire_ownership_lock().unwrap();
                let _identity = acquire_session_identity_lock_with_ownership(&root).unwrap();
                let _lifecycle = peer
                    .acquire_instance_lifecycle_lock_with_ownership(&root, &reserved.id)
                    .unwrap();
                peer.update_with_ownership(&root, |rows, _| {
                    rows[0].color = Some("blue".into());
                    Ok(())
                })
                .unwrap();
            },
            || restore_transition(&storage, &baseline, None),
        )
        .unwrap();
        assert_eq!(
            restored.project_path,
            baseline.pre_trash_project_path.unwrap()
        );
        assert!(!restored.is_trashed());
        assert_eq!(restored.status, crate::session::Status::Stopped);
        assert_eq!(restored.color.as_deref(), Some("blue"));
        assert!(Path::new(&restored.project_path).join(".git").exists());
        assert!(!Path::new(&baseline.project_path).exists());
        let durable = storage.load().unwrap().remove(0);
        assert!(!durable.is_trashed());
        assert_eq!(durable.project_path, restored.project_path);
        assert_eq!(durable.status, crate::session::Status::Stopped);
        assert_eq!(durable.color.as_deref(), Some("blue"));
        assert_eq!(durable.created_at, restored.created_at);
        assert_eq!(durable.lifecycle_generation, restored.lifecycle_generation);
        assert!(durable.lifecycle_reservation.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn restore_revalidates_incarnation_generation_profile_checkout_and_peer_claims() {
        for change in ["incarnation", "generation"] {
            let _app = crate::session::test_support::isolate_app_dir();
            let (_tmp, storage, baseline) = stored_restore_fixture();
            storage
                .update(|rows, _| {
                    if change == "incarnation" {
                        rows[0].created_at += chrono::Duration::seconds(1);
                    } else {
                        rows[0].try_acquire_lifecycle_reservation(
                            LifecycleOperation::Trash,
                            Instance::LIFECYCLE_RESERVATION_TTL,
                            Utc::now(),
                        )?;
                    }
                    Ok(())
                })
                .unwrap();
            let before = std::fs::read(storage.sessions_path()).unwrap();
            let current = storage.load().unwrap().remove(0);
            assert!(matches!(
                restore_transition(&storage, &baseline, None),
                Err(RestoreError::Busy(_))
            ));
            assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
            assert!(Path::new(&baseline.project_path).join(".git").exists());
            assert!(!Path::new(baseline.pre_trash_project_path.as_ref().unwrap()).exists());
            let durable = storage.load().unwrap().remove(0);
            assert_eq!(durable.created_at, current.created_at);
            assert_eq!(durable.lifecycle_generation, current.lifecycle_generation);
            assert_eq!(durable.lifecycle_reservation, current.lifecycle_reservation);
            if change == "generation" {
                assert!(durable.lifecycle_reservation_is_owned(
                    LifecycleOperation::Trash,
                    baseline.lifecycle_generation + 1,
                ));
            }
        }

        for change in ["incarnation", "generation", "profile", "destination-owner"] {
            let _app = crate::session::test_support::isolate_app_dir();
            let (_tmp, storage, baseline) = stored_restore_fixture();
            let peer = crate::session::Storage::new_unwatched("peer").unwrap();
            let edited = storage.clone();
            let restored = with_restore_preflight_for_test(
                move |reserved| {
                    let before = std::fs::read(edited.sessions_path()).unwrap();
                    let peer_before = std::fs::read(peer.sessions_path()).ok();
                    let marker = reserved.lifecycle_reservation.as_ref().unwrap();
                    assert!(marker.resource_intent.is_some());
                    let error = match change {
                        "incarnation" | "generation" => edited.update(|rows, _| {
                            if change == "incarnation" {
                                rows[0].created_at += chrono::Duration::seconds(1);
                            } else {
                                rows[0].release_lifecycle_reservation_if_owned(
                                    LifecycleOperation::Restore,
                                    reserved.lifecycle_generation,
                                );
                                rows[0].try_acquire_lifecycle_reservation(
                                    LifecycleOperation::Trash,
                                    Instance::LIFECYCLE_RESERVATION_TTL,
                                    Utc::now(),
                                )?;
                            }
                            Ok(())
                        }),
                        "profile" => crate::session::rename_profile("default", "renamed"),
                        "destination-owner" => peer.update(|rows, _| {
                            rows.push(Instance::new(
                                "adopter",
                                reserved.pre_trash_project_path.as_ref().unwrap(),
                            ));
                            Ok(())
                        }),
                        _ => unreachable!(),
                    }
                    .unwrap_err();
                    assert!(error.to_string().contains("busy"), "{change}: {error:#}");
                    if change == "profile" {
                        let error = crate::session::delete_profile("default").unwrap_err();
                        assert!(error.to_string().contains("busy"));
                        assert!(crate::session::get_profile_dir_path("default")
                            .unwrap()
                            .is_dir());
                        assert!(!crate::session::get_profile_dir_path("renamed")
                            .unwrap()
                            .exists());
                    }
                    assert_eq!(std::fs::read(edited.sessions_path()).unwrap(), before);
                    assert_eq!(std::fs::read(peer.sessions_path()).ok(), peer_before);
                    let durable = edited.load().unwrap().remove(0);
                    assert_eq!(durable.created_at, reserved.created_at);
                    assert_eq!(durable.lifecycle_generation, reserved.lifecycle_generation);
                    assert_eq!(
                        durable.lifecycle_reservation,
                        reserved.lifecycle_reservation
                    );
                    assert_eq!(durable.project_path, reserved.project_path);
                    assert!(peer.load().unwrap().is_empty());
                    assert!(Path::new(&reserved.project_path).join(".git").exists());
                    assert!(!Path::new(reserved.pre_trash_project_path.as_ref().unwrap()).exists());
                },
                || restore_transition(&storage, &baseline, None),
            )
            .unwrap();
            let durable = storage.load().unwrap().remove(0);
            assert_eq!(restored.created_at, baseline.created_at);
            assert_eq!(
                restored.lifecycle_generation,
                baseline.lifecycle_generation + 1
            );
            assert_eq!(
                restored.project_path,
                *baseline.pre_trash_project_path.as_ref().unwrap()
            );
            assert!(!restored.is_trashed());
            assert!(restored.lifecycle_reservation.is_none());
            assert!(restored.pre_trash_project_path.is_none());
            assert_eq!(durable.project_path, restored.project_path);
            assert_eq!(durable.created_at, restored.created_at);
            assert_eq!(durable.lifecycle_generation, restored.lifecycle_generation);
            assert!(durable.lifecycle_reservation.is_none());
            assert!(!durable.is_trashed());
            assert!(Path::new(&restored.project_path).join(".git").exists());
            assert!(!Path::new(&baseline.project_path).exists());
        }

        let _app = crate::session::test_support::isolate_app_dir();
        let (tmp, storage, baseline) = stored_restore_fixture();
        let retired = tmp.path().join("retired");
        let saved = retired.clone();
        let result = with_restore_preflight_for_test(
            move |reserved| {
                std::fs::rename(&reserved.project_path, &saved).unwrap();
                std::fs::create_dir(&reserved.project_path).unwrap();
                std::fs::write(
                    Path::new(&reserved.project_path).join("replacement"),
                    "keep",
                )
                .unwrap();
            },
            || restore_transition(&storage, &baseline, None),
        );
        assert!(result.is_err());
        assert!(!Path::new(baseline.pre_trash_project_path.as_ref().unwrap()).exists());
        assert_eq!(
            std::fs::read_to_string(Path::new(&baseline.project_path).join("replacement")).unwrap(),
            "keep",
        );
        assert!(retired.join(".git").exists());
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.project_path, baseline.project_path);
        assert_eq!(durable.created_at, baseline.created_at);
        assert!(durable.is_trashed());
    }

    fn default_branch_worktree_instance() -> (tempfile::TempDir, Instance) {
        let tmp = tempfile::TempDir::new().unwrap();
        let bare = tmp.path().join("project").join(".bare");
        let worktree_path = tmp.path().join("project").join("main");
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

        let mut inst = Instance::new("Infra", worktree_path.to_str().unwrap());
        inst.source_profile = "default".into();
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "main".to_string(),
            main_repo_path: bare.to_string_lossy().to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        (tmp, inst)
    }

    #[test]
    fn a_default_branch_checkout_is_never_planned_or_relocated() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = default_branch_worktree_instance();
        let original = inst.project_path.clone();
        inst.trash();
        assert_eq!(plan_trashed_reconcile(&inst), ReconcilePlan::Nothing);
        assert!(!reconcile_fixture(&mut inst));

        let out = relocate_fixture(&mut inst);
        assert!(
            matches!(out, RelocateOutcome::Skipped),
            "expected the relocation to be skipped, got {out:?}"
        );
        assert_eq!(inst.project_path, original);
        assert!(inst.pre_trash_project_path.is_none());
        assert!(PathBuf::from(&original).exists());
    }

    #[test]
    fn reconcile_moves_a_relocated_default_branch_checkout_back() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = default_branch_worktree_instance();
        let original = PathBuf::from(&inst.project_path);
        inst.trash();

        let holding = trash_holding_path(&original, &inst.id).unwrap();
        std::fs::create_dir_all(holding.parent().unwrap()).unwrap();
        let bare = inst.worktree_info.as_ref().unwrap().main_repo_path.clone();
        let out = std::process::Command::new("git")
            .args([
                "worktree",
                "move",
                original.to_str().unwrap(),
                holding.to_str().unwrap(),
            ])
            .current_dir(&bare)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree move failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        inst.pre_trash_project_path = Some(original.to_string_lossy().into_owned());
        inst.project_path = holding.to_string_lossy().into_owned();

        assert!(
            reconcile_fixture(&mut inst),
            "reconcile must move the checkout back and report the mutation"
        );
        assert_eq!(PathBuf::from(&inst.project_path), original);
        assert!(inst.pre_trash_project_path.is_none());
        assert!(original.exists());
        assert!(!holding.exists());

        assert!(
            !reconcile_fixture(&mut inst),
            "reconcile must be idempotent once the checkout is back"
        );
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn relocate_then_restore_round_trip() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let original = inst.project_path.clone();
        inst.trash();

        let out = relocate_fixture(&mut inst);
        assert!(
            matches!(out, RelocateOutcome::Relocated { .. }),
            "expected relocation, got {out:?}"
        );
        let holding = trash_holding_path(Path::new(&original), &inst.id).unwrap();
        assert_eq!(PathBuf::from(&inst.project_path), holding);
        assert!(holding.exists());
        assert!(!PathBuf::from(&original).exists());
        assert_eq!(
            inst.pre_trash_project_path.as_deref(),
            Some(original.as_str())
        );

        assert!(matches!(
            relocate_fixture(&mut inst),
            RelocateOutcome::Skipped
        ));

        std::fs::create_dir_all(&original).unwrap();
        let occupied = restore_fixture(&mut inst);
        assert!(
            occupied.is_err(),
            "restore should refuse an occupied original, got {occupied:?}"
        );
        assert!(inst.pre_trash_project_path.is_some());
        assert_ne!(inst.project_path, original);
        std::fs::remove_dir(&original).unwrap();

        let back = restore_fixture(&mut inst);
        assert!(back.is_ok(), "expected restore, got {back:?}");
        assert_eq!(inst.project_path, original);
        assert!(inst.pre_trash_project_path.is_none());
        assert!(PathBuf::from(&original).exists());
    }

    #[test]
    fn reconcile_backfills_legacy_then_is_idempotent() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let original = inst.project_path.clone();
        inst.trash();
        assert!(inst.pre_trash_project_path.is_none());

        assert!(
            reconcile_fixture(&mut inst),
            "reconcile should relocate a legacy trashed worktree"
        );
        let holding = trash_holding_path(Path::new(&original), &inst.id).unwrap();
        assert_eq!(PathBuf::from(&inst.project_path), holding);
        assert_eq!(
            inst.pre_trash_project_path.as_deref(),
            Some(original.as_str())
        );
        assert!(!PathBuf::from(&original).exists());

        assert!(!reconcile_fixture(&mut inst));
    }

    #[test]
    fn reconcile_never_retries_a_checkout_the_repo_no_longer_registers() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        for prune_admin_dir in [true, false] {
            let (_tmp, mut inst) = real_worktree_instance();
            let original = PathBuf::from(&inst.project_path);
            inst.trash();
            if prune_admin_dir {
                let link = std::fs::read_to_string(original.join(".git")).unwrap();
                let admin = link.split_once("gitdir:").unwrap().1.trim().to_string();
                std::fs::remove_dir_all(&admin).unwrap();
                assert!(original.join(".git").exists(), "the dangling link stays");
            } else {
                std::fs::remove_file(original.join(".git")).unwrap();
            }

            assert!(
                !reconcile_fixture(&mut inst),
                "a stranded checkout must not be retried (prune_admin_dir={prune_admin_dir})"
            );
            assert_eq!(PathBuf::from(&inst.project_path), original);
            assert!(inst.pre_trash_project_path.is_none());
            assert!(original.exists());
        }
    }

    #[test]
    fn stat_failures_and_relative_gitdir_links_are_not_stranded_checkouts() {
        let stat_tmp = tempfile::TempDir::new().unwrap();
        let worktree = stat_tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        let loop_a = stat_tmp.path().join("loop_a");
        let loop_b = stat_tmp.path().join("loop_b");
        std::os::unix::fs::symlink(&loop_b, &loop_a).unwrap();
        std::os::unix::fs::symlink(&loop_a, &loop_b).unwrap();
        assert!(
            loop_a.try_exists().is_err(),
            "the fixture must actually produce a stat error"
        );
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", loop_a.display()),
        )
        .unwrap();

        assert!(
            !is_stranded_checkout(&worktree),
            "a stat failure must stay retriable, not become terminal"
        );

        std::fs::write(
            worktree.join(".git"),
            format!(
                "gitdir: {}\n",
                stat_tmp.path().join("definitely-gone").display()
            ),
        )
        .unwrap();
        assert!(is_stranded_checkout(&worktree));
        if !git_available() {
            return;
        }
        use crate::git::test_support::run_git;

        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&main_repo).unwrap();
        run_git(&main_repo, &["init", "-q", "-b", "main", "."]);
        run_git(&main_repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        run_git(
            &main_repo,
            &[
                "-c",
                "worktree.useRelativePaths=true",
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                worktree.to_str().unwrap(),
            ],
        );

        let link = std::fs::read_to_string(worktree.join(".git")).unwrap();
        let target = link.split_once("gitdir:").unwrap().1.trim().to_string();
        if Path::new(&target).is_absolute() {
            return;
        }
        assert!(
            !is_stranded_checkout(&worktree),
            "a live checkout with a relative gitdir link must not read as stranded"
        );
        std::fs::remove_dir_all(worktree.join(&target)).unwrap();
        assert!(is_stranded_checkout(&worktree));
    }

    #[test]
    fn a_move_failure_over_a_live_checkout_stays_retriable() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let (_other, other) = real_worktree_instance();
        inst.worktree_info.as_mut().unwrap().main_repo_path =
            other.worktree_info.unwrap().main_repo_path;
        inst.trash();

        assert!(matches!(
            relocate_fixture(&mut inst),
            RelocateOutcome::Failed { .. }
        ));
        assert_eq!(plan_trashed_reconcile(&inst), ReconcilePlan::Relocate);
        assert!(!reconcile_fixture(&mut inst));
    }

    #[test]
    #[serial_test::serial]
    fn a_row_restored_after_the_scan_is_not_reserved() {
        if !git_available() {
            return;
        }
        let _guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let (_tmp, mut inst) = real_worktree_instance();
        inst.trash();
        let id = inst.id.clone();
        let snapshot = inst.clone();
        storage
            .update(|instances, _groups| {
                instances.push(inst);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            plan_trashed_reconcile(&snapshot),
            ReconcilePlan::Relocate,
            "the scan must see work to do, or the test proves nothing"
        );

        storage
            .update(|instances, _groups| {
                instances[0].untrash();
                Ok(())
            })
            .unwrap();

        assert!(
            reconcile_trashed_batch(&storage, std::slice::from_ref(&snapshot))
                .unwrap()
                .is_empty()
        );
        let stored = storage.load().unwrap().into_iter().next().unwrap();
        assert_eq!(stored.id, id);
        assert!(
            stored.lifecycle_reservation.is_none(),
            "a restored row must not be left carrying a Trash reservation"
        );
        assert_eq!(
            stored.lifecycle_generation, 0,
            "the restored row must not be reserved at all"
        );
        assert!(stored.pre_trash_project_path.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn profile_sweep_heals_every_row_that_needs_it_and_nothing_else() {
        if !git_available() {
            return;
        }
        let _guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut plain = Instance::new("plain", "/tmp/plain");
        plain.trash();
        storage
            .update(|instances, _groups| {
                instances.push(plain.clone());
                Ok(())
            })
            .unwrap();
        let mut keeps = Vec::new();
        let mut originals = Vec::new();
        for _ in 0..2 {
            let (tmp, mut inst) = real_worktree_instance();
            inst.trash();
            originals.push((inst.id.clone(), inst.project_path.clone()));
            keeps.push(tmp);
            storage
                .update(|instances, _groups| {
                    instances.push(inst.clone());
                    Ok(())
                })
                .unwrap();
        }

        let healed = reconcile_trashed_profile("default").unwrap();
        assert_eq!(healed.len(), 2);
        let stored = storage.load().unwrap();
        for (id, original) in &originals {
            let row = stored.iter().find(|row| &row.id == id).unwrap();
            let holding = trash_holding_path(Path::new(original), id).unwrap();
            assert_eq!(PathBuf::from(&row.project_path), holding);
            assert_eq!(
                row.pre_trash_project_path.as_deref(),
                Some(original.as_str())
            );
            assert!(row.lifecycle_reservation.is_none());
        }
        let generations = |rows: &[Instance]| -> Vec<(String, u64)> {
            rows.iter()
                .map(|row| (row.id.clone(), row.lifecycle_generation))
                .collect()
        };
        let plain_row = stored.iter().find(|row| row.id == plain.id).unwrap();
        assert_eq!(
            (
                plain_row.lifecycle_generation,
                plain_row.lifecycle_reservation.is_none()
            ),
            (0, true),
            "a row needing nothing must not be reserved"
        );

        assert!(
            reconcile_trashed_profile("default").unwrap().is_empty(),
            "the sweep is idempotent"
        );
        assert_eq!(
            generations(&storage.load().unwrap()),
            generations(&stored),
            "a consistent profile is left untouched"
        );
    }

    /// A markerless row whose pointer was lost is healed to the holding path, whether or not
    /// the original path was recreated; one already pointing at holding is left alone.
    #[test]
    fn reconcile_heals_a_markerless_pointer_to_holding_only_when_it_is_lost() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        // (pointer left at the original path, original recreated, healed)
        for (lost, recreated, healed) in [
            (false, false, false),
            (true, false, true),
            (true, true, true),
        ] {
            let (_tmp, mut inst) = real_worktree_instance();
            let original = inst.project_path.clone();
            inst.trash();
            assert!(matches!(
                relocate_fixture(&mut inst),
                RelocateOutcome::Relocated { .. }
            ));
            let holding = inst.project_path.clone();
            if lost {
                inst.project_path = original.clone();
            }
            inst.pre_trash_project_path = None;
            if recreated {
                std::fs::create_dir_all(&original).unwrap();
            }

            let case = format!("lost={lost} recreated={recreated}");
            assert_eq!(reconcile_fixture(&mut inst), healed, "{case}");
            assert_eq!(inst.project_path, holding, "{case}");
            assert_eq!(
                inst.pre_trash_project_path.as_deref(),
                healed.then_some(original.as_str()),
                "{case}"
            );
            if !healed {
                assert!(!PathBuf::from(&holding).join(".aoe-trash").exists());
            }
        }
    }
    #[test]
    fn purge_removes_relocated_worktree() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        inst.trash();
        assert!(matches!(
            relocate_fixture(&mut inst),
            RelocateOutcome::Relocated { .. }
        ));
        let holding = PathBuf::from(&inst.project_path);
        assert!(holding.exists());

        let result = crate::session::deletion::perform_deletion(
            &crate::session::deletion::DeletionRequest {
                session_id: inst.id.clone(),
                instance: inst.clone(),
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
            },
        );
        assert!(result.success, "purge failed: {:?}", result.errors);
        assert!(
            !holding.exists(),
            "relocated worktree should be gone after purge"
        );
    }

    // Regression: a trashed worktree is relocated + re-locked, then its holding checkout is cleared
    // out of band (a manual `.aoe-trash` cleanup, a partial prior delete) AND the session's stored
    // `project_path` has diverged from git's registered path (a reconcile heal-back / lost
    // persist).
    #[test]
    fn purge_recovers_when_project_path_diverged_and_locked_entry_survives() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let _storage = crate::session::Storage::new_unwatched("default").unwrap();
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let branch = inst.worktree_info.as_ref().unwrap().branch.clone();
        let main_repo = PathBuf::from(&inst.worktree_info.as_ref().unwrap().main_repo_path);
        let original = inst.project_path.clone();
        inst.trash();
        assert!(matches!(
            relocate_fixture(&mut inst),
            RelocateOutcome::Relocated { .. }
        ));
        let holding = PathBuf::from(&inst.project_path);
        assert!(holding.exists());

        inst.project_path = original;
        std::fs::remove_dir_all(&holding).unwrap();
        let git = GitWorktree::new(main_repo.clone()).unwrap();
        git.prune_worktrees().unwrap();
        assert!(
            git.branch_exists(&branch).unwrap(),
            "precondition: branch still held by the surviving locked entry"
        );

        let result = crate::session::deletion::perform_deletion(
            &crate::session::deletion::DeletionRequest {
                session_id: inst.id.clone(),
                instance: inst.clone(),
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: true,
                keep_scratch: false,
            },
        );
        assert!(
            result.success,
            "purge must recover from the stranded locked entry: {:?}",
            result.errors
        );
        assert!(
            !git.branch_exists(&branch).unwrap(),
            "branch must be deleted once the orphan entry is reaped"
        );
    }

    #[test]
    fn trash_revalidates_recreated_owner_and_adoption_after_shared_phase() {
        for change in ["identity", "generation"] {
            let _app = crate::session::test_support::isolate_app_dir();
            let storage = crate::session::Storage::new_unwatched("default").unwrap();
            let (_tmp, instance) = real_worktree_instance();
            let original = PathBuf::from(&instance.project_path);
            let (baseline, generation) = unbound_trash_fixture(&storage, instance);
            storage
                .update(|rows, _| {
                    if change == "identity" {
                        rows[0].created_at += chrono::Duration::seconds(1);
                    } else {
                        rows[0].release_lifecycle_reservation_if_owned(
                            LifecycleOperation::Trash,
                            generation,
                        );
                        rows[0].try_acquire_lifecycle_reservation(
                            LifecycleOperation::Restore,
                            Instance::LIFECYCLE_RESERVATION_TTL,
                            Utc::now(),
                        )?;
                    }
                    Ok(())
                })
                .unwrap();
            let before = std::fs::read(storage.sessions_path()).unwrap();
            let current = storage.load().unwrap().remove(0);
            assert!(prepare_trash(&storage, &baseline, generation).is_err());
            assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
            assert!(original.join(".git").exists());
            assert!(!trash_holding_path(&original, &baseline.id)
                .unwrap()
                .exists());
            let durable = storage.load().unwrap().remove(0);
            assert_eq!(durable.created_at, current.created_at);
            assert_eq!(durable.lifecycle_generation, current.lifecycle_generation);
            assert_eq!(durable.lifecycle_reservation, current.lifecycle_reservation);
            if change == "generation" {
                assert!(durable
                    .lifecycle_reservation_is_owned(LifecycleOperation::Restore, generation + 1));
            }
        }

        for change in ["identity", "generation", "adoption", "profile"] {
            let _app = crate::session::test_support::isolate_app_dir();
            let storage = crate::session::Storage::new_unwatched("default").unwrap();
            let peer = crate::session::Storage::new_unwatched("peer").unwrap();
            let (_tmp, instance) = real_worktree_instance();
            let original = PathBuf::from(&instance.project_path);
            let request = reserved_request(&storage, instance);
            let holding = trash_holding_path(&original, &request.session_id).unwrap();
            let result = trash_transition(&request, |reserved| {
                assert!(original.join(".git").exists());
                assert!(!holding.exists());
                let before = std::fs::read(storage.sessions_path()).unwrap();
                let peer_before = std::fs::read(peer.sessions_path()).ok();
                assert!(reserved
                    .lifecycle_reservation
                    .as_ref()
                    .unwrap()
                    .resource_intent
                    .is_some());
                let error = match change {
                    "identity" | "generation" => storage.update(|rows, _| {
                        if change == "identity" {
                            rows[0].created_at += chrono::Duration::seconds(1);
                        } else {
                            rows[0].release_lifecycle_reservation_if_owned(
                                LifecycleOperation::Trash,
                                request.generation,
                            );
                            rows[0].try_acquire_lifecycle_reservation(
                                LifecycleOperation::Restore,
                                Instance::LIFECYCLE_RESERVATION_TTL,
                                Utc::now(),
                            )?;
                        }
                        Ok(())
                    }),
                    "adoption" => peer.update(|rows, _| {
                        rows.push(Instance::new("adopter", original.to_str().unwrap()));
                        Ok(())
                    }),
                    "profile" => crate::session::rename_profile("default", "renamed"),
                    _ => unreachable!(),
                }
                .unwrap_err();
                assert!(error.to_string().contains("busy"), "{change}: {error:#}");
                if change == "profile" {
                    let error = crate::session::delete_profile("default").unwrap_err();
                    assert!(error.to_string().contains("busy"));
                    assert!(crate::session::get_profile_dir_path("default")
                        .unwrap()
                        .is_dir());
                    assert!(!crate::session::get_profile_dir_path("renamed")
                        .unwrap()
                        .exists());
                }
                assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
                assert_eq!(std::fs::read(peer.sessions_path()).ok(), peer_before);
                let durable = storage.load().unwrap().remove(0);
                assert_eq!(durable.created_at, reserved.created_at);
                assert_eq!(durable.lifecycle_generation, request.generation);
                assert_eq!(
                    durable.lifecycle_reservation,
                    reserved.lifecycle_reservation
                );
                assert_eq!(durable.project_path, reserved.project_path);
                peer.update(|rows, _| {
                    rows.push(Instance::new("metadata writer", "/unrelated"));
                    Ok(())
                })
                .unwrap();
            });
            assert!(matches!(result, Ok(RelocateOutcome::Relocated { .. })));
            assert!(!original.exists());
            assert!(holding.join(".git").exists());
            let durable = storage.load().unwrap().remove(0);
            assert_eq!(durable.created_at, request.instance.created_at);
            assert_eq!(durable.lifecycle_generation, request.generation);
            assert_eq!(durable.project_path, holding.to_string_lossy());
            assert_eq!(durable.pre_trash_project_path.as_deref(), original.to_str());
            assert!(durable.lifecycle_reservation.is_none());
            let peers = peer.load().unwrap();
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].project_path, "/unrelated");
        }

        let _app = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let (_tmp, instance) = real_worktree_instance();
        let original = PathBuf::from(&instance.project_path);
        let retired = original.with_extension("retired");
        let request = reserved_request(&storage, instance);
        let holding = trash_holding_path(&original, &request.session_id).unwrap();
        let result = trash_transition(&request, |_| {
            std::fs::rename(&original, &retired).unwrap();
            std::fs::create_dir(&original).unwrap();
            std::fs::write(original.join("sentinel"), "replacement").unwrap();
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(original.join("sentinel")).unwrap(),
            "replacement"
        );
        assert!(retired.join(".git").exists());
        assert!(!holding.exists());
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.project_path, request.instance.project_path);
        assert_eq!(durable.created_at, request.instance.created_at);
        assert_eq!(durable.lifecycle_generation, request.generation);
    }

    #[test]
    fn trash_teardown_releases_identity_and_lifecycle_for_an_unrelated_identity_writer() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let peer = crate::session::Storage::new_unwatched("peer").unwrap();
        let (_tmp, instance) = real_worktree_instance();
        let request = reserved_request(&storage, instance);
        let (done, completed) = std::sync::mpsc::channel();
        let id = request.session_id.clone();
        let source = storage.clone();
        let mut writer = None;
        let mut progressed = false;
        let outcome = trash_transition(&request, |_| {
            writer = Some(std::thread::spawn(move || {
                let root = crate::session::storage::acquire_ownership_read().unwrap();
                let _identity = acquire_session_identity_lock_with_ownership(&root).unwrap();
                let _lifecycle = source
                    .acquire_instance_lifecycle_lock_with_ownership(&root, &id)
                    .unwrap();
                assert!(source
                    .load()
                    .unwrap()
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .has_fresh_lifecycle_reservation(Utc::now()));
                peer.update_with_ownership(&root, |rows, _| {
                    rows.push(Instance::new("unrelated identity", "/unrelated"));
                    Ok(())
                })
                .unwrap();
                done.send(()).unwrap();
            }));
            progressed = completed
                .recv_timeout(std::time::Duration::from_millis(500))
                .is_ok();
        });
        writer.unwrap().join().unwrap();
        assert!(
            progressed,
            "identity/lifecycle writer remained blocked throughout teardown"
        );
        assert!(matches!(outcome, Ok(RelocateOutcome::Relocated { .. })));
    }
    fn unbound_trash_fixture(
        storage: &crate::session::Storage,
        mut instance: Instance,
    ) -> (Instance, u64) {
        instance.source_profile = storage.profile().to_string();
        instance.trash();
        let generation = instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Trash,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )
            .unwrap();
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        (instance, generation)
    }

    fn reserved_request(storage: &crate::session::Storage, instance: Instance) -> TrashRequest {
        let (instance, generation) = unbound_trash_fixture(storage, instance);
        prepare_trash(storage, &instance, generation).unwrap()
    }

    #[test]
    fn trash_preserves_peer_claim_on_future_holding_path() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let peer = crate::session::Storage::new_unwatched("peer").unwrap();
        let (_tmp, instance) = real_worktree_instance();
        let original = PathBuf::from(&instance.project_path);
        let holding = trash_holding_path(&original, &instance.id).unwrap();
        let mut peer_instance = instance.clone();
        peer_instance.project_path = holding.to_string_lossy().into_owned();
        peer.update(|instances, _groups| {
            instances.push(peer_instance.clone());
            Ok(())
        })
        .unwrap();
        let request = reserved_request(&storage, instance);
        let result = perform_trash(&request);
        assert!(result.relocation.is_none());
        assert!(result
            .relocate_warning
            .as_deref()
            .unwrap()
            .contains("shared or unknown"));
        assert!(original.join(".git").exists());
        assert!(!holding.exists());
        assert_eq!(
            peer.load().unwrap()[0].project_path,
            peer_instance.project_path
        );
        assert!(reconcile_trashed_storage(&storage).unwrap().is_empty());
        assert!(original.join(".git").exists());
    }

    #[test]
    fn trash_and_reconcile_preserve_same_id_peer_checkout() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let peer = crate::session::Storage::new_unwatched("peer").unwrap();
        let (_tmp, instance) = real_worktree_instance();
        let original = PathBuf::from(&instance.project_path);
        peer.update(|instances, _groups| {
            instances.push(instance.clone());
            Ok(())
        })
        .unwrap();
        let peer_before = std::fs::read(peer.sessions_path()).unwrap();
        let request = reserved_request(&storage, instance);
        let result = perform_trash(&request);
        assert!(result.relocation.is_none());
        assert!(result.relocate_warning.is_some());
        assert!(original.join(".git").exists());
        let holding = trash_holding_path(&original, &request.session_id).unwrap();
        assert!(!holding.exists());
        let before = std::fs::read(storage.sessions_path()).unwrap();
        assert!(reconcile_trashed_profile("default").unwrap().is_empty());
        let mut durable = storage.load().unwrap().remove(0);
        let generation = durable.lifecycle_generation;
        let reservation = durable.lifecycle_reservation.clone();
        assert!(reconcile_trashed_transition(&storage, &mut durable).is_err());
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        assert_eq!(std::fs::read(peer.sessions_path()).unwrap(), peer_before);
        assert_eq!(durable.lifecycle_generation, generation);
        assert_eq!(durable.lifecycle_reservation, reservation);
        assert!(durable.pre_trash_project_path.is_none());
        assert_eq!(durable.project_path, request.instance.project_path);
        assert_eq!(
            peer.load().unwrap()[0].project_path,
            request.instance.project_path
        );
        assert!(original.join(".git").exists());
        assert!(!holding.exists());
    }

    #[test]
    fn trash_relocation_refuses_unknown_peer_inventory() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let peer = crate::session::Storage::new_unwatched("peer").unwrap();
        let (_tmp, instance) = real_worktree_instance();
        let original = PathBuf::from(&instance.project_path);
        let request = reserved_request(&storage, instance);
        let before = std::fs::read(storage.sessions_path()).unwrap();
        std::fs::write(peer.sessions_path(), "[null]").unwrap();
        let result = trash_transition(&request, |_| {
            panic!("unreadable peers must refuse teardown")
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        assert_eq!(std::fs::read(peer.sessions_path()).unwrap(), b"[null]");
        assert!(original.join(".git").exists());
        let holding = trash_holding_path(&original, &request.session_id).unwrap();
        assert!(!holding.exists());
        assert!(reconcile_trashed_profile("default").unwrap().is_empty());
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        assert_eq!(std::fs::read(peer.sessions_path()).unwrap(), b"[null]");
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.lifecycle_generation, request.generation);
        assert_eq!(
            durable.lifecycle_reservation,
            request.instance.lifecycle_reservation
        );
        assert!(crate::session::move_journal::resource_inventory()
            .unwrap()
            .iter()
            .any(|(_, entry)| entry.ids.contains(&request.session_id)
                && matches!(
                    &entry.authority,
                    crate::session::move_journal::MoveAuthority::Resource { .. }
                )));
        assert!(original.join(".git").exists());
        assert!(!holding.exists());
    }

    #[test]
    fn restore_preserves_peer_claims_at_source_and_destination() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let peer = crate::session::Storage::new_unwatched("peer").unwrap();
        for claim_destination in [false, true] {
            let (_tmp, mut instance) = real_worktree_instance();
            let original = PathBuf::from(&instance.project_path);
            instance.trash();
            assert!(matches!(
                relocate_fixture(&mut instance),
                RelocateOutcome::Relocated { .. }
            ));
            let holding = PathBuf::from(&instance.project_path);
            storage
                .update(|instances, _groups| {
                    *instances = vec![instance.clone()];
                    Ok(())
                })
                .unwrap();
            let mut peer_instance = instance.clone();
            peer_instance.untrash();
            peer_instance.pre_trash_project_path = None;
            peer_instance.project_path = if claim_destination {
                original.to_string_lossy().into_owned()
            } else {
                instance.project_path.clone()
            };
            peer.update(|instances, _groups| {
                *instances = vec![peer_instance.clone()];
                Ok(())
            })
            .unwrap();
            let outcome = restore_fixture(&mut instance);
            assert!(outcome.is_err(), "{outcome:?}");
            assert!(holding.join(".git").exists());
            assert!(!original.exists());
            assert_eq!(
                peer.load().unwrap()[0].project_path,
                peer_instance.project_path
            );
            peer.update(|instances, _groups| {
                instances.clear();
                Ok(())
            })
            .unwrap();
            storage
                .update(|instances, _groups| {
                    instances.clear();
                    Ok(())
                })
                .unwrap();
        }
    }

    #[test]
    fn queued_trash_cannot_target_replacement_profile() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let (_tmp, instance) = real_worktree_instance();
        let original = PathBuf::from(&instance.project_path);
        let request = reserved_request(&storage, instance);
        let holding = trash_holding_path(&original, &request.session_id).unwrap();
        let profile_dir = crate::session::get_profile_dir_path("default").unwrap();
        let before = std::fs::read(storage.sessions_path()).unwrap();
        let unrelated = crate::session::Storage::new_unwatched("unrelated").unwrap();
        assert!(unrelated.load().unwrap().is_empty());
        let rename = crate::session::rename_profile("default", "renamed").unwrap_err();
        assert!(rename.to_string().contains("busy"));
        let delete = crate::session::delete_profile("default").unwrap_err();
        assert!(delete.to_string().contains("busy"));
        assert!(profile_dir.is_dir());
        assert!(!crate::session::get_profile_dir_path("renamed")
            .unwrap()
            .exists());
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.created_at, request.instance.created_at);
        assert_eq!(durable.lifecycle_generation, request.generation);
        assert_eq!(
            durable.lifecycle_reservation,
            request.instance.lifecycle_reservation
        );
        assert!(original.join(".git").exists());
        assert!(!holding.exists());
        let result = perform_trash(&request);
        assert!(result.relocation.is_some());
        assert!(result.relocate_warning.is_none());
        assert!(!original.exists());
        assert!(holding.join(".git").exists());
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.created_at, request.instance.created_at);
        assert_eq!(durable.lifecycle_generation, request.generation);
        assert_eq!(durable.project_path, holding.to_string_lossy());
        assert_eq!(durable.pre_trash_project_path.as_deref(), original.to_str());
        assert!(durable.lifecycle_reservation.is_none());
        assert!(unrelated.load().unwrap().is_empty());
    }

    #[test]
    fn queued_trash_revalidates_path_even_when_generation_is_unchanged() {
        {
            let _app = crate::session::test_support::isolate_app_dir();
            let storage = crate::session::Storage::new_unwatched("default").unwrap();
            let (_tmp, instance) = real_worktree_instance();
            let original = PathBuf::from(&instance.project_path);
            let (_other, other) = real_worktree_instance();
            let (baseline, generation) = unbound_trash_fixture(&storage, instance);
            storage
                .update(|rows, _| {
                    rows[0].project_path.clone_from(&other.project_path);
                    Ok(())
                })
                .unwrap();
            assert!(prepare_trash(&storage, &baseline, generation).is_err());
            let durable = storage.load().unwrap().remove(0);
            assert_eq!(durable.project_path, other.project_path);
            assert_eq!(durable.created_at, baseline.created_at);
            assert_eq!(durable.lifecycle_generation, generation);
            assert!(durable.lifecycle_reservation.is_none());
            assert!(original.join(".git").exists());
            assert!(Path::new(&other.project_path).join(".git").exists());
            assert!(!trash_holding_path(&original, &baseline.id)
                .unwrap()
                .exists());
        }

        let _app_guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let (_tmp, instance) = real_worktree_instance();
        let original = PathBuf::from(&instance.project_path);
        let (_other, other) = real_worktree_instance();
        let request = reserved_request(&storage, instance);
        let before = std::fs::read(storage.sessions_path()).unwrap();
        let error = storage
            .update(|instances, _groups| {
                instances[0].project_path.clone_from(&other.project_path);
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("busy"));
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.project_path, request.instance.project_path);
        assert_eq!(durable.lifecycle_generation, request.generation);
        assert_eq!(
            durable.lifecycle_reservation,
            request.instance.lifecycle_reservation
        );
        let result = perform_trash(&request);
        assert!(result.relocation.is_some());
        assert!(result.relocate_warning.is_none());
        let holding = trash_holding_path(&original, &request.session_id).unwrap();
        assert!(!original.exists());
        assert!(holding.join(".git").exists());
        assert!(Path::new(&other.project_path).join(".git").exists());
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(durable.project_path, holding.to_string_lossy());
        assert_eq!(durable.pre_trash_project_path.as_deref(), original.to_str());
        assert_eq!(durable.created_at, request.instance.created_at);
        assert_eq!(durable.lifecycle_generation, request.generation);
        assert!(durable.lifecycle_reservation.is_none());
    }

    #[test]
    #[serial_test::serial]
    fn pointer_repair_preserves_bare_directory_and_is_idempotent() {
        let _app = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let storage = crate::session::Storage::new_unwatched("owner").unwrap();
        let original = project.path().join("topic");
        let mut row = Instance::new("trashed", original.to_str().unwrap());
        row.source_profile = storage.profile().to_owned();
        row.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "topic".into(),
            main_repo_path: project.path().to_string_lossy().into_owned(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        row.trash();
        let baseline = row.clone();
        let holding = trash_holding_path(&original, &row.id).unwrap();
        std::fs::create_dir_all(&holding).unwrap();
        std::fs::write(holding.join("original"), b"keep").unwrap();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        assert!(reconcile_trashed_transition(&storage, &mut row).unwrap());
        let durable = storage.load().unwrap().remove(0);
        assert_eq!(row.project_path, holding.to_string_lossy());
        assert_eq!(durable.project_path, row.project_path);
        assert_eq!(row.pre_trash_project_path.as_deref(), original.to_str());
        assert_eq!(durable.pre_trash_project_path, row.pre_trash_project_path);
        assert_eq!(durable.id, baseline.id);
        assert_eq!(durable.created_at, baseline.created_at);
        assert_eq!(durable.trashed_at, baseline.trashed_at);
        assert_eq!(durable.trash_lifecycle_id, baseline.trash_lifecycle_id);
        assert_eq!(
            durable.lifecycle_generation,
            baseline.lifecycle_generation + 1
        );
        assert_eq!(row.lifecycle_generation, durable.lifecycle_generation);
        assert!(row.lifecycle_reservation.is_none());
        assert!(durable.lifecycle_reservation.is_none());
        assert!(!original.exists());
        assert_eq!(std::fs::read(holding.join("original")).unwrap(), b"keep");
        let before = std::fs::read(storage.sessions_path()).unwrap();
        assert!(!reconcile_trashed_transition(&storage, &mut row).unwrap());
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
        assert_eq!(row.lifecycle_generation, baseline.lifecycle_generation + 1);
        assert_eq!(std::fs::read(holding.join("original")).unwrap(), b"keep");
    }
}
