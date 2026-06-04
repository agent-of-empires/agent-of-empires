use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GitError {
    #[error("Path is not in a git repository")]
    NotAGitRepo,

    #[error("Worktree already exists at {}", .0.display())]
    WorktreeAlreadyExists(PathBuf),

    #[error("Branch '{0}' is already in use by another worktree")]
    BranchAlreadyCheckedOut(String),

    #[error("Worktree not found at {}", .0.display())]
    WorktreeNotFound(PathBuf),

    #[error("Branch '{0}' not found")]
    BranchNotFound(String),

    #[error("Git error: {0}")]
    Git2Error(#[from] git2::Error),

    #[error("Git worktree command failed: {0}")]
    WorktreeCommandFailed(String),

    #[error("Git clone failed: {0}")]
    CloneFailed(String),

    #[error("IO error: {0}")]
    IoError(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, GitError>;
