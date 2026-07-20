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
pub fn prepare_trashed_worktree(inst: &mut Instance) -> RelocateOutcome {
    prepare_trashed_worktree_with(
        inst,
        |id, is_sandboxed| {
            if let Err(e) = crate::session::worktree_edit::stop_sandbox_container(id, is_sandboxed)
            {
                tracing::warn!(
                    target: "session.trash",
                    session = %id,
                    "stopping sandbox container before trash relocation failed: {e}"
                );
            }
        },
        teardown_may_relocate,
    )
}

/// Whether the teardown still owns the durable row for `inst`. Consulted after
/// the (slow) container stop and immediately before the worktree move; see
/// [`prepare_trashed_worktree`]. The row must still read trashed AND not be
/// held by a fresh purge/restore claim that seized the teardown's Trash claim
/// (the teardown's own Trash claim, or no claim at all, passes). Fail-closed:
/// an unreadable storage, a missing row (purged by a peer), a restored row, or
/// a seized row all answer `false` and skip the move.
fn teardown_may_relocate(inst: &Instance) -> bool {
    let loaded = crate::session::Storage::open_unwatched(&inst.source_profile)
        .and_then(|storage| storage.load());
    match loaded {
        Ok(rows) => match rows.iter().find(|r| r.id == inst.id) {
            Some(row) if !row.is_trashed() => {
                tracing::info!(
                    target: "session.trash",
                    session = %inst.id,
                    "row was restored while the trash teardown was in flight; leaving the worktree in place"
                );
                false
            }
            Some(row) if row.is_seized_by_fresh_peer_claim(chrono::Utc::now()) => {
                tracing::info!(
                    target: "session.trash",
                    session = %inst.id,
                    claim = ?row.op_claim,
                    "a purge/restore claim seized the row mid-teardown; leaving the worktree in place"
                );
                false
            }
            Some(_) => true,
            None => {
                tracing::info!(
                    target: "session.trash",
                    session = %inst.id,
                    "row disappeared (purged) while the trash teardown was in flight; skipping relocation"
                );
                false
            }
        },
        Err(e) => {
            tracing::warn!(
                target: "session.trash",
                session = %inst.id,
                "could not re-check the durable row before trash relocation ({e}); leaving the worktree in place for the next reconcile pass"
            );
            false
        }
    }
}

fn prepare_trashed_worktree_with(
    inst: &mut Instance,
    stop_container: impl FnOnce(&str, bool),
    may_relocate: impl FnOnce(&Instance) -> bool,
) -> RelocateOutcome {
    stop_container(&inst.id, is_sandboxed(inst));
    if !may_relocate(inst) {
        return RelocateOutcome::Skipped;
    }
    relocate_worktree_to_trash(inst)
}

/// A request to run a freshly-trashed session's off-thread teardown: tmux
/// kill, sandbox container stop, and worktree relocation into the holding
/// area. Mirrors [`StopRequest`](crate::session::stop::StopRequest).
///
/// The container stop shells out to `docker stop`, which blocks for the
/// container's grace period (~10s; its PID-1 `sleep infinity` ignores
/// SIGTERM), so the TUI runs this on the `TrashPoller` worker thread instead of
/// the input thread. See [`perform_trash`].
pub struct TrashRequest {
    pub session_id: String,
    pub instance: Instance,
}

/// The worktree relocation a background trash-prepare produced, for the main
/// loop to persist. Only present when the move actually happened.
#[derive(Debug, Clone)]
pub struct TrashRelocation {
    /// The repointed worktree directory (now under the holding area).
    pub new_project_path: String,
    /// The original location, to persist as `pre_trash_project_path` so a
    /// later restore can move it back.
    pub pre_trash_project_path: Option<String>,
}

/// The outcome of a background trash-prepare, delivered back over the
/// `TrashPoller` channel. Mirrors [`StopResult`](crate::session::stop::StopResult).
#[derive(Debug)]
pub struct TrashResult {
    pub session_id: String,
    /// The relocation to persist, or `None` when nothing moved (`Skipped`) or
    /// the move could not run (`Failed`).
    pub relocation: Option<TrashRelocation>,
    /// Set when relocation could not run safely; surfaced as a `warn!` by the
    /// drain. The row stays durably trashed in place regardless; a later
    /// reconcile pass can move it.
    pub relocate_warning: Option<String>,
}

/// Run a trashed session's teardown off the caller's thread: kill its tmux
/// panes, stop its sandbox container, and relocate its worktree into the
/// holding area. Pure side effects on a cloned `Instance`; the caller persists
/// the returned [`TrashRelocation`] onto the real row.
///
/// This is the TUI's off-thread entry point (run on the `TrashPoller` worker),
/// the counterpart to [`perform_stop`](crate::session::stop::perform_stop) for
/// the stop path. The server runs the same `prepare_trashed_worktree` inside
/// `spawn_blocking` and the CLI runs it inline in a one-shot process; only the
/// TUI needs this wrapper, because only it has a UI thread to keep responsive.
pub fn perform_trash(request: &TrashRequest) -> TrashResult {
    let mut inst = request.instance.clone();
    // tmux teardown runs off-thread here for the same reason force_remove and
    // archive-group do it: N shellouts should not sit on the input thread.
    inst.kill_all_tmux_sessions();
    match prepare_trashed_worktree(&mut inst) {
        RelocateOutcome::Relocated { .. } => TrashResult {
            session_id: request.session_id.clone(),
            relocation: Some(TrashRelocation {
                new_project_path: inst.project_path.clone(),
                pre_trash_project_path: inst.pre_trash_project_path.clone(),
            }),
            relocate_warning: None,
        },
        RelocateOutcome::Skipped => TrashResult {
            session_id: request.session_id.clone(),
            relocation: None,
            relocate_warning: None,
        },
        RelocateOutcome::Failed { reason } => TrashResult {
            session_id: request.session_id.clone(),
            relocation: None,
            relocate_warning: Some(reason),
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
            prepare_trashed_worktree_with(
                &mut inst,
                move |_id, is_sandboxed| {
                    stop_calls.set(stop_calls.get() + 1);
                    saw_sandbox_flag.set(is_sandboxed);
                    original_present_at_stop.set(original.exists());
                },
                |_| true,
            )
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
            prepare_trashed_worktree_with(
                &mut inst,
                move |_id, is_sandboxed| {
                    saw_sandbox_flag.set(is_sandboxed);
                },
                |_| true,
            )
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

    /// Regression (#2930 follow-up): a restore that lands while the off-thread
    /// trash teardown is still running must not have the worktree moved out
    /// from under it. For a sandboxed session the teardown blocks ~10s in
    /// `docker stop` before the `git worktree move`, so a user who hits `d`
    /// and immediately restores wins that window: the durable row is untrashed
    /// (with no `pre_trash_project_path`, so the restore itself is a NoChange)
    /// while the worker still holds a trashed clone. The teardown must
    /// re-check the durable row before relocating and skip the move.
    #[test]
    #[serial_test::serial]
    fn teardown_skips_relocation_when_row_was_restored_mid_flight() {
        if !git_available() {
            return;
        }
        let _app = crate::session::test_support::isolate_app_dir();
        let (_tmp, mut inst) = real_worktree_instance();
        inst.source_profile = "default".to_string();
        let original = inst.project_path.clone();
        inst.trash();

        // The durable row was restored (untrashed) after the trash request was
        // queued: what the worker's clone says no longer holds.
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut durable = inst.clone();
        durable.untrash();
        storage
            .update(|rows, _groups| {
                rows.push(durable.clone());
                Ok(())
            })
            .unwrap();

        let result = perform_trash(&TrashRequest {
            session_id: inst.id.clone(),
            instance: inst.clone(),
        });

        assert!(
            result.relocation.is_none(),
            "a restored row's worktree must not be relocated: {:?}",
            result.relocation
        );
        assert!(
            PathBuf::from(&original).exists(),
            "the worktree must stay at its original path when a restore raced the teardown"
        );
    }

    /// A purge (or restore) that seized the teardown's Trash claim mid-flight
    /// owns the row: the teardown's pre-move re-check must observe the seized
    /// claim on the still-trashed durable row and leave the worktree in
    /// place for the claim owner to handle.
    #[test]
    #[serial_test::serial]
    fn teardown_skips_relocation_when_claim_was_seized_mid_flight() {
        if !git_available() {
            return;
        }
        let _app = crate::session::test_support::isolate_app_dir();
        let (_tmp, mut inst) = real_worktree_instance();
        inst.source_profile = "default".to_string();
        let original = inst.project_path.clone();
        inst.trash();

        // Durable row: still trashed, but a purge seized the Trash claim
        // while the teardown was stopping the container.
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut durable = inst.clone();
        durable
            .try_claim(
                crate::session::ClaimOp::Purge,
                Instance::OP_CLAIM_TTL,
                chrono::Utc::now(),
            )
            .unwrap();
        storage
            .update(|rows, _groups| {
                rows.push(durable.clone());
                Ok(())
            })
            .unwrap();

        let result = perform_trash(&TrashRequest {
            session_id: inst.id.clone(),
            instance: inst.clone(),
        });

        assert!(
            result.relocation.is_none(),
            "a seized row's worktree must not be relocated: {:?}",
            result.relocation
        );
        assert!(
            PathBuf::from(&original).exists(),
            "the worktree must stay in place for the claim owner"
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
