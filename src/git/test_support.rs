//! Git repository fixtures shared by in-crate tests.

use std::path::Path;

use git2::{Oid, Repository, Signature, Time};
use tempfile::TempDir;

/// Runs Git without inherited repository state or user configuration, asserting success.
pub(crate) fn run_git(path: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(path)
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
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t")
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

#[test]
fn run_git_ignores_inherited_repository_context() {
    const REPOSITORY: &str = "AOE_GIT_FIXTURE_REPOSITORY";
    const RECORD: &str = "AOE_GIT_FIXTURE_RECORD";
    if let (Some(repository), Some(record)) =
        (std::env::var_os(REPOSITORY), std::env::var_os(RECORD))
    {
        let repository = Path::new(&repository);
        let observed = [
            run_git(repository, &["rev-parse", "--absolute-git-dir"]),
            run_git(repository, &["config", "--get-all", "fixture.context"]),
        ];
        std::fs::write(record, serde_json::to_vec(&observed).unwrap()).unwrap();
        return;
    }

    let target = TempDir::new().unwrap();
    let foreign = TempDir::new().unwrap();
    for (directory, context) in [(target.path(), "target"), (foreign.path(), "foreign")] {
        Repository::init(directory)
            .unwrap()
            .config()
            .unwrap()
            .set_str("fixture.context", context)
            .unwrap();
    }
    let record = target.path().join("observed.json");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "git::test_support::run_git_ignores_inherited_repository_context",
            "--nocapture",
        ])
        .env(REPOSITORY, target.path())
        .env(RECORD, &record)
        .env("GIT_DIR", foreign.path().join(".git"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "fixture.context")
        .env("GIT_CONFIG_VALUE_0", "inherited")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let observed: [String; 2] = serde_json::from_slice(&std::fs::read(record).unwrap()).unwrap();
    assert_eq!(
        Path::new(observed[0].trim()).canonicalize().unwrap(),
        target.path().join(".git").canonicalize().unwrap()
    );
    assert_eq!(observed[1].trim(), "target");
}
