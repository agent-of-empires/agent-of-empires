//! Git spawn failures must not be reported as absent branches.
//! A separate test binary isolates its process-global PATH override.

use agent_of_empires::git::error::GitError;
use agent_of_empires::git::GitWorktree;
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

/// A missing Git executable must fail closed before mutation is authorized.
#[test]
#[serial(path_env)]
fn branch_exists_propagates_spawn_failure() {
    // Restore PATH even if the assertion panics.
    struct PathGuard(Option<String>);
    impl Drop for PathGuard {
        fn drop(&mut self) {
            // SAFETY: this binary serializes its environment-mutating tests.
            unsafe {
                match self.0.take() {
                    Some(v) => std::env::set_var("PATH", v),
                    None => std::env::remove_var("PATH"),
                }
            }
        }
    }

    let (dir, _repo) = setup_test_repo();
    let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();

    let _guard = PathGuard(std::env::var("PATH").ok());
    // SAFETY: the serial slot and guard isolate and restore this override.
    unsafe { std::env::set_var("PATH", "") };

    let direct = git_wt.branch_exists("any").unwrap_err();
    assert!(
        matches!(direct, GitError::WorktreeCommandFailed(_)),
        "direct call: expected WorktreeCommandFailed, got {direct:?}"
    );
}
