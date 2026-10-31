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
    let mut h = TuiTestHarness::new_in_tmp("project_registry");
    h.stop_daemon_on_drop();
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
        (vec!["project", "add", repo, "--scope", "profile"], "409"),
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
    let mut h = TuiTestHarness::new_in_tmp("project_add_flag");
    h.stop_daemon_on_drop();
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

#[test]
#[parallel]
fn project_remove_resolves_a_relative_path_in_the_cli_cwd() {
    let mut h = TuiTestHarness::new_in_tmp("project_rm_cwd");
    h.stop_daemon_on_drop();
    let (a, b) = (h.home_path().join("a"), h.home_path().join("b"));
    init_git_repo(&a.join("repo"));
    init_git_repo(&b.join("repo"));
    let run_in = |dir: &std::path::Path, args: &[&str]| {
        let out = h
            .cli_command(args)
            .current_dir(dir)
            .output()
            .expect("run aoe");
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    // The first command autostarts the Daemon, so its cwd is `a`.
    run_in(&a, &["project", "add", "./repo", "--name", "in-a"]);
    run_in(&b, &["project", "add", "./repo", "--name", "in-b"]);

    run_in(&b, &["project", "remove", "./repo"]);
    let listed = h.run_cli_ok(&["project", "list"]);
    assert!(
        listed.contains("in-a") && !listed.contains("in-b"),
        "{listed}"
    );
}

#[test]
#[parallel]
fn project_remove_prefers_the_name_over_a_colliding_client_path() {
    let mut h = TuiTestHarness::new_in_tmp("project_rm_name");
    h.stop_daemon_on_drop();
    let cwd = h.home_path().join("cwd");
    let named = h.home_path().join("named");
    init_git_repo(&named);
    init_git_repo(&cwd.join("org/repo"));
    let run = |args: &[&str]| {
        let out = h
            .cli_command(args)
            .current_dir(&cwd)
            .output()
            .expect("run aoe");
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    // A display name that looks like a relative path, and another project at
    // the path the same text resolves to in the CLI's directory.
    run(&[
        "project",
        "add",
        named.to_str().unwrap(),
        "--name",
        "org/repo",
    ]);
    run(&["project", "add", "org/repo", "--name", "at-path"]);

    run(&["project", "remove", "org/repo"]);
    let listed = h.run_cli_ok(&["project", "list"]);
    assert!(!listed.contains("named"), "{listed}");
    assert!(listed.contains("at-path"), "{listed}");
}

#[test]
#[parallel]
fn project_remove_anchors_a_bare_name_that_matches_no_entry() {
    let mut h = TuiTestHarness::new_in_tmp("project_rm_bare");
    h.stop_daemon_on_drop();
    let (a, b) = (h.home_path().join("a"), h.home_path().join("b"));
    init_git_repo(&a.join("repo"));
    init_git_repo(&b.join("repo"));
    let run_in = |dir: &std::path::Path, args: &[&str]| {
        let out = h
            .cli_command(args)
            .current_dir(dir)
            .output()
            .expect("run aoe");
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    // The Daemon starts in `a`; neither entry is named `repo`.
    run_in(&a, &["project", "add", "./repo", "--name", "in-a"]);
    run_in(&b, &["project", "add", "./repo", "--name", "in-b"]);

    run_in(&b, &["project", "remove", "repo"]);
    let listed = h.run_cli_ok(&["project", "list"]);
    assert!(
        listed.contains("in-a") && !listed.contains("in-b"),
        "{listed}"
    );
}

/// The Daemon, not the CLI's own store, decides which profiles exist.
#[test]
#[parallel]
fn profile_and_project_writes_use_the_selected_daemons_store() {
    let mut server = TuiTestHarness::new_in_tmp("remote_store_server");
    server.stop_daemon_on_drop();
    let port = server.start_daemon();
    server.run_cli_ok(&["profile", "create", "remote-only"]);
    let repo = server.home_path().join("remote-repo");
    init_git_repo(&repo);

    let client = TuiTestHarness::new_in_tmp("remote_store_client");
    let url = format!("http://127.0.0.1:{port}");
    let run = |args: &[&str]| {
        let out = client
            .cli_command(args)
            .env("AOE_DAEMON_URL", &url)
            .output()
            .expect("run aoe");
        assert!(out.status.success(), "{args:?}: {out:?}");
    };
    run(&["profile", "default", "remote-only"]);
    run(&[
        "-p",
        "remote-only",
        "project",
        "add",
        repo.to_str().unwrap(),
    ]);
    let listed = server.run_cli_ok(&["-p", "remote-only", "project", "list", "--scope", "profile"]);
    assert!(listed.contains("remote-repo"), "{listed}");
}
