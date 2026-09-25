//! Post-create editing of a managed worktree session's workdir name.

use std::path::{Path, PathBuf};

use crate::containers::{DockerContainer, Probe, Teardown};
use crate::git::error::GitError;
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

/// The tied-rename destination directory for `title`, as a string: the path
/// [`edit_worktree_workdir`] would move `current_path` to, or `current_path` unchanged when it has
/// no parent to rename within.
pub(crate) fn derived_worktree_path(current_path: &Path, title: &str) -> String {
    let leaf = worktree_leaf_from_title(title);
    target_worktree_path(current_path, &leaf)
        .unwrap_or_else(|| current_path.to_path_buf())
        .to_string_lossy()
        .into_owned()
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

/// Inputs for an in-place worktree workdir edit.
pub struct WorktreeEditRequest<'a> {
    /// The session's current worktree metadata.
    pub worktree_info: &'a WorktreeInfo,
    /// The session's current `project_path` (the worktree directory).
    pub current_path: &'a Path,
    /// User-supplied new workdir name (raw; sanitized here).
    pub new_name: &'a str,
    /// Whether to also rename the git branch to match the new name.
    pub rename_branch: bool,
}

/// Result of a successful edit: the values the caller must persist.
#[derive(Debug)]
pub struct WorktreeEditOutcome {
    /// New worktree directory; assign to `Instance.project_path`.
    pub new_path: PathBuf,
    /// `Some(new_branch)` when the branch was renamed; assign to
    /// `worktree_info.branch`. `None` means the branch was left untouched.
    pub new_branch: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorktreeEditError {
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
    #[error(
        "worktree move failed ({move_err}), and rolling the branch rename back to '{branch}' also failed ({rollback_err}); the repo may be left on the new branch"
    )]
    RollbackFailed {
        move_err: String,
        rollback_err: String,
        branch: String,
    },
    #[error(transparent)]
    Git(#[from] GitError),
}

/// Validate and apply an in-place worktree workdir edit.
pub fn edit_worktree_workdir(
    req: WorktreeEditRequest,
) -> Result<WorktreeEditOutcome, WorktreeEditError> {
    if !req.worktree_info.managed_by_aoe {
        return Err(WorktreeEditError::NotManaged);
    }
    if req.new_name.trim().is_empty() {
        return Err(WorktreeEditError::EmptyName);
    }

    // The new branch name uses the same git-ref sanitizer as creation; the directory leaf uses the
    // path-safe sanitizer (slashes become dashes), mirroring how `resolve_template` derives a leaf
    // from a branch.
    let new_branch = git_sanitize_branch_name(req.new_name);

    let new_path = target_worktree_path(req.current_path, req.new_name)
        .ok_or_else(|| WorktreeEditError::NoParent(req.current_path.to_path_buf()))?;

    let branch_changes =
        worktree_branch_rename_required(req.worktree_info, req.new_name, req.rename_branch);
    let path_changes = new_path != req.current_path;
    if !branch_changes && !path_changes {
        return Err(WorktreeEditError::Unchanged);
    }

    let git = GitWorktree::new(PathBuf::from(&req.worktree_info.main_repo_path))?;

    if !req.current_path.exists() {
        return Err(WorktreeEditError::SourceMissing(
            req.current_path.to_path_buf(),
        ));
    }
    // fail-closed gate: swallowing `Err` as "absent" would clobber a branch that actually existed
    // or explode inside `rename_branch`.
    if branch_changes && git.branch_exists(&new_branch)? {
        return Err(WorktreeEditError::BranchExists(new_branch));
    }
    if path_changes && new_path.exists() {
        return Err(WorktreeEditError::TargetExists(new_path));
    }

    // Branch first: a ref rename is cheap to undo if the directory move
    // (the riskier step) then fails.
    let mut renamed_branch = false;
    if branch_changes {
        git.rename_branch(&req.worktree_info.branch, &new_branch)?;
        renamed_branch = true;
    }

    if path_changes {
        if let Err(e) = git.move_worktree(req.current_path, &new_path) {
            if renamed_branch {
                if let Err(rollback) = git.rename_branch(&new_branch, &req.worktree_info.branch) {
                    tracing::error!(
                        target: "git.worktree",
                        new = %new_branch,
                        old = %req.worktree_info.branch,
                        "worktree edit: branch-rename rollback failed after move error: {rollback}"
                    );
                    // The repo is now on `new_branch` with the directory still at its old path.
                    return Err(WorktreeEditError::RollbackFailed {
                        move_err: e.to_string(),
                        rollback_err: rollback.to_string(),
                        branch: new_branch.clone(),
                    });
                }
            }
            return Err(e.into());
        }
    }

    Ok(WorktreeEditOutcome {
        new_path,
        new_branch: renamed_branch.then_some(new_branch),
    })
}

/// Rename a managed branch without moving its directory or touching its files.
/// The caller holds the session identity and lifecycle locks and persists the result.
pub fn rename_worktree_branch(
    info: &WorktreeInfo,
    path: &Path,
    branch: &str,
) -> anyhow::Result<bool> {
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
        return Ok(false);
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
    anyhow::ensure!(
        !git.branch_exists(branch)?,
        "Target branch already exists: {branch}"
    );
    git.rename_branch(&info.branch, branch)?;
    Ok(true)
}

/// Roll back a branch rename only while the worktree still uses the renamed branch.
pub fn rollback_worktree_branch(
    info: &WorktreeInfo,
    path: &Path,
    renamed_branch: &str,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        GitWorktree::get_current_branch(path)? == renamed_branch,
        "Worktree branch changed concurrently; refusing branch rollback"
    );
    GitWorktree::new(PathBuf::from(&info.main_repo_path))?
        .rename_branch(renamed_branch, &info.branch)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

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
    fn edit_worktree_workdir_rejects_invalid_requests() {
        let cases = [
            (false, "new", WorktreeEditError::NotManaged),
            (true, "   ", WorktreeEditError::EmptyName),
            (true, "old", WorktreeEditError::Unchanged),
        ];
        for (managed, new_name, want) in cases {
            let info = wt_info("old", "/tmp/repo", managed);
            let err = edit_worktree_workdir(WorktreeEditRequest {
                worktree_info: &info,
                current_path: Path::new("/tmp/wt/old"),
                new_name,
                rename_branch: false,
            })
            .unwrap_err();
            assert_eq!(
                std::mem::discriminant(&err),
                std::mem::discriminant(&want),
                "{new_name:?}"
            );
        }
    }
}
