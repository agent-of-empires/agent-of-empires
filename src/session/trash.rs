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
/// Caller holds workspace -> identity -> lifecycle locks and supplies the freshly durable row.
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

    if !inst.runner_journal.proves_quiescent() {
        return RelocateOutcome::Failed {
            reason: "durable runner history does not prove checkout quiescence".into(),
        };
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
/// Same authoritative-row, owned-reservation, and lock preconditions as relocation.
pub fn prepare_trashed_worktree(inst: &mut Instance) -> RelocateOutcome {
    if !inst.runner_journal.proves_quiescent() {
        return RelocateOutcome::Failed {
            reason: "durable runner history does not prove checkout quiescence".into(),
        };
    }
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
    if !inst.runner_journal.proves_quiescent() {
        return RelocateOutcome::Failed {
            reason: "durable runner history does not prove checkout quiescence".into(),
        };
    }
    stop_container(&inst.id, is_sandboxed(inst));
    relocate_worktree_to_trash(inst)
}

pub struct TrashRequest {
    pub storage: crate::session::Storage,
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
    pub authoritative: Option<Instance>,
}

/// Execute and commit a TUI trash transition under one per-instance flock.
pub fn perform_trash(request: &TrashRequest) -> TrashResult {
    let failed = |reason: String| TrashResult {
        session_id: request.session_id.clone(),
        relocation: None,
        relocate_warning: Some(reason),
        authoritative: None,
    };
    let _workspace_claim_lock = match crate::session::acquire_session_workspace_claim_lock() {
        Ok(lock) => lock,
        Err(error) => return failed(format!("could not acquire workspace claim lock: {error}")),
    };
    let _identity_lock = match crate::session::acquire_session_identity_lock() {
        Ok(lock) => lock,
        Err(error) => return failed(format!("could not acquire identity lock: {error}")),
    };
    let storage = &request.storage;
    if let Err(error) = storage.verify_profile_identity() {
        return failed(format!("trash profile owner changed: {error}"));
    }
    let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(&request.session_id) {
        Ok(lock) => lock,
        Err(error) => {
            return failed(format!("could not acquire lifecycle lock: {error}"));
        }
    };
    let snapshot = match storage.load().ok().and_then(|rows| {
        rows.into_iter().find(|row| {
            row.id == request.session_id
                && row.lifecycle_reservation_is_owned(
                    crate::session::LifecycleOperation::Trash,
                    request.generation,
                )
        })
    }) {
        Some(row) => row,
        None => return failed("trash lifecycle reservation was superseded before teardown".into()),
    };
    if snapshot.has_managed_worktree_or_workspace() {
        let mut candidate_paths = vec![PathBuf::from(&snapshot.project_path)];
        if let Some(workspace) = &snapshot.workspace_info {
            candidate_paths.push(PathBuf::from(&workspace.workspace_dir));
        }
        candidate_paths.extend(
            snapshot
                .all_repos()
                .iter()
                .map(|repo| PathBuf::from(&repo.worktree_path)),
        );
        if let Some(holding) = trash_holding_path(Path::new(&snapshot.project_path), &snapshot.id) {
            candidate_paths.push(holding);
        }
        if let Err(error) = crate::session::deletion::ensure_unclaimed_paths(
            crate::session::deletion::SessionPathOwner {
                profile: storage.profile(),
                session_id: &request.session_id,
            },
            &candidate_paths,
        ) {
            let _ = storage.update_under_workspace_claim_lock(|instances, _groups| {
                if let Some(stored) = instances
                    .iter_mut()
                    .find(|instance| instance.id == request.session_id)
                {
                    stored.untrash();
                    stored.release_lifecycle_reservation_if_owned(
                        crate::session::LifecycleOperation::Trash,
                        request.generation,
                    );
                }
                Ok(())
            });
            let authoritative = storage.load().ok().and_then(|instances| {
                instances
                    .into_iter()
                    .find(|instance| instance.id == request.session_id)
            });
            return TrashResult {
                session_id: request.session_id.clone(),
                relocation: None,
                relocate_warning: Some(format!(
                    "trash skipped because worktree ownership is shared or unknown: {error}"
                )),
                authoritative,
            };
        }
    }

    let commit = storage.update_under_workspace_claim_lock(|instances, _groups| {
        let stored = instances
            .iter()
            .find(|row| row.id == request.session_id)
            .ok_or_else(|| anyhow::anyhow!("session disappeared before trash relocation"))?;
        anyhow::ensure!(
            stored.lifecycle_reservation_is_owned(
                crate::session::LifecycleOperation::Trash,
                request.generation
            ) && plan_inputs_unchanged(&snapshot, stored),
            "trash lifecycle reservation or relocation plan was superseded"
        );
        if !stored.runner_journal.proves_quiescent() {
            crate::session::claim::release_trash_reservation(
                instances,
                &request.session_id,
                request.generation,
            );
            return Ok((
                RelocateOutcome::Failed {
                    reason: "durable runner history does not prove checkout quiescence".into(),
                },
                None,
            ));
        }
        let mut inst = stored.clone();
        inst.source_profile = storage.profile().to_owned();
        inst.kill_all_tmux_sessions_locked();
        let outcome = prepare_trashed_worktree(&mut inst);
        let relocation = match &outcome {
            RelocateOutcome::Relocated { .. } => Some(TrashRelocation {
                new_project_path: inst.project_path.clone(),
                pre_trash_project_path: inst.pre_trash_project_path.clone(),
            }),
            RelocateOutcome::Skipped | RelocateOutcome::Failed { .. } => None,
        };
        if let Some(relocation) = &relocation {
            anyhow::ensure!(
                crate::session::claim::commit_trash_relocation(
                    instances,
                    &request.session_id,
                    request.generation,
                    relocation
                ) == crate::session::claim::RelocationCommit::Persisted,
                "trash relocation reservation was superseded"
            );
        } else {
            crate::session::claim::release_trash_reservation(
                instances,
                &request.session_id,
                request.generation,
            );
        }
        Ok((outcome, relocation))
    });
    let (outcome, relocation) = match commit {
        Ok(result) => result,
        Err(error) => return failed(format!("could not commit trash transition: {error}")),
    };

    let authoritative = storage.load().ok().and_then(|instances| {
        instances
            .into_iter()
            .find(|instance| instance.id == request.session_id)
    });
    TrashResult {
        session_id: request.session_id.clone(),
        relocation,
        relocate_warning: match outcome {
            RelocateOutcome::Failed { reason } => Some(reason),
            RelocateOutcome::Relocated { .. } | RelocateOutcome::Skipped => None,
        },
        authoritative,
    }
}

/// Move a trashed session's worktree back to its pre-trash location and clear
/// `pre_trash_project_path`.
/// Caller holds workspace -> identity -> lifecycle locks and supplies the freshly durable row.
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
    if !inst.runner_journal.proves_quiescent() {
        return RestoreOutcome::Failed {
            reason: "durable runner history does not prove checkout quiescence".into(),
        };
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
/// Lock-free worker: caller holds workspace -> identity -> lifecycle -> storage and owns
/// the durable reservation/plan; moving plans also require a stable path ownership check.
pub fn reconcile_trashed_location(inst: &mut Instance) -> anyhow::Result<bool> {
    Ok(match plan_trashed_reconcile(inst) {
        ReconcilePlan::Nothing => false,
        ReconcilePlan::RestoreDefaultBranch => match restore_worktree_location(inst) {
            RestoreOutcome::Restored { .. } => true,
            // The marker was set but nothing had actually moved, so restore
            // dropped it. That is still a mutation worth persisting.
            RestoreOutcome::NoChange => inst.pre_trash_project_path.is_none(),
            RestoreOutcome::Failed { reason } => {
                anyhow::bail!("default branch restore left checkout ownership unproven: {reason}")
            }
        },
        ReconcilePlan::Relocate => match relocate_worktree_to_trash(inst) {
            RelocateOutcome::Relocated { .. } => true,
            RelocateOutcome::Failed { reason } => {
                anyhow::bail!("trash relocation left checkout ownership unproven: {reason}")
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
    })
}

/// Reconcile trashed rows across original profiles using one fenced ownership inventory.
/// Returned rows remain associated with the physical Storage that committed them.
pub fn reconcile_trashed_profiles(
    storages: &[crate::session::Storage],
) -> anyhow::Result<Vec<(crate::session::Storage, Vec<Instance>)>> {
    if storages.is_empty() {
        return Ok(Vec::new());
    }
    let _workspace_claim_lock = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity_lock = crate::session::acquire_session_identity_lock()?;
    let mut ownership = crate::session::deletion::PathClaimIndex::load(storages)?;
    let mut cache = ProtectedBranchCache::default();
    let mut reconciled = Vec::new();
    for (target, profile, rows) in ownership.take_targets() {
        let storage = &storages[target];
        let mut candidates: Vec<Instance> = rows
            .into_iter()
            .filter(|inst| {
                plan_trashed_reconcile_cached(inst, &mut cache) != ReconcilePlan::Nothing
            })
            .collect();
        candidates.sort_by(|a, b| a.id.cmp(&b.id));
        let mut healed = Vec::new();
        for batch in candidates.chunks(RECONCILE_BATCH) {
            match reconcile_trashed_batch(storage, profile, batch, &mut ownership) {
                Ok(batch_healed) => healed.extend(batch_healed),
                Err(error) => {
                    ownership.invalidate();
                    return Err(error.context(format!(
                        "trash reconciliation for profile {} stopped after uncertain relocation",
                        storage.profile()
                    )));
                }
            }
        }
        if !healed.is_empty() {
            reconciled.push((storage.clone(), healed));
        }
    }
    Ok(reconciled)
}

/// Whether the durable row still matches the snapshot the plan was decided from.
fn plan_inputs_unchanged(snapshot: &Instance, durable: &Instance) -> bool {
    durable.lifecycle_generation == snapshot.lifecycle_generation
        && durable.is_trashed()
        && durable.project_path == snapshot.project_path
        && durable.pre_trash_project_path == snapshot.pre_trash_project_path
        && durable.worktree_info == snapshot.worktree_info
        && durable.scratch == snapshot.scratch
        && durable
            .workspace_info
            .as_ref()
            .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
            == snapshot
                .workspace_info
                .as_ref()
                .map(|workspace| (&workspace.workspace_dir, &workspace.repos))
        && is_sandboxed(durable) == is_sandboxed(snapshot)
}

/// How many rows one batch reserves at once.
const RECONCILE_BATCH: usize = 8;

fn reconcile_trashed_batch(
    storage: &crate::session::Storage,
    profile: usize,
    batch: &[Instance],
    ownership: &mut crate::session::deletion::PathClaimIndex,
) -> anyhow::Result<Vec<Instance>> {
    let now = Utc::now();
    let reserved = storage.update_metadata(|instances, _groups| {
        let mut reserved: Vec<(u64, Instance)> = Vec::new();
        for snapshot in batch {
            let Some(stored) = instances
                .iter_mut()
                .find(|candidate| candidate.id == snapshot.id)
            else {
                continue;
            };
            // Keep the original inventory plan until the reservation CAS succeeds.
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

    let mut healed = Vec::new();
    for (generation, snapshot) in reserved {
        let _lifecycle_lock = storage.acquire_instance_lifecycle_lock(&snapshot.id)?;
        let (changed, durable) =
            reconcile_reserved_locked(storage, profile, &snapshot, generation, ownership)?;
        if changed {
            ownership.update(profile, &durable);
            healed.push(durable);
        }
    }
    Ok(healed)
}

// Workspace and identity locks fence the pass; lifecycle fences this row.
fn reconcile_reserved_locked(
    storage: &crate::session::Storage,
    profile: usize,
    snapshot: &Instance,
    generation: u64,
    inventory: &crate::session::deletion::PathClaimIndex,
) -> anyhow::Result<(bool, Instance)> {
    let plan = plan_trashed_reconcile(snapshot);
    let mut paths = vec![PathBuf::from(&snapshot.project_path)];
    if let Some(original) = &snapshot.pre_trash_project_path {
        paths.push(PathBuf::from(original));
    }
    match &plan {
        ReconcilePlan::PointAtHolding { holding, .. } => paths.push(holding.clone()),
        ReconcilePlan::PointAtOriginal(original) => paths.push(original.clone()),
        ReconcilePlan::Relocate | ReconcilePlan::RestoreDefaultBranch => {
            if let Some(holding) =
                trash_holding_path(Path::new(&snapshot.project_path), &snapshot.id)
            {
                paths.push(holding);
            }
        }
        ReconcilePlan::Nothing => {}
    }
    let ownership = inventory.ensure_unclaimed(profile, &snapshot.id, &paths);
    storage.update_with_claim_index_under_workspace_lock(inventory, |instances, _groups| {
        let stored = instances
            .iter()
            .find(|row| row.id == snapshot.id)
            .ok_or_else(|| anyhow::anyhow!("session disappeared before trash reconciliation"))?;
        anyhow::ensure!(
            stored.lifecycle_reservation_is_owned(
                crate::session::LifecycleOperation::Trash,
                generation
            ),
            "trash reconciliation reservation was superseded"
        );
        let unchanged =
            plan_inputs_unchanged(snapshot, stored) && plan_trashed_reconcile(stored) == plan;
        let mut durable = stored.clone();
        durable.source_profile = storage.profile().to_owned();
        let changed = unchanged && ownership.is_ok() && reconcile_trashed_location(&mut durable)?;
        if changed {
            let relocation = TrashRelocation {
                new_project_path: durable.project_path.clone(),
                pre_trash_project_path: durable.pre_trash_project_path.clone(),
            };
            anyhow::ensure!(
                crate::session::claim::commit_trash_relocation(
                    instances,
                    &snapshot.id,
                    generation,
                    &relocation
                ) == crate::session::claim::RelocationCommit::Persisted,
                "trash reconciliation reservation was superseded"
            );
        } else {
            crate::session::claim::release_trash_reservation(instances, &snapshot.id, generation);
        }
        durable.lifecycle_reservation = None;
        Ok((changed, durable))
    })
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
        assert!(!reconcile_trashed_location(&mut inst).unwrap());

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
            reconcile_trashed_location(&mut inst).unwrap(),
            "reconcile must move the checkout back and report the mutation"
        );
        assert_eq!(PathBuf::from(&inst.project_path), original);
        assert!(inst.pre_trash_project_path.is_none());
        assert!(original.exists());
        assert!(!holding.exists());

        assert!(
            !reconcile_trashed_location(&mut inst).unwrap(),
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
    #[serial_test::serial]
    fn review_repro_reconciliation_does_not_move_a_live_runners_checkout() {
        use crate::process::worker_registry::{self, WorkerRecord};
        let _home = crate::session::test_support::isolate_app_dir();
        let (_tmp, mut instance) = real_worktree_instance();
        instance.source_profile = "owner".into();
        instance.view = crate::session::View::Structured;
        instance.trash();
        let original = PathBuf::from(&instance.project_path);
        std::fs::write(original.join("sentinel"), "live agent checkout").unwrap();
        let storage = crate::session::Storage::new_unwatched("owner").unwrap();
        storage
            .update(|instances, _| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let mut command = std::process::Command::new("/bin/sh");
        command
            .args(["-c", "read -r line"])
            .current_dir(&original)
            .stdin(std::process::Stdio::piped());
        crate::process::configure_process_group(&mut command);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        let record = WorkerRecord::new(
            instance.id.clone(),
            pid,
            worker_registry::socket_path_for(&instance.id).unwrap(),
            "agent".into(),
            "agent".into(),
            original.clone(),
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some("owner".into()),
        );
        worker_registry::save(&record).unwrap();
        instance.runner_journal = serde_json::from_value(serde_json::json!({
            "coverage": "complete", "preparations": [],
            "launches": [{
                "nonce": *uuid::Uuid::new_v4().as_bytes(),
                "boot": *uuid::Uuid::parse_str(&crate::process::boot_id().unwrap()).unwrap().as_bytes(),
                "generation": 0,
                "incarnation": crate::process::process_incarnation(pid).unwrap().unwrap(),
                "profile_identity": storage.original_profile_identity().unwrap(),
            }],
        })).unwrap();
        storage
            .update(|instances, _| {
                instances
                    .iter_mut()
                    .find(|row| row.id == instance.id)
                    .unwrap()
                    .runner_journal = instance.runner_journal.clone();
                instances
                    .iter_mut()
                    .find(|row| row.id == instance.id)
                    .unwrap()
                    .view = crate::session::View::Terminal;
                Ok(())
            })
            .unwrap();
        std::fs::remove_file(worker_registry::record_path(&instance.id).unwrap()).unwrap();
        // Only storage is authoritative, not this stale consumer snapshot.
        instance.runner_journal = crate::session::runner_journal::RunnerExecutionJournal::new();
        assert!(crate::process::worker::is_process_group_alive(pid));
        let result = reconcile_trashed_profiles(std::slice::from_ref(&storage));
        assert!(
            result.is_err(),
            "a live unproven owner must invalidate this reconciliation pass"
        );
        let group_was_alive = crate::process::worker::is_process_group_alive(pid);
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(
            group_was_alive,
            "the runner remained live through reconciliation"
        );
        assert!(
            original.join("sentinel").exists(),
            "a live runner must retain its checkout"
        );
        assert_eq!(PathBuf::from(&instance.project_path), original);
        let holding = trash_holding_path(&original, &instance.id).unwrap();
        assert!(!holding.exists());
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
            reconcile_trashed_location(&mut inst).unwrap(),
            "reconcile should relocate a legacy trashed worktree"
        );
        let holding = trash_holding_path(Path::new(&original), &inst.id).unwrap();
        assert_eq!(PathBuf::from(&inst.project_path), holding);
        assert_eq!(
            inst.pre_trash_project_path.as_deref(),
            Some(original.as_str())
        );
        assert!(!PathBuf::from(&original).exists());

        assert!(!reconcile_trashed_location(&mut inst).unwrap());
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
                !reconcile_trashed_location(&mut inst).unwrap(),
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
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .args(&args)
                .current_dir(&main_repo)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
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
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .current_dir(&main_repo)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr)
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
        assert!(reconcile_trashed_location(&mut inst).is_err());
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

        let mut ownership =
            crate::session::deletion::PathClaimIndex::load(std::slice::from_ref(&storage)).unwrap();
        let (_, profile, _) = ownership.take_targets().pop().unwrap();
        assert!(reconcile_trashed_batch(
            &storage,
            profile,
            std::slice::from_ref(&snapshot),
            &mut ownership,
        )
        .unwrap()
        .is_empty());
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

        let healed = reconcile_trashed_profiles(std::slice::from_ref(&storage)).unwrap();
        assert_eq!(healed.iter().map(|(_, rows)| rows.len()).sum::<usize>(), 2);
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
            reconcile_trashed_profiles(std::slice::from_ref(&storage))
                .unwrap()
                .is_empty(),
            "the sweep is idempotent"
        );
        assert_eq!(
            generations(&storage.load().unwrap()),
            generations(&stored),
            "a consistent profile is left untouched"
        );
    }

    #[test]
    #[serial_test::serial]
    fn pointer_heals_refuse_homonymous_peer_claims_in_other_profiles() {
        assert!(git_available(), "native Git fixture requires git");
        let _guard = crate::session::test_support::isolate_app_dir();
        for toward_holding in [true, false] {
            let name = if toward_holding {
                "heal-holding"
            } else {
                "heal-original"
            };
            let peer_name = format!("{name}-peer");
            crate::session::create_profile(name).unwrap();
            crate::session::create_profile(&peer_name).unwrap();
            let storage = crate::session::Storage::open_unwatched(name).unwrap();
            let peer_storage = crate::session::Storage::open_unwatched(&peer_name).unwrap();
            let (_tmp, mut row) = real_worktree_instance();
            let original = row.project_path.clone();
            row.trash();
            let destination = if toward_holding {
                assert!(matches!(
                    relocate_worktree_to_trash(&mut row),
                    RelocateOutcome::Relocated { .. }
                ));
                let holding = row.project_path.clone();
                row.project_path = original.clone();
                row.pre_trash_project_path = None;
                holding
            } else {
                row.project_path = format!("{original}-missing-pointer");
                row.pre_trash_project_path = Some(original.clone());
                original
            };
            assert!(matches!(
                plan_trashed_reconcile(&row),
                ReconcilePlan::PointAtHolding { .. } | ReconcilePlan::PointAtOriginal(_)
            ));
            let mut peer = Instance::new("peer", &destination);
            peer.id = row.id.clone();
            storage
                .update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })
                .unwrap();
            peer_storage
                .update(|rows, _| {
                    rows.push(peer.clone());
                    Ok(())
                })
                .unwrap();
            assert!(reconcile_trashed_profiles(std::slice::from_ref(&storage))
                .unwrap()
                .is_empty());
            let refused = storage.load().unwrap().remove(0);
            assert_eq!(refused.project_path, row.project_path);
            assert_eq!(refused.pre_trash_project_path, row.pre_trash_project_path);
            assert!(refused.lifecycle_reservation.is_none());
            assert!(Path::new(&destination).exists());
            peer_storage
                .update(|rows, _| {
                    rows.clear();
                    Ok(())
                })
                .unwrap();
            let healed = reconcile_trashed_profiles(std::slice::from_ref(&storage)).unwrap();
            assert_eq!(healed.len(), 1);
            assert_eq!(storage.load().unwrap()[0].project_path, destination);
        }
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
            assert_eq!(
                reconcile_trashed_location(&mut inst).unwrap(),
                healed,
                "{case}"
            );
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
    #[serial_test::serial]
    fn trash_ownership_rejection_untrashes_and_releases_reservation() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        let owner = crate::session::Storage::new_unwatched("owner").unwrap();
        let other = crate::session::Storage::new_unwatched("other").unwrap();
        let mut instance = Instance::new("session", "/tmp/session");
        instance.source_profile = "owner".to_string();
        instance.worktree_info = Some(crate::session::WorktreeInfo {
            branch: "feature/shared".to_string(),
            main_repo_path: "/tmp/main".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        owner
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        other
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        // The cross-profile inventory is unverifiable because a peer profile
        // cannot be read at all, which is what makes the ownership verdict
        // `Unknown` and the trash refuse.
        std::fs::write(other.sessions_path(), b"{ not json").unwrap();
        let generation = owner
            .update(|instances, _groups| {
                let row = instances
                    .iter_mut()
                    .find(|row| row.id == instance.id)
                    .unwrap();
                let generation = row
                    .try_acquire_lifecycle_reservation(
                        crate::session::LifecycleOperation::Trash,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .unwrap();
                row.trash();
                Ok(generation)
            })
            .unwrap();
        let result = perform_trash(&TrashRequest {
            storage: owner.clone(),
            session_id: instance.id.clone(),
            instance: instance.clone(),
            generation,
        });
        assert!(result.relocate_warning.is_some());
        let stored = owner.load().unwrap().into_iter().next().unwrap();
        assert!(!stored.is_trashed());
        assert!(stored.lifecycle_reservation.is_none());
    }

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

        let original_present_at_stop = Rc::new(Cell::new(false));

        let outcome = {
            let stop_calls = Rc::clone(&stop_calls);

            let original_present_at_stop = Rc::clone(&original_present_at_stop);
            let original = original.clone();
            prepare_trashed_worktree_with(&mut inst, move |_id, _is_sandboxed| {
                stop_calls.set(stop_calls.get() + 1);
                original_present_at_stop.set(original.exists());
            })
        };

        assert_eq!(
            stop_calls.get(),
            1,
            "trash must run the container-stop step exactly once"
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
}
