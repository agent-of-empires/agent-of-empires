//! `aoe session add-project` (#3103) against real git repos, asserting on the
//! worktrees on disk and the persisted `sessions.json`. No agent runs.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use serial_test::parallel;

use crate::harness::{session_by_title, TuiTestHarness};

fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
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

/// `--workspace` starts a worktree session as a one-repo workspace, so a later
/// attach appends beside it and the session's working directory never moves.
#[test]
#[parallel]
fn a_workspace_session_attaches_a_project_without_moving() {
    let h = TuiTestHarness::new("add_project_workspace_start");
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
        "Solo",
        "-w",
        "feat/solo",
        "-b",
        "--workspace",
    ]);
    let sessions = h.read_sessions();
    let before = session_by_title(&sessions, "Solo");
    let workspace_dir = before["workspace_info"]["workspace_dir"]
        .as_str()
        .expect("--workspace records a workspace")
        .to_string();
    assert_eq!(
        before["project_path"].as_str(),
        Some(workspace_dir.as_str())
    );
    assert_eq!(
        before["workspace_info"]["repos"].as_array().unwrap().len(),
        1
    );
    assert!(before["worktree_info"].is_null());

    h.run_cli_ok(&["session", "add-project", "Solo", frontend.to_str().unwrap()]);
    let sessions = h.read_sessions();
    let after = session_by_title(&sessions, "Solo");
    assert_eq!(after["project_path"].as_str(), Some(workspace_dir.as_str()));
    let names: Vec<&str> = after["workspace_info"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["name"].as_str())
        .collect();
    assert_eq!(names, vec!["backend", "frontend"]);
    assert!(Path::new(&workspace_dir).join("frontend/.git").exists());
}

/// `worktree.workspace_by_default` is resolved by the Daemon, and only worktree
/// sessions that did not pass `--no-workspace` follow it.
#[test]
#[parallel]
fn workspace_by_default_applies_on_the_daemon() {
    let h = TuiTestHarness::new("add_workspace_by_default");
    h.append_config("[worktree]\nworkspace_by_default = true");
    let repo = h.home_path().join("backend");
    init_repo(&repo);
    let add = |title: &str, branch: &str, extra: &[&str]| {
        let mut args = vec![
            "add",
            repo.to_str().unwrap(),
            "--cmd",
            "claude",
            "-t",
            title,
            "-w",
            branch,
            "-b",
        ];
        args.extend_from_slice(extra);
        h.run_cli_ok(&args);
    };
    add("Default", "feat/default", &[]);
    add("Opted", "feat/opted", &["--no-workspace"]);
    let sessions = h.read_sessions();
    assert!(!session_by_title(&sessions, "Default")["workspace_info"].is_null());
    let opted = session_by_title(&sessions, "Opted");
    assert!(opted["workspace_info"].is_null());
    assert!(!opted["worktree_info"].is_null());

    // Attaching a branch checked out in a worktree reuses that directory.
    let checkout = h.home_path().join("existing-checkout");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "feat/existing",
            checkout.to_str().unwrap(),
        ],
    );
    h.run_cli_ok(&[
        "add",
        repo.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        "Attached",
        "-w",
        "feat/existing",
    ]);
    let sessions = h.read_sessions();
    let attached = session_by_title(&sessions, "Attached");
    assert!(attached["workspace_info"].is_null());
    assert_eq!(
        Path::new(attached["project_path"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        checkout.canonicalize().unwrap()
    );
}

/// A workspace directory holds no repo config, so repo hooks come from and run in the
/// launch repo's checkout, even after another project is added. Global hooks stay in
/// the workspace directory.
#[test]
#[parallel]
fn workspace_repo_hooks_run_in_the_launch_repo_checkout() {
    crate::harness::require_tmux!();
    let h = TuiTestHarness::new("workspace_repo_hooks");
    let agent = h.home_path().join("held-agent");
    std::fs::write(&agent, "#!/bin/sh\nexec sleep 600\n").unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = h.home_path().join("hooks.log");
    let record = |kind: &str| format!("echo {kind} $PWD $AOE_PROJECT_PATH >> {}", log.display());
    h.append_config(&format!(
        "[session]\ndefault_tool = \"held-agent\"\ncustom_agents = {{ held-agent = {:?} }}\n\
         [hooks]\non_destroy = [{:?}]",
        agent.to_str().unwrap(),
        record("destroy"),
    ));
    let backend = h.home_path().join("backend");
    let frontend = h.home_path().join("frontend");
    init_repo(&backend);
    init_repo(&frontend);
    std::fs::create_dir(backend.join(".agent-of-empires")).unwrap();
    std::fs::write(
        backend.join(".agent-of-empires/config.toml"),
        format!(
            "[hooks]\non_create = [{:?}]\non_launch = [{:?}]\n",
            record("create"),
            record("launch"),
        ),
    )
    .unwrap();

    h.run_cli_ok(&[
        "add",
        backend.to_str().unwrap(),
        "-t",
        "Solo",
        "-w",
        "feat/solo",
        "-b",
        "--workspace",
        "--trust-hooks",
        "--launch",
    ]);
    let sessions = h.read_sessions();
    let workspace = &session_by_title(&sessions, "Solo")["workspace_info"];
    let workspace_dir = workspace["workspace_dir"].as_str().unwrap().to_string();
    let checkout = workspace["repos"][0]["worktree_path"]
        .as_str()
        .unwrap()
        .to_string();
    let lines = |kinds: &[&str]| {
        kinds
            .iter()
            .map(|kind| format!("{kind} {checkout} {workspace_dir}\n"))
            .collect::<String>()
    };
    let take_log = || {
        let content = std::fs::read_to_string(&log).unwrap();
        std::fs::remove_file(&log).unwrap();
        content
    };
    assert_eq!(take_log(), lines(&["create", "launch"]));

    h.run_cli_ok(&["session", "restart", "Solo"]);
    assert_eq!(take_log(), lines(&["launch"]));

    h.run_cli_ok(&["session", "stop", "Solo"]);
    h.run_cli_ok(&["session", "add-project", "Solo", frontend.to_str().unwrap()]);
    h.run_cli_ok(&["session", "start", "Solo"]);
    assert_eq!(take_log(), lines(&["launch"]));

    h.run_cli_ok(&["remove", "Solo", "--purge", "--force"]);
    assert_eq!(
        take_log(),
        format!("destroy {workspace_dir} {workspace_dir}\n")
    );
}
