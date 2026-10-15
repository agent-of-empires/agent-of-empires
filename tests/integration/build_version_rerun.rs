//! Build-version watches must resolve worktree HEAD and ignore index churn.

#[path = "../../build_git_watch.rs"]
mod build_git_watch;

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_PREFIX")
        .env("GIT_TEMPLATE_DIR", "")
        .output()
        .expect("failed to run git");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Resolve a watch path (absolute as-is, relative against `base`) and check it
/// points at an existing file.
fn watch_path_exists(base: &Path, watched: &str) -> bool {
    let p = Path::new(watched);
    let resolved = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    resolved.exists()
}

#[test]
#[serial_test::parallel]
fn watch_paths_resolve_in_a_git_worktree() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let main_repo = tmp.path().join("main");
    std::fs::create_dir(&main_repo).expect("create main repo dir");

    git(&main_repo, &["init", "-q"]);
    git(&main_repo, &["config", "user.email", "test@example.com"]);
    git(&main_repo, &["config", "user.name", "Test"]);
    git(&main_repo, &["commit", "--allow-empty", "-qm", "init"]);

    let worktree = tmp.path().join("wt");
    git(
        &main_repo,
        &[
            "worktree",
            "add",
            "-q",
            worktree.to_str().unwrap(),
            "-b",
            "wt-branch",
        ],
    );

    // Sanity: in a worktree `.git` is a file, so the naive hardcoded path the
    // bug used does not exist. This is precisely why hardcoding broke caching.
    assert!(
        !worktree.join(".git/HEAD").exists(),
        "expected `.git/HEAD` to be absent in a worktree"
    );

    let paths = build_git_watch::git_watch_paths(&worktree);
    let wt_git_dir = String::from_utf8(git(&worktree, &["rev-parse", "--absolute-git-dir"]).stdout)
        .expect("utf8 git dir");
    let wt_git_dir = wt_git_dir.trim();
    for expected in ["HEAD", "logs/HEAD"] {
        let want = format!("{wt_git_dir}/{expected}");
        assert!(
            paths.iter().any(|p| p == &want),
            "expected the per-worktree {expected} in {paths:?}"
        );
    }
    assert_eq!(
        paths.len(),
        2,
        "expected only the per-worktree HEAD and logs/HEAD, got {paths:?}"
    );
    for watched in &paths {
        assert!(
            watch_path_exists(&worktree, watched),
            "watched path does not exist: {watched}"
        );
    }
}

#[test]
#[serial_test::parallel]
fn watch_paths_ignore_inherited_repository_context() {
    const CHILD_DIR: &str = "AOE_BUILD_WATCH_CHILD_DIR";
    const CHILD_RECORD: &str = "AOE_BUILD_WATCH_CHILD_RECORD";
    if let (Some(directory), Some(record)) =
        (std::env::var_os(CHILD_DIR), std::env::var_os(CHILD_RECORD))
    {
        let paths = build_git_watch::git_watch_paths(Path::new(&directory));
        std::fs::write(record, serde_json::to_vec(&paths).unwrap()).unwrap();
        return;
    }
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }

    let temporary = tempfile::tempdir().unwrap();
    let target = temporary.path().join("target");
    let foreign = temporary.path().join("foreign");
    for repository in [&target, &foreign] {
        std::fs::create_dir(repository).unwrap();
        git(repository, &["init", "-q"]);
        git(repository, &["config", "user.email", "test@example.com"]);
        git(repository, &["config", "user.name", "Test"]);
        git(repository, &["commit", "--allow-empty", "-qm", "init"]);
    }
    let foreign_git = foreign.join(".git");
    let record = temporary.path().join("observed.json");
    let configuration = temporary.path().join("git-config");
    std::fs::write(&configuration, "").unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "build_version_rerun::watch_paths_ignore_inherited_repository_context",
            "--nocapture",
        ])
        .env(CHILD_DIR, &target)
        .env(CHILD_RECORD, &record)
        .env("GIT_DIR", &foreign_git)
        .env("GIT_WORK_TREE", &foreign)
        .env("GIT_COMMON_DIR", &foreign_git)
        .env("GIT_INDEX_FILE", foreign_git.join("index"))
        .env("GIT_OBJECT_DIRECTORY", foreign_git.join("objects"))
        .env(
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            foreign_git.join("objects"),
        )
        .env("GIT_NAMESPACE", "foreign")
        .env("GIT_CONFIG_GLOBAL", &configuration)
        .env("GIT_CONFIG_SYSTEM", &configuration)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let paths: Vec<String> = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    let mut observed: Vec<_> = paths
        .iter()
        .map(|path| {
            let path = Path::new(path);
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                target.join(path)
            }
            .canonicalize()
            .unwrap()
        })
        .collect();
    let mut expected: Vec<_> = ["HEAD", "logs/HEAD"]
        .iter()
        .map(|path| target.join(".git").join(path).canonicalize().unwrap())
        .collect();
    observed.sort();
    expected.sort();
    assert_eq!(observed, expected);
}

#[test]
#[serial_test::parallel]
fn git_watch_paths_empty_when_git_cannot_resolve() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }
    // Point at a path with no repository so `git -C` fails: resolution yields
    // nothing and the build version stays pinned to CARGO_PKG_VERSION with no
    // rerun trigger (the source-tarball / no-VCS fallback). Using a
    // nonexistent directory keeps this deterministic regardless of whether the
    // system temp dir happens to sit inside a git repository.
    let tmp = tempfile::tempdir().expect("tempdir");
    let no_repo = tmp.path().join("nonexistent");
    assert!(build_git_watch::git_watch_paths(&no_repo).is_empty());
}

/// The watch set has to fire on a revision change and stay quiet through the
/// `git status` churn that made watching `index` unusable. Assertions compare
/// file *contents* rather than mtimes so the test does not depend on
/// filesystem timestamp granularity; a rewrite is what moves the mtime cargo
/// actually compares.
#[test]
#[serial_test::parallel]
fn watched_paths_move_on_a_commit_but_not_on_a_plain_status() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).expect("create repo dir");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    std::fs::write(repo.join("tracked.txt"), "one\n").expect("write tracked file");
    git(&repo, &["add", "tracked.txt"]);
    git(&repo, &["commit", "-qm", "init"]);

    let snapshot = |repo: &Path| -> Vec<(String, Vec<u8>)> {
        build_git_watch::git_watch_paths(repo)
            .into_iter()
            .map(|p| {
                let resolved = if Path::new(&p).is_absolute() {
                    PathBuf::from(&p)
                } else {
                    repo.join(&p)
                };
                let bytes = std::fs::read(&resolved).unwrap_or_else(|e| panic!("read {p}: {e}"));
                (p, bytes)
            })
            .collect()
    };
    let before = snapshot(&repo);
    assert!(!before.is_empty(), "expected at least one watched path");

    // Stat-only churn plus a prompt-style `git status`: git revalidates the
    // racily-clean entry and rewrites `index`. Nothing cargo watches may move.
    let index = repo.join(".git/index");
    let index_before = std::fs::read(&index).expect("read index");
    std::fs::File::options()
        .write(true)
        .open(repo.join("tracked.txt"))
        .expect("open tracked file")
        .set_times(
            std::fs::FileTimes::new()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
        )
        .expect("backdate tracked file");
    git(&repo, &["status", "--porcelain"]);
    assert_ne!(
        index_before,
        std::fs::read(&index).expect("read index"),
        "expected `git status` to rewrite the index stat cache"
    );
    assert_eq!(
        before,
        snapshot(&repo),
        "a plain `git status` must not disturb a watched path"
    );

    // A commit advances the branch ref; `HEAD` itself never changes, so the
    // reflog is what keeps the embedded version from going stale.
    git(&repo, &["commit", "-qm", "second", "--allow-empty"]);
    assert_ne!(
        before,
        snapshot(&repo),
        "a commit must disturb a watched path so the build version recomputes"
    );
}
