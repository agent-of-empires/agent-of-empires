//! E2E tests for the `aoe project` registry CLI surface.
//!
//! Exercises `aoe project add`, `aoe project list`, and `aoe project remove`
//! against an isolated home, plus the `aoe add --project NAME` shortcut and
//! the cross-scope override guard.

use serial_test::parallel;

use crate::harness::{init_git_repo, TuiTestHarness};

#[test]
#[parallel]
fn test_project_add_list_remove_round_trip() {
    let h = TuiTestHarness::new("project_round_trip");
    let repo = h.home_path().join("repoA");
    init_git_repo(&repo);

    let empty = h.run_cli_ok(&["project", "list"]);
    assert!(empty.contains("No projects registered"), "{empty}");

    // Default scope is global.
    let added = h.run_cli_ok(&["project", "add", repo.to_str().unwrap()]);
    assert!(
        added.contains("repoA") && added.contains("[global]"),
        "{added}"
    );
    let listed = h.run_cli_ok(&["project", "list"]);
    assert!(
        listed.contains("repoA") && listed.contains("[global]"),
        "{listed}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&h.run_cli_ok(&["project", "list", "--json"])).expect("JSON output");
    let arr = json.as_array().expect("expected JSON array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "repoA");
    assert_eq!(arr[0]["scope"], "global");

    // Removal matches the name case-insensitively.
    h.run_cli_ok(&["project", "remove", "REPOA"]);
    let after = h.run_cli_ok(&["project", "list"]);
    assert!(after.contains("No projects registered"), "{after}");
}

#[test]
#[parallel]
fn test_project_add_accepts_non_git_dir() {
    let h = TuiTestHarness::new("project_non_git");
    let plain = h.home_path().join("plain-dir");
    std::fs::create_dir_all(&plain).expect("create plain dir");

    let stdout = h.run_cli_ok(&["project", "add", plain.to_str().unwrap()]);
    assert!(
        stdout.contains("Registered project") && stdout.contains("not a git repository"),
        "{stdout}"
    );
    assert!(h.run_cli_ok(&["project", "list"]).contains("plain-dir"));
}

#[test]
#[parallel]
fn test_project_add_rejects_nonexistent_path() {
    let h = TuiTestHarness::new("project_nonexistent");
    let missing = h.home_path().join("does-not-exist");

    let stderr = h.run_cli_err(&["project", "add", missing.to_str().unwrap()]);
    assert!(
        stderr.contains("does not exist or is not a directory"),
        "expected directory-validation message, got: {stderr}"
    );
}

#[test]
#[parallel]
fn test_project_add_duplicate_within_scope() {
    let h = TuiTestHarness::new("project_dup_within_scope");
    let repo = h.home_path().join("repoB");
    init_git_repo(&repo);

    h.run_cli_ok(&["project", "add", repo.to_str().unwrap()]);
    let stderr = h.run_cli_err(&["project", "add", repo.to_str().unwrap()]);
    assert!(
        stderr.contains("already registered"),
        "expected 'already registered' message, got: {stderr}"
    );
}

#[test]
#[parallel]
fn test_project_cross_scope_override() {
    let h = TuiTestHarness::new("project_cross_scope_override");
    let repo = h.home_path().join("repoC");
    init_git_repo(&repo);

    let repo = repo.to_str().unwrap();
    h.run_cli_ok(&["project", "add", repo]);

    let stderr = h.run_cli_err(&["project", "add", repo, "--scope", "profile"]);
    assert!(stderr.contains("--allow-override"), "{stderr}");

    h.run_cli_ok(&[
        "project",
        "add",
        repo,
        "--scope",
        "profile",
        "--allow-override",
    ]);
    // The profile entry shadows the global one in the merged listing.
    let listed = h.run_cli_ok(&["project", "list"]);
    assert!(listed.contains("[profile]"), "{listed}");
}

#[test]
#[parallel]
fn test_aoe_add_project_flag_requires_worktree() {
    let h = TuiTestHarness::new("project_add_requires_worktree");
    let primary = h.home_path().join("primary");
    let extra = h.home_path().join("extra");
    init_git_repo(&primary);
    init_git_repo(&extra);

    h.run_cli_ok(&["project", "add", extra.to_str().unwrap()]);

    let stderr = h.run_cli_err(&["add", primary.to_str().unwrap(), "--project", "extra"]);
    assert!(
        stderr.contains("--worktree") || stderr.contains("--project"),
        "expected message about --worktree requirement, got: {stderr}"
    );
}

#[test]
#[parallel]
fn test_aoe_add_project_unknown_name_fails_fast() {
    let h = TuiTestHarness::new("project_unknown_name");
    let primary = h.home_path().join("primary2");
    init_git_repo(&primary);

    let stderr = h.run_cli_err(&[
        "add",
        primary.to_str().unwrap(),
        "--project",
        "ghost-project",
        "-w",
        "feat-branch",
        "-b",
    ]);
    assert!(
        stderr.contains("ghost-project") || stderr.contains("Unknown project"),
        "expected message naming the unknown project, got: {stderr}"
    );
}
