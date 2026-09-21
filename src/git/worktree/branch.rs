//! Branch queries and mutations: default-branch detection, deletion, rename.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::relocate::{classify_ref_observations, TimedMutationOutcome};
use super::{GitWorktree, FETCH_REMOTE, MUTATION_OBSERVATION_TIMEOUT, WORKTREE_MUTATION_TIMEOUT};
use crate::git::command::{run_git, run_git_with_timeout};
use crate::git::error::{GitError, Result};
use crate::git::open_repo_at;

/// A default branch and the remote it came from. The same short name can mean
/// different commits on different remotes (#1029), so callers that fetch or
/// branch off it need both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultBranchInfo {
    pub name: String,
    /// `None` for a purely local branch.
    pub remote: Option<String>,
}

impl DefaultBranchInfo {
    /// `"<remote>/<name>"`, or `name` for a local branch.
    pub fn qualified_ref(&self) -> String {
        match &self.remote {
            Some(remote) => format!("{remote}/{name}", name = self.name),
            None => self.name.clone(),
        }
    }
}

struct Candidate {
    name: String,
    remote: Option<String>,
    /// From a `refs/remotes/<r>/HEAD` target rather than the naming convention.
    is_stated_default: bool,
    oid: git2::Oid,
    commit_time: i64,
}

/// Remote HEAD targets, then `main`/`master` on every remote, then locally.
fn collect_default_branch_candidates(repo: &git2::Repository) -> Vec<Candidate> {
    let remote_names: Vec<String> = repo
        .remotes()
        .map(|rs| rs.iter().flatten().flatten().map(String::from).collect())
        .unwrap_or_default();

    let mut wanted: Vec<(String, Option<String>, bool)> = Vec::new();
    for remote in &remote_names {
        let prefix = format!("refs/remotes/{remote}/");
        let target = repo
            .find_reference(&format!("{prefix}HEAD"))
            .ok()
            .and_then(|r| r.symbolic_target().ok().flatten().map(str::to_string));
        if let Some(branch) = target.as_deref().and_then(|t| t.strip_prefix(&prefix)) {
            if branch != "HEAD" && !branch.is_empty() {
                wanted.push((branch.to_string(), Some(remote.clone()), true));
            }
        }
    }
    for remote in &remote_names {
        for name in ["main", "master"] {
            wanted.push((name.to_string(), Some(remote.clone()), false));
        }
    }
    for name in ["main", "master"] {
        wanted.push((name.to_string(), None, false));
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (name, remote, is_stated_default) in wanted {
        if seen.contains(&(name.clone(), remote.clone())) {
            continue;
        }
        let (full, kind) = match &remote {
            Some(r) => (format!("{r}/{name}"), git2::BranchType::Remote),
            None => (name.clone(), git2::BranchType::Local),
        };
        let Some(commit) = repo
            .find_branch(&full, kind)
            .ok()
            .and_then(|b| b.get().peel_to_commit().ok())
        else {
            continue;
        };
        seen.insert((name.clone(), remote.clone()));
        out.push(Candidate {
            name,
            remote,
            is_stated_default,
            oid: commit.id(),
            commit_time: commit.time().seconds(),
        });
    }
    out
}

impl GitWorktree {
    /// The remote whose `<remote>/<branch_name>` has the newest commit; ties go
    /// to `origin`, then alphabetically. `None` when no remote tracks it.
    pub fn pick_remote_for_branch(&self, branch_name: &str) -> Option<String> {
        let repo = open_repo_at(&self.repo_path).ok()?;
        let remotes = repo.remotes().ok()?;
        let mut best: Option<(String, i64)> = None;
        for remote in remotes.iter().flatten().flatten() {
            let Some(t) = repo
                .find_branch(&format!("{remote}/{branch_name}"), git2::BranchType::Remote)
                .ok()
                .and_then(|b| b.get().peel_to_commit().ok())
                .map(|c| c.time().seconds())
            else {
                continue;
            };
            let take = match &best {
                None => true,
                Some((cur_name, cur_t)) if t == *cur_t => {
                    cur_name != FETCH_REMOTE
                        && (remote == FETCH_REMOTE || remote < cur_name.as_str())
                }
                Some((_, cur_t)) => t > *cur_t,
            };
            if take {
                best = Some((remote.to_string(), t));
            }
        }
        best.map(|(name, _)| name)
    }

    /// Branches git itself states are this repo's default, which session
    /// teardown must never remove (#3215): a bare repo's `HEAD` target and every
    /// `refs/remotes/<r>/HEAD` target, else local `main`/`master`. A non-bare
    /// `HEAD` names what that checkout has, not the default, so it is ignored.
    /// Unlike [`Self::detect_default_branch_info`] there is no recency scoring
    /// or first-branch fallback, which would protect ordinary session branches.
    pub fn protected_default_branch_names(&self) -> Result<HashSet<String>> {
        let repo = open_repo_at(&self.repo_path)?;
        let mut names = HashSet::new();
        let symbolic_branch = |reference: &str, prefix: &str| -> Option<String> {
            let reference = repo.find_reference(reference).ok()?;
            let target = reference.symbolic_target().ok().flatten()?;
            target.strip_prefix(prefix).map(str::to_string)
        };
        if repo.is_bare() {
            names.extend(symbolic_branch("HEAD", "refs/heads/"));
        }
        for remote in repo.remotes()?.iter().flatten().flatten() {
            names.extend(symbolic_branch(
                &format!("refs/remotes/{remote}/HEAD"),
                &format!("refs/remotes/{remote}/"),
            ));
        }
        if names.is_empty() {
            names.extend(
                ["main", "master"]
                    .into_iter()
                    .filter(|b| repo.find_branch(b, git2::BranchType::Local).is_ok())
                    .map(str::to_string),
            );
        }
        Ok(names)
    }

    pub fn detect_default_branch(&self) -> Result<String> {
        self.detect_default_branch_info().map(|i| i.name)
    }

    /// The default branch to base new work on, across every remote (#1029).
    ///
    /// Among candidates HEAD descends from (or all of them, for unrelated
    /// history) the newest commit wins; ties prefer a remote's stated default,
    /// then remote over local, then `origin`. With no candidates at all, the
    /// first local branch.
    pub fn detect_default_branch_info(&self) -> Result<DefaultBranchInfo> {
        let repo = open_repo_at(&self.repo_path)?;
        let candidates = collect_default_branch_candidates(&repo);

        if candidates.is_empty() {
            if let Some(Ok((branch, _))) = repo.branches(Some(git2::BranchType::Local))?.next() {
                if let Ok(Some(name)) = branch.name() {
                    return Ok(DefaultBranchInfo {
                        name: name.to_string(),
                        remote: None,
                    });
                }
            }
            return Err(GitError::BranchNotFound(
                "No default branch found".to_string(),
            ));
        }

        let head = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let ancestors: Vec<&Candidate> = candidates
            .iter()
            .filter(|c| {
                head.as_ref().is_some_and(|head| {
                    repo.merge_base(head.id(), c.oid)
                        .is_ok_and(|base| base == c.oid)
                })
            })
            .collect();
        let pool = if ancestors.is_empty() {
            candidates.iter().collect()
        } else {
            ancestors
        };
        let pick = pool
            .into_iter()
            .max_by(|a, b| {
                a.commit_time
                    .cmp(&b.commit_time)
                    .then_with(|| a.is_stated_default.cmp(&b.is_stated_default))
                    .then_with(|| a.remote.is_some().cmp(&b.remote.is_some()))
                    .then_with(|| {
                        let origin = |c: &Candidate| c.remote.as_deref() == Some(FETCH_REMOTE);
                        origin(a).cmp(&origin(b))
                    })
                    .then_with(|| b.name.cmp(&a.name))
            })
            .expect("pool is non-empty when candidates is non-empty");
        Ok(DefaultBranchInfo {
            name: pick.name.clone(),
            remote: pick.remote.clone(),
        })
    }

    /// Delete a local branch; an absent branch is success.
    ///
    /// Ownership and existence come from refs and porcelain output, never from
    /// git's localized diagnostics. An unmerged branch no worktree holds is
    /// force-deleted.
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        tracing::debug!(target: "git.worktree",
            branch,
            repo = %self.repo_path.display(),
            "delete_branch: invoking `git branch -d`"
        );
        if !self.branch_exists(branch)? {
            tracing::debug!(target: "git.worktree", branch,
                "delete_branch: branch already absent, treating as success");
            return Ok(());
        }

        // A relocated, re-locked worktree whose checkout later vanished keeps
        // its admin entry (prune skips locked entries), so the branch stays
        // held; reap that entry once and retry.
        let mut used_by_worktree_retried = false;
        loop {
            let output = run_git(&self.repo_path, ["branch", "-d", branch])?;
            if output.status.success() {
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(target: "git.worktree",
                branch,
                exit = ?output.status.code(),
                stderr = %stderr,
                stdout = %String::from_utf8_lossy(&output.stdout),
                "delete_branch: `git branch -d` failed"
            );

            if !self.branch_exists(branch)? {
                tracing::debug!(target: "git.worktree", branch,
                    "delete_branch: branch concurrently removed, treating as success");
                return Ok(());
            }

            if let Some(path) = self.worktree_path_for_branch(branch)? {
                if !used_by_worktree_retried && checkout_confirmed_absent(path.try_exists()) {
                    used_by_worktree_retried = true;
                    tracing::warn!(target: "git.worktree",
                        branch,
                        path = %path.display(),
                        "delete_branch: branch held by a lingering worktree entry whose checkout is gone; reaping and retrying"
                    );
                    self.reap_worktree_entry(&path);
                    continue;
                }
                return Err(GitError::WorktreeCommandFailed(format!(
                    "git branch -d {}: {}",
                    branch,
                    stderr.trim()
                )));
            }

            let force_output = run_git(&self.repo_path, ["branch", "-D", branch])?;
            if !force_output.status.success() {
                let force_stderr = String::from_utf8_lossy(&force_output.stderr);
                tracing::debug!(target: "git.worktree",
                    branch,
                    exit = ?force_output.status.code(),
                    stderr = %force_stderr,
                    "delete_branch: `git branch -D` (force) also failed"
                );
                return Err(GitError::WorktreeCommandFailed(format!(
                    "git branch -D {}: {}",
                    branch,
                    force_stderr.trim()
                )));
            }
            return Ok(());
        }
    }

    /// The registered checkout path holding `branch`, including a stale locked
    /// entry whose checkout no longer exists.
    pub(super) fn worktree_path_for_branch(&self, branch: &str) -> Result<Option<PathBuf>> {
        let output = run_git(&self.repo_path, ["worktree", "list", "--porcelain", "-z"])?;
        if !output.status.success() {
            return Err(GitError::WorktreeCommandFailed(format!(
                "git worktree list --porcelain: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        let wanted = format!("refs/heads/{branch}");
        let mut path = None;
        for field in output.stdout.split(|byte| *byte == 0) {
            if let Some(raw_path) = field.strip_prefix(b"worktree ") {
                path = Some(PathBuf::from(
                    String::from_utf8_lossy(raw_path).into_owned(),
                ));
            } else if field.strip_prefix(b"branch ") == Some(wanted.as_bytes()) {
                return Ok(path);
            }
        }
        Ok(None)
    }

    /// Unlock and prune the one entry git ties to the branch being deleted.
    /// Git allows one worktree per branch, so this never reaches another live
    /// session's worktree (the over-reap #2414's lock prevents).
    fn reap_worktree_entry(&self, path: &Path) {
        self.unlock_worktree(path);
        let _ = self.prune_worktrees();
    }

    /// Whether `refs/heads/<branch>` exists. `Err` means the check itself
    /// failed or timed out; callers must fail closed on it (#2653).
    pub fn branch_exists(&self, branch: &str) -> Result<bool> {
        let refname = format!("refs/heads/{branch}");
        let output = run_git_with_timeout(
            &self.repo_path,
            ["show-ref", "--verify", "--quiet", &refname],
            MUTATION_OBSERVATION_TIMEOUT,
        )
        .map_err(|e| {
            GitError::WorktreeCommandFailed(format!("git show-ref --verify {refname}: {e}"))
        })?
        .ok_or_else(|| {
            GitError::WorktreeCommandFailed(format!(
                "git show-ref --verify {refname} timed out after {}s",
                MUTATION_OBSERVATION_TIMEOUT.as_secs()
            ))
        })?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            code => Err(GitError::WorktreeCommandFailed(format!(
                "git show-ref --verify {refname} exited {code:?}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }

    /// The branch's short upstream (e.g. `origin/hi`), used to warn that a
    /// rename leaves the remote branch behind.
    pub fn branch_upstream(&self, branch: &str) -> Option<String> {
        let refname = format!("refs/heads/{branch}");
        let output = run_git(
            &self.repo_path,
            ["for-each-ref", "--format=%(upstream:short)", &refname],
        )
        .ok()?;
        if !output.status.success() {
            return None;
        }
        let upstream = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!upstream.is_empty()).then_some(upstream)
    }

    /// `git branch -m`, which also updates a worktree that has `old` checked out.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<()> {
        tracing::info!(target: "git.worktree",
            old, new,
            repo = %self.repo_path.display(),
            "rename_branch: invoking `git branch -m`"
        );
        let Some(output) = run_git_with_timeout(
            &self.repo_path,
            ["branch", "-m", old, new],
            WORKTREE_MUTATION_TIMEOUT,
        )?
        else {
            let outcome = classify_ref_observations(
                observe_local_branch_ref(&self.repo_path, old),
                observe_local_branch_ref(&self.repo_path, new),
            );
            let secs = WORKTREE_MUTATION_TIMEOUT.as_secs();
            return match outcome {
                TimedMutationOutcome::Applied => {
                    tracing::warn!(target: "git.worktree", old, new, "timed-out branch rename completed before termination");
                    Ok(())
                }
                TimedMutationOutcome::Unchanged => Err(GitError::WorktreeCommandFailed(format!(
                    "`git branch -m` timed out after {secs}s without renaming"
                ))),
                TimedMutationOutcome::Indeterminate => {
                    Err(GitError::WorktreeCommandFailed(format!(
                        "`git branch -m` timed out after {secs}s and ref state is indeterminate"
                    )))
                }
            };
        };
        if !output.status.success() {
            return Err(super::command_failed(&output));
        }
        Ok(())
    }
}

/// Only a confirmed absence may reap; a `stat` error could hide a live checkout.
fn checkout_confirmed_absent(probe: std::io::Result<bool>) -> bool {
    matches!(probe, Ok(false))
}

fn observe_local_branch_ref(repo_path: &Path, branch: &str) -> Option<bool> {
    let refname = format!("refs/heads/{branch}");
    match run_git_with_timeout(
        repo_path,
        ["show-ref", "--verify", "--quiet", &refname],
        MUTATION_OBSERVATION_TIMEOUT,
    ) {
        Ok(Some(output)) if output.status.success() => Some(true),
        Ok(Some(output)) if output.status.code() == Some(1) => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::create::tests::fork_upstream_layout;
    use super::*;
    use crate::git::test_support::{commit, init_repo, run_git};
    use git2::Repository;
    use tempfile::TempDir;

    /// A repo with exact default-branch metadata: bare or not, an optional
    /// `HEAD` target, symbolic remote HEADs, and local branches.
    fn repo_with_default_metadata(
        bare: bool,
        head: Option<&str>,
        remote_heads: &[(&str, &str)],
        branches: &[&str],
    ) -> (TempDir, GitWorktree) {
        let dir = TempDir::new().unwrap();
        let repo = if bare {
            Repository::init_bare(dir.path()).unwrap()
        } else {
            Repository::init(dir.path()).unwrap()
        };
        let (first, rest) = branches.split_first().unwrap();
        let oid = commit(
            &repo,
            Some(&format!("refs/heads/{first}")),
            &[("file.txt", b"hello")],
            &[],
            None,
        );
        for branch in rest {
            repo.branch(branch, &repo.find_commit(oid).unwrap(), true)
                .unwrap();
        }
        if let Some(head) = head {
            repo.set_head(&format!("refs/heads/{head}")).unwrap();
        }
        for (remote, branch) in remote_heads {
            repo.remote(remote, "https://example.invalid/r.git")
                .unwrap();
            repo.reference_symbolic(
                &format!("refs/remotes/{remote}/HEAD"),
                &format!("refs/remotes/{remote}/{branch}"),
                true,
                "test",
            )
            .unwrap();
        }
        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        (dir, git_wt)
    }

    /// #3215: explicit metadata wins; `main`/`master` apply only when nothing
    /// is stated; a non-bare `HEAD` is never a source.
    #[test]
    fn protected_default_branch_names_authority() {
        type Case<'a> = (
            &'a str,
            bool,
            Option<&'a str>,
            &'a [(&'a str, &'a str)],
            &'a [&'a str],
            &'a [&'a str],
        );
        let cases: &[Case] = &[
            ("bare HEAD", true, Some("main"), &[], &["main"], &["main"]),
            (
                "remote HEAD",
                false,
                None,
                &[("origin", "develop")],
                &["develop", "main"],
                &["develop"],
            ),
            (
                "convention",
                false,
                None,
                &[],
                &["main", "master"],
                &["main", "master"],
            ),
            (
                "explicit beats stale main",
                true,
                Some("trunk"),
                &[],
                &["trunk", "main"],
                &["trunk"],
            ),
            (
                "disagreeing authorities",
                true,
                Some("trunk"),
                &[("origin", "develop")],
                &["trunk", "develop", "main"],
                &["trunk", "develop"],
            ),
            ("nothing to protect", false, None, &[], &["feature/x"], &[]),
        ];
        for (label, bare, head, remote_heads, branches, expected) in cases {
            let (_dir, git_wt) = repo_with_default_metadata(*bare, *head, remote_heads, branches);
            let expected: HashSet<String> = expected.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                git_wt.protected_default_branch_names().unwrap(),
                expected,
                "{label}"
            );
        }
    }

    #[test]
    fn detect_default_branch_candidates() {
        for (branches, want) in [
            (&["main"][..], "main"),
            (&["master"][..], "master"),
            (&["develop"][..], "develop"),
        ] {
            let (_dir, git_wt) =
                repo_with_default_metadata(false, Some(branches[0]), &[], branches);
            assert_eq!(git_wt.detect_default_branch().unwrap(), want);
        }

        // A remote's stated default beats local `main` at the same commit.
        let remote_dir = TempDir::new().unwrap();
        let remote = Repository::init_bare(remote_dir.path()).unwrap();
        let oid = commit(
            &remote,
            Some("refs/heads/develop"),
            &[("file.txt", b"hello")],
            &[],
            None,
        );
        remote
            .branch("main", &remote.find_commit(oid).unwrap(), true)
            .unwrap();
        remote.set_head("refs/heads/develop").unwrap();
        let local_dir = TempDir::new().unwrap();
        let local =
            Repository::clone(remote_dir.path().to_str().unwrap(), local_dir.path()).unwrap();
        local
            .branch("main", &local.find_commit(oid).unwrap(), true)
            .unwrap();
        let git_wt = GitWorktree::new(local_dir.path().to_path_buf()).unwrap();
        assert_eq!(git_wt.detect_default_branch().unwrap(), "develop");

        // HEAD on unrelated history still yields a candidate.
        let (dir, repo) = init_repo();
        let orphan = commit(&repo, None, &[("orphan.txt", b"orphan")], &[], None);
        repo.set_head_detached(orphan).unwrap();
        let info = GitWorktree::new(dir.path().to_path_buf())
            .unwrap()
            .detect_default_branch_info()
            .unwrap();
        assert!(info.name == "main" || info.name == "master", "{info:?}");
        assert!(info.remote.is_none());
    }

    /// #1029: with a stale fork `origin`, the fresher `upstream/main` wins.
    #[test]
    fn detect_default_branch_picks_upstream_over_stale_origin() {
        let (_dirs, local, upstream_tip, _) = fork_upstream_layout("main");
        let repo = Repository::open(&local).unwrap();
        repo.branch("feature", &repo.find_commit(upstream_tip).unwrap(), false)
            .unwrap();
        repo.set_head("refs/heads/feature").unwrap();

        let git_wt = GitWorktree::new(local).unwrap();
        let info = git_wt.detect_default_branch_info().unwrap();
        assert_eq!(info.qualified_ref(), "upstream/main", "{info:?}");
        assert_eq!(
            DefaultBranchInfo {
                name: "develop".into(),
                remote: None
            }
            .qualified_ref(),
            "develop"
        );

        assert_eq!(
            git_wt.pick_remote_for_branch("main").as_deref(),
            Some("upstream")
        );
        assert_eq!(git_wt.pick_remote_for_branch("does-not-exist"), None);
    }

    #[test]
    fn pick_remote_for_branch_prefers_origin_on_tie() {
        let seed = || {
            let dir = TempDir::new().unwrap();
            let repo = Repository::init_bare(dir.path()).unwrap();
            repo.set_head("refs/heads/main").unwrap();
            commit(
                &repo,
                Some("refs/heads/main"),
                &[("file.txt", b"hello")],
                &[],
                Some(1_700_000_000),
            );
            dir
        };
        let (a, b) = (seed(), seed());
        let local = TempDir::new().unwrap();
        Repository::clone(a.path().to_str().unwrap(), local.path()).unwrap();
        run_git(
            local.path(),
            &["remote", "add", "upstream", b.path().to_str().unwrap()],
        );
        run_git(local.path(), &["fetch", "upstream"]);
        let git_wt = GitWorktree::new(local.path().to_path_buf()).unwrap();
        assert_eq!(
            git_wt.pick_remote_for_branch("main").as_deref(),
            Some("origin")
        );
    }

    #[test]
    fn delete_branch_outcomes() {
        let (dir, repo) = init_repo();
        let git = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        let head = repo.head().unwrap();
        repo.branch("to-delete", &head.peel_to_commit().unwrap(), false)
            .unwrap();

        git.delete_branch("to-delete").unwrap();
        assert!(repo
            .find_branch("to-delete", git2::BranchType::Local)
            .is_err());
        // Absent and never-valid names are already gone.
        git.delete_branch("nonexistent").unwrap();
        git.delete_branch("too many open files").unwrap();
        // The checked-out branch is a real failure.
        assert!(git.delete_branch(head.shorthand().unwrap()).is_err());
    }

    /// A relocated, locked worktree whose checkout vanished is reaped so its
    /// branch can go; a live one is never unlocked or force-deleted.
    #[test]
    fn delete_branch_reaps_only_vanished_locked_worktrees() {
        let (dir, _repo) = init_repo();
        let main = dir.path();
        let git = GitWorktree::new(main.to_path_buf()).unwrap();

        let holding = main.join(".aoe-trash/sess1");
        std::fs::create_dir_all(main.join(".aoe-trash")).unwrap();
        run_git(
            main,
            &["worktree", "add", "-b", "feat", holding.to_str().unwrap()],
        );
        git.lock_worktree(&holding).unwrap();
        std::fs::remove_dir_all(&holding).unwrap();
        git.prune_worktrees().unwrap();
        assert!(
            git.branch_exists("feat").unwrap(),
            "held by the locked entry"
        );
        git.delete_branch("feat").unwrap();
        assert!(!git.branch_exists("feat").unwrap());

        let live = main.join("wt-live");
        run_git(
            main,
            &["worktree", "add", "-b", "live", live.to_str().unwrap()],
        );
        git.lock_worktree(&live).unwrap();
        assert!(git.delete_branch("live").is_err());
        assert!(live.exists());
        assert!(git.branch_exists("live").unwrap());
        assert!(run_git(main, &["worktree", "list", "--porcelain"]).contains("locked"));

        use std::io::{Error, ErrorKind};
        assert!(checkout_confirmed_absent(Ok(false)));
        assert!(!checkout_confirmed_absent(Ok(true)));
        assert!(!checkout_confirmed_absent(Err(Error::from(
            ErrorKind::PermissionDenied
        ))));
    }
}
