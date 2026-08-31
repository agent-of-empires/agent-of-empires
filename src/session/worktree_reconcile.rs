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
//!     leave the row alone rather than picking one. A lone candidate that
//!     another session already records is refused for the same reason: git
//!     only forbids the second worktree while the first registration is live,
//!     so a pruned session's branch can legally be taken by a later checkout.
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

use std::collections::HashMap;
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
fn select_live_worktree(
    entries: &[WorktreeEntry],
    branch: &str,
    main_repo: &Path,
) -> WorktreePathResolution {
    // The main worktree is never a candidate. `list_worktrees` reports it like
    // any other entry, and it can legitimately end up on the session's branch
    // once the linked checkout is gone (`git worktree unlock` plus `prune` plus
    // `git checkout`, or a single `git checkout --ignore-other-worktrees`).
    // Repointing a managed session there would hand its agent the user's
    // primary checkout to work in. Same canonicalize-and-compare guard
    // `crate::git::cleanup::remove_worktree_dir` applies before deleting a
    // directory, down to falling back on the path as written when it cannot
    // be canonicalized, so an unresolvable main repo still excludes itself.
    let main = main_repo
        .canonicalize()
        .unwrap_or_else(|_| main_repo.to_path_buf());
    let mut candidates: Vec<PathBuf> = entries
        .iter()
        .filter(|entry| !entry.is_detached && entry.branch.as_deref() == Some(branch))
        .filter_map(|entry| entry.path.canonicalize().ok())
        .filter(|path| path != &main)
        .collect();
    candidates.sort();
    candidates.dedup();

    match candidates.len() {
        0 => WorktreePathResolution::Missing,
        1 => WorktreePathResolution::Moved(candidates.remove(0)),
        _ => WorktreePathResolution::Ambiguous(candidates),
    }
}

/// One pass's worth of `git worktree list` results, keyed by main repo path.
///
/// [`GitWorktree::list_worktrees`] opens the repo once for the listing and
/// again per registered worktree (`get_current_branch`), so N broken sessions
/// sharing a repo that holds M worktrees would otherwise cost N*(M+1) libgit2
/// opens. The TUI pays that synchronously before its first paint, and a
/// worktree-heavy repo makes M large, so the whole sweep shares one cache.
#[derive(Default)]
pub struct ReconcileCache(HashMap<String, Vec<WorktreeEntry>>);

impl ReconcileCache {
    /// The listing for `main_repo`, fetched once per pass.
    ///
    /// A failure is not cached: it is nearly always "this repo is unreachable
    /// right now", and the next session in the same repo should get a fresh
    /// attempt rather than inherit a stale verdict.
    fn entries(&mut self, main_repo: &str) -> crate::git::error::Result<&[WorktreeEntry]> {
        if !self.0.contains_key(main_repo) {
            let git = GitWorktree::new(PathBuf::from(main_repo))?;
            self.0.insert(main_repo.to_string(), git.list_worktrees()?);
        }
        Ok(&self.0[main_repo])
    }
}

/// Resolve where `info`'s checkout is, given git's current worktree listing.
///
/// Takes the entries rather than a [`GitWorktree`] so one listing can serve
/// every session in a repo; obtaining it is [`ReconcileCache`]'s job, which
/// keeps the "git could not be consulted" failure distinct from
/// [`WorktreePathResolution::Missing`] by surfacing it before this is called.
pub fn resolve_worktree_path(
    entries: &[WorktreeEntry],
    recorded: &Path,
    info: &WorktreeInfo,
) -> WorktreePathResolution {
    if !info.managed_by_aoe || recorded.exists() {
        return WorktreePathResolution::Current;
    }
    select_live_worktree(entries, &info.branch, Path::new(&info.main_repo_path))
}

/// Reconcile one session: on [`WorktreePathResolution::Moved`], rewrite
/// `inst.project_path` and persist it, so every later path-derived decision
/// (the rename pre-flight gates, attach, status, diff) sees the live location.
///
/// Best-effort by design. Every non-`Moved` outcome, including a git failure,
/// leaves the row exactly as it was and logs why, mirroring
/// [`crate::session::trash::reconcile_trashed_location`]. Takes the `Storage`
/// rather than deriving one from `inst.source_profile`: `Storage::load` does
/// not set that field, so on the CLI it is empty, and an empty profile resolves
/// to the *default* profile rather than failing.
pub fn reconcile_and_persist(
    storage: &Storage,
    inst: &mut Instance,
    cache: &mut ReconcileCache,
) -> anyhow::Result<WorktreePathResolution> {
    let Some(info) = inst.worktree_info.clone() else {
        return Ok(WorktreePathResolution::Current);
    };
    // A trashed session's directory belongs to [`crate::session::trash`], which
    // relocates the checkout into a holding dir and back and keeps its own
    // pre-trash marker alongside `project_path`. Both surfaces that run this
    // pass run the trash reconcile first, and repointing a row it owns (or one
    // whose relocation it just failed to complete) would fight it.
    if inst.is_trashed() {
        return Ok(WorktreePathResolution::Current);
    }
    let recorded = PathBuf::from(&inst.project_path);
    if !info.managed_by_aoe || recorded.exists() {
        return Ok(WorktreePathResolution::Current);
    }

    let resolution = resolve_worktree_path(cache.entries(&info.main_repo_path)?, &recorded, &info);
    match &resolution {
        WorktreePathResolution::Moved(found) => {
            let id = inst.id.clone();
            let stale = inst.project_path.clone();
            let new_path = found.to_string_lossy().into_owned();
            // Both guards below need the storage lock the git lookup ran
            // without, so they live inside the update rather than beside it.
            let mut claimed_by: Option<String> = None;
            let applied = storage.update(|instances, _groups| {
                // Never adopt a checkout another session already records. git
                // forbids a second worktree on a branch only while the first
                // registration is live, so once this session's entry is pruned
                // a fresh checkout of the branch is legal and the branch-keyed
                // lookup lands this row on it. Two rows naming one directory
                // means trashing or deleting the stale one takes the live
                // one's checkout with it, so leave the stale path alone.
                if let Some(owner) = instances.iter().find(|c| {
                    c.id != id
                        && Path::new(&c.project_path).canonicalize().ok().as_deref()
                            == Some(found.as_path())
                }) {
                    claimed_by = Some(owner.id.clone());
                    return Ok(false);
                }
                // Compare and set: a peer process could have renamed or
                // trashed this session while the lookup ran, and its path is
                // fresher than a location we resolved from the old one.
                let Some(stored) = instances.iter_mut().find(|c| c.id == id) else {
                    return Ok(false);
                };
                if stored.project_path != stale {
                    return Ok(false);
                }
                stored.project_path = new_path.clone();
                Ok(true)
            })?;
            if let Some(owner) = claimed_by {
                tracing::warn!(
                    target: "session.worktree",
                    session = %inst.id,
                    branch = %info.branch,
                    owner = %owner,
                    candidate = %found.display(),
                    "the only live checkout of the branch already belongs to another session; refusing to adopt it"
                );
                return Ok(WorktreePathResolution::Current);
            }
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

/// Reconcile every session in one profile against git's worktree listing.
///
/// Returns true when a row was repointed, so the caller can refresh. One
/// [`ReconcileCache`] is shared across the pass, and a healthy row costs one
/// `stat`, so an untouched profile spawns no git at all.
///
/// Storage is opened unwatched: the callers that sweep a whole profile are
/// background workers whose writes the view picks up from the returned verdict
/// rather than from a local-change notification.
///
/// BLOCKING: opens repos and stats every recorded worktree. Never call it on an
/// event loop or the async runtime.
pub fn reconcile_profile(profile: &str) -> bool {
    let storage = match Storage::open_unwatched(profile) {
        Ok(storage) => storage,
        Err(error) => {
            tracing::warn!(
                target: "session.worktree",
                profile = %profile,
                "worktree path reconciliation skipped: {error}",
            );
            return false;
        }
    };
    let Ok(mut instances) = storage.load() else {
        return false;
    };
    let mut cache = ReconcileCache::default();
    let mut changed = false;
    for instance in &mut instances {
        match reconcile_and_persist(&storage, instance, &mut cache) {
            Ok(WorktreePathResolution::Moved(_)) => changed = true,
            Ok(_) => {}
            Err(error) => tracing::warn!(
                target: "session.worktree",
                session = %instance.id,
                "worktree path reconciliation skipped: {error}",
            ),
        }
    }
    changed
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
        let main_repo = dir.path().join("main-repo");
        std::fs::create_dir(&live).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::fs::create_dir(&main_repo).unwrap();
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
            (
                // The main repo can be left on the branch once the linked
                // checkout is gone. Selecting it would point the session at
                // the user's primary checkout.
                "the main worktree on the branch is never selected",
                vec![entry(&main_repo, Some("feat"))],
                WorktreePathResolution::Missing,
            ),
            (
                // Nor does excluding it turn a real relocation into an
                // ambiguity.
                "the main worktree does not make a real move ambiguous",
                vec![entry(&main_repo, Some("feat")), entry(&live, Some("feat"))],
                WorktreePathResolution::Moved(canon_live.clone()),
            ),
        ];

        for (name, entries, expected) in cases {
            assert_eq!(
                select_live_worktree(&entries, "feat", &main_repo),
                expected,
                "{name}"
            );
        }
    }
}
