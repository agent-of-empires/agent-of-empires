//! Reconcile a managed worktree session's recorded `project_path` against
//! git's own worktree listing when the directory moved outside aoe (#2002).
//!
//! A worktree session records the directory it was created in and nothing
//! syncs that string afterwards, so a `git worktree move` from another shell
//! leaves `project_path` naming a directory that no longer exists. git already
//! knows where the checkout went, keyed by branch, so the repair is a lookup
//! rather than new bookkeeping.
//!
//! Design notes:
//!   - **Triggered by absence.** The recorded path existing is treated as
//!     proof it is still the right one, so a healthy row costs one `stat` and
//!     never shells out. This does not catch a recorded path that survived as
//!     an unrelated directory; that is not the reported failure and paying a
//!     git listing per session per load to detect it is not worth it.
//!   - **Never guesses.** git normally forbids two worktrees on one branch, but
//!     a `--force`d or hand-edited repo can produce it. Two live candidates
//!     leave the row alone rather than picking one.
//!   - **Rewrites a pointer, nothing else.** Unlike the trash relocation in
//!     [`crate::session::trash`], which moves a directory and therefore needs a
//!     lifecycle reservation, this only corrects a string to match where git
//!     already says the checkout is. A plain `Storage::update` is enough.
//!   - **Reconcile before the caller's pre-flight, never inside the
//!     operation.** The rename path derives its duplicate-identity check and
//!     its sandbox-container release from `project_path` before calling
//!     `edit_worktree_workdir`. Healing inside that call would leave those
//!     gates computed from the stale path: a stale leaf that happens to equal
//!     the requested leaf makes `worktree_move_required` false, the container
//!     is never released, and the `git worktree move` then runs against a live
//!     bind mount.

use std::path::{Path, PathBuf};

use crate::git::{GitWorktree, WorktreeEntry};
use crate::session::storage::Storage;
use crate::session::{Instance, WorktreeInfo};

/// Where a managed worktree session's checkout actually is, relative to the
/// path the session recorded.
#[derive(Debug, PartialEq, Eq)]
pub enum WorktreePathResolution {
    /// The recorded path is present on disk, or the session is not an
    /// aoe-managed worktree. Nothing to reconcile.
    Current,
    /// Exactly one live worktree checks out the session's branch, at a
    /// different path than the one recorded.
    Moved(PathBuf),
    /// No unique live checkout of the branch was discoverable. Deliberately
    /// not phrased as "deleted": [`GitWorktree::list_worktrees`] omits linked
    /// entries whose path it cannot canonicalize, so a removed worktree, a
    /// dead registration, and a plain `mv` (which leaves git's record naming
    /// the old path) all land here indistinguishably. If absent and
    /// inaccessible ever need different handling, the upgrade path is to feed
    /// the selector a lossless `git worktree list --porcelain` inventory
    /// instead.
    Missing,
    /// More than one live worktree checks out the branch. The recorded path is
    /// left alone; picking one could point the session at another session's
    /// checkout.
    Ambiguous(Vec<PathBuf>),
}

/// Pick the live worktree that owns `branch`, if there is exactly one.
///
/// Split out from the git call so the selection rules are testable without a
/// repo. Never returns [`WorktreePathResolution::Current`]: the callers
/// short-circuit on a present recorded path before there is anything to select
/// between.
///
/// Canonicalizing doubles as the liveness filter, since it fails for a path
/// that is not there, and as the de-duplicator. Both matter:
/// `list_worktrees` canonicalizes linked worktrees but leaves the main
/// worktree's path as configured, so on macOS, where `/var` is a symlink to
/// `/private/var`, one checkout can arrive under two spellings and would
/// otherwise read as [`WorktreePathResolution::Ambiguous`].
fn select_live_worktree(entries: &[WorktreeEntry], branch: &str) -> WorktreePathResolution {
    let mut candidates: Vec<PathBuf> = entries
        .iter()
        .filter(|entry| !entry.is_detached && entry.branch.as_deref() == Some(branch))
        .filter_map(|entry| entry.path.canonicalize().ok())
        .collect();
    candidates.sort();
    candidates.dedup();

    match candidates.len() {
        0 => WorktreePathResolution::Missing,
        1 => WorktreePathResolution::Moved(candidates.remove(0)),
        _ => WorktreePathResolution::Ambiguous(candidates),
    }
}

/// Resolve where `info`'s checkout is, consulting git only when `recorded` is
/// absent from disk.
///
/// `Err` means git itself could not be consulted, which is deliberately
/// distinct from [`WorktreePathResolution::Missing`]: a broken repo must not be
/// logged as "the worktree is gone".
pub fn resolve_worktree_path(
    git: &GitWorktree,
    recorded: &Path,
    info: &WorktreeInfo,
) -> crate::git::error::Result<WorktreePathResolution> {
    if !info.managed_by_aoe || recorded.exists() {
        return Ok(WorktreePathResolution::Current);
    }
    Ok(select_live_worktree(&git.list_worktrees()?, &info.branch))
}

/// Reconcile one session: on [`WorktreePathResolution::Moved`], rewrite
/// `inst.project_path` and persist it, so every later path-derived decision
/// (the rename pre-flight gates, attach, status, diff) sees the live location.
///
/// Best-effort by design. Every non-`Moved` outcome, including a git failure,
/// leaves the row exactly as it was and logs why, mirroring
/// [`crate::session::trash::reconcile_trashed_location`]. Takes the `Storage`
/// rather than deriving one from `inst.source_profile`, which only the TUI
/// populates.
pub fn reconcile_and_persist(
    storage: &Storage,
    inst: &mut Instance,
) -> anyhow::Result<WorktreePathResolution> {
    let Some(info) = inst.worktree_info.clone() else {
        return Ok(WorktreePathResolution::Current);
    };
    let recorded = PathBuf::from(&inst.project_path);
    if !info.managed_by_aoe || recorded.exists() {
        return Ok(WorktreePathResolution::Current);
    }

    let git = GitWorktree::new(PathBuf::from(&info.main_repo_path))?;
    let resolution = resolve_worktree_path(&git, &recorded, &info)?;
    match &resolution {
        WorktreePathResolution::Moved(found) => {
            let id = inst.id.clone();
            let stale = inst.project_path.clone();
            let found_str = found.to_string_lossy().into_owned();
            // Compare and set. The git lookup runs without holding the storage
            // lock, so a peer process could have renamed or trashed this
            // session in the meantime; its path is fresher than ours and must
            // not be clobbered with a location we resolved from the old one.
            let applied = storage.update(|instances, _groups| {
                Ok(instances
                    .iter_mut()
                    .find(|c| c.id == id)
                    .is_some_and(|stored| {
                        let fresh = stored.project_path == stale;
                        if fresh {
                            stored.project_path = found_str.clone();
                        }
                        fresh
                    }))
            })?;
            if !applied {
                tracing::info!(
                    target: "session.worktree",
                    session = %inst.id,
                    "worktree path changed under the reconcile; keeping the newer record"
                );
                return Ok(WorktreePathResolution::Current);
            }
            inst.project_path = found.to_string_lossy().into_owned();
            tracing::info!(
                target: "session.worktree",
                session = %inst.id,
                branch = %info.branch,
                from = %recorded.display(),
                to = %found.display(),
                "reconciled worktree path from git after an external move"
            );
        }
        WorktreePathResolution::Missing => tracing::warn!(
            target: "session.worktree",
            session = %inst.id,
            branch = %info.branch,
            path = %recorded.display(),
            "recorded worktree path is gone and no live worktree checks out the branch; leaving it alone"
        ),
        WorktreePathResolution::Ambiguous(candidates) => tracing::warn!(
            target: "session.worktree",
            session = %inst.id,
            branch = %info.branch,
            candidates = ?candidates,
            "several live worktrees check out the branch; refusing to guess which one this session owns"
        ),
        WorktreePathResolution::Current => {}
    }
    Ok(resolution)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &Path, branch: Option<&str>) -> WorktreeEntry {
        WorktreeEntry {
            path: path.to_path_buf(),
            branch: branch.map(str::to_string),
            is_detached: false,
        }
    }

    #[test]
    fn select_live_worktree_never_guesses() {
        let dir = tempfile::tempdir().unwrap();
        let live = dir.path().join("live");
        let other = dir.path().join("other");
        let gone = dir.path().join("gone");
        std::fs::create_dir(&live).unwrap();
        std::fs::create_dir(&other).unwrap();
        let canon_live = live.canonicalize().unwrap();

        let cases = [
            (
                "the one live checkout of the branch is the new location",
                vec![entry(&live, Some("feat"))],
                WorktreePathResolution::Moved(canon_live.clone()),
            ),
            (
                "no entry for the branch",
                vec![entry(&live, Some("other-branch"))],
                WorktreePathResolution::Missing,
            ),
            (
                // A registration git kept but whose directory is gone (a plain
                // `mv`, or a reaped checkout) is not a candidate.
                "the branch's only entry no longer exists on disk",
                vec![entry(&gone, Some("feat"))],
                WorktreePathResolution::Missing,
            ),
            (
                "a detached checkout is never matched",
                vec![WorktreeEntry {
                    is_detached: true,
                    ..entry(&live, Some("feat"))
                }],
                WorktreePathResolution::Missing,
            ),
            (
                "branch comparison is exact, not case-folded",
                vec![entry(&live, Some("Feat"))],
                WorktreePathResolution::Missing,
            ),
            (
                "an entry with no readable branch is skipped",
                vec![entry(&live, None)],
                WorktreePathResolution::Missing,
            ),
            (
                "two live checkouts of one branch are left for a human",
                vec![entry(&live, Some("feat")), entry(&other, Some("feat"))],
                WorktreePathResolution::Ambiguous({
                    let mut both = vec![canon_live.clone(), other.canonicalize().unwrap()];
                    both.sort();
                    both
                }),
            ),
            (
                // `list_worktrees` canonicalizes linked worktrees but not the
                // main one, so one checkout can arrive under two spellings
                // wherever a parent is a symlink (every macOS tempdir).
                "one checkout under two spellings is not ambiguous",
                vec![entry(&live, Some("feat")), entry(&canon_live, Some("feat"))],
                WorktreePathResolution::Moved(canon_live.clone()),
            ),
        ];

        for (name, entries, expected) in cases {
            assert_eq!(select_live_worktree(&entries, "feat"), expected, "{name}");
        }
    }
}
