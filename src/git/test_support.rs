//! Repository fixtures shared by the git module's tests.

use std::path::Path;

use git2::{Oid, Repository, Signature, Time};
use tempfile::TempDir;

/// Runs `git <args>` in `path`, asserting success, and returns stdout.
pub(crate) fn run_git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {} failed:\nstdout: {}\nstderr: {}",
        path.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Commits `files` (path, content) on top of `parents` to `refname`, at
/// `time` seconds when given so tests can order commits deterministically.
pub(crate) fn commit(
    repo: &Repository,
    refname: Option<&str>,
    files: &[(&str, &[u8])],
    parents: &[Oid],
    time: Option<i64>,
) -> Oid {
    let sig = match time {
        Some(t) => Signature::new("Test", "test@example.com", &Time::new(t, 0)).unwrap(),
        None => Signature::now("Test", "test@example.com").unwrap(),
    };
    let base = parents
        .first()
        .map(|p| repo.find_commit(*p).unwrap().tree().unwrap());
    let mut builder = repo.treebuilder(base.as_ref()).unwrap();
    for (name, content) in files {
        let blob = repo.blob(content).unwrap();
        builder.insert(name, blob, 0o100644).unwrap();
    }
    let tree = repo.find_tree(builder.write().unwrap()).unwrap();
    let parents: Vec<_> = parents
        .iter()
        .map(|p| repo.find_commit(*p).unwrap())
        .collect();
    let parents: Vec<_> = parents.iter().collect();
    repo.commit(refname, &sig, &sig, "commit", &tree, &parents)
        .unwrap()
}

/// A non-bare repository with one empty commit on `HEAD`.
pub(crate) fn init_repo() -> (TempDir, Repository) {
    let dir = TempDir::new().unwrap();
    let repo = Repository::init(dir.path()).unwrap();
    commit(&repo, Some("HEAD"), &[], &[], None);
    (dir, repo)
}
