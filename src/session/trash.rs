//! Trash retention helpers.
//!
//! A trashed session (see [`Instance::trash`](crate::session::Instance::trash))
//! stays recoverable until the user purges it or its retention window
//! elapses. Retention auto-purge is enforced by the serve daemon only (a
//! startup pass plus an hourly tick), routed through the same purge path the
//! `DELETE /api/sessions/{id}` handler uses, so ACP teardown, event-store
//! deletion, sidecar cleanup, and the storage row removal all stay
//! consistent and there is no multi-process purge race. Without a running
//! daemon, expired trash is purged on the next daemon start or by an explicit
//! manual purge / empty-trash. This module owns the pure "which rows are
//! expired" decision so it can be unit-tested in isolation.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::git::GitWorktree;
use crate::session::worktree_edit::{
    discard_sandbox_container_after_move, ensure_sandbox_container_released,
};
use crate::session::Instance;

/// Hidden, product-owned holding directory for trashed worktrees. A relocated
/// worktree lands at `<original-worktree-parent>/.aoe-trash/<session-id>`. The
/// name is namespaced (not a generic `.trash`) so it cannot collide with a
/// user's own tooling, and keeping it a sibling of the active worktree leaf
/// means `git worktree move` stays a same-filesystem rename rather than a
/// cross-device copy that git refuses.
const TRASH_DIR_NAME: &str = ".aoe-trash";

/// Where a trashed session's worktree is parked. `None` when `original` has no
/// parent (a filesystem root), in which case relocation is skipped.
pub fn trash_holding_path(original: &Path, session_id: &str) -> Option<PathBuf> {
    Some(original.parent()?.join(TRASH_DIR_NAME).join(session_id))
}

/// True when `path` is already a holding path for this session, i.e. its leaf
/// is the session id sitting directly under a `.aoe-trash` dir. Guards the
/// backfill branch of reconciliation from nesting an already-relocated (but
/// markerless) worktree under `.aoe-trash/.aoe-trash/<id>`.
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
    /// The move could not run safely (sandbox container still mounting the
    /// dir, locked, cross-device, git error). `project_path` is untouched;
    /// the caller trashes in place and surfaces `reason`. Never blocks trash.
    Failed { reason: String },
}

/// Result of attempting to move a worktree back out of the holding area.
#[derive(Debug)]
pub enum RestoreOutcome {
    /// The worktree was moved back to its pre-trash location.
    Restored { from: PathBuf, to: PathBuf },
    /// No relocation had happened (plain/non-managed session, or a row trashed
    /// before relocation existed), so there is nothing to move. The caller
    /// still clears `trashed_at`.
    NoChange,
    /// The worktree could not be moved back (its original path is now occupied
    /// by something else, or git refused). The session stays trashed and the
    /// caller surfaces `reason`. Restore is strict: it never lands the
    /// worktree somewhere other than where it came from.
    Failed { reason: String },
}

fn is_managed_single_worktree(inst: &Instance) -> bool {
    !inst.scratch
        && inst
            .worktree_info
            .as_ref()
            .is_some_and(|w| w.managed_by_aoe)
}

/// Whether the session's branch is one git states is the repo's default, so its
/// checkout must be left where it is (#3215). Only meaningful for a managed
/// single-repo worktree, which every caller has already established.
fn is_protected_default_branch(inst: &Instance) -> bool {
    is_protected_default_branch_cached(inst, &mut ProtectedBranchCache::default())
}

/// One sweep's worth of `protected_default_branch_names` results, keyed by main
/// repo path.
///
/// The lookup opens the repo through libgit2 and walks its remotes and refs, and
/// every already-relocated trashed row asks for it, so a store's worth of rows
/// in one repo would otherwise pay that per row. A failure is not cached: it
/// usually means the repo is unreachable right now, and the next row should get
/// a fresh attempt rather than inherit a stale verdict. Mirrors
/// [`crate::session::worktree_reconcile::ReconcileCache`].
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

/// Whether a managed worktree's directory has outlived its registration, so
/// `git worktree move` can only ever answer "not a working tree".
///
/// A linked worktree carries a `.git` file naming its admin dir under the main
/// repo. Pruning that admin dir (or deleting the file) strands the checkout:
/// the directory is still there, git no longer knows about it, and no retry
/// changes that. Two `stat`s, no spawn, no error-string sniffing.
///
/// Resolution goes through [`crate::git::cleanup::read_linked_worktree_gitdir`]
/// so the pointer is read the same way everywhere: aoe rewrites every managed
/// worktree's pointer to a relative target in `create_worktree`, so resolving
/// it against the process directory instead of the worktree would read a live
/// checkout as stranded and refuse to relocate it for good.
///
/// Only a definite absence counts as gone, for the `.git` entry and for the
/// admin dir it names. Anything else unreadable is treated as present, since
/// this decides whether to stop retrying and the sweep runs once per launch, so
/// guessing "stranded" from a transient stat error suppresses the relocation
/// until the app is started again.
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
    // `Path::exists` reports false for every error, so a permission or I/O
    // blip on the admin dir would read a live checkout as stranded. Only a
    // definite absence is terminal; anything else stays retriable.
    match crate::git::cleanup::read_linked_worktree_gitdir(worktree) {
        Some(admin) => matches!(admin.try_exists(), Ok(false)),
        None => false,
    }
}

fn is_sandboxed(inst: &Instance) -> bool {
    inst.sandbox_info.as_ref().is_some_and(|s| s.enabled)
}

/// Move a freshly-trashed session's managed worktree into the holding area and
/// repoint `project_path`, capturing the original location in
/// `pre_trash_project_path`. The caller MUST have stopped the live agent first
/// (a running sandbox container holds the dir and the move fails EBUSY); this
/// checks that gate and returns [`RelocateOutcome::Failed`] rather than
/// blocking. Idempotent: a session that already carries
/// `pre_trash_project_path` is [`RelocateOutcome::Skipped`].
pub fn relocate_worktree_to_trash(inst: &mut Instance) -> RelocateOutcome {
    if !inst.is_trashed() || !is_managed_single_worktree(inst) {
        return RelocateOutcome::Skipped;
    }
    if inst.pre_trash_project_path.is_some() {
        return RelocateOutcome::Skipped;
    }
    // A default branch's checkout is infrastructure: sibling tooling expects
    // `<project>/main` to stay where it is, so moving it into the holding area
    // breaks that layout even though the move is reversible. Leaving it in place
    // also keeps the purge from stranding it in `.aoe-trash` forever, since the
    // purge now refuses to remove it (#3215). Skipped, not Failed: this is the
    // intended outcome, not a move that could not run.
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

/// Bring a freshly-trashed session's sandbox container down, then relocate its
/// worktree into the holding area.
///
/// This is the container + worktree half of trashing (`trash_session_by_id`),
/// split from [`relocate_worktree_to_trash`] because trashing must first stop
/// the sandbox container. A sandbox container runs `sleep infinity` for the
/// life of the session and bind-mounts the worktree dir, so trashing without a
/// stop leaves it running for the whole retention window and its live mount
/// makes the relocation's `git worktree move` fail `EBUSY` (the row then stays
/// in the active dir). Stopping it releases the mount so the relocation's own
/// [`discard_sandbox_container_after_move`] can then drop it entirely.
///
/// `relocate_worktree_to_trash` alone is still the right call for the reconcile
/// passes (they run on load against already-stopped rows); only the trash
/// *action*, where the container is still live, needs the stop.
///
/// The container stop is injected so the sandbox path is exercisable without a
/// live docker runtime (mirrors `deletion::perform_deletion_with`).
///
/// The container stop blocks for up to the stop grace period (~10s), which is
/// plenty of time for a restore to land on the durable row (a user who hit `d`
/// by accident restores immediately; the restore itself is a NoChange because
/// no relocation has been recorded yet). The durable row is therefore
/// re-checked between the stop and the move, and the move is skipped when the
/// row is no longer trashed, was seized by a fresh purge/restore claim, is
/// gone, or storage cannot be read (fail closed, since a skipped move on a
/// still-trashed row is healed by the next reconcile pass, while a move on a
/// restored row strands a live session's worktree in the holding area). The
/// re-check reads storage via `inst.source_profile`, so callers must pass an
/// instance whose profile is stamped and must have durably trashed the row
/// before calling.
///
/// BLOCKING: the container stop shells out to `docker stop` (~10s grace period)
/// and the relocation runs `git worktree move`, so never call this on an event
/// loop / UI thread. The TUI goes through [`perform_trash`] on the
/// `TrashPoller`, the server wraps it in `spawn_blocking`, and the CLI is a
/// one-shot process.
/// Stop the sandbox container and move a managed worktree into the trash.
/// The caller must hold the session's lifecycle flock and own its Trash reservation.
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

/// Undo a trash relocation that landed after the row had already been
/// restored: the worker's still-trashed re-check and the `git worktree move`
/// are not atomic, so a restore squeezing between them leaves a live,
/// untrashed row pointing at its original path while the worktree sits in the
/// holding area. Moves the worktree back so the live row's `project_path` is
/// real again; the row itself needs no persist (it already points at the
/// original). `live` supplies the repo metadata and container gate; the
/// relocation supplies the two paths. Strict like restore: never lands the
/// worktree anywhere but where it came from.
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
/// `pre_trash_project_path`. Strict: if the original path is now occupied, the
/// session stays trashed and the caller surfaces the failure, rather than
/// silently restoring it to a different path.
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
///
/// Decided from the recorded paths and the filesystem alone, so a sweep can
/// drop a row that needs nothing before opening storage, taking its lifecycle
/// flock, or spawning git. That pre-filter is the whole point of the split:
/// the cost used to be paid per trashed row whether or not anything changed
/// (#3611).
#[derive(Debug, PartialEq, Eq)]
enum ReconcilePlan {
    /// The row is consistent, or is not one this pass owns.
    Nothing,
    /// Move a protected default branch's checkout back out of the holding
    /// area (#3215).
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

    // Upgrade path for #3215: a default branch's checkout that an earlier
    // version relocated is still sitting in the holding area, and the purge now
    // refuses to remove it, so clearing the row would leave that checkout there
    // with nothing pointing at it. Move it back instead. Strict like every
    // restore: an occupied original leaves the row untouched.
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
        // Legacy backfill: a trashed managed worktree still sitting in the
        // active dir with no marker gets relocated now. An already-relocated
        // row (marker set, current == holding) is left alone, as is a
        // markerless row that already sits in the holding area (relocating it
        // again would nest it under .aoe-trash/.aoe-trash/<id>).
        if inst.pre_trash_project_path.is_some()
            || current == holding
            || is_holding_path(&current, &inst.id)
        {
            return ReconcilePlan::Nothing;
        }
        // Crash case: the worktree was already moved to `holding` but the
        // marker/pointer persist was lost and something was recreated at the
        // original path. Retrying the move would fail (the target exists) and
        // leave project_path on the wrong dir, so heal to the existing holding
        // path and record the marker. Restore can then fail cleanly if the
        // original stays occupied.
        if holding.exists() {
            return ReconcilePlan::PointAtHolding { holding, original };
        }
        // Terminal state for a relocation that can never succeed (#3611). An
        // orphaned checkout fails `git worktree move` with "not a working
        // tree" no matter how often it is retried, and the old pass re-ran it
        // on every launch and every poller tick forever. Derived from the
        // filesystem rather than recorded on the row, so a repaired repo
        // becomes relocatable again on its own and no transient git failure
        // can ever freeze into `sessions.json`.
        if is_stranded_checkout(&current) {
            tracing::warn!(
                target: "session.trash",
                session = %inst.id,
                path = %current.display(),
                "trashed worktree is no longer registered with its repo; leaving it in place"
            );
            return ReconcilePlan::Nothing;
        }
        // A default branch's checkout is never relocated (#3215), so planning
        // the move would reserve the row, take its flock, and write twice on
        // every sweep for a relocation that always answers Skipped.
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

/// Load-time reconciliation for a single trashed session. Returns `true` when
/// it mutated the instance (the caller must then persist).
///
/// Three jobs, all idempotent:
///   - Backfill: a managed worktree trashed before relocation existed (no
///     `pre_trash_project_path`, worktree still in the active dir) is relocated
///     into the holding area now.
///   - Heal-after-crash: if `project_path` no longer exists on disk but the
///     deterministic holding path does, the move landed but the second persist
///     was lost; repoint `project_path` and set `pre_trash_project_path`.
///   - Heal-back: if `project_path` is gone and only the original survives, the
///     move never took (or was undone); point back at the original.
///
/// Best-effort and non-fatal: a git failure logs and leaves the row as-is. A
/// checkout the repo no longer registers never reaches the move at all, since
/// the plan step treats it as terminal (#3611).
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
///
/// Returns the rows whose durable record changed. Rows that need nothing are
/// decided from the recorded paths and the filesystem, so a profile whose
/// trash is already consistent takes no lock, spawns no git, and writes
/// nothing; the one libgit2 open the default-branch check needs is shared
/// across every row in a repo. The rows that do need work are
/// reserved in one [`crate::session::Storage::update`] per batch, do their
/// blocking filesystem work outside the storage lock, and commit in a second,
/// instead of two full read-parse-serialize-write cycles per row (#3611).
///
/// Only one lifecycle flock is ever held at a time, so this cannot deadlock
/// against a peer and cannot make one wait behind an unrelated row's git call.
///
/// BLOCKING: takes cross-process locks and shells out to git. Never call it on
/// an event loop or the async runtime.
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
        // One batch's write failure must not abandon the rest of the profile:
        // the pass this replaced logged per row and carried on, and a batch
        // that bails leaves its reservations to expire on the TTL.
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

/// Whether the durable row still matches the snapshot the plan was decided
/// from. Everything [`plan_trashed_reconcile`] reads, so a row a peer touched
/// between the scan and the reservation is dropped rather than reserved.
fn plan_inputs_unchanged(snapshot: &Instance, durable: &Instance) -> bool {
    durable.is_trashed()
        && durable.project_path == snapshot.project_path
        && durable.pre_trash_project_path == snapshot.pre_trash_project_path
        && durable.worktree_info == snapshot.worktree_info
        && durable.scratch == snapshot.scratch
}

/// How many rows one batch reserves at once.
///
/// A batch holds its reservations from the first write to the second, and
/// `git worktree move` is bounded at 30s per row, so the batch size sets how
/// long that window can get. It has to stay well inside
/// [`Instance::LIFECYCLE_RESERVATION_TTL`] (10 minutes) or a slow batch would
/// outlive its own reservations and commit nothing.
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
            // Compare and set: the plan was decided from a snapshot taken
            // without any lock, so a peer can have restored, purged, or moved
            // the row since. Reserving it anyway would put a Trash reservation
            // on a live session and hold it for the rest of the batch, making
            // that session's launch, restore, and purge report Busy behind
            // unrelated worktree moves.
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

    // Each row takes its own lifecycle flock only across its own filesystem
    // work. Holding the batch's flocks throughout would make a peer wanting any
    // one of these sessions wait behind every other row's `git worktree move`.
    // The reservation taken above, not the flock, is what keeps peers off these
    // rows for the whole batch.
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
///
/// The caller's snapshot is replaced with the durable row after commit. A
/// fresh peer reservation refuses the pass; an expired reservation is superseded.
pub fn reconcile_trashed_transition(inst: &mut Instance) -> anyhow::Result<bool> {
    // Decide from the caller's snapshot before paying for storage, the
    // lifecycle flock, and two write cycles. The pass is best-effort and
    // idempotent, so a snapshot that has gone stale just defers to the next
    // one (#3611).
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

/// True when a trashed session is past its retention window and should be
/// auto-purged. `retention_days == 0` means "keep forever" (manual purge
/// only), so it never expires. A non-trashed session never expires.
pub fn is_expired(instance: &Instance, retention_days: u32, now: DateTime<Utc>) -> bool {
    if retention_days == 0 {
        return false;
    }
    match instance.trashed_at {
        Some(trashed_at) => now >= trashed_at + chrono::Duration::days(retention_days as i64),
        None => false,
    }
}

/// Ids of every trashed session whose retention window has elapsed, in the
/// order they appear in `instances`. Empty when retention is disabled
/// (`retention_days == 0`) or nothing has expired.
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
    fn not_expired_when_retention_zero() {
        let inst = trashed_days_ago(9999);
        assert!(!is_expired(&inst, 0, Utc::now()), "0 days = keep forever");
    }

    #[test]
    fn not_expired_when_not_trashed() {
        let inst = Instance::new("s", "/tmp/x");
        assert!(!is_expired(&inst, 30, Utc::now()));
    }

    #[test]
    fn expires_exactly_at_window() {
        let now = Utc::now();
        let mut inst = Instance::new("s", "/tmp/x");
        inst.trashed_at = Some(now - chrono::Duration::days(30));
        assert!(
            is_expired(&inst, 30, now),
            "trashed >= retention => expired"
        );

        inst.trashed_at = Some(now - chrono::Duration::days(29));
        assert!(!is_expired(&inst, 30, now), "still within window");
    }

    #[test]
    fn expired_ids_filters_and_preserves_order() {
        let fresh = trashed_days_ago(1);
        let old_a = trashed_days_ago(40);
        let live = Instance::new("s", "/tmp/x");
        let old_b = trashed_days_ago(31);
        let instances = vec![fresh, old_a.clone(), live, old_b.clone()];

        let ids = expired_trashed_ids(&instances, 30, Utc::now());
        assert_eq!(ids, vec![old_a.id, old_b.id]);
    }

    #[test]
    fn holding_path_is_namespaced_sibling() {
        let p = trash_holding_path(Path::new("/repo-worktrees/feature"), "abc123").unwrap();
        assert_eq!(p, PathBuf::from("/repo-worktrees/.aoe-trash/abc123"));
        assert!(trash_holding_path(Path::new("/"), "abc123").is_none());
    }

    #[test]
    fn relocate_skips_plain_session() {
        let mut inst = Instance::new("plain", "/tmp/plain");
        inst.trash();
        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
            RelocateOutcome::Skipped
        ));
        assert_eq!(inst.project_path, "/tmp/plain");
        assert!(inst.pre_trash_project_path.is_none());
    }

    /// Build a real aoe-managed worktree on disk and return (tmp, instance).
    /// Mirrors the harness in `src/session/deletion.rs` tests.
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

    /// The layout #3215 reports: a bare repo whose default branch is checked out
    /// as a linked worktree at `<project>/main`, which sibling tooling expects
    /// to stay exactly there.
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

    /// #3215: trashing must not move a default branch's checkout. Relocation is
    /// reversible, but it still breaks a layout that expects `<project>/main` to
    /// exist, and the purge now refuses to remove the checkout, so a relocated
    /// one would sit in the holding area forever.
    #[test]
    fn relocate_leaves_a_default_branch_checkout_in_place() {
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = default_branch_worktree_instance();
        let original = inst.project_path.clone();
        inst.trash();

        let out = relocate_worktree_to_trash(&mut inst);
        assert!(
            matches!(out, RelocateOutcome::Skipped),
            "expected the relocation to be skipped, got {out:?}"
        );
        assert_eq!(inst.project_path, original);
        assert!(inst.pre_trash_project_path.is_none());
        assert!(PathBuf::from(&original).exists());
    }

    /// #3611: the relocation refuses a default branch's checkout (#3215), so
    /// planning it costs a reservation, a flock, and two writes on every sweep
    /// for work that can never make progress. The plan step has to refuse it
    /// too.
    #[test]
    fn a_default_branch_checkout_is_never_planned_for_relocation() {
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = default_branch_worktree_instance();
        inst.trash();
        assert_eq!(plan_trashed_reconcile(&inst), ReconcilePlan::Nothing);
        assert!(!reconcile_trashed_location(&mut inst));
    }

    /// Upgrade path for #3215: a row relocated by an earlier version. The purge
    /// now preserves the checkout, so leaving it in the holding area would
    /// orphan it once the row is cleared. Reconciliation moves it back.
    #[test]
    fn reconcile_moves_a_relocated_default_branch_checkout_back() {
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = default_branch_worktree_instance();
        let original = PathBuf::from(&inst.project_path);
        inst.trash();

        // Reproduce what the previous version left behind: the worktree moved
        // into the holding area with the marker recorded.
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
        // Worktree moved into the holding area, original dir gone.
        let holding = trash_holding_path(Path::new(&original), &inst.id).unwrap();
        assert_eq!(PathBuf::from(&inst.project_path), holding);
        assert!(holding.exists());
        assert!(!PathBuf::from(&original).exists());
        assert_eq!(
            inst.pre_trash_project_path.as_deref(),
            Some(original.as_str())
        );

        // Relocate again is a no-op (idempotent).
        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
            RelocateOutcome::Skipped
        ));

        // Restore moves it back and clears the marker.
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
    fn restore_fails_when_original_occupied() {
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
        // Something now occupies the original path.
        std::fs::create_dir_all(&original).unwrap();

        let out = restore_worktree_location(&mut inst);
        assert!(
            matches!(out, RestoreOutcome::Failed { .. }),
            "restore should refuse an occupied original, got {out:?}"
        );
        // Still relocated, still recoverable later.
        assert!(inst.pre_trash_project_path.is_some());
        assert_ne!(inst.project_path, original);
    }

    #[test]
    fn reconcile_backfills_legacy_then_is_idempotent() {
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        let original = inst.project_path.clone();
        // Legacy trashed row: trashed, worktree still in the active dir, no marker.
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

        // Second pass changes nothing.
        assert!(!reconcile_trashed_location(&mut inst));
    }

    /// #3611: an orphaned managed worktree makes `git worktree move` fail with
    /// "not a working tree" no matter how often it runs, and the old pass
    /// retried it on every launch and every poller tick forever. Both ways a
    /// checkout gets stranded are terminal: the reported one, where the repo's
    /// admin dir was pruned and the `.git` file is left dangling, and the one
    /// where the `.git` entry is gone outright.
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
                // The shape #3611 reports: the `.git` file survives and still
                // names an admin dir that is no longer there.
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
            // Left exactly as found, so restore and purge still see the
            // worktree where it actually is.
            assert_eq!(PathBuf::from(&inst.project_path), original);
            assert!(inst.pre_trash_project_path.is_none());
            assert!(original.exists());
        }
    }

    /// A worktree whose `.git` names its admin dir by a relative path is still
    /// live. aoe rewrites every managed worktree's pointer that way in
    /// `create_worktree`, and git does the same under
    /// `worktree.useRelativePaths`, so resolving the target from the process
    /// directory instead of the worktree would read essentially every managed
    /// trashed worktree as stranded and refuse to relocate it for good.
    #[test]
    fn a_relative_gitdir_link_is_not_mistaken_for_a_stranded_checkout() {
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
            // This git predates `worktree.useRelativePaths`; nothing to assert.
            return;
        }
        assert!(
            !is_stranded_checkout(&worktree),
            "a live checkout with a relative gitdir link must not read as stranded"
        );
        // And the real thing still does, relative link or not.
        std::fs::remove_dir_all(worktree.join(&target)).unwrap();
        assert!(is_stranded_checkout(&worktree));
    }

    /// `Path::exists` cannot tell absence from a stat failure, so
    /// an EACCES or ELOOP on the admin dir would read a live checkout as
    /// stranded. The sweep runs once per launch, so that suppresses the
    /// relocation until the app is restarted. A symlink loop stands in for the
    /// error class because it needs no permission games (tests run as root).
    #[test]
    fn a_stat_failure_on_the_admin_dir_is_not_a_stranded_checkout() {
        let tmp = tempfile::TempDir::new().unwrap();
        let worktree = tmp.path().join("wt");
        std::fs::create_dir_all(&worktree).unwrap();
        // `loop_a -> loop_b -> loop_a`, so resolving either yields ELOOP.
        let loop_a = tmp.path().join("loop_a");
        let loop_b = tmp.path().join("loop_b");
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

        // A definite absence is still terminal.
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", tmp.path().join("definitely-gone").display()),
        )
        .unwrap();
        assert!(is_stranded_checkout(&worktree));
    }

    /// #3611: only a stranded checkout is terminal. A move that fails while the
    /// checkout is still registered (a timeout, a failed spawn, a permission or
    /// lock condition) must stay retriable, so one bad moment cannot strand the
    /// relocation for good.
    #[test]
    fn a_move_failure_over_a_live_checkout_stays_retriable() {
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        // Point the row at an unrelated repo so `git worktree move` refuses the
        // checkout while its registration is still intact.
        let (_other, other) = real_worktree_instance();
        inst.worktree_info.as_mut().unwrap().main_repo_path =
            other.worktree_info.unwrap().main_repo_path;
        inst.trash();

        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
            RelocateOutcome::Failed { .. }
        ));
        // Still a candidate: the next pass tries again rather than giving up.
        assert_eq!(plan_trashed_reconcile(&inst), ReconcilePlan::Relocate);
        assert!(!reconcile_trashed_location(&mut inst));
    }

    /// #3611 review: the plan is decided from an unlocked snapshot, so a peer
    /// can restore the row before the batch reserves it. Reserving anyway would
    /// pin a Trash reservation on a live session for the rest of the batch and
    /// make its launch, restore, and purge report Busy.
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

        // The peer restore lands between the scan and the batch.
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
        // And its worktree was left where the restore put it.
        assert!(stored.pre_trash_project_path.is_none());
    }

    /// #3611: the sweep decides from the filesystem alone, so a profile whose
    /// trash is already consistent takes no reservation. A bumped
    /// `lifecycle_generation` is the durable trace of the reserve/release pair
    /// the old per-row pass ran on every launch.
    #[test]
    #[serial_test::serial]
    fn profile_sweep_leaves_a_consistent_profile_untouched() {
        let _guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut plain = Instance::new("plain", "/tmp/plain");
        plain.trash();
        let (_tmp, mut relocated) = if git_available() {
            let (tmp, mut inst) = real_worktree_instance();
            inst.trash();
            assert!(matches!(
                relocate_worktree_to_trash(&mut inst),
                RelocateOutcome::Relocated { .. }
            ));
            (Some(tmp), Some(inst))
        } else {
            (None, None)
        };
        let ids: Vec<String> = std::iter::once(plain.id.clone())
            .chain(relocated.as_ref().map(|inst| inst.id.clone()))
            .collect();
        storage
            .update(|instances, _groups| {
                instances.push(plain.clone());
                if let Some(inst) = relocated.take() {
                    instances.push(inst);
                }
                Ok(())
            })
            .unwrap();

        assert!(reconcile_trashed_profile("default").unwrap().is_empty());
        for stored in storage.load().unwrap() {
            assert!(ids.contains(&stored.id));
            assert_eq!(
                stored.lifecycle_generation, 0,
                "a row needing nothing must not be reserved"
            );
            assert!(stored.lifecycle_reservation.is_none());
        }
    }

    /// #3611: every row that does need work is reserved, moved, and committed
    /// in one pass rather than two `Storage::update` cycles each.
    #[test]
    #[serial_test::serial]
    fn profile_sweep_heals_every_row_that_needs_it() {
        if !git_available() {
            return;
        }
        let _guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
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

        assert!(
            reconcile_trashed_profile("default").unwrap().is_empty(),
            "the sweep is idempotent"
        );
    }

    #[test]
    fn reconcile_skips_markerless_row_already_in_holding() {
        // A trashed worktree that already lives in the holding area but lost
        // its marker must not be relocated again (which would nest it under
        // .aoe-trash/.aoe-trash/<id>).
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        inst.trash();
        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
            RelocateOutcome::Relocated { .. }
        ));
        let holding = inst.project_path.clone();
        // Drop the marker: the row now points at the holding path with no record.
        inst.pre_trash_project_path = None;

        assert!(
            !reconcile_trashed_location(&mut inst),
            "a markerless row already in holding must be left alone"
        );
        assert_eq!(inst.project_path, holding);
        assert!(!PathBuf::from(&holding).join(".aoe-trash").exists());
    }

    #[test]
    fn reconcile_heals_to_holding_when_original_recreated() {
        // Crash case: worktree already moved to the holding path, but the
        // marker was lost and the original path was recreated. Reconcile must
        // point at the existing holding worktree and record the marker, not
        // retry the (now-failing) move and leave project_path on the recreated
        // original.
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
        let holding = inst.project_path.clone();

        // Lost persist + recreated original.
        inst.project_path = original.clone();
        inst.pre_trash_project_path = None;
        std::fs::create_dir_all(&original).unwrap();

        assert!(
            reconcile_trashed_location(&mut inst),
            "reconcile should heal to the existing holding path"
        );
        assert_eq!(inst.project_path, holding);
        assert_eq!(
            inst.pre_trash_project_path.as_deref(),
            Some(original.as_str())
        );
    }

    #[test]
    fn reconcile_heals_pointer_after_lost_persist() {
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
        let holding = inst.project_path.clone();

        // Simulate the crash-after-move window: the durable row still points at
        // the (now-missing) original and never recorded the marker.
        inst.project_path = original.clone();
        inst.pre_trash_project_path = None;

        assert!(
            reconcile_trashed_location(&mut inst),
            "reconcile should heal the pointer to the holding area"
        );
        assert_eq!(inst.project_path, holding);
        assert_eq!(
            inst.pre_trash_project_path.as_deref(),
            Some(original.as_str())
        );
    }

    #[test]
    fn relocated_worktree_is_a_working_checkout() {
        // The structured-view preview and diff read the worktree at
        // project_path; after relocation that must still be a live git
        // worktree, not a detached directory.
        if !git_available() {
            return;
        }
        let (_tmp, mut inst) = real_worktree_instance();
        inst.trash();
        assert!(matches!(
            relocate_worktree_to_trash(&mut inst),
            RelocateOutcome::Relocated { .. }
        ));
        let status = std::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(&inst.project_path)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git status must work in the relocated worktree: {}",
            String::from_utf8_lossy(&status.stderr)
        );
    }

    #[test]
    fn purge_removes_relocated_worktree() {
        // Acceptance criterion: purging a trashed session deletes the worktree
        // at its relocated holding path, leaving nothing behind.
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

    /// Regression: a trashed worktree is relocated + re-locked, then its holding
    /// checkout is cleared out of band (a manual `.aoe-trash` cleanup, a partial
    /// prior delete) AND the session's stored `project_path` has diverged from
    /// git's registered path (a reconcile heal-back / lost persist). The
    /// worktree cleanup then can't unlock the locked entry by the stored path,
    /// and `git worktree prune` skips it, so the branch stays "used by worktree"
    /// and the purge used to fail with only a `Branch:` error, stranding the row
    /// in the trash forever. The scoped `delete_branch` self-heal must reap the
    /// entry git names for this branch and let the purge succeed.
    #[test]
    fn purge_recovers_when_project_path_diverged_and_locked_entry_survives() {
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

        // Divergence: the row now points back at the (gone) pre-move original,
        // while git's registered path for the still-locked entry is `holding`.
        inst.project_path = original;
        // Holding checkout removed out of band; the locked admin entry remains,
        // so a plain prune cannot reap it and the branch is still held.
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

    /// Regression (#the-d-key): trashing must run the sandbox container-stop
    /// step BEFORE relocating the worktree. Before the fix, `trash_session_by_id`
    /// only killed tmux and called `relocate_worktree_to_trash` directly, so a
    /// sandbox container was left running for the whole retention window and its
    /// live bind mount made this very relocation fail EBUSY. The container stop
    /// is injected here so the wiring/ordering is verified without a live docker
    /// runtime; a non-sandbox session exercises the happy path end to end.
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

    /// A sandboxed session hands `is_sandboxed = true` to the container-stop
    /// step. Uses a plain (non-worktree) session so the relocation short-circuits
    /// to `Skipped` without touching a real docker runtime; the seam still fires
    /// first, which is what proves the flag is wired through.
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

    /// A relocation that lands after the row was restored (the not-atomic
    /// window between the worker's still-trashed re-check and its move) is
    /// undone: the worktree moves back to the original path the live row
    /// points at.
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

        // The live row a raced restore produced: untrashed, pointing at the
        // original path, no relocation marker.
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

    /// The container-stop helper is a no-op (and never shells out) when the
    /// session is not sandboxed, so trashing a plain session stays docker-free.
    #[test]
    fn stop_sandbox_container_is_noop_when_not_sandboxed() {
        assert!(
            crate::session::worktree_edit::stop_sandbox_container("no-such-session", false).is_ok()
        );
    }
}
