//! Reconcile a managed worktree session's recorded `project_path` against git's own worktree
//! listing when the directory moved outside aoe.

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
    /// No unique live checkout of the branch was discoverable.
    Missing,
    /// More than one live worktree checks out the branch.
    Ambiguous(Vec<PathBuf>),
}

/// Pick the live worktree that owns `branch`, if there is exactly one.
fn select_live_worktree(
    entries: &[WorktreeEntry],
    branch: &str,
    main_repo: &Path,
) -> WorktreePathResolution {
    // The main worktree is never a candidate.
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
#[derive(Default)]
pub struct ReconcileCache(HashMap<String, Vec<WorktreeEntry>>);

impl ReconcileCache {
    /// The listing for `main_repo`, fetched once per pass.
    fn entries(&mut self, main_repo: &str) -> crate::git::error::Result<&[WorktreeEntry]> {
        if !self.0.contains_key(main_repo) {
            let git = GitWorktree::new(PathBuf::from(main_repo))?;
            self.0.insert(main_repo.to_string(), git.list_worktrees()?);
        }
        Ok(&self.0[main_repo])
    }
}

/// Resolve where `info`'s checkout is, given git's current worktree listing.
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

/// Reconcile one session: on [`WorktreePathResolution::Moved`], rewrite `inst.project_path` and
/// persist it, so every later path-derived decision (the rename pre-flight gates, attach, status,
/// diff) sees the live location.
pub fn reconcile_and_persist(
    storage: &Storage,
    inst: &mut Instance,
    cache: &mut ReconcileCache,
) -> anyhow::Result<WorktreePathResolution> {
    let Some(info) = inst.worktree_info.clone() else {
        return Ok(WorktreePathResolution::Current);
    };
    // A trashed session's directory belongs to [`crate::session::trash`], which relocates the
    // checkout into a holding dir and back and keeps its own pre-trash marker alongside
    // `project_path`.
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
                // Never adopt a checkout another session already records.
                if let Some(owner) = instances.iter().find(|c| {
                    c.id != id
                        && Path::new(&c.project_path).canonicalize().ok().as_deref()
                            == Some(found.as_path())
                }) {
                    claimed_by = Some(owner.id.clone());
                    return Ok(false);
                }
                // Compare and set: a peer process could have renamed or trashed this session while
                // the lookup ran, and its path is fresher than a location we resolved from the old
                // one.
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
                "one checkout under two spellings is not ambiguous",
                vec![entry(&live, Some("feat")), entry(&canon_live, Some("feat"))],
                WorktreePathResolution::Moved(canon_live.clone()),
            ),
            (
                "the main worktree on the branch is never selected",
                vec![entry(&main_repo, Some("feat"))],
                WorktreePathResolution::Missing,
            ),
            (
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
