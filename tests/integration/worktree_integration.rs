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
#[serial_test::parallel]
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

// --- Edit-workdir-name (#1723) ---

use agent_of_empires::cli::session::{self, SessionCommands};
use agent_of_empires::session::worktree_edit::WorktreeEditError;
use anyhow::Context as _;

fn managed_info(branch: &str, repo: &std::path::Path) -> WorktreeInfo {
    WorktreeInfo {
        branch: branch.to_string(),
        main_repo_path: repo.to_string_lossy().to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    }
}

fn seed_edit_session(info: &WorktreeInfo, path: &std::path::Path) -> (Storage, String) {
    agent_of_empires::session::config::update_config(|config| {
        config.session.tie_workdir_to_name = false;
    })
    .unwrap();
    let storage = Storage::new_unwatched("worktree-edit").unwrap();
    let mut instance = Instance::new("Edit", &path.to_string_lossy());
    instance.worktree_info = Some(info.clone());
    let id = instance.id.clone();
    storage
        .update(|rows, _| {
            rows.push(instance);
            Ok(())
        })
        .unwrap();
    (storage, id)
}

fn edit_workdir(
    storage: &Storage,
    id: &str,
    name: &str,
    rename_branch: bool,
) -> anyhow::Result<Instance> {
    use clap::Parser;
    #[derive(Parser)]
    struct Command {
        #[command(subcommand)]
        session: SessionCommands,
    }
    let args = Command::try_parse_from(
        ["session", "set-worktree-name", id, "--name", name]
            .into_iter()
            .chain(rename_branch.then_some("--rename-branch")),
    )?;
    tokio::runtime::Runtime::new()?.block_on(session::run(storage.profile(), args.session))?;
    storage
        .load()?
        .into_iter()
        .find(|row| row.id == id)
        .context("edited session disappeared")
}

#[test]
#[serial]
fn edit_workdir_moves_dir_and_optionally_renames_branch() {
    let _home = crate::common::setup_temp_home();
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();

    let old_path = repo_dir.path().join("old-name");
    git_wt
        .create_worktree("old-name", &old_path, true, None)
        .unwrap();
    assert!(old_path.exists());
    let info = managed_info("old-name", repo_dir.path());
    let (storage, id) = seed_edit_session(&info, &old_path);

    // Path-only edit: directory moves, branch untouched.
    let outcome = edit_workdir(&storage, &id, "fresh-name", false).unwrap();
    let fresh_path = repo_dir.path().join("fresh-name");
    assert_eq!(
        std::path::Path::new(&outcome.project_path),
        fresh_path.as_path()
    );
    assert_eq!(outcome.worktree_info.as_ref().unwrap().branch, "old-name");
    assert!(!old_path.exists());
    assert!(fresh_path.exists());
    assert!(git_wt.branch_exists("old-name").unwrap());

    // Branch rename opted in: directory moves and branch is renamed.
    let outcome2 = edit_workdir(&storage, &id, "renamed", true).unwrap();
    let renamed_path = repo_dir.path().join("renamed");
    assert_eq!(outcome2.worktree_info.as_ref().unwrap().branch, "renamed");
    assert!(!fresh_path.exists());
    assert!(renamed_path.exists());
    assert!(git_wt.branch_exists("renamed").unwrap());
    assert!(!git_wt.branch_exists("old-name").unwrap());
}

// Tied workdir/title (#1927)

#[test]
#[serial_test::parallel]
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
fn edit_workdir_rejects_invalid_cases_without_partial_changes() {
    let _home = crate::common::setup_temp_home();
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
    let (storage, id) = seed_edit_session(&info, &old_path);

    // Target directory already exists: nothing moves.
    let err = edit_workdir(&storage, &id, "bbb", false).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<WorktreeEditError>(),
        Some(WorktreeEditError::TargetExists(_))
    ));
    assert!(old_path.exists());

    // Branch rename onto an existing branch is rejected; the source
    // directory and branch are untouched.
    let err = edit_workdir(&storage, &id, "test-feature", true).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<WorktreeEditError>(),
        Some(WorktreeEditError::BranchExists(_))
    ));
    assert!(old_path.exists());
    assert!(git_wt.branch_exists("aaa").unwrap());

    // Unmanaged worktrees cannot be edited.
    let unmanaged = WorktreeInfo {
        managed_by_aoe: false,
        ..info.clone()
    };
    storage
        .update(|rows, _| {
            rows.iter_mut()
                .find(|row| row.id == id)
                .unwrap()
                .worktree_info = Some(unmanaged);
            Ok(())
        })
        .unwrap();
    let err = edit_workdir(&storage, &id, "ccc", false).unwrap_err();
    assert!(matches!(
        err.downcast_ref::<WorktreeEditError>(),
        Some(WorktreeEditError::NotManaged)
    ));
    assert!(old_path.exists());
}

// --- Reconciling a stale project_path (#2002) ---

use agent_of_empires::session::worktree_reconcile::{
    reconcile_and_persist, resolve_worktree_path, WorktreePathResolution,
};

/// Run a git subcommand in `repo` the way a user in another shell would,
/// bypassing aoe entirely.
fn git_in(repo: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An external `git worktree move` is discovered from git's own listing, and
/// the recorded path is repaired in storage so the rename that would otherwise
/// fail with `SourceMissing` proceeds against the relocated directory.
///
/// `#[serial]` because `setup_temp_home` mutates the process-global `HOME` and
/// `XDG_CONFIG_HOME` that `Storage` resolves its app dir from. Setting `HOME`
/// alone is not enough: on macOS the app dir is `$XDG_CONFIG_HOME/agent-of-empires`
/// whenever that variable is set, so a bare `HOME` override would write a real
/// profile dir on any machine that exports it.
#[test]
#[serial]
fn reconcile_heals_a_worktree_moved_outside_aoe() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let _temp_home = crate::common::setup_temp_home();
    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();

    let created = repo_dir.path().join("recorded-name");
    git_wt
        .create_worktree("recorded-name", &created, true, None)
        .unwrap();
    let info = managed_info("recorded-name", repo_dir.path());

    let mut instance = Instance::new("Moved Session", created.to_str().unwrap());
    instance.worktree_info = Some(info.clone());
    let storage = Storage::new_unwatched("worktree-reconcile-profile").unwrap();
    let seeded = vec![instance.clone()];
    storage
        .update(|i, g| {
            *i = seeded.to_vec();
            *g = GroupTree::new_with_groups(&seeded, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    // aoe locks every worktree it creates, so a user relocating one from
    // another shell has to unlock it first; `git worktree move` refuses a
    // locked tree.
    let relocated = repo_dir.path().join("moved-by-hand");
    git_in(
        repo_dir.path(),
        &["worktree", "unlock", created.to_str().unwrap()],
    );
    git_in(
        repo_dir.path(),
        &[
            "worktree",
            "move",
            created.to_str().unwrap(),
            relocated.to_str().unwrap(),
        ],
    );
    assert!(!created.exists());
    assert!(relocated.exists());

    let mut stale = instance.clone();
    assert_eq!(
        reconcile_and_persist(&storage, &mut stale, &mut Default::default()).unwrap(),
        WorktreePathResolution::Moved(relocated.canonicalize().unwrap())
    );
    assert_eq!(
        std::path::PathBuf::from(&stale.project_path),
        relocated.canonicalize().unwrap()
    );
    // The repair is durable, not just in the caller's copy.
    let reloaded = storage.load().unwrap();
    assert_eq!(
        std::path::PathBuf::from(&reloaded[0].project_path),
        relocated.canonicalize().unwrap()
    );

    agent_of_empires::session::config::update_config(|config| {
        config.session.tie_workdir_to_name = false;
    })
    .unwrap();
    let outcome = edit_workdir(&storage, &instance.id, "settled-name", false).unwrap();
    let settled_path = relocated
        .parent()
        .unwrap()
        .canonicalize()
        .unwrap()
        .join("settled-name");
    assert_eq!(
        std::path::Path::new(&outcome.project_path),
        settled_path.as_path()
    );
    assert!(settled_path.exists());

    // Reconciling again is a no-op: the recorded path is present, so git is
    // never consulted.
    let mut settled = stale.clone();
    settled.project_path = outcome.project_path;
    assert_eq!(
        reconcile_and_persist(&storage, &mut settled, &mut Default::default()).unwrap(),
        WorktreePathResolution::Current
    );
}

/// What git can and cannot tell us. A bare `mv` leaves git's record naming the
/// old path, so the new location is not discoverable until the user runs
/// `git worktree repair`; a deleted checkout is never discoverable. Both leave
/// the recorded path alone rather than guessing.
///
/// `#[serial]` even though this test sets no env var of its own: it shells out
/// to `git`, which reads `$HOME/.gitconfig`, so it must not overlap a peer that
/// has pointed `HOME` at a temp dir.
#[test]
#[serial]
fn resolve_reports_missing_when_git_cannot_place_the_branch() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();

    let created = repo_dir.path().join("hand-moved");
    git_wt
        .create_worktree("hand-moved", &created, true, None)
        .unwrap();
    let info = managed_info("hand-moved", repo_dir.path());

    // A bare `mv`: the directory is at its new location but git still points
    // at the old one, which is indistinguishable from a deletion.
    let elsewhere = repo_dir.path().join("elsewhere");
    std::fs::rename(&created, &elsewhere).unwrap();
    assert_eq!(
        resolve_worktree_path(&git_wt.list_worktrees().unwrap(), &created, &info),
        WorktreePathResolution::Missing
    );

    // `git worktree repair` is git's own remedy: once its record catches up,
    // the same lookup finds the checkout.
    git_in(
        repo_dir.path(),
        &["worktree", "repair", elsewhere.to_str().unwrap()],
    );
    assert_eq!(
        resolve_worktree_path(&git_wt.list_worktrees().unwrap(), &created, &info),
        WorktreePathResolution::Moved(elsewhere.canonicalize().unwrap())
    );

    // A checkout that is gone for good stays Missing.
    std::fs::remove_dir_all(&elsewhere).unwrap();
    assert_eq!(
        resolve_worktree_path(&git_wt.list_worktrees().unwrap(), &created, &info),
        WorktreePathResolution::Missing
    );

    // An unmanaged worktree is out of scope and never consults git.
    let unmanaged = WorktreeInfo {
        managed_by_aoe: false,
        ..info.clone()
    };
    assert_eq!(
        resolve_worktree_path(&git_wt.list_worktrees().unwrap(), &created, &unmanaged),
        WorktreePathResolution::Current
    );
}

/// A pruned registration frees the branch, so a later checkout of it belongs to
/// somebody else. The reconcile refuses that lone candidate rather than pointing
/// two sessions at one directory, where trashing or deleting the stale row would
/// take the live row's checkout with it.
///
/// `#[serial]` for the same reason as the test above: `setup_temp_home` mutates
/// the process-global `HOME` and `XDG_CONFIG_HOME` that `Storage` resolves its
/// app dir from.
#[test]
#[serial]
fn reconcile_refuses_a_checkout_another_session_records() {
    let (repo_dir, _repo, _config_dir) = setup_test_environment();
    let _temp_home = crate::common::setup_temp_home();
    let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();

    let stale_dir = repo_dir.path().join("first-session");
    git_wt
        .create_worktree("shared-branch", &stale_dir, true, None)
        .unwrap();
    let info = managed_info("shared-branch", repo_dir.path());

    // The first checkout is removed by hand and its registration pruned, which
    // is what lets a second worktree take the branch at all.
    git_in(
        repo_dir.path(),
        &["worktree", "unlock", stale_dir.to_str().unwrap()],
    );
    std::fs::remove_dir_all(&stale_dir).unwrap();
    git_in(repo_dir.path(), &["worktree", "prune"]);

    // A later session (or the user's own shell) checks the branch out again.
    let live_dir = repo_dir.path().join("second-session");
    git_in(
        repo_dir.path(),
        &[
            "worktree",
            "add",
            live_dir.to_str().unwrap(),
            "shared-branch",
        ],
    );
    let live_canonical = live_dir.canonicalize().unwrap();
    // git left the branch discoverable, so the lookup does find a lone
    // candidate; only the ownership check keeps it from being adopted.
    assert_eq!(
        resolve_worktree_path(&git_wt.list_worktrees().unwrap(), &stale_dir, &info),
        WorktreePathResolution::Moved(live_canonical.clone())
    );

    let mut stale = Instance::new("First Session", stale_dir.to_str().unwrap());
    stale.worktree_info = Some(info.clone());
    let mut live = Instance::new("Second Session", live_canonical.to_str().unwrap());
    live.worktree_info = Some(info.clone());
    let storage = Storage::new_unwatched("worktree-reconcile-claimed-profile").unwrap();
    let seeded = vec![stale.clone(), live.clone()];
    storage
        .update(|i, g| {
            *i = seeded.to_vec();
            *g = GroupTree::new_with_groups(&seeded, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let mut reconciled = stale.clone();
    assert_eq!(
        reconcile_and_persist(&storage, &mut reconciled, &mut Default::default()).unwrap(),
        WorktreePathResolution::Current
    );
    assert_eq!(reconciled.project_path, stale.project_path);
    let reloaded = storage.load().unwrap();
    let stale_row = reloaded.iter().find(|i| i.id == stale.id).unwrap();
    assert_eq!(stale_row.project_path, stale.project_path);
    let live_row = reloaded.iter().find(|i| i.id == live.id).unwrap();
    assert_eq!(live_row.project_path, live.project_path);
}

#[cfg(unix)]
#[test]
#[serial_test::parallel]
fn cli_publishes_and_removes_detached_without_owning_a_local_branch_label() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let xdg = temp.path().join("xdg");
    let bin = temp.path().join("bin");
    for path in [&home, &xdg, &bin] {
        std::fs::create_dir(path).unwrap();
    }
    let repo_path = temp.path().join("repo");
    let repo = git2::Repository::init(&repo_path).unwrap();
    let signature = git2::Signature::now("fixture", "fixture@example.com").unwrap();
    let tree = repo.index().unwrap().write_tree().unwrap();
    let tip = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "fixture",
            &repo.find_tree(tree).unwrap(),
            &[],
        )
        .unwrap();
    repo.reference("refs/remotes/origin/main", tip, true, "fixture")
        .unwrap();
    let hook = repo.path().join("hooks/post-checkout");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf complete > \"$AOE_TEST_POSTCHECKOUT_MARKER\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
    let agent = bin.join("claude");
    std::fs::write(
        &agent,
        "#!/bin/sh\nprintf denied > \"$AOE_TEST_AGENT_LOG\"\nexit 97\n",
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let app = xdg.join(if cfg!(debug_assertions) {
        "agent-of-empires-dev"
    } else {
        "agent-of-empires"
    });
    std::fs::create_dir_all(app.join("profiles/owner")).unwrap();
    crate::common::disable_systemd_scopes(&app);
    let marker = temp.path().join("hook-completed");
    let agent_log = temp.path().join("agent-invocations");
    let path = std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    )))
    .unwrap();
    let cli = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_aoe"));
        command
            .args(["-p", "owner"])
            .current_dir(temp.path())
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &xdg)
            .env("AOE_TMUX_SOCKET", temp.path().join("tmux.sock"))
            .env("AOE_TEST_POSTCHECKOUT_MARKER", &marker)
            .env("AOE_TEST_AGENT_LOG", &agent_log)
            .env("PATH", &path)
            .env_remove("AGENT_OF_EMPIRES_PROFILE")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_PREFIX");
        command
    };
    let mut command = cli();
    command.arg("add").arg(&repo_path).args([
        "--title",
        "detached-publication",
        "--worktree",
        "origin/main",
        "--cmd",
        "claude",
        "--trust-hooks",
        "--no-submodules",
    ]);
    let output = agent_of_empires::process::run_with_timeout_process_group(
        &mut command,
        std::time::Duration::from_secs(30),
    )
    .unwrap()
    .expect("detached CLI creation exceeded its deadline");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read(&marker).unwrap(), b"complete");
    assert!(!agent_log.exists());
    let rows: Vec<Instance> =
        serde_json::from_slice(&std::fs::read(app.join("profiles/owner/sessions.json")).unwrap())
            .unwrap();
    assert_eq!(rows.len(), 1);
    let info = rows[0].worktree_info.as_ref().unwrap();
    assert_eq!(info.branch, "origin/main");
    assert!(info.managed_by_aoe);
    let checkout = git2::Repository::open(&rows[0].project_path).unwrap();
    assert!(checkout.head_detached().unwrap());
    assert_eq!(checkout.head().unwrap().peel_to_commit().unwrap().id(), tip);
    assert!(checkout.path().join("locked").is_file());
    assert!(GitWorktree::new(repo_path)
        .unwrap()
        .list_worktrees()
        .unwrap()
        .iter()
        .any(|entry| entry.is_detached
            && entry.path.canonicalize().ok()
                == std::path::Path::new(&rows[0].project_path)
                    .canonicalize()
                    .ok()));
    assert!(repo.find_reference("refs/heads/origin/main").is_err());
    assert_eq!(
        repo.find_reference("refs/remotes/origin/main")
            .unwrap()
            .target(),
        Some(tip)
    );
    repo.reference(
        "refs/heads/origin/main",
        tip,
        true,
        "later independent local branch",
    )
    .unwrap();
    let mut remove = cli();
    remove.args([
        "remove",
        rows[0].id.as_str(),
        "--delete-worktree",
        "--delete-branch",
        "--purge",
    ]);
    let removed = agent_of_empires::process::run_with_timeout_process_group(
        &mut remove,
        std::time::Duration::from_secs(30),
    )
    .unwrap()
    .expect("detached CLI deletion exceeded its deadline");
    assert!(
        removed.status.success(),
        "{}{}",
        String::from_utf8_lossy(&removed.stdout),
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!std::path::Path::new(&rows[0].project_path).exists());
    let remaining: Vec<Instance> =
        serde_json::from_slice(&std::fs::read(app.join("profiles/owner/sessions.json")).unwrap())
            .unwrap();
    assert!(remaining.is_empty());
    assert_eq!(
        repo.find_reference("refs/heads/origin/main")
            .unwrap()
            .target(),
        Some(tip)
    );
    assert_eq!(
        repo.find_reference("refs/remotes/origin/main")
            .unwrap()
            .target(),
        Some(tip)
    );
    assert!(!agent_log.exists());
}
