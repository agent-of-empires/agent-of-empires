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

/// Derive the worktree directory leaf for a tied session from its title.
///
/// Reuses the creation-time title slugger (`branch_name_from_title`) so a tied
/// rename produces the same leaf the session would have been created with: an
/// accent-folded, lowercased, dash-collapsed single path component. It never
/// yields an empty string, a path separator, or `.`/`..` (it falls back to
/// `"session"`), so the result is always a safe sibling-leaf name. Feeding it
/// back through [`edit_worktree_workdir`]'s internal sanitizer is idempotent.
pub fn worktree_leaf_from_title(title: &str) -> String {
    crate::session::builder::branch_name_from_title(title)
}

/// Whether a sandbox session's container is still running and therefore
/// bind-mounting its worktree directory.
///
/// A sandbox container runs `sleep infinity` for the life of the session
/// (`src/containers/runtime_base.rs`), so it holds the worktree dir as an
/// active mount source even while the agent is Idle. A `git worktree move`
/// then `rename(2)`s that dir and the kernel refuses with `EBUSY`, surfaced
/// as `fatal: failed to move`. Callers about to move the worktree must refuse
/// and have the user stop the session first, which tears the container down
/// and releases the mount. `is_sandboxed` is taken so non-sandbox sessions
/// skip the `docker inspect` subprocess entirely. See #1927 follow-up.
pub fn sandbox_container_holds_worktree(session_id: &str, is_sandboxed: bool) -> bool {
    is_sandboxed
        && crate::containers::DockerContainer::from_session_id(session_id)
            .is_running()
            .unwrap_or(false)
}

/// Drop a sandbox session's container after its worktree directory has been
/// moved by a rename.
///
/// A container's bind mounts and working dir are baked in at creation time
/// (`src/containers/runtime_base.rs`); they do NOT follow a host-side
/// `git worktree move`. `get_container_for_instance` reuses an existing
/// stopped container as-is, so without this the restarted container would
/// still mount (and `cd` into) the old path. Removing it here forces a fresh
/// `create` with the new path on next start. `remove(force)` drops only the
/// container and its anonymous volumes; named ignore volumes (node_modules,
/// target) are keyed by session id and survive for the recreated container.
///
/// No-op for non-sandbox sessions. The rename gate requires a stopped session
/// (see [`sandbox_container_holds_worktree`]), so the container is not running
/// here. Best-effort: a failure is logged, not surfaced, since the rename
/// itself has already succeeded. See #1927 follow-up.
pub fn discard_sandbox_container_after_move(session_id: &str, is_sandboxed: bool) {
    if !is_sandboxed {
        return;
    }
    let container = crate::containers::DockerContainer::from_session_id(session_id);
    match container.exists() {
        Ok(true) => match container.remove(true) {
            Ok(()) => tracing::info!(
                target: "containers.runtime",
                session = %session_id,
                "removed stale sandbox container after worktree move; it will be recreated with the new path on next start"
            ),
            Err(e) => tracing::warn!(
                target: "containers.runtime",
                session = %session_id,
                "failed to remove stale sandbox container after worktree move: {e}"
            ),
        },
        Ok(false) => {}
        Err(e) => tracing::warn!(
            target: "containers.runtime",
            session = %session_id,
            "could not check sandbox container existence after worktree move: {e}"
        ),
    }
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
    fn leaf_from_title_slugifies() {
        assert_eq!(worktree_leaf_from_title("Auth refactor"), "auth-refactor");
        assert_eq!(
            worktree_leaf_from_title("Fix: the/thing (v2)"),
            "fix-the-thing-v2"
        );
    }

    #[test]
    fn leaf_from_title_never_empty_or_traversal() {
        // Punctuation-only and dot titles fall back / collapse rather than
        // producing an empty leaf or a "."/".." path component.
        assert_eq!(worktree_leaf_from_title("..."), "session");
        assert_eq!(worktree_leaf_from_title("   "), "session");
        let leaf = worktree_leaf_from_title("../escape");
        assert!(!leaf.contains('/') && leaf != ".." && !leaf.is_empty());
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
