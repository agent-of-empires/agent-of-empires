//! Regression for #2653: `GitWorktree::branch_exists` fail-closed on
//! spawn / PATH / I/O errors.
//!
//! Lives in its own integration test binary (not in `src/git/worktree.rs`
//! nor in the shared `tests/integration/` binary) so its `PATH=""` seam
//! cannot race with concurrent git-spawning tests in the lib binary or
//! the integration binary. Each `tests/*.rs` file at the top level of
//! `tests/` gets its own process, so process-global env mutation here is
//! isolated by construction.

use agent_of_empires::git::error::GitError;
use agent_of_empires::git::GitWorktree;
use agent_of_empires::session::worktree_edit::{
    edit_worktree_workdir, WorktreeEditError, WorktreeEditRequest,
};
use agent_of_empires::session::WorktreeInfo;
use chrono::Utc;
use serial_test::serial;
use tempfile::TempDir;

fn setup_test_repo() -> (TempDir, git2::Repository) {
    let dir = TempDir::new().unwrap();
    let repo = git2::Repository::init(dir.path()).unwrap();
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let tree_id = {
        let mut index = repo.index().unwrap();
        index.write_tree().unwrap()
    };
    {
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();
    }
    (dir, repo)
}

/// A spawn / PATH / I/O failure inside `run_git` used to be swallowed
/// as `false` (via the `Err(_) => false` arm this PR eliminated),
/// letting callers apply mutations gated on a false negative. Locks
/// the check-failed branch of the tri-state contract (surfaces as
/// `Err`) at both the direct call and the `edit_worktree_workdir`
/// gate. The seam is a clobbered `PATH`: `Command::output()` fails
/// at `execve` with `ENOENT`, the same shape as a user without
/// `git` on `PATH`.
///
/// The `Ok(true)` and `Ok(false)` branches are locked by the
/// integration tests in `tests/integration/worktree_integration.rs`
/// (`edit_workdir_moves_dir_and_optionally_renames_branch`,
/// `tied_rename_moves_dir_to_title_leaf_without_touching_branch`,
/// `edit_workdir_rejects_invalid_cases_without_partial_changes`).
///
/// Any future test in this file that mutates `PATH` MUST use this
/// same `#[serial(path_env)]` slot; racing with another
/// `set_var("PATH", ...)` would poison this test's seam or vice
/// versa. Cross-file races are already prevented by the own-binary
/// isolation this file provides.
#[test]
#[serial(path_env)]
fn branch_exists_propagates_spawn_failure_and_caller_refuses() {
    // RAII guard: restores `PATH` on drop even if the test panics
    // mid-way. `#[serial(path_env)]` covers cross-test races in this
    // binary; this covers intra-test panic-safety so `PATH=""` cannot
    // leak past the test body under any control flow.
    struct PathGuard(Option<String>);
    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: env mutation is unsafe in the 2024 edition; the
            // `#[serial(path_env)]` slot serializes against other tests
            // in this binary, and `Drop` runs deterministically on this
            // test body.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    let (dir, _repo) = setup_test_repo();
    let current = dir.path().join("leaf");
    std::fs::create_dir(&current).unwrap();
    let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
    let info = WorktreeInfo {
        branch: "original".to_string(),
        main_repo_path: dir.path().to_string_lossy().to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    };

    let _guard = PathGuard(std::env::var("PATH").ok());
    // SAFETY: env mutation is unsafe in the 2024 edition; the
    // `#[serial(path_env)]` slot serializes against other tests in this
    // binary, and `_guard` restores `PATH` on drop even on panic.
    unsafe { std::env::set_var("PATH", "") };

    let direct = git_wt.branch_exists("any").unwrap_err();
    let caller = edit_worktree_workdir(WorktreeEditRequest {
        worktree_info: &info,
        current_path: &current,
        new_name: "renamed",
        rename_branch: true,
    })
    .unwrap_err();

    // Variant match alone is sufficient specificity: the only
    // subprocess call in either flow is `branch_exists`'s `run_git`
    // (`GitWorktree::new` uses libgit2), and with `PATH=""` its only
    // failure mode is `WorktreeCommandFailed`. Not asserting on the
    // message substring keeps the test resilient to future wording
    // harmonization.
    assert!(
        matches!(direct, GitError::WorktreeCommandFailed(_)),
        "direct call: expected WorktreeCommandFailed, got {direct:?}"
    );
    assert!(
        matches!(
            caller,
            WorktreeEditError::Git(GitError::WorktreeCommandFailed(_))
        ),
        "caller must refuse the rename and propagate, got {caller:?}"
    );
}
