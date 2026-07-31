//! End-to-end coverage for `aoe session add-project` (#3103).
//!
//! Drives the real `aoe` binary as a subprocess (`run_cli`, no tmux) against
//! real git repos in the temp home, then asserts on the worktree on disk and on
//! the persisted `sessions.json`. No agent runs, so the attach path is exercised
//! end to end deterministically anywhere `cargo test` runs.
//!
//! The interesting assertions are the ones a unit test cannot make: that the
//! worktree really exists at the aoe-owned per-session path, that a second
//! attach of the same repo is refused without leaving anything behind, and that
//! a pre-existing branch in the added repo is refused unless the caller opts in.

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

/// Attaching a repo creates its worktree under the aoe-owned per-session
/// directory and records it on the session, without moving the session's own
/// project_path.
#[test]
#[parallel]
fn add_project_creates_a_worktree_and_records_it() {
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
    let attached = session["attached_repos"]
        .as_array()
        .expect("attached_repos recorded");
    assert_eq!(attached.len(), 1, "exactly one repo attached");
    assert_eq!(attached[0]["name"].as_str(), Some("frontend"));
    assert_eq!(
        attached[0]["worktree_managed_by_aoe"].as_bool(),
        Some(true),
        "aoe created this worktree and may remove it"
    );

    let worktree = attached[0]["worktree_path"]
        .as_str()
        .expect("worktree_path recorded");
    assert!(
        std::path::Path::new(worktree).join(".git").exists(),
        "worktree should exist on disk at {worktree}"
    );
    assert!(
        worktree.contains("attached-repos"),
        "a non-workspace session's attachment belongs in the aoe-owned dir, got {worktree}"
    );
    assert!(
        !worktree.starts_with(frontend.to_str().unwrap()),
        "nothing should be created inside the user's own checkout, got {worktree}"
    );

    // The session's own project_path is untouched: attaching widens the
    // session's view, it does not move it. Compared canonicalized, because the
    // temp home is under the macOS `/var` -> `/private/var` symlink.
    let recorded = std::path::Path::new(session["project_path"].as_str().unwrap())
        .canonicalize()
        .expect("recorded project_path exists");
    assert_eq!(recorded, backend.canonicalize().unwrap());
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
        session_by_title(&sessions, "Dup")["attached_repos"]
            .as_array()
            .map(Vec::len),
        Some(1),
        "the refused attach must not add a second record"
    );
}

/// A branch that already exists in the repo being attached is refused, because
/// it can hold unrelated commits. `--attach-existing-branch` opts in and records
/// that aoe does not own the branch.
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
    let attached = session_by_title(&sessions, "Branchy")["attached_repos"]
        .as_array()
        .expect("attached_repos recorded")
        .clone();
    assert_eq!(attached.len(), 1);
    assert_eq!(attached[0]["branch"].as_str(), Some("feat/shared"));
    assert_eq!(
        attached[0]["branch_created_by_aoe"].as_bool(),
        Some(false),
        "a reused branch is not aoe's to delete when the session goes away"
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
