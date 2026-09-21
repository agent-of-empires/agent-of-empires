//! `GitWorktree`: repository discovery and the worktree lifecycle. Creation,
//! branch operations and relocation live in the submodules.

mod branch;
mod create;
mod relocate;

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use super::error::{GitError, Result};
use super::open_repo_at;
use super::template::{resolve_template, TemplateVars};

/// Remote assumed when no candidate remote can be picked from local refs.
const FETCH_REMOTE: &str = "origin";

/// Bound for local metadata mutations that normally take milliseconds but can
/// hang on a stalled filesystem.
const WORKTREE_MUTATION_TIMEOUT: Duration = Duration::from_secs(30);
const MUTATION_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(5);

/// Reason on every aoe-created worktree lock. The lock keeps a `git worktree
/// prune` run from a context that cannot see this checkout (a sibling sandbox,
/// or the host) from reaping its admin entry (#2414); aoe unlocks before every
/// intentional remove or move.
const WORKTREE_LOCK_REASON: &str = "aoe-managed worktree (prevents cross-boundary prune)";

pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_detached: bool,
}

pub struct GitWorktree {
    pub repo_path: PathBuf,
    /// Whether `create_worktree` runs `git submodule update --init --recursive`.
    init_submodules: bool,
    /// `-c key=value` flags for that command. Fixtures use it to allow the
    /// `file://` transport per command instead of mutating process env (#2863).
    #[cfg(test)]
    submodule_config: Vec<String>,
}

fn path_str(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid path").into())
}

fn command_failed(output: &Output) -> GitError {
    GitError::WorktreeCommandFailed(String::from_utf8_lossy(&output.stderr).trim().to_string())
}

/// Runs a bounded git command in `cwd`, turning a timeout or non-zero exit
/// into `WorktreeCommandFailed`.
fn run_bounded<const N: usize>(cwd: &Path, args: [&str; N], what: &str) -> Result<Output> {
    let Some(output) = super::command::run_git_with_timeout(cwd, args, WORKTREE_MUTATION_TIMEOUT)?
    else {
        return Err(GitError::WorktreeCommandFailed(format!(
            "`{what}` timed out after {}s",
            WORKTREE_MUTATION_TIMEOUT.as_secs()
        )));
    };
    if !output.status.success() {
        return Err(command_failed(&output));
    }
    Ok(output)
}

impl GitWorktree {
    pub fn new(repo_path: PathBuf) -> Result<Self> {
        if !Self::is_git_repo(&repo_path) {
            return Err(GitError::NotAGitRepo);
        }
        Ok(Self {
            repo_path,
            init_submodules: true,
            #[cfg(test)]
            submodule_config: Vec::new(),
        })
    }

    pub fn with_init_submodules(mut self, init_submodules: bool) -> Self {
        self.init_submodules = init_submodules;
        self
    }

    /// Lets this instance's submodule update use `file://` URLs, which git
    /// blocks by default (CVE-2022-39253).
    #[cfg(test)]
    fn allow_submodule_file_transport(mut self) -> Self {
        self.submodule_config
            .push("protocol.file.allow=always".to_string());
        self
    }

    pub fn is_git_repo(path: &Path) -> bool {
        open_repo_at(path).is_ok()
            || Self::find_main_repo_from_linked_worktree_gitfile(path).is_some()
    }

    pub fn is_bare_repo(path: &Path) -> bool {
        open_repo_at(path).is_ok_and(|repo| repo.is_bare())
    }

    pub fn find_main_repo(path: &Path) -> Result<PathBuf> {
        let Ok(repo) = open_repo_at(path) else {
            return Self::find_main_repo_from_linked_worktree_gitfile(path)
                .ok_or(GitError::NotAGitRepo);
        };
        if let Some(main_repo) = Self::find_main_repo_from_worktree_gitdir(repo.path()) {
            return Ok(main_repo);
        }
        if let Some(workdir) = repo.workdir() {
            return Ok(workdir.to_path_buf());
        }
        let bare_repo_path = repo.path().to_path_buf();
        let parent_dir = bare_repo_path.parent().ok_or(GitError::NotAGitRepo)?;
        // A `.git` file (`gitdir: ./.bare`) marks the parent as the project
        // root. `is_file`, because tools such as opencode create stray `.git/`
        // directories wherever they run.
        if parent_dir.join(".git").is_file() {
            return Ok(parent_dir.to_path_buf());
        }
        Ok(bare_repo_path)
    }

    /// Resolves `<path>/.git` (a `gitdir:` file) pointing into
    /// `.../worktrees/<name>` to its repository root, without walking up.
    fn find_main_repo_from_linked_worktree_gitfile(path: &Path) -> Option<PathBuf> {
        let dir = if path.is_file() { path.parent()? } else { path };
        let git_file = dir.join(".git");
        if !git_file.is_file() {
            return None;
        }
        let content = std::fs::read_to_string(&git_file).ok()?;
        let gitdir = content
            .lines()
            .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))?;
        let gitdir = git_file.parent()?.join(gitdir).canonicalize().ok()?;
        Self::find_main_repo_from_worktree_gitdir(&gitdir)
    }

    fn find_main_repo_from_worktree_gitdir(gitdir: &Path) -> Option<PathBuf> {
        let worktrees_dir = gitdir.parent()?;
        if worktrees_dir.file_name() != Some(OsStr::new("worktrees")) {
            return None;
        }
        let git_or_bare_dir = worktrees_dir.parent()?;
        let parent_dir = git_or_bare_dir.parent()?;
        if git_or_bare_dir.file_name() == Some(OsStr::new(".git"))
            || parent_dir.join(".git").is_file()
        {
            return Some(parent_dir.to_path_buf());
        }
        Some(git_or_bare_dir.to_path_buf())
    }

    /// Prune stale worktree entries whose directories no longer exist on disk.
    pub fn prune_worktrees(&self) -> Result<()> {
        let output = super::command::run_git(&self.repo_path, ["worktree", "prune"])?;
        if !output.status.success() {
            return Err(command_failed(&output));
        }
        Ok(())
    }

    /// Lock `path`'s admin entry (see `WORKTREE_LOCK_REASON`). A failure only
    /// forfeits prune protection, so callers surface it as a warning.
    pub fn lock_worktree(&self, path: &Path) -> Result<()> {
        run_bounded(
            &self.repo_path,
            [
                "worktree",
                "lock",
                "--reason",
                WORKTREE_LOCK_REASON,
                path_str(path)?,
            ],
            "git worktree lock",
        )
        .map(drop)
    }

    /// Unlock `path`'s admin entry, best-effort and idempotent. Must precede
    /// `git worktree remove`/`move` (both refuse a locked tree) and any prune
    /// meant to reap the entry. Works from the admin side even when the
    /// checkout is gone; a non-zero exit is expected and logged at DEBUG.
    pub fn unlock_worktree(&self, path: &Path) {
        let Some(path_str) = path.to_str() else {
            return;
        };
        let _ = super::command::run_git_quiet_with_timeout(
            &self.repo_path,
            ["worktree", "unlock", path_str],
            WORKTREE_MUTATION_TIMEOUT,
        );
    }

    /// Rewrite a worktree's absolute `gitdir:` pointer as a relative one, so
    /// the checkout still resolves when mounted elsewhere (e.g. in a container).
    fn convert_git_file_to_relative(worktree_path: &Path) -> Result<()> {
        let git_file = worktree_path.join(".git");
        if !git_file.is_file() {
            return Ok(());
        }
        let content = std::fs::read_to_string(&git_file)?;
        let Some(gitdir_line) = content.lines().find(|l| l.starts_with("gitdir:")) else {
            return Ok(());
        };
        let absolute_path = Path::new(gitdir_line.trim_start_matches("gitdir:").trim());
        if absolute_path.is_relative() {
            return Ok(());
        }
        let worktree_canonical = worktree_path.canonicalize()?;
        let gitdir_canonical = absolute_path.canonicalize()?;
        if let Some(relative) = Self::diff_paths(&gitdir_canonical, &worktree_canonical) {
            std::fs::write(&git_file, format!("gitdir: {}\n", relative.display()))?;
        }
        Ok(())
    }

    /// Relative path from `base` to `target`.
    pub(crate) fn diff_paths(target: &Path, base: &Path) -> Option<PathBuf> {
        let mut target_components = target.components().peekable();
        let mut base_components = base.components().peekable();
        while let (Some(t), Some(b)) = (target_components.peek(), base_components.peek()) {
            if t != b {
                break;
            }
            target_components.next();
            base_components.next();
        }
        let mut result: PathBuf = base_components.map(|_| "..").collect();
        result.extend(target_components);
        Some(result)
    }

    pub fn list_worktrees(&self) -> Result<Vec<WorktreeEntry>> {
        let repo = open_repo_at(&self.repo_path)?;
        let mut entries = vec![];
        // Bare repos have no main worktree, only linked ones.
        if !repo.is_bare() {
            entries.push(WorktreeEntry {
                path: self.repo_path.clone(),
                branch: Self::get_current_branch(&self.repo_path).ok(),
                is_detached: repo.head_detached()?,
            });
        }
        for name in repo.worktrees()?.iter().filter_map(|r| r.ok().flatten()) {
            let Ok(wt) = repo.find_worktree(name) else {
                continue;
            };
            let Ok(path) = wt.path().canonicalize() else {
                continue;
            };
            entries.push(WorktreeEntry {
                branch: Self::get_current_branch(&path).ok(),
                path,
                is_detached: false,
            });
        }
        Ok(entries)
    }

    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        if !path.exists() {
            return Err(GitError::WorktreeNotFound(path.to_path_buf()));
        }
        // A single `--force` does not override a lock.
        self.unlock_worktree(path);
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(path_str(path)?);
        let output = super::command::run_git(&self.repo_path, &args)?;
        if !output.status.success() {
            return Err(command_failed(&output));
        }
        Ok(())
    }

    pub fn compute_path(&self, branch: &str, template: &str, session_id: &str) -> Result<PathBuf> {
        let vars = TemplateVars {
            repo_name: self
                .repo_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("repo")
                .to_string(),
            branch: branch.to_string(),
            session_id: session_id.to_string(),
            base_path: self.repo_path.clone(),
        };
        resolve_template(template, &vars)
    }

    pub fn get_current_branch(path: &Path) -> Result<String> {
        let repo = open_repo_at(path)?;
        let head = repo.head()?;
        head.shorthand()
            .map(str::to_string)
            .map_err(|_| GitError::NotAGitRepo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::test_support::{init_repo, run_git};
    use tempfile::TempDir;

    /// `.bare/` with `.git` containing `gitdir: ./.bare` and a `main` worktree.
    fn linked_bare_layout() -> TempDir {
        let dir = TempDir::new().unwrap();
        let bare = dir.path().join(".bare");
        let repo = git2::Repository::init_bare(&bare).unwrap();
        crate::git::test_support::commit(&repo, Some("HEAD"), &[], &[], None);
        std::fs::write(dir.path().join(".git"), "gitdir: ./.bare\n").unwrap();
        run_git(
            &bare,
            &[
                "worktree",
                "add",
                dir.path().join("main").to_str().unwrap(),
                "HEAD",
            ],
        );
        dir
    }

    /// `fe/foo.git` bare repo with a `fe/master` worktree on `main`.
    fn sibling_bare_layout() -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new().unwrap();
        let bare = dir.path().join("fe/foo.git");
        std::fs::create_dir_all(&bare).unwrap();
        let repo = git2::Repository::init_bare(&bare).unwrap();
        crate::git::test_support::commit(
            &repo,
            Some("refs/heads/main"),
            &[("README.md", b"hello\n")],
            &[],
            None,
        );
        let worktree = dir.path().join("fe/master");
        run_git(
            &bare,
            &["worktree", "add", worktree.to_str().unwrap(), "main"],
        );
        (dir, bare, worktree)
    }

    /// A bare repo under a parent holding a stray `.git/` directory, as
    /// opencode leaves behind.
    fn bare_under_spurious_git_dir() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join(".git/opencode"), "some-sha\n").unwrap();
        let bare = dir.path().join("bare");
        let repo = git2::Repository::init_bare(&bare).unwrap();
        crate::git::test_support::commit(&repo, Some("HEAD"), &[], &[], None);
        run_git(
            &bare,
            &[
                "worktree",
                "add",
                bare.join("main").to_str().unwrap(),
                "HEAD",
            ],
        );
        (dir, bare)
    }

    #[test]
    fn find_main_repo_resolves_each_layout_without_walking_up() {
        let plain = TempDir::new().unwrap();
        assert!(!GitWorktree::is_git_repo(plain.path()));
        assert!(GitWorktree::find_main_repo(plain.path()).is_err());

        let (regular, _repo) = init_repo();
        let linked = linked_bare_layout();
        let linked_main = linked.path().join("main");
        // Relative pointer, as `create_worktree` leaves it.
        GitWorktree::convert_git_file_to_relative(&linked_main).unwrap();
        let (_sibling, sibling_bare, sibling_wt) = sibling_bare_layout();
        let (_spurious, spurious_bare) = bare_under_spurious_git_dir();

        let canonical = |p: &Path| p.canonicalize().unwrap();
        let cases: [(&str, PathBuf, PathBuf, bool); 7] = [
            (
                "regular",
                regular.path().to_path_buf(),
                canonical(regular.path()),
                false,
            ),
            (
                "linked bare root",
                linked.path().to_path_buf(),
                canonical(linked.path()),
                true,
            ),
            (
                "linked bare worktree",
                linked_main.clone(),
                canonical(linked.path()),
                true,
            ),
            (
                "sibling bare",
                sibling_bare.clone(),
                canonical(&sibling_bare),
                true,
            ),
            (
                "sibling bare worktree",
                sibling_wt.clone(),
                canonical(&sibling_bare),
                true,
            ),
            (
                "bare under stray .git",
                spurious_bare.clone(),
                canonical(&spurious_bare),
                true,
            ),
            (
                "its worktree",
                spurious_bare.join("main"),
                canonical(&spurious_bare),
                true,
            ),
        ];
        for (label, start, want, bare) in cases {
            assert!(GitWorktree::is_git_repo(&start), "{label}");
            let main = GitWorktree::find_main_repo(&start).unwrap();
            assert_eq!(canonical(&main), want, "{label}");
            assert_eq!(GitWorktree::is_bare_repo(&main), bare, "{label}");
            let git_wt = GitWorktree::new(main).unwrap();
            assert!(!git_wt.list_worktrees().unwrap().is_empty(), "{label}");

            let nested = start.join("nested");
            std::fs::create_dir_all(&nested).unwrap();
            if label.contains("worktree") {
                assert!(!GitWorktree::is_git_repo(&nested), "{label}");
                assert!(GitWorktree::find_main_repo(&nested).is_err(), "{label}");
            }
        }
    }

    #[test]
    fn create_worktree_in_each_layout() {
        let (regular, _repo) = init_repo();
        let linked = linked_bare_layout();
        let (_sibling, sibling_bare, sibling_wt) = sibling_bare_layout();
        let (_spurious, spurious_bare) = bare_under_spurious_git_dir();

        for (i, (label, start)) in [
            ("regular", regular.path().to_path_buf()),
            ("linked bare root", linked.path().to_path_buf()),
            ("linked bare worktree", linked.path().join("main")),
            ("sibling bare", sibling_bare),
            ("sibling bare worktree", sibling_wt),
            ("bare under stray .git", spurious_bare),
        ]
        .into_iter()
        .enumerate()
        {
            let main = GitWorktree::find_main_repo(&start).unwrap();
            let git_wt = GitWorktree::new(main.clone()).unwrap();
            let template = if GitWorktree::is_bare_repo(&main) {
                "./{branch}"
            } else {
                "./{repo-name}-worktrees/{branch}"
            };
            let new_branch = format!("feat/new-{i}");
            let new_path = git_wt.compute_path(&new_branch, template, "abc").unwrap();
            assert!(
                new_path
                    .to_string_lossy()
                    .contains(&format!("feat-new-{i}")),
                "{label}"
            );
            git_wt
                .create_worktree(&new_branch, &new_path, true, None)
                .unwrap();
            assert!(new_path.join(".git").is_file(), "{label}");

            let repo = open_repo_at(&main).unwrap();
            let tip = repo
                .branches(Some(git2::BranchType::Local))
                .unwrap()
                .find_map(|b| b.ok()?.0.get().target())
                .unwrap();
            let existing = format!("existing-{i}");
            repo.branch(&existing, &repo.find_commit(tip).unwrap(), false)
                .unwrap();
            let existing_path = git_wt.compute_path(&existing, template, "abc").unwrap();
            git_wt
                .create_worktree(&existing, &existing_path, false, None)
                .unwrap();
            assert!(existing_path.join(".git").is_file(), "{label}");
            if label == "linked bare root" {
                assert_eq!(
                    new_path.parent().unwrap().canonicalize().unwrap(),
                    linked.path().canonicalize().unwrap(),
                    "bare layouts keep worktrees inside the project dir"
                );
            }
        }
    }

    /// #2414: the lock keeps a prune that cannot see the checkout from reaping
    /// its admin entry; a checkout deleted out of band is still recreatable.
    #[test]
    fn worktree_lock_survives_prune_and_remove_unlocks() {
        let (dir, repo) = init_repo();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("locked", &head, false).unwrap();
        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        let parent = TempDir::new().unwrap();
        let wt_path = parent.path().join("locked-worktree");
        git_wt
            .create_worktree("locked", &wt_path, false, None)
            .unwrap();

        let admin_dir = dir.path().join(".git/worktrees/locked-worktree");
        assert!(admin_dir.join("locked").exists());
        let hidden = parent.path().join("hidden");
        std::fs::rename(&wt_path, &hidden).unwrap();
        git_wt.prune_worktrees().unwrap();
        assert!(admin_dir.exists());
        std::fs::rename(&hidden, &wt_path).unwrap();

        git_wt.remove_worktree(&wt_path, true).unwrap();
        assert!(!wt_path.exists());
        assert!(!admin_dir.exists());
        assert!(matches!(
            git_wt.remove_worktree(&wt_path, false),
            Err(GitError::WorktreeNotFound(_))
        ));

        git_wt
            .create_worktree("locked", &wt_path, false, None)
            .unwrap();
        std::fs::remove_dir_all(&wt_path).unwrap();
        git_wt
            .create_worktree("locked", &wt_path, false, None)
            .unwrap();
        assert!(wt_path.join(".git").is_file());
    }

    /// Every git subprocess reachable from a worktree move or edit is bounded:
    /// the profile-move transaction runs them under app-global locks, so one
    /// hang pins every `aoe` process. Source-level, since a git hang on
    /// metadata is not reproducible in a unit test.
    #[test]
    fn every_worktree_mutation_subprocess_is_bounded() {
        let flat = [
            include_str!("mod.rs"),
            include_str!("branch.rs"),
            include_str!("relocate.rs"),
        ]
        .map(|source| source.split_once("\n#[cfg(test)]\nmod tests").unwrap().0)
        .concat()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
        for (needle, label) in [
            ("\"worktree\", \"lock\"", "git worktree lock"),
            ("\"worktree\", \"unlock\"", "git worktree unlock"),
            ("\"show-ref\", \"--verify\"", "git show-ref --verify"),
            ("\"worktree\", \"move\"", "git worktree move"),
            ("\"branch\", \"-m\"", "git branch -m"),
        ] {
            let at = flat
                .find(needle)
                .unwrap_or_else(|| panic!("{label} not found"));
            let call = flat[..at]
                .rfind("run_")
                .map(|i| &flat[i..])
                .unwrap_or_default();
            assert!(
                call.starts_with("run_git_with_timeout")
                    || call.starts_with("run_git_quiet_with_timeout")
                    || call.starts_with("run_bounded"),
                "{label} must go through a bounded helper, found `{}`",
                call.split('(').next().unwrap_or(call)
            );
        }
    }
}
