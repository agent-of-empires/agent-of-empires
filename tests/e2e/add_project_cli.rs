//! End-to-end coverage for `aoe session add-project` (#3103).
//!
//! Drives the real `aoe` binary as a subprocess (`run_cli`, no tmux) against
//! real git repos in the temp home, then asserts on the worktrees on disk and on
//! the persisted `sessions.json`. No agent runs, so the attach path is exercised
//! end to end deterministically anywhere `cargo test` runs.
//!
//! The interesting assertions are the ones a unit test cannot make: that
//! attaching really converts the session into the same on-disk shape
//! `create_workspace` produces, that the user's own checkout is left where it
//! was, that a second attach of the same repo is refused without leaving
//! anything behind, and that a pre-existing branch in the added repo is refused
//! unless the caller opts in.

use serial_test::parallel;

use crate::harness::TuiTestHarness;

fn sessions_path(h: &TuiTestHarness) -> std::path::PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

fn read_sessions(h: &TuiTestHarness) -> serde_json::Value {
    let path = sessions_path(h);
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    serde_json::from_str(&content).expect("invalid sessions JSON")
}

fn session_by_title<'a>(sessions: &'a serde_json::Value, title: &str) -> &'a serde_json::Value {
    sessions
        .as_array()
        .and_then(|arr| arr.iter().find(|s| s["title"].as_str() == Some(title)))
        .unwrap_or_else(|| panic!("no session titled '{title}' in sessions.json"))
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} in {} failed: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A git repo with one commit, so branches and worktrees can be created.
fn init_repo(path: &std::path::Path) {
    std::fs::create_dir_all(path).expect("create repo dir");
    git(path, &["init", "-q"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("README.md"), "x").expect("seed file");
    git(path, &["add", "."]);
    git(path, &["commit", "-qm", "init"]);
}

/// The conversion, end to end: an in-place session becomes a multi-repo
/// workspace with both repos side by side, its working directory moves into that
/// workspace, and the user's own checkout is left exactly where it was.
#[test]
#[parallel]
fn add_project_converts_the_session_into_a_workspace() {
    let h = TuiTestHarness::new("add_project_happy");
    let backend = h.home_path().join("backend");
    let frontend = h.home_path().join("frontend");
    init_repo(&backend);
    init_repo(&frontend);

    let add = h.run_cli(&[
        "add",
        backend.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "Attach",
    ]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let out = h.run_cli(&[
        "session",
        "add-project",
        "Attach",
        frontend.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "add-project failed: {}\nstdout: {}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );

    let sessions = read_sessions(&h);
    let session = session_by_title(&sessions, "Attach");
    let workspace = &session["workspace_info"];
    let repos = workspace["repos"]
        .as_array()
        .expect("workspace_info.repos recorded");
    let names: Vec<&str> = repos.iter().filter_map(|r| r["name"].as_str()).collect();
    assert_eq!(
        names,
        vec!["backend", "frontend"],
        "the session's own repo comes first, then the attached one"
    );

    // Both worktrees really exist, side by side under the workspace directory.
    let workspace_dir = workspace["workspace_dir"].as_str().expect("workspace_dir");
    for repo in repos {
        let worktree = repo["worktree_path"].as_str().expect("worktree_path");
        assert!(
            std::path::Path::new(worktree).join(".git").exists(),
            "worktree should exist on disk at {worktree}"
        );
        assert!(
            worktree.starts_with(workspace_dir),
            "{worktree} should sit under the workspace {workspace_dir}"
        );
        assert_eq!(
            repo["managed_by_aoe"].as_bool(),
            Some(true),
            "aoe created both worktrees and may remove them"
        );
    }

    // The session now works in the workspace, not in the original checkout.
    // Compared canonicalized, because the temp home is under the macOS
    // `/var` -> `/private/var` symlink.
    let recorded = std::path::Path::new(session["project_path"].as_str().unwrap())
        .canonicalize()
        .expect("recorded project_path exists");
    assert_eq!(
        recorded,
        std::path::Path::new(workspace_dir).canonicalize().unwrap()
    );

    // The conversion creates a fresh worktree of the session's own repo rather
    // than adopting the checkout, so the user's directory is still theirs.
    assert!(
        backend.join(".git").is_dir(),
        "the user's own checkout must not be moved or removed"
    );
}

/// The same repo cannot be attached twice, and the refusal leaves the session
/// exactly as it was.
#[test]
#[parallel]
fn add_project_refuses_a_duplicate_repo() {
    let h = TuiTestHarness::new("add_project_duplicate");
    let backend = h.home_path().join("backend");
    let frontend = h.home_path().join("frontend");
    init_repo(&backend);
    init_repo(&frontend);

    let seed = h.run_cli(&[
        "add",
        backend.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "Dup",
    ]);
    assert!(
        seed.status.success(),
        "aoe add seed failed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );
    let first = h.run_cli(&["session", "add-project", "Dup", frontend.to_str().unwrap()]);
    assert!(first.status.success());

    let second = h.run_cli(&["session", "add-project", "Dup", frontend.to_str().unwrap()]);
    assert!(
        !second.status.success(),
        "attaching the same repo twice must fail"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already attached"),
        "expected a duplicate refusal, got: {stderr}"
    );

    let sessions = read_sessions(&h);
    assert_eq!(
        session_by_title(&sessions, "Dup")["workspace_info"]["repos"]
            .as_array()
            .map(Vec::len),
        Some(2),
        "the refused attach must not add a third repo"
    );
}

/// A branch that already exists in the repo being attached is refused, because
/// it can hold unrelated commits. `--attach-existing-branch` opts in and records
/// that aoe does not own the branch.
///
/// Runs against a worktree session, which is the shape whose existing worktree
/// is moved into the new workspace rather than recreated.
#[test]
#[parallel]
fn add_project_gates_an_existing_branch_behind_the_opt_in() {
    let h = TuiTestHarness::new("add_project_branch");
    let backend = h.home_path().join("backend");
    let frontend = h.home_path().join("frontend");
    init_repo(&backend);
    init_repo(&frontend);

    // A worktree session so the session carries a branch name to mirror, and
    // give the added repo that same branch with its own unrelated history.
    let add = h.run_cli(&[
        "add",
        backend.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "Branchy",
        "-w",
        "feat/shared",
        "-b",
    ]);
    assert!(
        add.status.success(),
        "aoe add -w failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    git(&frontend, &["branch", "feat/shared"]);

    let refused = h.run_cli(&[
        "session",
        "add-project",
        "Branchy",
        frontend.to_str().unwrap(),
    ]);
    assert!(
        !refused.status.success(),
        "an existing branch in the added repo must be refused by default"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("already exists"),
        "expected a branch-exists refusal, got: {stderr}"
    );
    // Refused before anything is stopped or moved, so the session is still a
    // plain worktree session.
    assert!(
        session_by_title(&read_sessions(&h), "Branchy")["workspace_info"].is_null(),
        "a refusal must not half-convert the session"
    );

    let opted_in = h.run_cli(&[
        "session",
        "add-project",
        "Branchy",
        frontend.to_str().unwrap(),
        "--attach-existing-branch",
    ]);
    assert!(
        opted_in.status.success(),
        "--attach-existing-branch should attach: {}",
        String::from_utf8_lossy(&opted_in.stderr)
    );

    let sessions = read_sessions(&h);
    let session = session_by_title(&sessions, "Branchy");
    let repos = session["workspace_info"]["repos"]
        .as_array()
        .expect("workspace_info.repos recorded")
        .clone();
    assert_eq!(repos.len(), 2);
    let frontend_repo = repos
        .iter()
        .find(|r| r["name"].as_str() == Some("frontend"))
        .expect("the added repo is recorded");
    assert_eq!(frontend_repo["branch"].as_str(), Some("feat/shared"));
    assert_eq!(
        frontend_repo["branch_preexisting"].as_bool(),
        Some(true),
        "a reused branch is not aoe's to delete when the session goes away"
    );

    // The session's original worktree was moved into the workspace, so its old
    // path is gone and its new one holds the same branch.
    let backend_repo = repos
        .iter()
        .find(|r| r["name"].as_str() == Some("backend"))
        .expect("the session's own repo is recorded");
    let moved_to = backend_repo["worktree_path"].as_str().unwrap();
    assert!(
        std::path::Path::new(moved_to).join(".git").exists(),
        "the moved worktree should exist at {moved_to}"
    );
    assert_eq!(
        session["worktree_info"],
        serde_json::Value::Null,
        "the single-repo worktree record is superseded by the workspace entry"
    );
}

/// Attaching a path that is not a git repo is refused with a message that says
/// why, rather than a bare git error.
#[test]
#[parallel]
fn add_project_refuses_a_non_repo() {
    let h = TuiTestHarness::new("add_project_non_repo");
    let backend = h.home_path().join("backend");
    let plain = h.home_path().join("just-a-dir");
    init_repo(&backend);
    std::fs::create_dir_all(&plain).unwrap();

    let seed = h.run_cli(&[
        "add",
        backend.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "NonRepo",
    ]);
    assert!(
        seed.status.success(),
        "aoe add seed failed: {}",
        String::from_utf8_lossy(&seed.stderr)
    );
    let out = h.run_cli(&["session", "add-project", "NonRepo", plain.to_str().unwrap()]);
    assert!(!out.status.success(), "a non-repo must be refused");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not a git repository"),
        "expected a not-a-repo refusal, got: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
