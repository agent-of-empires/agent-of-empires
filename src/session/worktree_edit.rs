//! Post-create editing of a managed worktree session's workdir name.

use std::path::{Path, PathBuf};

use crate::containers::{DockerContainer, Probe, Teardown};
use crate::git::template::sanitize_branch_name;
use crate::git::GitWorktree;
use crate::session::builder::git_sanitize_branch_name;
use crate::session::WorktreeInfo;

/// Derive the worktree directory leaf for a tied session from its title.
pub fn worktree_leaf_from_title(title: &str) -> String {
    sanitize_branch_name(&crate::session::builder::branch_name_from_title(title))
}

/// The path [`edit_worktree_workdir`] would relocate the worktree to for `new_name`, or `None` when
/// `current_path` has no parent to rename within.
pub(crate) fn target_worktree_path(current_path: &Path, new_name: &str) -> Option<PathBuf> {
    let parent = current_path.parent()?;
    let new_leaf = sanitize_branch_name(&git_sanitize_branch_name(new_name));
    Some(parent.join(new_leaf))
}

/// Whether a workdir edit for `new_name` would actually move the worktree directory.
pub fn worktree_move_required(current_path: &Path, new_name: &str) -> bool {
    if new_name.trim().is_empty() {
        return false;
    }
    target_worktree_path(current_path, new_name).is_some_and(|p| p != current_path)
}

/// Whether a workdir edit for `new_name` would actually rename the git branch.
pub fn worktree_branch_rename_required(
    worktree_info: &WorktreeInfo,
    new_name: &str,
    rename_branch: bool,
) -> bool {
    rename_branch && git_sanitize_branch_name(new_name) != worktree_info.branch
}

/// Release a sandbox session's hold on its worktree directory ahead of a `git worktree move`, and
/// report whether the worktree is *still* held.
pub fn ensure_sandbox_container_released(session_id: &str, is_sandboxed: bool) -> bool {
    if !is_sandboxed {
        return false;
    }
    let container = DockerContainer::from_session_id(session_id);
    match container.probe_running() {
        Probe::Running => true,
        Probe::NotRunning => {
            // Stopped, but a surviving container still pins the bind mount.
            match container.discard_if_stopped() {
                Teardown::Removed => {
                    tracing::info!(
                        target: "containers.runtime",
                        session = %session_id,
                        "removed stopped sandbox container to release its bind mount before the worktree move"
                    );
                    false
                }
                Teardown::AlreadyGone => false,
                // Couldn't drop it, so assume it still holds the mount and fail the rename with a
                // real reason instead of letting git fail with a bare `Permission denied`.
                Teardown::Failed(e) => {
                    tracing::warn!(
                        target: "containers.runtime",
                        session = %session_id,
                        error = %e,
                        "failed to remove stopped sandbox container before the worktree move; reporting the worktree as held"
                    );
                    true
                }
            }
        }
        Probe::Unknown(e) => {
            tracing::warn!(
                target: "containers.runtime",
                session = %session_id,
                error = %e,
                "docker inspect failed while probing sandbox container for the worktree rename gate; failing closed and reporting the worktree as held to prevent a failed rename against a possibly-live container"
            );
            true
        }
    }
}

/// Drop a sandbox session's container after its worktree directory has been moved by a rename.
pub fn discard_sandbox_container_after_move(session_id: &str, is_sandboxed: bool) {
    if !is_sandboxed {
        return;
    }
    let container = DockerContainer::from_session_id(session_id);
    match container.discard() {
        Teardown::Removed => tracing::info!(
            target: "containers.runtime",
            session = %session_id,
            "removed stale sandbox container after worktree move; it will be recreated with the new path on next start"
        ),
        Teardown::AlreadyGone => {}
        Teardown::Failed(e) => tracing::warn!(
            target: "containers.runtime",
            session = %session_id,
            "failed to remove stale sandbox container after worktree move: {e}"
        ),
    }
}

/// Stop a sandbox session's container without removing it, so it can be restarted on re-attach.
pub fn stop_sandbox_container(session_id: &str, is_sandboxed: bool) -> anyhow::Result<()> {
    if !is_sandboxed {
        return Ok(());
    }
    let container = DockerContainer::from_session_id(session_id);
    match container.probe_running() {
        Probe::Running => container.stop()?,
        Probe::NotRunning => {}
        Probe::Unknown(e) => {
            tracing::warn!(
                target: "containers.runtime",
                session = %session_id,
                error = %e,
                "docker inspect failed while probing sandbox container before stop; attempting stop anyway to avoid leaving a possibly-live container behind"
            );
            if let Err(stop_err) = container.stop() {
                tracing::warn!(
                    target: "containers.runtime",
                    session = %session_id,
                    error = %stop_err,
                    "sandbox container stop failed after probe failure; container may already be gone or docker is unreachable"
                );
            }
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeEditError {
    #[error("{0}")]
    Ownership(String),
    #[error("this worktree is not managed by aoe; its workdir name cannot be edited")]
    NotManaged,
    #[error("the new workdir name is empty")]
    EmptyName,
    #[error("the workdir name is unchanged")]
    Unchanged,
    #[error("cannot determine the parent directory of {}", .0.display())]
    NoParent(PathBuf),
    #[error("the current worktree directory {} does not exist", .0.display())]
    SourceMissing(PathBuf),
    #[error("a directory already exists at {}", .0.display())]
    TargetExists(PathBuf),
    #[error("branch '{0}' already exists")]
    BranchExists(String),
}

pub(crate) fn ensure_worktree_unshared(
    ownership: &super::storage::OwnershipGuard,
    profile: &str,
    id: &str,
    path: &Path,
    info: &WorktreeInfo,
    destination: Option<&Path>,
    renames_branch: bool,
) -> Result<(), WorktreeEditError> {
    if destination.is_some() || renames_branch {
        let claims = super::deletion::paths_in_use_except_with_ownership(ownership, profile, &[id]);
        if claims.covers(path) || destination.is_some_and(|target| claims.covers(target)) {
            return Err(WorktreeEditError::Ownership(
                "Another session claims the worktree source or destination, or its owners could not be checked".to_string(),
            ));
        }
    }
    if renames_branch {
        let in_use = super::deletion::branch_in_use_with_ownership(
            ownership,
            profile,
            &[id],
            Path::new(&info.main_repo_path),
            &info.branch,
        )
        .map_err(|error| WorktreeEditError::Ownership(error.to_string()))?;
        if in_use {
            return Err(WorktreeEditError::Ownership(
                "Another session shares this branch".to_string(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn prepare_worktree_branch_rename(
    info: &WorktreeInfo,
    path: &Path,
    branch: &str,
) -> anyhow::Result<Option<GitWorktree>> {
    let git = revalidate_worktree_branch_rename(info, path, branch)?;
    if let Some(git) = &git {
        if git.branch_exists(branch)? {
            return Err(WorktreeEditError::BranchExists(branch.to_owned()).into());
        }
    }
    Ok(git)
}

pub(crate) fn revalidate_worktree_branch_rename(
    info: &WorktreeInfo,
    path: &Path,
    branch: &str,
) -> anyhow::Result<Option<GitWorktree>> {
    anyhow::ensure!(
        info.managed_by_aoe,
        "Branch-only rename requires a managed worktree"
    );
    anyhow::ensure!(
        !branch.is_empty() && branch.trim() == branch && !branch.starts_with('-'),
        "Provide an exact Git branch name"
    );
    anyhow::ensure!(
        git2::Reference::is_valid_name(&format!("refs/heads/{branch}")),
        "Provide an exact valid Git branch name"
    );
    let git = GitWorktree::new(PathBuf::from(&info.main_repo_path))?;
    let canonical = path.canonicalize()?;
    anyhow::ensure!(
        Path::new(&info.main_repo_path).canonicalize()? != canonical,
        "The main checkout cannot be renamed as a task"
    );
    let worktrees = git.list_worktrees()?;
    anyhow::ensure!(
        worktrees.iter().any(
            |wt| wt.path.canonicalize().ok().as_ref() == Some(&canonical)
                && wt.branch.as_deref() == Some(&info.branch)
        ),
        "Worktree path or branch no longer matches session metadata"
    );
    anyhow::ensure!(
        GitWorktree::get_current_branch(path)? == info.branch,
        "Worktree branch changed; refresh session metadata before renaming"
    );
    if branch == info.branch {
        return Ok(None);
    }
    let protected = git.protected_default_branch_names()?;
    anyhow::ensure!(
        !["main", "master"].contains(&info.branch.as_str()) && !protected.contains(&info.branch),
        "The default branch cannot be renamed as a task"
    );
    anyhow::ensure!(
        !worktrees.iter().any(|wt| {
            wt.branch.as_deref() == Some(&info.branch)
                && wt.path.canonicalize().ok().as_ref() != Some(&canonical)
        }),
        "Source branch is shared by another Git worktree"
    );
    if git.branch_ref_exists(branch)? {
        return Err(WorktreeEditError::BranchExists(branch.to_owned()).into());
    }
    Ok(Some(git))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    #[test]
    #[serial_test::serial]
    fn worktree_move_refuses_same_id_owner_in_another_profile() {
        for restore_destination in [false, true] {
            let app = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(app.path());
            crate::session::config::update_config(|config| {
                config.session.tie_workdir_to_name = false;
            })
            .unwrap();
            let dirs = tempfile::tempdir().unwrap();
            let main = dirs.path().join("repo");
            let path = dirs.path().join("checkout");
            let repo = git2::Repository::init(&main).unwrap();
            let signature = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
                .unwrap();
            let git = GitWorktree::new(main.clone()).unwrap();
            let base = repo.head().unwrap().shorthand().unwrap().to_owned();
            git.create_worktree("feature", &path, true, Some(&base))
                .unwrap();
            let info = wt_info("feature", &main.to_string_lossy(), true);
            let mut original = crate::session::Instance::new("original", &path.to_string_lossy());
            original.worktree_info = Some(info.clone());
            original.status = crate::session::Status::Stopped;
            let id = original.id.clone();
            let a = crate::session::Storage::new_unwatched("rename-a").unwrap();
            let b = crate::session::Storage::new_unwatched("rename-b").unwrap();
            a.update(|rows, _| {
                rows.push(original.clone());
                Ok(())
            })
            .unwrap();
            let mut peer = original;
            if restore_destination {
                let holding = dirs.path().join("holding");
                git.create_worktree("peer", &holding, true, Some(&base))
                    .unwrap();
                peer.project_path = holding.to_string_lossy().into_owned();
                peer.worktree_info = Some(wt_info("peer", &main.to_string_lossy(), true));
                peer.trashed_at = Some(Utc::now());
                peer.pre_trash_project_path =
                    Some(dirs.path().join("moved").to_string_lossy().into_owned());
            }
            b.update(|rows, _| {
                rows.push(peer);
                Ok(())
            })
            .unwrap();
            let mut baseline = a
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == id)
                .unwrap();
            baseline.source_profile = a.profile().to_owned();
            let request = || crate::session::rename::Request {
                baseline: baseline.clone(),
                edit: crate::session::rename::Edit::Workdir {
                    name: "moved".to_owned(),
                    rename_branch: true,
                },
                file_watch: a.file_watch(),
            };
            let error = crate::session::rename::perform(request(), |_, _| Ok(()))
                .err()
                .expect("shared source or destination must be rejected");
            assert!(matches!(
                error.downcast_ref::<WorktreeEditError>(),
                Some(WorktreeEditError::Ownership(_))
            ));
            assert!(path.is_dir());
            assert!(!dirs.path().join("moved").exists());
            assert!(git.branch_exists("feature").unwrap());
            assert!(!git.branch_exists("moved").unwrap());
            b.update(|rows, _| {
                rows.clear();
                Ok(())
            })
            .unwrap();
            let outcome = crate::session::rename::perform(request(), |_, _| Ok(())).unwrap();
            let new_path = PathBuf::from(&outcome.instance.project_path);
            assert_eq!(new_path, dirs.path().join("moved"));
            assert!(!path.exists());
            assert_eq!(GitWorktree::get_current_branch(&new_path).unwrap(), "moved");
        }
    }

    fn wt_info(branch: &str, main_repo: &str, managed: bool) -> WorktreeInfo {
        WorktreeInfo {
            branch: branch.to_string(),
            main_repo_path: main_repo.to_string(),
            managed_by_aoe: managed,
            created_at: Utc::now(),
            base_branch: None,
        }
    }

    #[test]
    fn worktree_move_required_only_when_the_leaf_changes() {
        let cur = Path::new("/repos/wt/feature-login");

        assert!(worktree_move_required(cur, "feature-logout"));

        assert!(!worktree_move_required(cur, "feature-login"));

        assert!(!worktree_move_required(cur, "feature/login"));

        assert!(worktree_move_required(cur, "Feature Login"));

        assert!(!worktree_move_required(cur, ""));
        assert!(!worktree_move_required(cur, "   "));

        assert!(!worktree_move_required(Path::new("/"), "anything"));
    }

    // `worktree_move_required` must agree with `edit_worktree_workdir`'s own `path_changes`
    // decision, since it exists purely to predict it.
    #[test]
    fn worktree_move_required_agrees_with_the_target_path_it_gates() {
        let cur = Path::new("/repos/wt/feature-login");
        for name in [
            "feature-logout",
            "feature-login",
            "feature/login",
            "Feature Login",
            "wild  name",
        ] {
            let target = target_worktree_path(cur, name).expect("has a parent");
            assert_eq!(
                worktree_move_required(cur, name),
                target != cur,
                "disagreement for {name:?}: target resolved to {}",
                target.display()
            );
        }
    }

    #[test]
    fn worktree_branch_rename_required_matches_the_sanitized_branch_change() {
        let info = wt_info("feature/login", "/tmp/repo", true);
        let cases = [
            ("feature/logout", false, false),
            ("feature/logout", true, true),
            ("feature/login", true, false),
            ("feature/login~", true, false),
        ];
        for (name, rename_branch, expected) in cases {
            assert_eq!(
                worktree_branch_rename_required(&info, name, rename_branch),
                expected,
                "name={name:?} rename_branch={rename_branch}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn discard_after_move_short_circuits_without_sandbox() {
        use std::os::unix::fs::PermissionsExt;
        let home = crate::session::test_support::isolate_app_dir();
        let bin = home.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let marker = home.path().join("runtime-calls");
        let _env =
            crate::session::test_support::EnvGuard::set(&[("AOE_TEST_RUNTIME_CALLS", &marker)]);
        for binary in ["docker", "podman", "container"] {
            let script = bin.join(binary);
            std::fs::write(
                &script,
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$AOE_TEST_RUNTIME_CALLS\"\nexit 0\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let _path = crate::session::test_support::path_prepended(&bin);
        discard_sandbox_container_after_move("worktree-discard-probe", false);
        assert!(
            !marker.exists(),
            "non-sandbox move invoked the native runtime"
        );
        discard_sandbox_container_after_move("worktree-discard-probe", true);
        let calls = std::fs::read_to_string(marker).unwrap();
        assert!(
            calls.lines().any(|line| line.starts_with("rm ")),
            "sandbox control must reach the runtime removal command: {calls}"
        );
    }

    #[test]
    fn leaf_from_title_is_one_safe_non_empty_component() {
        for (title, leaf) in [
            ("Auth refactor", "auth-refactor"),
            ("Fix: the/thing (v2)", "fix-the-thing-v2"),
            ("jacob/feature-1", "jacob-feature-1"),
            ("...", "session"),
            ("   ", "session"),
            ("../escape", "escape"),
        ] {
            assert_eq!(worktree_leaf_from_title(title), leaf, "{title:?}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn canonical_workdir_rename_rejects_invalid_requests() {
        for (managed, name, expected) in [
            (false, "new", WorktreeEditError::NotManaged),
            (true, "   ", WorktreeEditError::EmptyName),
            (true, "old", WorktreeEditError::Unchanged),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(&temp.path().join("home"));
            crate::session::config::update_config(|config| {
                config.session.tie_workdir_to_name = false;
            })
            .unwrap();
            let main = temp.path().join("repo");
            let path = temp.path().join("old");
            let repo = git2::Repository::init(&main).unwrap();
            let signature = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree = repo
                .find_tree(repo.index().unwrap().write_tree().unwrap())
                .unwrap();
            repo.commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
                .unwrap();
            let base = repo.head().unwrap().shorthand().unwrap().to_owned();
            let git = GitWorktree::new(main.clone()).unwrap();
            git.create_worktree("old", &path, true, Some(&base))
                .unwrap();
            let storage = crate::session::Storage::new_unwatched("edit").unwrap();
            let mut row = crate::session::Instance::new("original", &path.to_string_lossy());
            row.source_profile = storage.profile().to_owned();
            row.status = crate::session::Status::Stopped;
            row.worktree_info = Some(wt_info("old", &main.to_string_lossy(), managed));
            storage
                .update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })
                .unwrap();
            let error = crate::session::rename::perform(
                crate::session::rename::Request {
                    baseline: row,
                    edit: crate::session::rename::Edit::Workdir {
                        name: name.to_owned(),
                        rename_branch: false,
                    },
                    file_watch: storage.file_watch(),
                },
                |_, _| Ok(()),
            )
            .err()
            .expect("invalid edit must be rejected");
            assert_eq!(
                std::mem::discriminant(error.downcast_ref::<WorktreeEditError>().unwrap()),
                std::mem::discriminant(&expected)
            );
            assert!(path.is_dir());
            assert_eq!(GitWorktree::get_current_branch(&path).unwrap(), "old");
            assert!(storage.load().unwrap()[0].lifecycle_reservation.is_none());
        }
    }
}
