//! Integration tests for the diff view functionality

use std::fs;
use std::path::Path;
use tempfile::TempDir;

/// Create a test git repository with some initial content
fn setup_test_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let repo = git2::Repository::init(dir.path()).unwrap();

    // Create initial commit
    let sig = git2::Signature::now("Test", "test@example.com").unwrap();

    // Create a test file
    let file_path = dir.path().join("test.txt");
    fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

    // Add and commit
    {
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("test.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();
    }

    dir
}

mod git_diff {
    use super::*;
    use agent_of_empires::git::diff::{
        compute_changed_files, compute_file_diff, get_default_branch, list_branches, FileStatus,
    };

    #[test]
    fn test_empty_diff_on_fresh_repo() {
        let dir = setup_test_repo();
        let files = compute_changed_files(dir.path(), "HEAD").unwrap();
        assert!(files.is_empty(), "Expected no changes after initial commit");
    }

    #[test]
    fn test_modified_file_detected() {
        let dir = setup_test_repo();

        // Modify the file
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1 - modified\nline 2\nline 3\n").unwrap();

        let files = compute_changed_files(dir.path(), "HEAD").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Modified);
        assert_eq!(files[0].path.to_str().unwrap(), "test.txt");
    }

    #[test]
    fn test_added_file_detected() {
        let dir = setup_test_repo();

        // Add a new file
        let new_file = dir.path().join("new_file.txt");
        fs::write(&new_file, "new content\n").unwrap();

        let files = compute_changed_files(dir.path(), "HEAD").unwrap();
        // New untracked file should appear as Untracked
        assert!(files.iter().any(|f| f.status == FileStatus::Untracked));
    }

    #[test]
    fn test_deleted_file_detected() {
        let dir = setup_test_repo();

        // Delete the file
        let file_path = dir.path().join("test.txt");
        fs::remove_file(&file_path).unwrap();

        let files = compute_changed_files(dir.path(), "HEAD").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, FileStatus::Deleted);
    }

    #[test]
    fn test_file_diff_shows_changes() {
        let dir = setup_test_repo();

        // Modify the file
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1 - modified\nline 2\nnew line\nline 3\n").unwrap();

        let diff = compute_file_diff(dir.path(), Path::new("test.txt"), "HEAD", 3).unwrap();

        assert!(!diff.is_binary);
        assert!(!diff.hunks.is_empty());
        assert!(diff.file.additions > 0);
        assert!(diff.file.deletions > 0);
    }

    #[test]
    fn test_list_branches() {
        let dir = setup_test_repo();
        let repo = git2::Repository::open(dir.path()).unwrap();

        // Create another branch
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("feature-branch", &commit, false).unwrap();

        let branches = list_branches(dir.path()).unwrap();
        assert!(branches.len() >= 2);
        assert!(branches.iter().any(|b| b == "feature-branch"));
    }

    #[test]
    fn test_get_default_branch() {
        let dir = setup_test_repo();
        let branch = get_default_branch(dir.path()).unwrap();
        // Should return the current branch (master or main depending on git config)
        assert!(!branch.is_empty());
    }

    #[test]
    fn test_diff_with_context_lines() {
        let dir = setup_test_repo();

        // Create a file with more content
        let file_path = dir.path().join("test.txt");
        fs::write(
            &file_path,
            "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\nline 7\nline 8\n",
        )
        .unwrap();

        // Commit this
        let repo = git2::Repository::open(dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("test.txt")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let head = repo.head().unwrap();
            let parent = head.peel_to_commit().unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "More content", &tree, &[&parent])
                .unwrap();
        }

        // Now modify line 4 only
        fs::write(
            &file_path,
            "line 1\nline 2\nline 3\nline 4 - modified\nline 5\nline 6\nline 7\nline 8\n",
        )
        .unwrap();

        // Test with different context lines
        let diff_1 = compute_file_diff(dir.path(), Path::new("test.txt"), "HEAD", 1).unwrap();
        let diff_3 = compute_file_diff(dir.path(), Path::new("test.txt"), "HEAD", 3).unwrap();

        // More context should mean more lines in the hunk
        let lines_1: usize = diff_1.hunks.iter().map(|h| h.lines.len()).sum();
        let lines_3: usize = diff_3.hunks.iter().map(|h| h.lines.len()).sum();
        assert!(lines_3 >= lines_1, "More context should produce more lines");
    }
}

mod config {
    use agent_of_empires::session::config::{Config, DiffConfig};

    #[test]
    fn test_diff_config_defaults() {
        let config = DiffConfig::default();
        assert!(config.default_branch.is_none());
        assert_eq!(config.context_lines, 3);
    }

    #[test]
    fn test_diff_config_in_full_config() {
        let toml = r#"
            [diff]
            default_branch = "develop"
            context_lines = 5
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.diff.default_branch, Some("develop".to_string()));
        assert_eq!(config.diff.context_lines, 5);
    }

    #[test]
    fn test_diff_config_serialization() {
        let config = DiffConfig {
            default_branch: Some("main".to_string()),
            context_lines: 10,
            split_view: false,
        };

        let serialized = toml::to_string(&config).unwrap();
        let deserialized: DiffConfig = toml::from_str(&serialized).unwrap();

        assert_eq!(config.default_branch, deserialized.default_branch);
        assert_eq!(config.context_lines, deserialized.context_lines);
        assert_eq!(config.split_view, deserialized.split_view);
    }
}
