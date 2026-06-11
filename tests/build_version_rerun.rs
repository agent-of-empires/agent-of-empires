//! Regression test for issue #1962: inside a git worktree the build script
//! must watch the *real* per-worktree `HEAD`/`index`, not the literal
//! `.git/HEAD` (which does not exist there). A missing `rerun-if-changed`
//! input makes cargo treat the build script as perpetually stale, recompiling
//! the lib + binary on every build.
//!
//! The test drives the actual watch-path logic used by `build.rs`
//! (`build_git_watch.rs`, shared via `include!`) against a temporary git
//! worktree and asserts every watched path exists on disk.

#[path = "../build_git_watch.rs"]
mod build_git_watch;

use std::path::Path;
use std::process::Command;

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("failed to run git")
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
fn watch_paths_resolve_in_a_git_worktree() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let main_repo = tmp.path().join("main");
    std::fs::create_dir(&main_repo).expect("create main repo dir");

    assert!(git(&main_repo, &["init", "-q"]).status.success());
    // Identity + a commit so HEAD points at something and a worktree can branch.
    git(&main_repo, &["config", "user.email", "test@example.com"]);
    git(&main_repo, &["config", "user.name", "Test"]);
    assert!(git(&main_repo, &["commit", "--allow-empty", "-qm", "init"])
        .status
        .success());

    let worktree = tmp.path().join("wt");
    assert!(
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
        )
        .status
        .success(),
        "failed to create worktree"
    );

    // Sanity: in a worktree `.git` is a file, so the naive hardcoded path the
    // bug used does not exist. This is precisely why hardcoding broke caching.
    assert!(
        !worktree.join(".git/HEAD").exists(),
        "expected `.git/HEAD` to be absent in a worktree"
    );

    let paths = build_git_watch::git_watch_paths(&worktree);
    assert_eq!(
        paths.len(),
        2,
        "expected HEAD and index watch paths, got {paths:?}"
    );
    for watched in &paths {
        assert!(
            watch_path_exists(&worktree, watched),
            "watched path does not exist: {watched}"
        );
    }
}

#[test]
fn watch_paths_resolve_in_a_normal_checkout() {
    if !git_available() {
        eprintln!("skipping: git not available");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path().join("repo");
    std::fs::create_dir(&repo).expect("create repo dir");

    assert!(git(&repo, &["init", "-q"]).status.success());
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    assert!(git(&repo, &["commit", "--allow-empty", "-qm", "init"])
        .status
        .success());

    let paths = build_git_watch::git_watch_paths(&repo);
    assert!(
        paths.iter().any(|p| p.ends_with("HEAD")),
        "expected a HEAD watch path, got {paths:?}"
    );
    for watched in &paths {
        assert!(
            watch_path_exists(&repo, watched),
            "watched path does not exist: {watched}"
        );
    }
}

#[test]
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
