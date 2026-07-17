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
    discard_sandbox_container_after_move, sandbox_container_holds_worktree,
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
    if sandbox_container_holds_worktree(&inst.id, is_sandboxed(inst)) {
        return RelocateOutcome::Failed {
            reason: "sandbox container is still running and holds the worktree".to_string(),
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
pub fn prepare_trashed_worktree(inst: &mut Instance) -> RelocateOutcome {
    prepare_trashed_worktree_with(inst, |id, is_sandboxed| {
        if let Err(e) = crate::session::worktree_edit::stop_sandbox_container(id, is_sandboxed) {
            tracing::warn!(
                target: "session.trash",
                session = %id,
                "stopping sandbox container before trash relocation failed: {e}"
            );
        }
    })
}

fn prepare_trashed_worktree_with(
    inst: &mut Instance,
    stop_container: impl FnOnce(&str, bool),
) -> RelocateOutcome {
    stop_container(&inst.id, is_sandboxed(inst));
    relocate_worktree_to_trash(inst)
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
    if sandbox_container_holds_worktree(&inst.id, is_sandboxed(inst)) {
        return RestoreOutcome::Failed {
            reason: "sandbox container is still running and holds the worktree".to_string(),
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
/// Best-effort and non-fatal: a git failure logs and leaves the row as-is.
pub fn reconcile_trashed_location(inst: &mut Instance) -> bool {
    if !inst.is_trashed() || !is_managed_single_worktree(inst) {
        return false;
    }
    let current = PathBuf::from(&inst.project_path);
    // The pre-trash location: the recorded marker if we have one, else the
    // current path (an un-relocated legacy row points at its own original).
    let original = inst
        .pre_trash_project_path
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(|| current.clone());
    let Some(target) = trash_holding_path(&original, &inst.id) else {
        return false;
    };

    if current.exists() {
        // Legacy backfill: a trashed managed worktree still sitting in the
        // active dir with no marker gets relocated now. An already-relocated
        // row (marker set, current == holding) is left alone, as is a
        // markerless row that already sits in the holding area (relocating it
        // again would nest it under .aoe-trash/.aoe-trash/<id>).
        if inst.pre_trash_project_path.is_none()
            && current != target
            && !is_holding_path(&current, &inst.id)
        {
            // Crash case: the worktree was already moved to `target` but the
            // marker/pointer persist was lost and something was recreated at
            // the original path. Retrying the move would fail (target exists)
            // and leave project_path on the wrong dir, so heal to the existing
            // holding path and record the marker. Restore can then fail
            // cleanly if the original stays occupied.
            if target.exists() {
                inst.project_path = target.to_string_lossy().into_owned();
                inst.pre_trash_project_path = Some(original.to_string_lossy().into_owned());
                tracing::info!(
                    target: "session.trash",
                    session = %inst.id,
                    to = %target.display(),
                    "reconciled trashed worktree pointer to existing holding area"
                );
                return true;
            }
            return match relocate_worktree_to_trash(inst) {
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
            };
        }
        return false;
    }

    // The recorded path is gone. Heal the pointer toward wherever the worktree
    // actually landed.
    if target.exists() {
        inst.project_path = target.to_string_lossy().into_owned();
        if inst.pre_trash_project_path.is_none() {
            inst.pre_trash_project_path = Some(original.to_string_lossy().into_owned());
        }
        tracing::info!(
            target: "session.trash",
            session = %inst.id,
            to = %target.display(),
            "reconciled trashed worktree pointer to holding area"
        );
        return true;
    }
    if original.exists() && original != current {
        inst.project_path = original.to_string_lossy().into_owned();
        inst.pre_trash_project_path = None;
        tracing::info!(
            target: "session.trash",
            session = %inst.id,
            to = %original.display(),
            "reconciled trashed worktree pointer back to original (holding move never landed)"
        );
        return true;
    }
    false
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

    /// The container-stop helper is a no-op (and never shells out) when the
    /// session is not sandboxed, so trashing a plain session stays docker-free.
    #[test]
    fn stop_sandbox_container_is_noop_when_not_sandboxed() {
        assert!(
            crate::session::worktree_edit::stop_sandbox_container("no-such-session", false).is_ok()
        );
    }
}
