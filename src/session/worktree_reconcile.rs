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

fn already_current(inst: &Instance) -> bool {
    inst.worktree_info.as_ref().is_none_or(|info| {
        inst.is_trashed() || !info.managed_by_aoe || Path::new(&inst.project_path).exists()
    })
}

/// Reconcile one session: on [`WorktreePathResolution::Moved`], rewrite `inst.project_path` and
/// persist it, so every later path-derived decision (the rename pre-flight gates, attach, status,
/// diff) sees the live location.
pub fn reconcile_and_persist(
    storage: &Storage,
    inst: &mut Instance,
    cache: &mut ReconcileCache,
) -> anyhow::Result<WorktreePathResolution> {
    if already_current(inst) {
        return Ok(WorktreePathResolution::Current);
    }
    let ownership = super::storage::acquire_ownership_lock()?;
    let _identity = super::storage::acquire_session_identity_lock_with_ownership(&ownership)?;
    let _lifecycle =
        storage.acquire_instance_lifecycle_lock_with_ownership(&ownership, &inst.id)?;
    reconcile_and_persist_with_ownership(storage, &ownership, inst, cache)
}

/// The caller already holds exclusive ownership, identity and this session's lifecycle lock.
/// Do not reacquire them here: rename uses this variant inside its own fenced operation.
pub(crate) fn reconcile_and_persist_with_ownership(
    storage: &Storage,
    ownership: &super::storage::OwnershipGuard,
    inst: &mut Instance,
    cache: &mut ReconcileCache,
) -> anyhow::Result<WorktreePathResolution> {
    ownership.require_exclusive()?;
    if already_current(inst) {
        return Ok(WorktreePathResolution::Current);
    }
    let info = inst
        .worktree_info
        .as_ref()
        .expect("managed reconciliation has worktree info");
    let recorded = PathBuf::from(&inst.project_path);
    if super::deletion::resolve_claim_path(&recorded).is_none() {
        return Ok(WorktreePathResolution::Current);
    }

    let resolution = resolve_worktree_path(cache.entries(&info.main_repo_path)?, &recorded, info);
    match &resolution {
        WorktreePathResolution::Moved(found) => {
            let id = inst.id.clone();
            let stale = inst.project_path.clone();
            let new_path = found.to_string_lossy().into_owned();
            let paths = super::deletion::paths_in_use_except_with_ownership(
                ownership,
                storage.profile(),
                &[&id],
            );
            if paths.covers(found) {
                return Ok(WorktreePathResolution::Current);
            }
            let applied = storage.update_with_ownership(ownership, |instances, _groups| {
                // The UI loader is forgiving; never commit a claim from an incomplete inventory.
                storage.load_strict_for_worktree_ownership()?;
                let Some(stored) = instances.iter_mut().find(|c| c.id == id) else {
                    return Ok(false);
                };
                if stored.project_path != stale
                    || stored.worktree_info.as_ref() != Some(info)
                    || stored.is_trashed()
                    || stored.has_fresh_lifecycle_reservation(chrono::Utc::now())
                {
                    return Ok(false);
                }
                stored.project_path = new_path.clone();
                Ok(true)
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

    fn git_at(repo: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn moved_checkout_fixture() -> (tempfile::TempDir, Instance, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let old = temp.path().join("old");
        let moved = temp.path().join("moved");
        std::fs::create_dir(&repo).unwrap();
        git_at(&repo, &["init"]);
        git_at(
            &repo,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-m",
                "init",
            ],
        );
        git_at(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "feature/reconcile",
                old.to_str().unwrap(),
            ],
        );
        let mut row = Instance::new("owner", old.to_str().unwrap());
        row.worktree_info = Some(WorktreeInfo {
            branch: "feature/reconcile".into(),
            main_repo_path: repo.to_string_lossy().into_owned(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        git_at(
            &repo,
            &[
                "worktree",
                "move",
                old.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        (temp, row, moved)
    }

    #[test]
    #[serial_test::serial]
    fn ownership_review_current_returns_while_an_unrelated_root_lease_is_held() {
        let _home = crate::session::test_support::isolate_app_dir();
        let temp = tempfile::tempdir().unwrap();
        let existing = temp.path().join("exists");
        std::fs::create_dir(&existing).unwrap();
        let storage = Storage::new_unwatched("current").unwrap();
        for case in ["ordinary", "unmanaged", "trashed", "present"] {
            let mut row = Instance::new(case, &temp.path().join("missing").to_string_lossy());
            if case != "ordinary" {
                row.worktree_info = Some(WorktreeInfo {
                    branch: "feature".into(),
                    main_repo_path: temp.path().to_string_lossy().into_owned(),
                    managed_by_aoe: case != "unmanaged",
                    created_at: chrono::Utc::now(),
                    base_branch: None,
                });
            }
            if case == "trashed" {
                row.trash();
            }
            if case == "present" {
                row.project_path = existing.to_string_lossy().into_owned();
            }
            let ownership = super::super::storage::acquire_ownership_lock().unwrap();
            std::thread::scope(|scope| {
                let (tx, rx) = std::sync::mpsc::channel();
                let storage = &storage;
                let worker = scope.spawn(move || {
                    let result = reconcile_and_persist(storage, &mut row, &mut Default::default());
                    tx.send(result).unwrap();
                });
                let result = rx.recv_timeout(std::time::Duration::from_secs(2));
                drop(ownership);
                worker.join().unwrap();
                assert_eq!(
                    result.expect(case).unwrap(),
                    WorktreePathResolution::Current,
                    "{case}"
                );
            });
        }
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_refuses_cross_profile_and_pretrash_claims() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (_temp, row, moved) = moved_checkout_fixture();
        let storage = Storage::new_unwatched("owner").unwrap();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        for pretrash in [false, true] {
            let mut other = Instance::new("peer", moved.to_str().unwrap());
            other.id = row.id.clone();
            if pretrash {
                other.pre_trash_project_path = Some(other.project_path.clone());
                other.project_path = "/holding".into();
                other.trash();
            }
            peer.update(|rows, _| {
                *rows = vec![other];
                Ok(())
            })
            .unwrap();
            let mut local = row.clone();
            assert_eq!(
                reconcile_and_persist(&storage, &mut local, &mut Default::default()).unwrap(),
                WorktreePathResolution::Current
            );
            assert_eq!(local.project_path, row.project_path);
            assert_eq!(storage.load().unwrap()[0].project_path, row.project_path);
        }
        std::fs::write(peer.sessions_path(), br#"[{"id":"opaque"}]"#).unwrap();
        let mut local = row.clone();
        assert_eq!(
            reconcile_and_persist(&storage, &mut local, &mut Default::default()).unwrap(),
            WorktreePathResolution::Current
        );
        assert_eq!(storage.load().unwrap()[0].project_path, row.project_path);
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_commit_gate_checks_fresh_row_and_holds_all_claim_locks() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (_temp, row, moved) = moved_checkout_fixture();
        let storage = Storage::new_unwatched("owner").unwrap();
        for change in ["reservation", "trash", "worktree-info", "path", "unchanged"] {
            storage
                .update(|rows, _| {
                    *rows = vec![row.clone()];
                    Ok(())
                })
                .unwrap();
            let mut fresh = row.clone();
            match change {
                "reservation" => {
                    fresh
                        .try_acquire_lifecycle_reservation(
                            crate::session::LifecycleOperation::Restore,
                            Instance::LIFECYCLE_RESERVATION_TTL,
                            chrono::Utc::now(),
                        )
                        .unwrap();
                }
                "trash" => fresh.trash(),
                "worktree-info" => fresh.worktree_info.as_mut().unwrap().branch = "changed".into(),
                "path" => fresh.project_path = "/newer-path".into(),
                _ => {}
            }
            let path = storage.sessions_path().to_path_buf();
            let app = crate::session::get_app_dir().unwrap();
            let id = row.id.clone();
            let entered = std::rc::Rc::new(std::cell::Cell::new(false));
            let seen = entered.clone();
            let observer = super::super::storage::observe_updates_for_test(move |_| {
                assert!(!seen.replace(true));
                let root = std::fs::File::open(app.join(".workspace-claim.lock")).unwrap();
                assert_eq!(
                    fs2::FileExt::try_lock_shared(&root).unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock
                );
                for lock in [
                    app.join(".title-mutation.lock"),
                    path.parent()
                        .unwrap()
                        .join(format!(".instance-lifecycle-{id}.lock")),
                ] {
                    let file = std::fs::File::open(lock).unwrap();
                    assert_eq!(
                        fs2::FileExt::try_lock_exclusive(&file).unwrap_err().kind(),
                        std::io::ErrorKind::WouldBlock
                    );
                }
                // Gate a durable row replacement between inventory and the authoritative CAS read.
                std::fs::write(&path, serde_json::to_vec(&vec![fresh.clone()]).unwrap()).unwrap();
            });
            let mut local = row.clone();
            let result =
                reconcile_and_persist(&storage, &mut local, &mut Default::default()).unwrap();
            drop(observer);
            assert!(entered.get());
            if change == "unchanged" {
                assert_eq!(
                    result,
                    WorktreePathResolution::Moved(moved.canonicalize().unwrap())
                );
                assert_eq!(
                    Path::new(&storage.load().unwrap()[0].project_path)
                        .canonicalize()
                        .unwrap(),
                    moved.canonicalize().unwrap()
                );
            } else {
                assert_eq!(result, WorktreePathResolution::Current);
                assert_eq!(local.project_path, row.project_path);
                let stored = storage.load().unwrap().remove(0);
                if change == "path" {
                    assert_eq!(stored.project_path, "/newer-path");
                } else {
                    assert_eq!(stored.project_path, row.project_path);
                }
            }
        }
    }

    #[test]
    #[serial_test::serial]
    fn reconciliation_accepts_an_unclaimed_checkout_inside_rename_owned_locks() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (_temp, mut row, moved) = moved_checkout_fixture();
        let storage = Storage::new_unwatched("owner").unwrap();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let ownership = super::super::storage::acquire_ownership_lock().unwrap();
        let _identity =
            super::super::storage::acquire_session_identity_lock_with_ownership(&ownership)
                .unwrap();
        let _lifecycle = storage
            .acquire_instance_lifecycle_lock_with_ownership(&ownership, &row.id)
            .unwrap();
        assert_eq!(
            reconcile_and_persist_with_ownership(
                &storage,
                &ownership,
                &mut row,
                &mut Default::default()
            )
            .unwrap(),
            WorktreePathResolution::Moved(moved.canonicalize().unwrap())
        );
        assert_eq!(
            Path::new(&storage.load().unwrap()[0].project_path)
                .canonicalize()
                .unwrap(),
            moved.canonicalize().unwrap()
        );
    }

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
