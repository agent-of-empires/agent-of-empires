//! `aoe session add-project` (#3103) against real git repos, asserting on the
//! worktrees on disk and the persisted `sessions.json`. No agent runs.

use std::path::Path;

use serial_test::parallel;

use crate::harness::{session_by_title, TuiTestHarness};

fn git(dir: &Path, args: &[&str]) {
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
fn init_repo(path: &Path) {
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

    h.run_cli_ok(&[
        "add",
        backend.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "Attach",
    ]);
    h.run_cli_ok(&[
        "session",
        "add-project",
        "Attach",
        frontend.to_str().unwrap(),
    ]);

    let sessions = h.read_sessions();
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
            Path::new(worktree).join(".git").exists(),
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

    // The session now works in the workspace, not the original checkout
    // (canonicalized: the temp home is under macOS's `/var` symlink).
    let recorded = Path::new(session["project_path"].as_str().unwrap())
        .canonicalize()
        .expect("recorded project_path exists");
    assert_eq!(recorded, Path::new(workspace_dir).canonicalize().unwrap());

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

    h.run_cli_ok(&[
        "add",
        backend.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "Dup",
    ]);
    h.run_cli_ok(&["session", "add-project", "Dup", frontend.to_str().unwrap()]);

    let stderr = h.run_cli_err(&["session", "add-project", "Dup", frontend.to_str().unwrap()]);
    assert!(stderr.contains("already attached"), "{stderr}");

    let sessions = h.read_sessions();
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

    // A worktree session carries a branch name to mirror; give the added repo
    // that same branch with its own unrelated history.
    h.run_cli_ok(&[
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
    git(&frontend, &["branch", "feat/shared"]);

    let stderr = h.run_cli_err(&[
        "session",
        "add-project",
        "Branchy",
        frontend.to_str().unwrap(),
    ]);
    assert!(stderr.contains("already exists"), "{stderr}");
    let sessions = h.read_sessions();
    assert!(
        session_by_title(&sessions, "Branchy")["workspace_info"].is_null(),
        "a refusal must not half-convert the session"
    );

    h.run_cli_ok(&[
        "session",
        "add-project",
        "Branchy",
        frontend.to_str().unwrap(),
        "--attach-existing-branch",
    ]);

    let sessions = h.read_sessions();
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
        Path::new(moved_to).join(".git").exists(),
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

    h.run_cli_ok(&[
        "add",
        backend.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "NonRepo",
    ]);
    let stderr = h.run_cli_err(&["session", "add-project", "NonRepo", plain.to_str().unwrap()]);
    assert!(stderr.contains("not a git repository"), "{stderr}");
}
