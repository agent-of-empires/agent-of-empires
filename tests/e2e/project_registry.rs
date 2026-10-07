//! E2E tests for the `aoe project` registry CLI surface.
//!
//! Exercises `aoe project add`, `aoe project list`, and `aoe project remove`
//! against an isolated home, plus the `aoe add --project NAME` shortcut and
//! the cross-scope override guard.

use serial_test::parallel;

use crate::harness::{init_git_repo, TuiTestHarness};

#[test]
#[parallel]
fn test_project_registry_add_list_override_and_remove() {
    let h = TuiTestHarness::new("project_registry");
    let repo = h.home_path().join("repoA");
    init_git_repo(&repo);
    let repo = repo.to_str().unwrap();
    let plain = h.home_path().join("plain-dir");
    std::fs::create_dir_all(&plain).expect("create plain dir");
    let missing = h.home_path().join("does-not-exist");

    let empty = h.run_cli_ok(&["project", "list"]);
    assert!(empty.contains("No projects registered"), "{empty}");

    // Default scope is global.
    let added = h.run_cli_ok(&["project", "add", repo]);
    assert!(
        added.contains("repoA") && added.contains("[global]"),
        "{added}"
    );
    let json: serde_json::Value =
        serde_json::from_str(&h.run_cli_ok(&["project", "list", "--json"])).expect("JSON output");
    let arr = json.as_array().expect("expected JSON array");
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["name"], "repoA");
    assert_eq!(arr[0]["scope"], "global");

    let stdout = h.run_cli_ok(&["project", "add", plain.to_str().unwrap()]);
    assert!(
        stdout.contains("Registered project") && stdout.contains("not a git repository"),
        "{stdout}"
    );

    for (args, expected) in [
        (
            vec!["project", "add", missing.to_str().unwrap()],
            "does not exist or is not a directory",
        ),
        (vec!["project", "add", repo], "already registered"),
        (
            vec!["project", "add", repo, "--scope", "profile"],
            "--allow-override",
        ),
    ] {
        let stderr = h.run_cli_err(&args);
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
    }

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
    assert!(
        listed.contains("[profile]") && listed.contains("plain-dir"),
        "{listed}"
    );

    // Removal matches the name case-insensitively.
    h.run_cli_ok(&["project", "remove", "PLAIN-DIR"]);
    assert!(!h.run_cli_ok(&["project", "list"]).contains("plain-dir"));
}

/// `aoe add --project` needs a worktree and a registered project name.
#[test]
#[parallel]
fn test_aoe_add_project_flag_rejects_invalid_requests() {
    let h = TuiTestHarness::new("project_add_flag");
    let primary = h.home_path().join("primary");
    let extra = h.home_path().join("extra");
    init_git_repo(&primary);
    init_git_repo(&extra);
    let primary = primary.to_str().unwrap();
    h.run_cli_ok(&["project", "add", extra.to_str().unwrap()]);

    let stderr = h.run_cli_err(&["add", primary, "--project", "extra"]);
    assert!(
        stderr.contains("--worktree") || stderr.contains("--project"),
        "expected message about --worktree requirement, got: {stderr}"
    );

    let stderr = h.run_cli_err(&[
        "add",
        primary,
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
