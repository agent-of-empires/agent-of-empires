//! E2E tests for the `aoe project` registry CLI surface.
//!
//! Exercises `aoe project add`, `aoe project list`, and `aoe project remove`
//! against an isolated home, plus the `aoe add --project NAME` shortcut and
//! the cross-scope override guard.

use serial_test::serial;
use std::path::Path;
use std::process::Command;

use crate::harness::TuiTestHarness;

/// Initialize a directory as a git repo with one empty commit so
/// `aoe project add` accepts it.
fn init_git_repo(path: &Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git invocation");
        assert!(
            status.success(),
            "git {:?} failed in {}",
            args,
            path.display()
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "Test"]);
    run(&["commit", "--allow-empty", "-q", "-m", "init"]);
}

#[test]
#[serial]
fn test_project_add_list_remove_round_trip() {
    let h = TuiTestHarness::new("project_round_trip");
    let repo = h.home_path().join("repoA");
    init_git_repo(&repo);

    // 1. List on empty registry
    let list_empty = h.run_cli(&["project", "list"]);
    assert!(list_empty.status.success());
    let stdout = String::from_utf8_lossy(&list_empty.stdout);
    assert!(
        stdout.contains("No projects registered"),
        "expected empty-registry hint, got: {stdout}"
    );

    // 2. Add a project (default scope: global)
    let add = h.run_cli(&["project", "add", repo.to_str().unwrap()]);
    assert!(
        add.status.success(),
        "project add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let add_stdout = String::from_utf8_lossy(&add.stdout);
    assert!(
        add_stdout.contains("repoA") && add_stdout.contains("[global]"),
        "add output should include name and scope, got: {add_stdout}"
    );

    // 3. List shows it
    let list = h.run_cli(&["project", "list"]);
    assert!(list.status.success());
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("repoA") && list_stdout.contains("[global]"),
        "list output should show added project, got: {list_stdout}"
    );

    // 4. JSON output is valid and contains the entry
    let list_json = h.run_cli(&["project", "list", "--json"]);
    assert!(list_json.status.success());
    let json: serde_json::Value =
        serde_json::from_slice(&list_json.stdout).expect("invalid JSON output");
    let arr = json.as_array().expect("expected JSON array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "repoA");
    assert_eq!(arr[0]["scope"], "global");

    // 5. Remove (case-insensitive name match)
    let remove = h.run_cli(&["project", "remove", "REPOA"]);
    assert!(
        remove.status.success(),
        "remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );

    // 6. List is empty again
    let list_after = h.run_cli(&["project", "list"]);
    let after_stdout = String::from_utf8_lossy(&list_after.stdout);
    assert!(
        after_stdout.contains("No projects registered"),
        "expected empty after remove, got: {after_stdout}"
    );
}

#[test]
#[serial]
fn test_project_add_accepts_non_git_dir() {
    let h = TuiTestHarness::new("project_non_git");
    let plain = h.home_path().join("plain-dir");
    std::fs::create_dir_all(&plain).expect("create plain dir");

    let out = h.run_cli(&["project", "add", plain.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "expected non-git dir to be accepted, stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Registered project"),
        "expected registration confirmation, got: {stdout}"
    );
    assert!(
        stdout.contains("not a git repository"),
        "expected non-git note in output, got: {stdout}"
    );

    // It should now be listed.
    let list = h.run_cli(&["project", "list"]);
    assert!(list.status.success());
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("plain-dir"),
        "expected non-git project in list, got: {list_stdout}"
    );
}

#[test]
#[serial]
fn test_project_add_rejects_nonexistent_path() {
    let h = TuiTestHarness::new("project_nonexistent");
    let missing = h.home_path().join("does-not-exist");

    let out = h.run_cli(&["project", "add", missing.to_str().unwrap()]);
    assert!(
        !out.status.success(),
        "expected failure for nonexistent path, stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("does not exist or is not a directory"),
        "expected directory-validation message, got: {stderr}"
    );
}

#[test]
#[serial]
fn test_project_add_duplicate_within_scope() {
    let h = TuiTestHarness::new("project_dup_within_scope");
    let repo = h.home_path().join("repoB");
    init_git_repo(&repo);

    let first = h.run_cli(&["project", "add", repo.to_str().unwrap()]);
    assert!(first.status.success());

    let dup = h.run_cli(&["project", "add", repo.to_str().unwrap()]);
    assert!(
        !dup.status.success(),
        "duplicate add should fail, stdout: {}",
        String::from_utf8_lossy(&dup.stdout)
    );
    let stderr = String::from_utf8_lossy(&dup.stderr);
    assert!(
        stderr.contains("already registered"),
        "expected 'already registered' message, got: {stderr}"
    );
}

#[test]
#[serial]
fn test_project_cross_scope_override() {
    let h = TuiTestHarness::new("project_cross_scope_override");
    let repo = h.home_path().join("repoC");
    init_git_repo(&repo);

    // Add globally first.
    let add_global = h.run_cli(&["project", "add", repo.to_str().unwrap()]);
    assert!(add_global.status.success());

    // Re-add to profile without override -> error.
    let dup = h.run_cli(&[
        "project",
        "add",
        repo.to_str().unwrap(),
        "--scope",
        "profile",
    ]);
    assert!(!dup.status.success(), "cross-scope dup should fail");
    let stderr = String::from_utf8_lossy(&dup.stderr);
    assert!(
        stderr.contains("--allow-override"),
        "expected --allow-override hint, got: {stderr}"
    );

    // Re-add with override -> succeeds.
    let with_override = h.run_cli(&[
        "project",
        "add",
        repo.to_str().unwrap(),
        "--scope",
        "profile",
        "--allow-override",
    ]);
    assert!(
        with_override.status.success(),
        "override add failed: {}",
        String::from_utf8_lossy(&with_override.stderr)
    );

    // Merged listing shows one row (profile shadows global).
    let list = h.run_cli(&["project", "list"]);
    let list_stdout = String::from_utf8_lossy(&list.stdout);
    assert!(
        list_stdout.contains("[profile]"),
        "merged list should show the profile entry, got: {list_stdout}"
    );
}

#[test]
#[serial]
fn test_aoe_add_project_flag_requires_worktree() {
    let h = TuiTestHarness::new("project_add_requires_worktree");
    let primary = h.home_path().join("primary");
    let extra = h.home_path().join("extra");
    init_git_repo(&primary);
    init_git_repo(&extra);

    // Register the extra repo.
    let reg = h.run_cli(&["project", "add", extra.to_str().unwrap()]);
    assert!(reg.status.success());

    // Using --project without -w should fail with the worktree-required message.
    let out = h.run_cli(&["add", primary.to_str().unwrap(), "--project", "extra"]);
    assert!(!out.status.success(), "should fail without --worktree");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("--worktree") || stderr.contains("--project"),
        "expected message about --worktree requirement, got: {stderr}"
    );
}

#[test]
#[serial]
fn test_aoe_add_project_unknown_name_fails_fast() {
    let h = TuiTestHarness::new("project_unknown_name");
    let primary = h.home_path().join("primary2");
    init_git_repo(&primary);

    let out = h.run_cli(&[
        "add",
        primary.to_str().unwrap(),
        "--project",
        "ghost-project",
        "-w",
        "feat-branch",
        "-b",
    ]);
    assert!(
        !out.status.success(),
        "unknown --project name should fail fast"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("ghost-project") || stderr.contains("Unknown project"),
        "expected message naming the unknown project, got: {stderr}"
    );
}
