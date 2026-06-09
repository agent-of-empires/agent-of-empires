// Integration tests for git worktree functionality
// These tests verify end-to-end worktree workflows

use agent_of_empires::git::error::GitError;
use agent_of_empires::git::GitWorktree;
use agent_of_empires::session::{GroupTree, Instance, Storage, WorktreeInfo};
use chrono::Utc;
use serial_test::serial;
use tempfile::TempDir;

fn setup_test_environment() -> (TempDir, git2::Repository, TempDir) {
    let repo_dir = TempDir::new().unwrap();
    let repo = git2::Repository::init(repo_dir.path()).unwrap();

    let sig = git2::Signature::now("Test", "test@example.com").unwrap();
    let tree_id = {
        let mut index = repo.index().unwrap();
        index.write_tree().unwrap()
    };
    {
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial", &tree, &[])
            .unwrap();

        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("test-feature", &commit, false).unwrap();
    }

    let config_dir = TempDir::new().unwrap();

    (repo_dir, repo, config_dir)
}

#[test]
fn test_add_session_with_worktree_flag() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();

    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
    let wt_path = repo_dir.path().join("worktree-test-feature");

    git_wt
        .create_worktree("test-feature", &wt_path, false, None)
        .unwrap();

    let mut instance = Instance::new("Test Session", wt_path.to_str().unwrap());
    instance.worktree_info = Some(WorktreeInfo {
        branch: "test-feature".to_string(),
        main_repo_path: repo_dir.path().to_string_lossy().to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });

    assert!(wt_path.exists());
    assert!(instance.worktree_info.is_some());
    let info = instance.worktree_info.as_ref().unwrap();
    assert_eq!(info.branch, "test-feature");
    assert!(info.managed_by_aoe);
}

#[test]
fn test_session_has_worktree_info_after_creation() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();

    let mut instance = Instance::new("Test Session", repo_dir.path().to_str().unwrap());
    let now = Utc::now();

    instance.worktree_info = Some(WorktreeInfo {
        branch: "test-feature".to_string(),
        main_repo_path: repo_dir.path().to_string_lossy().to_string(),
        managed_by_aoe: true,
        created_at: now,
        base_branch: None,
    });

    let info = instance.worktree_info.as_ref().unwrap();
    assert_eq!(info.branch, "test-feature");
    assert_eq!(
        info.main_repo_path,
        repo_dir.path().to_string_lossy().to_string()
    );
    assert!(info.managed_by_aoe);
    assert_eq!(info.created_at, now);
}

// `#[serial]` because this test mutates the process-global `HOME` env var.
// Without serialization, parallel tests in the same binary that also set HOME
// will race with us.
#[test]
#[serial]
fn test_worktree_info_persists_across_save_load() {
    let temp_home = TempDir::new().unwrap();
    std::env::set_var("HOME", temp_home.path());

    let storage = Storage::new_unwatched("worktree-test-profile").unwrap();

    let mut instance = Instance::new("Worktree Session", "/tmp/test");
    instance.worktree_info = Some(WorktreeInfo {
        branch: "feature-branch".to_string(),
        main_repo_path: "/original/repo".to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });

    let seeded = vec![instance.clone()];
    storage
        .update(|i, g| {
            *i = seeded.to_vec();
            *g = GroupTree::new_with_groups(&seeded, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let loaded = storage.load().unwrap();
    assert_eq!(loaded.len(), 1);

    let loaded_info = loaded[0].worktree_info.as_ref().unwrap();
    assert_eq!(loaded_info.branch, "feature-branch");
    assert_eq!(loaded_info.main_repo_path, "/original/repo");
    assert!(loaded_info.managed_by_aoe);
}

#[test]
fn test_session_without_worktree_has_none_worktree_info() {
    let instance = Instance::new("Regular Session", "/tmp/project");

    assert!(instance.worktree_info.is_none());
}

#[test]
fn test_manual_worktree_detection() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();

    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
    let wt_path = repo_dir.path().join("detected-worktree");

    git_wt
        .create_worktree("test-feature", &wt_path, false, None)
        .unwrap();

    let worktrees = git_wt.list_worktrees().unwrap();

    assert!(worktrees.len() >= 2);

    let main_wt = worktrees.iter().find(|w| w.path == repo_dir.path());
    assert!(main_wt.is_some());

    let added_wt = worktrees.iter().find(|w| {
        w.branch
            .as_ref()
            .map(|b| b == "test-feature")
            .unwrap_or(false)
    });
    assert!(added_wt.is_some());
}

#[test]
fn test_worktree_cleanup_on_session_removal() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let worktree_container = TempDir::new().unwrap();

    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
    let wt_path = worktree_container.path().join("cleanup-worktree");

    git_wt
        .create_worktree("test-feature", &wt_path, false, None)
        .unwrap();
    assert!(wt_path.exists());

    let mut instance = Instance::new("Cleanup Session", wt_path.to_str().unwrap());
    instance.worktree_info = Some(WorktreeInfo {
        branch: "test-feature".to_string(),
        main_repo_path: repo_dir.path().to_string_lossy().to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });

    git_wt.remove_worktree(&wt_path, false).unwrap();

    assert!(!wt_path.exists());
}

#[test]
fn test_worktree_preserved_when_keep_flag_used() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let worktree_container = TempDir::new().unwrap();

    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
    let wt_path = worktree_container.path().join("keep-worktree");

    git_wt
        .create_worktree("test-feature", &wt_path, false, None)
        .unwrap();
    assert!(wt_path.exists());

    let mut instance = Instance::new("Keep Session", wt_path.to_str().unwrap());
    instance.worktree_info = Some(WorktreeInfo {
        branch: "test-feature".to_string(),
        main_repo_path: repo_dir.path().to_string_lossy().to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });

    assert!(wt_path.exists());
}

#[test]
fn test_error_when_worktree_already_exists() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();

    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
    let wt_path = repo_dir.path().join("duplicate-worktree");

    git_wt
        .create_worktree("test-feature", &wt_path, false, None)
        .unwrap();

    let result = git_wt.create_worktree("test-feature", &wt_path, false, None);

    assert!(result.is_err());
    match result.unwrap_err() {
        GitError::WorktreeAlreadyExists(path) => {
            assert_eq!(path, wt_path);
        }
        other => panic!("Expected WorktreeAlreadyExists, got {:?}", other),
    }
}

#[test]
fn test_error_when_branch_does_not_exist() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();

    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
    let wt_path = repo_dir.path().join("nonexistent-branch-worktree");

    let result = git_wt.create_worktree("nonexistent-branch", &wt_path, false, None);

    assert!(result.is_err());
    match result.unwrap_err() {
        GitError::BranchNotFound(branch) => {
            assert_eq!(branch, "nonexistent-branch");
        }
        other => panic!("Expected BranchNotFound, got {:?}", other),
    }
}

#[test]
fn test_create_new_branch_with_b_flag() {
    let (repo_dir, repo, _config_dir) = setup_test_environment();

    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
    let wt_path = repo_dir.path().join("new-branch-worktree");

    let branch_exists_before = repo
        .find_branch("brand-new-branch", git2::BranchType::Local)
        .is_ok();
    assert!(!branch_exists_before);

    git_wt
        .create_worktree("brand-new-branch", &wt_path, true, None)
        .unwrap();

    assert!(wt_path.exists());

    let branch_exists_after = repo
        .find_branch("brand-new-branch", git2::BranchType::Local)
        .is_ok();
    assert!(branch_exists_after);
}

// --- Edit-workdir-name (#1723) ---

use agent_of_empires::session::worktree_edit::{
    edit_worktree_workdir, WorktreeEditError, WorktreeEditRequest,
};

fn managed_info(branch: &str, repo: &std::path::Path) -> WorktreeInfo {
    WorktreeInfo {
        branch: branch.to_string(),
        main_repo_path: repo.to_string_lossy().to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    }
}

#[test]
#[serial]
fn edit_workdir_moves_dir_and_optionally_renames_branch() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();

    let old_path = repo_dir.path().join("old-name");
    git_wt
        .create_worktree("old-name", &old_path, true, None)
        .unwrap();
    assert!(old_path.exists());
    let info = managed_info("old-name", repo_dir.path());

    // Path-only edit: directory moves, branch untouched.
    let outcome = edit_worktree_workdir(WorktreeEditRequest {
        worktree_info: &info,
        current_path: &old_path,
        new_name: "fresh-name",
        rename_branch: false,
    })
    .unwrap();
    let fresh_path = repo_dir.path().join("fresh-name");
    assert_eq!(outcome.new_path, fresh_path);
    assert_eq!(outcome.new_branch, None);
    assert!(!old_path.exists());
    assert!(fresh_path.exists());
    assert!(git_wt.branch_exists("old-name"));

    // Branch rename opted in: directory moves and branch is renamed.
    let outcome2 = edit_worktree_workdir(WorktreeEditRequest {
        worktree_info: &info,
        current_path: &fresh_path,
        new_name: "renamed",
        rename_branch: true,
    })
    .unwrap();
    let renamed_path = repo_dir.path().join("renamed");
    assert_eq!(outcome2.new_branch.as_deref(), Some("renamed"));
    assert!(!fresh_path.exists());
    assert!(renamed_path.exists());
    assert!(git_wt.branch_exists("renamed"));
    assert!(!git_wt.branch_exists("old-name"));
}

// Tied workdir/title (#1927)

use agent_of_empires::session::worktree_edit::worktree_leaf_from_title;

#[test]
fn tie_workdir_applies_only_for_managed_worktrees() {
    // Non-worktree session: never tied, regardless of the setting.
    let scratch = Instance::new("Scratch", "/tmp/x");
    assert!(!scratch.tie_workdir_applies(true));

    // Managed worktree: tied follows the setting.
    let mut managed = Instance::new("S", "/tmp/wt");
    managed.worktree_info = Some(managed_info("b", std::path::Path::new("/tmp/repo")));
    assert!(managed.tie_workdir_applies(true));
    assert!(!managed.tie_workdir_applies(false));

    // Attached (unmanaged) worktree: never tied.
    let mut attached = Instance::new("S", "/tmp/wt");
    attached.worktree_info = Some(WorktreeInfo {
        managed_by_aoe: false,
        ..managed_info("b", std::path::Path::new("/tmp/repo"))
    });
    assert!(!attached.tie_workdir_applies(true));
}

#[test]
#[serial]
fn tied_rename_moves_dir_to_title_leaf_without_touching_branch() {
    // Models the surface flow: a tied rename derives the directory leaf from
    // the new title and moves the worktree, leaving the branch alone.
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();

    let old_path = repo_dir.path().join("byzantines");
    git_wt
        .create_worktree("byzantines", &old_path, true, None)
        .unwrap();
    let info = managed_info("byzantines", repo_dir.path());

    let leaf = worktree_leaf_from_title("Auth Refactor");
    assert_eq!(leaf, "auth-refactor");
    let outcome = edit_worktree_workdir(WorktreeEditRequest {
        worktree_info: &info,
        current_path: &old_path,
        new_name: &leaf,
        rename_branch: false,
    })
    .unwrap();

    let new_path = repo_dir.path().join("auth-refactor");
    assert_eq!(outcome.new_path, new_path);
    assert_eq!(outcome.new_branch, None);
    assert!(!old_path.exists());
    assert!(new_path.exists());
    // Branch is never swept in by a tied title rename.
    assert!(git_wt.branch_exists("byzantines"));
}

#[test]
#[serial]
fn tied_rename_into_occupied_leaf_keeps_title_and_dir_in_sync() {
    // A title whose leaf collides with a sibling directory fails the move, so
    // the surface keeps the old title too: the two never drift apart.
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();

    let old_path = repo_dir.path().join("aaa");
    git_wt
        .create_worktree("aaa", &old_path, true, None)
        .unwrap();
    let occupied = repo_dir.path().join("taken");
    git_wt
        .create_worktree("taken", &occupied, true, None)
        .unwrap();
    let info = managed_info("aaa", repo_dir.path());

    let leaf = worktree_leaf_from_title("Taken");
    assert_eq!(leaf, "taken");
    let err = edit_worktree_workdir(WorktreeEditRequest {
        worktree_info: &info,
        current_path: &old_path,
        new_name: &leaf,
        rename_branch: false,
    })
    .unwrap_err();
    assert!(matches!(err, WorktreeEditError::TargetExists(_)));
    assert!(old_path.exists());
}

#[test]
#[serial]
fn edit_workdir_rejects_invalid_cases_without_partial_changes() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();

    let old_path = repo_dir.path().join("aaa");
    git_wt
        .create_worktree("aaa", &old_path, true, None)
        .unwrap();
    let occupied = repo_dir.path().join("bbb");
    git_wt
        .create_worktree("bbb", &occupied, true, None)
        .unwrap();
    let info = managed_info("aaa", repo_dir.path());

    // Target directory already exists: nothing moves.
    let err = edit_worktree_workdir(WorktreeEditRequest {
        worktree_info: &info,
        current_path: &old_path,
        new_name: "bbb",
        rename_branch: false,
    })
    .unwrap_err();
    assert!(matches!(err, WorktreeEditError::TargetExists(_)));
    assert!(old_path.exists());

    // Branch rename onto an existing branch is rejected; the source
    // directory and branch are untouched.
    let err = edit_worktree_workdir(WorktreeEditRequest {
        worktree_info: &info,
        current_path: &old_path,
        new_name: "test-feature",
        rename_branch: true,
    })
    .unwrap_err();
    assert!(matches!(err, WorktreeEditError::BranchExists(_)));
    assert!(old_path.exists());
    assert!(git_wt.branch_exists("aaa"));

    // Unmanaged worktrees cannot be edited.
    let unmanaged = WorktreeInfo {
        managed_by_aoe: false,
        ..info.clone()
    };
    let err = edit_worktree_workdir(WorktreeEditRequest {
        worktree_info: &unmanaged,
        current_path: &old_path,
        new_name: "ccc",
        rename_branch: false,
    })
    .unwrap_err();
    assert!(matches!(err, WorktreeEditError::NotManaged));
    assert!(old_path.exists());
}
