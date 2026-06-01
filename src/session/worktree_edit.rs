//! Post-create editing of a managed worktree session's workdir name.
//!
//! A session created in worktree mode bakes its directory name (and,
//! optionally, its branch) at creation time. This module performs the
//! in-place edit the user asks for later: move the worktree directory to a
//! new leaf name and, when opted in, rename the underlying git branch.
//!
//! Design notes (see #1723):
//!   - The new directory is a *sibling-leaf* rename: we keep the existing
//!     parent directory and only swap the final path component. We do NOT
//!     recompute the path from the current config template, because the
//!     random session-id seed used at creation is unrecoverable and the
//!     template may have drifted since, either of which would silently
//!     relocate the session somewhere unexpected.
//!   - Branch rename is opt-in. A session may have already done meaningful
//!     work on its branch (commits, an upstream), so renaming the branch is
//!     a separate, explicit choice from renaming the workdir directory.
//!   - Ordering is branch-rename first, then `git worktree move`. The
//!     filesystem move is the more failure-prone step (open handles, locks),
//!     so it goes last where a best-effort rollback of the branch rename is
//!     a cheap ref operation.

use std::path::{Path, PathBuf};

use crate::git::error::GitError;
use crate::git::template::sanitize_branch_name;
use crate::git::GitWorktree;
use crate::session::builder::git_sanitize_branch_name;
use crate::session::WorktreeInfo;

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
///
/// On success the git side effects (optional branch rename, directory move)
/// have already been applied; the returned [`WorktreeEditOutcome`] carries
/// the values the caller must persist to storage and in-memory state. On
/// error nothing is left partially applied: a failed directory move rolls
/// back any branch rename performed in the same call.
pub fn edit_worktree_workdir(
    req: WorktreeEditRequest,
) -> Result<WorktreeEditOutcome, WorktreeEditError> {
    if !req.worktree_info.managed_by_aoe {
        return Err(WorktreeEditError::NotManaged);
    }
    if req.new_name.trim().is_empty() {
        return Err(WorktreeEditError::EmptyName);
    }

    // The new branch name uses the same git-ref sanitizer as creation; the
    // directory leaf uses the path-safe sanitizer (slashes become dashes),
    // mirroring how `resolve_template` derives a leaf from a branch.
    let new_branch = git_sanitize_branch_name(req.new_name);
    let new_leaf = sanitize_branch_name(&new_branch);

    let parent = req
        .current_path
        .parent()
        .ok_or_else(|| WorktreeEditError::NoParent(req.current_path.to_path_buf()))?;
    let new_path = parent.join(&new_leaf);

    let branch_changes = req.rename_branch && new_branch != req.worktree_info.branch;
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
    if branch_changes && git.branch_exists(&new_branch) {
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
                    // The repo is now on `new_branch` with the directory still
                    // at its old path. Surface both failures so the caller does
                    // not treat this as a clean "move failed, nothing changed".
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
    fn rejects_unmanaged_worktree() {
        let info = wt_info("old", "/tmp/repo", false);
        let err = edit_worktree_workdir(WorktreeEditRequest {
            worktree_info: &info,
            current_path: Path::new("/tmp/wt/old"),
            new_name: "new",
            rename_branch: false,
        })
        .unwrap_err();
        assert!(matches!(err, WorktreeEditError::NotManaged));
    }

    #[test]
    fn rejects_empty_name() {
        let info = wt_info("old", "/tmp/repo", true);
        let err = edit_worktree_workdir(WorktreeEditRequest {
            worktree_info: &info,
            current_path: Path::new("/tmp/wt/old"),
            new_name: "   ",
            rename_branch: false,
        })
        .unwrap_err();
        assert!(matches!(err, WorktreeEditError::EmptyName));
    }

    #[test]
    fn rejects_unchanged_name_without_branch_rename() {
        // Leaf derived from "old" is "old", so the path does not change and
        // branch rename is off: nothing would happen.
        let info = wt_info("old", "/tmp/repo", true);
        let err = edit_worktree_workdir(WorktreeEditRequest {
            worktree_info: &info,
            current_path: Path::new("/tmp/wt/old"),
            new_name: "old",
            rename_branch: false,
        })
        .unwrap_err();
        assert!(matches!(err, WorktreeEditError::Unchanged));
    }
}
