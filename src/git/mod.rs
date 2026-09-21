//! Git repository and worktree operations.

use std::ffi::OsStr;
use std::path::Path;

pub mod cleanup;
pub(crate) mod command;
pub mod diff;
pub mod error;
mod remote;
pub mod template;
#[cfg(test)]
pub(crate) mod test_support;
mod worktree;

pub use remote::{
    clone_bare_repo, clone_repo, get_remote_owner, get_remote_owner_with_key, get_remote_slug,
    get_remote_url,
};
pub use worktree::{GitWorktree, WorktreeEntry};

/// Open the repository at `path` without searching parents, so an unrelated
/// ancestor repo (e.g. a dotfile-managed home) is never found.
pub(crate) fn open_repo_at(path: &Path) -> std::result::Result<git2::Repository, git2::Error> {
    git2::Repository::open_ext(
        path,
        git2::RepositoryOpenFlags::NO_SEARCH,
        std::iter::empty::<&OsStr>(),
    )
}
