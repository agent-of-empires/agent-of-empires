//! Trash retention helpers.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::git::GitWorktree;
use crate::session::worktree_edit::{
    discard_sandbox_container_after_move, ensure_sandbox_container_released,
};
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

/// Result of attempting to move a worktree back out of the holding area.
#[derive(Debug)]
pub enum RestoreOutcome {
    /// The worktree was moved back to its pre-trash location.
    Restored { from: PathBuf, to: PathBuf },
    /// No relocation had happened (plain/non-managed session, or a row trashed before relocation
    /// existed), so there is nothing to move.
    NoChange,
    /// The worktree could not be moved back (its original path is now occupied by something else,
    /// or git refused).
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

/// Move a freshly-trashed session's managed worktree into the holding area and repoint
/// `project_path`, capturing the original location in `pre_trash_project_path`.
pub fn relocate_worktree_to_trash(inst: &mut Instance) -> RelocateOutcome {
    if !inst.is_trashed() || !is_managed_single_worktree(inst) {
        return RelocateOutcome::Skipped;
    }
    if inst.pre_trash_project_path.is_some() {
        return RelocateOutcome::Skipped;
    }
    // A default branch's checkout is infrastructure: sibling tooling expects `<project>/main` to
    // stay where it is, so moving it into the holding area breaks that layout even though the move
    // is reversible.
    if is_protected_default_branch(inst) {
        tracing::info!(
            target: "session.trash",
            session = %inst.id,
            path = %inst.project_path,
            "leaving a default branch's checkout in place instead of relocating it"
        );
        return RelocateOutcome::Skipped;
    }

    let current = PathBuf::from(&inst.project_path);
    let Some(target) = trash_holding_path(&current, &inst.id) else {
        return RelocateOutcome::Failed {
            reason: format!("worktree path {} has no parent dir", current.display()),
        };
    };
    if target.exists() {
        return RelocateOutcome::Failed {
            reason: format!("trash holding path {} already exists", target.display()),
        };
    }
    if ensure_sandbox_container_released(&inst.id, is_sandboxed(inst)) {
        return RelocateOutcome::Failed {
            reason: "sandbox container still holds the worktree; stop the session first"
                .to_string(),
        };
    }

    let main_repo = inst
        .worktree_info
        .as_ref()
        .map(|w| w.main_repo_path.clone())
        .unwrap_or_default();
    let git = match GitWorktree::new(PathBuf::from(&main_repo)) {
        Ok(g) => g,
        Err(e) => {
            return RelocateOutcome::Failed {
                reason: format!("open main repo {main_repo}: {e}"),
            }
        }
    };
    if let Some(parent) = target.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return RelocateOutcome::Failed {
                reason: format!("create {}: {e}", parent.display()),
            };
        }
    }
    if let Err(e) = git.move_worktree(&current, &target) {
        return RelocateOutcome::Failed {
            reason: format!("git worktree move: {e}"),
        };
    }

    discard_sandbox_container_after_move(&inst.id, is_sandboxed(inst));
    inst.pre_trash_project_path = Some(inst.project_path.clone());
    inst.project_path = target.to_string_lossy().into_owned();
    tracing::info!(
        target: "session.trash",
        session = %inst.id,
        from = %current.display(),
        to = %target.display(),
        "relocated trashed worktree into holding area"
    );
    RelocateOutcome::Relocated {
        from: current,
        to: target,
    }
}

/// Bring a freshly-trashed session's sandbox container down, then relocate its worktree into the
/// holding area.
pub fn prepare_trashed_worktree(inst: &mut Instance) -> RelocateOutcome {
    if let Err(error) =
        crate::session::worktree_edit::stop_sandbox_container(&inst.id, is_sandboxed(inst))
    {
        tracing::warn!(
            target: "session.trash",
            session = %inst.id,
            "stopping sandbox container before trash relocation failed: {error}"
        );
    }
    relocate_worktree_to_trash(inst)
}

#[cfg(test)]
fn prepare_trashed_worktree_with(
    inst: &mut Instance,
    stop_container: impl FnOnce(&str, bool),
) -> RelocateOutcome {
    stop_container(&inst.id, is_sandboxed(inst));
    relocate_worktree_to_trash(inst)
}

pub struct TrashRequest {
    pub session_id: String,
    pub instance: Instance,
    pub generation: u64,
}

#[derive(Debug, Clone)]
pub struct TrashRelocation {
    pub new_project_path: String,
    pub pre_trash_project_path: Option<String>,
}

#[derive(Debug)]
pub struct TrashResult {
    pub session_id: String,
    pub relocation: Option<TrashRelocation>,
    pub relocate_warning: Option<String>,
}

/// Execute and commit a TUI trash transition under one per-instance flock.
pub fn perform_trash(request: &TrashRequest) -> TrashResult {
    let failed = |reason: String| TrashResult {
        session_id: request.session_id.clone(),
        relocation: None,
        relocate_warning: Some(reason),
    };
    let storage = match crate::session::Storage::open_unwatched(&request.instance.source_profile) {
        Ok(storage) => storage,
        Err(error) => return failed(format!("could not open lifecycle storage: {error}")),
    };
    let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(&request.session_id) {
        Ok(lock) => lock,
        Err(error) => {
            return failed(format!("could not acquire lifecycle lock: {error}"));
        }
    };
    let owns = storage
        .update(|instances, _groups| {
            Ok(instances
                .iter()
                .find(|instance| instance.id == request.session_id)
                .is_some_and(|instance| {
                    instance.lifecycle_reservation_is_owned(
                        crate::session::LifecycleOperation::Trash,
                        request.generation,
                    )
                }))
        })
        .unwrap_or(false);
    if !owns {
        return failed("trash lifecycle reservation was superseded before teardown".to_string());
    }

    let mut inst = request.instance.clone();
    inst.kill_all_tmux_sessions_locked();
    let outcome = prepare_trashed_worktree(&mut inst);
    let relocation = match &outcome {
        RelocateOutcome::Relocated { .. } => Some(TrashRelocation {
            new_project_path: inst.project_path.clone(),
            pre_trash_project_path: inst.pre_trash_project_path.clone(),
        }),
        RelocateOutcome::Skipped | RelocateOutcome::Failed { .. } => None,
    };
    let commit = storage.update(|instances, _groups| {
        if let Some(relocation) = &relocation {
            let _ = crate::session::claim::commit_trash_relocation(
                instances,
                &request.session_id,
                request.generation,
                relocation,
            );
        } else {
            crate::session::claim::release_trash_reservation(
                instances,
                &request.session_id,
                request.generation,
            );
        }
        Ok(())
    });
    if let Err(error) = commit {
        return failed(format!("could not commit trash transition: {error}"));
    }

    TrashResult {
        session_id: request.session_id.clone(),
        relocation,
        relocate_warning: match outcome {
            RelocateOutcome::Failed { reason } => Some(reason),
            RelocateOutcome::Relocated { .. } | RelocateOutcome::Skipped => None,
        },
    }
}

/// Undo a trash relocation that landed after the row had already been restored: the worker's
/// still-trashed re-check and the `git worktree move` are not atomic, so a restore squeezing
/// between them leaves a live, untrashed row pointing at its original path while the worktree sits
/// in the holding area.
pub fn undo_raced_relocation(live: &Instance, relocation: &TrashRelocation) -> RestoreOutcome {
    let Some(original) = relocation.pre_trash_project_path.clone() else {
        return RestoreOutcome::NoChange;
    };
    let mut tmp = live.clone();
    tmp.project_path = relocation.new_project_path.clone();
    tmp.pre_trash_project_path = Some(original);
    restore_worktree_location(&mut tmp)
}

/// Move a trashed session's worktree back to its pre-trash location and clear
/// `pre_trash_project_path`.
pub fn restore_worktree_location(inst: &mut Instance) -> RestoreOutcome {
    let Some(original) = inst.pre_trash_project_path.clone() else {
        return RestoreOutcome::NoChange;
    };
    let original = PathBuf::from(original);
    let current = PathBuf::from(&inst.project_path);
    if current == original {
        // Never actually moved (relocation failed at trash time), or already
        // back. Drop the marker so the row looks un-relocated again.
        inst.pre_trash_project_path = None;
        return RestoreOutcome::NoChange;
    }
    if ensure_sandbox_container_released(&inst.id, is_sandboxed(inst)) {
        return RestoreOutcome::Failed {
            reason: "sandbox container still holds the worktree; stop the session first"
                .to_string(),
        };
    }
    if original.exists() {
        return RestoreOutcome::Failed {
            reason: format!(
                "original worktree path {} is occupied; move or remove it first",
                original.display()
            ),
        };
    }
    let main_repo = inst
        .worktree_info
        .as_ref()
        .map(|w| w.main_repo_path.clone())
        .unwrap_or_default();
    let git = match GitWorktree::new(PathBuf::from(&main_repo)) {
        Ok(g) => g,
        Err(e) => {
            return RestoreOutcome::Failed {
                reason: format!("open main repo {main_repo}: {e}"),
            }
        }
    };
    if let Err(e) = git.move_worktree(&current, &original) {
        return RestoreOutcome::Failed {
            reason: format!("git worktree move: {e}"),
        };
    }
    discard_sandbox_container_after_move(&inst.id, is_sandboxed(inst));
    inst.project_path = original.to_string_lossy().into_owned();
    inst.pre_trash_project_path = None;
    tracing::info!(
        target: "session.trash",
        session = %inst.id,
        from = %current.display(),
        to = %original.display(),
        "restored worktree from holding area"
    );
    RestoreOutcome::Restored {
        from: current,
        to: original,
    }
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

/// Load-time reconciliation for a single trashed session.
pub fn reconcile_trashed_location(inst: &mut Instance) -> bool {
    match plan_trashed_reconcile(inst) {
        ReconcilePlan::Nothing => false,
        ReconcilePlan::RestoreDefaultBranch => match restore_worktree_location(inst) {
            RestoreOutcome::Restored { .. } => true,
            // The marker was set but nothing had actually moved, so restore
            // dropped it. That is still a mutation worth persisting.
            RestoreOutcome::NoChange => inst.pre_trash_project_path.is_none(),
            RestoreOutcome::Failed { reason } => {
                tracing::warn!(
                    target: "session.trash",
                    session = %inst.id,
                    "could not move a default branch's checkout back out of the holding area: {reason}"
                );
                false
            }
        },
        ReconcilePlan::Relocate => match relocate_worktree_to_trash(inst) {
            RelocateOutcome::Relocated { .. } => true,
            RelocateOutcome::Failed { reason } => {
                tracing::warn!(
                    target: "session.trash",
                    session = %inst.id,
                    "trash worktree reconcile relocation failed: {reason}"
                );
                false
            }
            RelocateOutcome::Skipped => false,
        },
        ReconcilePlan::PointAtHolding { holding, original } => {
            inst.project_path = holding.to_string_lossy().into_owned();
            inst.pre_trash_project_path = Some(original.to_string_lossy().into_owned());
            tracing::info!(
                target: "session.trash",
                session = %inst.id,
                to = %holding.display(),
                "reconciled trashed worktree pointer to holding area"
            );
            true
        }
        ReconcilePlan::PointAtOriginal(original) => {
            inst.project_path = original.to_string_lossy().into_owned();
            inst.pre_trash_project_path = None;
            tracing::info!(
                target: "session.trash",
                session = %inst.id,
                to = %original.display(),
                "reconciled trashed worktree pointer back to original (holding move never landed)"
            );
            true
        }
    }
}

/// Reconcile every trashed row in one profile, batched.
pub fn reconcile_trashed_profile(profile: &str) -> anyhow::Result<Vec<Instance>> {
    let storage = crate::session::Storage::open_unwatched(profile)?;
    let mut cache = ProtectedBranchCache::default();
    let mut candidates: Vec<Instance> = storage
        .load()?
        .into_iter()
        .filter(|inst| plan_trashed_reconcile_cached(inst, &mut cache) != ReconcilePlan::Nothing)
        .collect();
    candidates.sort_by(|a, b| a.id.cmp(&b.id));

    let mut healed = Vec::new();
    for batch in candidates.chunks(RECONCILE_BATCH) {
        // One batch's write failure must not abandon the rest of the profile: the pass this
        // replaced logged per row and carried on, and a batch that bails leaves its reservations to
        // expire on the TTL.
        match reconcile_trashed_batch(&storage, batch) {
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

/// Whether the durable row still matches the snapshot the plan was decided from.
fn plan_inputs_unchanged(snapshot: &Instance, durable: &Instance) -> bool {
    durable.is_trashed()
        && durable.project_path == snapshot.project_path
        && durable.pre_trash_project_path == snapshot.pre_trash_project_path
        && durable.worktree_info == snapshot.worktree_info
        && durable.scratch == snapshot.scratch
}

/// How many rows one batch reserves at once.
const RECONCILE_BATCH: usize = 8;

fn reconcile_trashed_batch(
    storage: &crate::session::Storage,
    batch: &[Instance],
) -> anyhow::Result<Vec<Instance>> {
    let now = Utc::now();
    let reserved = storage.update(|instances, _groups| {
        let mut reserved: Vec<(u64, Instance)> = Vec::new();
        for snapshot in batch {
            let Some(stored) = instances
                .iter_mut()
                .find(|candidate| candidate.id == snapshot.id)
            else {
                continue;
            };
            // Compare and set: the plan was decided from a snapshot taken without any lock, so a
            // peer can have restored, purged, or moved the row since.
            if !plan_inputs_unchanged(snapshot, stored) {
                tracing::debug!(
                    target: "session.trash",
                    session = %snapshot.id,
                    "trash reconciliation skipped: the row changed after it was scanned"
                );
                continue;
            }
            match stored.try_acquire_lifecycle_reservation(
                crate::session::LifecycleOperation::Trash,
                Instance::LIFECYCLE_RESERVATION_TTL,
                now,
            ) {
                Ok(generation) => reserved.push((generation, stored.clone())),
                Err(error) => tracing::debug!(
                    target: "session.trash",
                    session = %snapshot.id,
                    "trash reconciliation deferred: {error}"
                ),
            }
        }
        Ok(reserved)
    })?;

    // Each row takes its own lifecycle flock only across its own filesystem work.
    let reconciled: Vec<(u64, bool, Instance)> = reserved
        .into_iter()
        .map(|(generation, mut durable)| {
            let changed = match storage.acquire_instance_lifecycle_lock(&durable.id) {
                Ok(_lifecycle_lock) => reconcile_trashed_location(&mut durable),
                Err(error) => {
                    tracing::warn!(
                        target: "session.trash",
                        session = %durable.id,
                        "trash reconciliation skipped: could not acquire lifecycle lock: {error}"
                    );
                    false
                }
            };
            (generation, changed, durable)
        })
        .collect();
    if reconciled.is_empty() {
        return Ok(Vec::new());
    }

    storage.update(|instances, _groups| {
        let mut healed = Vec::new();
        for (generation, changed, durable) in &reconciled {
            if !changed {
                if let Some(stored) = instances
                    .iter_mut()
                    .find(|candidate| candidate.id == durable.id)
                {
                    stored.release_lifecycle_reservation_if_owned(
                        crate::session::LifecycleOperation::Trash,
                        *generation,
                    );
                }
                continue;
            }
            let relocation = TrashRelocation {
                new_project_path: durable.project_path.clone(),
                pre_trash_project_path: durable.pre_trash_project_path.clone(),
            };
            match crate::session::claim::commit_trash_relocation(
                instances,
                &durable.id,
                *generation,
                &relocation,
            ) {
                crate::session::claim::RelocationCommit::Persisted => healed.push(durable.clone()),
                outcome => tracing::warn!(
                    target: "session.trash",
                    session = %durable.id,
                    "trash reconciliation not committed: {outcome:?}"
                ),
            }
        }
        Ok(healed)
    })
}

/// Reconcile one trashed worktree as a serialized lifecycle transition.
pub fn reconcile_trashed_transition(inst: &mut Instance) -> anyhow::Result<bool> {
    // Decide from the caller's snapshot before paying for storage, the lifecycle flock, and two
    // write cycles.
    if plan_trashed_reconcile(inst) == ReconcilePlan::Nothing {
        return Ok(false);
    }
    let profile = inst.source_profile.clone();
    anyhow::ensure!(
        !profile.is_empty(),
        "session has no source profile; refusing trash reconciliation"
    );
    let storage = crate::session::Storage::open_unwatched(&profile)?;
    let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(&inst.id)?;
    let id = inst.id.clone();
    let (generation, mut durable) = storage.update(|instances, _groups| {
        let Some(stored) = instances.iter_mut().find(|candidate| candidate.id == id) else {
            anyhow::bail!("session disappeared before trash reconciliation");
        };
        let generation = stored.try_acquire_lifecycle_reservation(
            crate::session::LifecycleOperation::Trash,
            Instance::LIFECYCLE_RESERVATION_TTL,
            Utc::now(),
        )?;
        Ok((generation, stored.clone()))
    })?;

    let changed = reconcile_trashed_location(&mut durable);
    let relocation = TrashRelocation {
        new_project_path: durable.project_path.clone(),
        pre_trash_project_path: durable.pre_trash_project_path.clone(),
    };
    storage.update(|instances, _groups| {
        if changed {
            let commit = crate::session::claim::commit_trash_relocation(
                instances,
                &id,
                generation,
                &relocation,
            );
            anyhow::ensure!(
                commit == crate::session::claim::RelocationCommit::Persisted,
                "trash reconciliation reservation was superseded"
            );
        } else if let Some(stored) = instances.iter_mut().find(|candidate| candidate.id == id) {
            stored.release_lifecycle_reservation_if_owned(
                crate::session::LifecycleOperation::Trash,
                generation,
            );
        }
        Ok(())
    })?;
    durable.lifecycle_reservation = None;
    *inst = durable;
    Ok(changed)
}

/// True when a trashed session is past its retention window and should be auto-purged.
pub fn is_expired(instance: &Instance, retention_days: u32, now: DateTime<Utc>) -> bool {
    if retention_days == 0 {
        return false;
    }
    match instance.trashed_at {
        Some(trashed_at) => now >= trashed_at + chrono::Duration::days(retention_days as i64),
        None => false,
    }
}

/// Ids of every trashed session whose retention window has elapsed, in the order they appear in
/// `instances`.
pub fn expired_trashed_ids(
    instances: &[Instance],
    retention_days: u32,
    now: DateTime<Utc>,
) -> Vec<String> {
    instances
        .iter()
        .filter(|i| is_expired(i, retention_days, now))
        .map(|i| i.id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trashed_days_ago(days: i64) -> Instance {
        let mut inst = Instance::new("s", "/tmp/x");
        inst.trashed_at = Some(Utc::now() - chrono::Duration::days(days));
        inst
    }

    #[test]
    fn is_expired_cases() {
        let now = Utc::now();
        // (case, trashed days ago, retention days, expected)
        let cases = [
            ("retention 0 keeps forever", Some(9999), 0, false),
            ("never trashed", None, 30, false),
            ("at the retention window", Some(30), 30, true),
            ("one day inside the window", Some(29), 30, false),
        ];
        for (case, trashed_days, retention, expected) in cases {
            let mut inst = Instance::new("s", "/tmp/x");
            inst.trashed_at = trashed_days.map(|days| now - chrono::Duration::days(days));
            assert_eq!(is_expired(&inst, retention, now), expected, "{case}");
        }
        let fresh = trashed_days_ago(1);
        let old_a = trashed_days_ago(40);
        let live = Instance::new("s", "/tmp/x");
        let old_b = trashed_days_ago(31);
        let instances = vec![fresh, old_a.clone(), live, old_b.clone()];
        assert_eq!(
            expired_trashed_ids(&instances, 30, now),
            vec![old_a.id, old_b.id],
            "filters and preserves order"
        );
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
        inst.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "feature/relocate-me".to_string(),
            main_repo_path: main_repo.to_string_lossy().to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        (tmp, inst)
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
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = default_branch_worktree_instance();
        let original = inst.project_path.clone();
        inst.trash();
        assert_eq!(plan_trashed_reconcile(&inst), ReconcilePlan::Nothing);
        assert!(!reconcile_trashed_location(&mut inst));

        let out = relocate_worktree_to_trash(&mut inst);
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
            reconcile_trashed_location(&mut inst),
            "reconcile must move the checkout back and report the mutation"
        );
        assert_eq!(PathBuf::from(&inst.project_path), original);
        assert!(inst.pre_trash_project_path.is_none());
        assert!(original.exists());
        assert!(!holding.exists());

        assert!(
            !reconcile_trashed_location(&mut inst),
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
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let original = inst.project_path.clone();
        inst.trash();

        let out = relocate_worktree_to_trash(&mut inst);
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
            relocate_worktree_to_trash(&mut inst),
            RelocateOutcome::Skipped
        ));

        std::fs::create_dir_all(&original).unwrap();
        let occupied = restore_worktree_location(&mut inst);
        assert!(
            matches!(occupied, RestoreOutcome::Failed { .. }),
            "restore should refuse an occupied original, got {occupied:?}"
        );
        assert!(inst.pre_trash_project_path.is_some());
        assert_ne!(inst.project_path, original);
        std::fs::remove_dir(&original).unwrap();

        let back = restore_worktree_location(&mut inst);
        assert!(
            matches!(back, RestoreOutcome::Restored { .. }),
            "expected restore, got {back:?}"
        );
        assert_eq!(inst.project_path, original);
        assert!(inst.pre_trash_project_path.is_none());
        assert!(PathBuf::from(&original).exists());
    }

    #[test]
    fn reconcile_backfills_legacy_then_is_idempotent() {
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let original = inst.project_path.clone();
        inst.trash();
        assert!(inst.pre_trash_project_path.is_none());

        assert!(
            reconcile_trashed_location(&mut inst),
            "reconcile should relocate a legacy trashed worktree"
        );
        let holding = trash_holding_path(Path::new(&original), &inst.id).unwrap();
        assert_eq!(PathBuf::from(&inst.project_path), holding);
        assert_eq!(
            inst.pre_trash_project_path.as_deref(),
            Some(original.as_str())
        );
        assert!(!PathBuf::from(&original).exists());

        assert!(!reconcile_trashed_location(&mut inst));
    }

    #[test]
    fn reconcile_never_retries_a_checkout_the_repo_no_longer_registers() {
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
                !reconcile_trashed_location(&mut inst),
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
        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&main_repo).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main", "."],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            let out = std::process::Command::new("git")
                .args(&args)
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
        }
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "worktree.useRelativePaths=true",
                "worktree",
                "add",
                "-q",
                "-b",
                "feat",
                worktree.to_str().unwrap(),
            ])
            .current_dir(&main_repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git worktree add failed");

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
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let (_other, other) = real_worktree_instance();
        inst.worktree_info.as_mut().unwrap().main_repo_path =
            other.worktree_info.unwrap().main_repo_path;
        inst.trash();

        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
            RelocateOutcome::Failed { .. }
        ));
        assert_eq!(plan_trashed_reconcile(&inst), ReconcilePlan::Relocate);
        assert!(!reconcile_trashed_location(&mut inst));
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
                relocate_worktree_to_trash(&mut inst),
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
            assert_eq!(reconcile_trashed_location(&mut inst), healed, "{case}");
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
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        inst.trash();
        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
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
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let branch = inst.worktree_info.as_ref().unwrap().branch.clone();
        let main_repo = PathBuf::from(&inst.worktree_info.as_ref().unwrap().main_repo_path);
        let original = inst.project_path.clone();
        inst.trash();
        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
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

    // Regression (#the-d-key): trashing must run the sandbox container-stop step BEFORE relocating
    // the worktree.
    #[test]
    fn trash_prep_stops_container_before_relocating() {
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        inst.trash();
        let original = PathBuf::from(&inst.project_path);

        use std::cell::Cell;
        use std::rc::Rc;
        let stop_calls = Rc::new(Cell::new(0u32));
        let saw_sandbox_flag = Rc::new(Cell::new(true));
        let original_present_at_stop = Rc::new(Cell::new(false));

        let outcome = {
            let stop_calls = Rc::clone(&stop_calls);
            let saw_sandbox_flag = Rc::clone(&saw_sandbox_flag);
            let original_present_at_stop = Rc::clone(&original_present_at_stop);
            let original = original.clone();
            prepare_trashed_worktree_with(&mut inst, move |_id, is_sandboxed| {
                stop_calls.set(stop_calls.get() + 1);
                saw_sandbox_flag.set(is_sandboxed);
                original_present_at_stop.set(original.exists());
            })
        };

        assert_eq!(
            stop_calls.get(),
            1,
            "trash must run the container-stop step exactly once"
        );
        assert!(
            !saw_sandbox_flag.get(),
            "a non-sandbox session reports is_sandboxed=false to the stop step"
        );
        assert!(
            original_present_at_stop.get(),
            "the container stop must run BEFORE the worktree is moved"
        );
        assert!(
            matches!(outcome, RelocateOutcome::Relocated { .. }),
            "relocation still succeeds after the stop step: {outcome:?}"
        );
        let holding = trash_holding_path(&original, &inst.id).unwrap();
        assert_eq!(PathBuf::from(&inst.project_path), holding);
        assert!(holding.exists(), "worktree moved into the holding area");
        assert!(!original.exists(), "worktree left its original active path");
    }

    #[test]
    fn trash_prep_passes_sandbox_flag_to_container_stop() {
        let mut inst = Instance::new("sandboxed", "/tmp/sandboxed");
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "ubuntu:latest".to_string(),
            container_name: "aoe-sandbox-test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        inst.trash();

        use std::cell::Cell;
        use std::rc::Rc;
        let saw_sandbox_flag = Rc::new(Cell::new(false));
        let outcome = {
            let saw_sandbox_flag = Rc::clone(&saw_sandbox_flag);
            prepare_trashed_worktree_with(&mut inst, move |_id, is_sandboxed| {
                saw_sandbox_flag.set(is_sandboxed);
            })
        };
        assert!(
            saw_sandbox_flag.get(),
            "a sandboxed session must report is_sandboxed=true to the stop step"
        );
        assert!(
            matches!(outcome, RelocateOutcome::Skipped),
            "a plain session has no managed worktree to relocate: {outcome:?}"
        );
    }

    #[test]
    fn undo_raced_relocation_moves_worktree_back() {
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let original = inst.project_path.clone();
        inst.trash();
        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
            RelocateOutcome::Relocated { .. }
        ));
        let reloc = TrashRelocation {
            new_project_path: inst.project_path.clone(),
            pre_trash_project_path: inst.pre_trash_project_path.clone(),
        };

        let mut live = inst.clone();
        live.untrash();
        live.project_path = original.clone();
        live.pre_trash_project_path = None;

        let out = undo_raced_relocation(&live, &reloc);
        assert!(
            matches!(out, RestoreOutcome::Restored { .. }),
            "undo must move the worktree back, got {out:?}"
        );
        assert!(
            PathBuf::from(&original).exists(),
            "worktree must be back at the path the live row points at"
        );
        assert!(
            !PathBuf::from(&reloc.new_project_path).exists(),
            "holding area copy must be gone"
        );
    }
}
