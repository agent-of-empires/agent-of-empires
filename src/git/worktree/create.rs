//! `create_worktree`: fetching the base, resolving the branch, checkout and
//! submodule initialisation.

use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

use regex::Regex;

use super::branch::DefaultBranchInfo;
use super::{path_str, GitWorktree, FETCH_REMOTE};
use crate::git::error::{GitError, Result};
use crate::git::open_repo_at;

/// Redacts the userinfo of `scheme://user:token@host` URLs, which git echoes
/// in fetch errors, before stderr is logged or shown.
fn sanitize_remote_credentials(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"([a-zA-Z][a-zA-Z0-9+\-.]*://)[^/\s@]+@").expect("static regex always compiles")
    });
    re.replace_all(s, "${1}<redacted>@").into_owned()
}

/// A branch checked out elsewhere becomes `BranchAlreadyCheckedOut` (dropping
/// the other worktree's path); anything else keeps its redacted output.
fn classify_worktree_add_failure(combined: &str, branch: &str) -> GitError {
    let lower = combined.to_ascii_lowercase();
    if lower.contains("already used by worktree") || lower.contains("already checked out at") {
        GitError::BranchAlreadyCheckedOut(branch.to_string())
    } else {
        GitError::WorktreeCommandFailed(sanitize_remote_credentials(combined))
    }
}

/// Result of [`GitWorktree::fetch_branch`]. Non-`Ok` outcomes carry a detail
/// that callers surface as a stale-base warning.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FetchOutcome {
    Ok,
    Failed(String),
    Skipped(String),
    TimedOut,
}

impl GitWorktree {
    /// `git fetch <remote> <branch>` with stdin nulled (no passphrase prompts)
    /// and a 10 second bound.
    fn fetch_branch(&self, remote: &str, branch: &str) -> FetchOutcome {
        let mut cmd = std::process::Command::new("git");
        cmd.args(["fetch", remote, branch])
            .current_dir(&self.repo_path)
            .stdin(std::process::Stdio::null());
        let timeout = std::time::Duration::from_secs(10);
        let start = Instant::now();
        match crate::process::run_with_timeout(&mut cmd, timeout) {
            Ok(Some(output)) if output.status.success() => {
                tracing::info!(target: "git.worktree", "git fetch {remote}/{branch} ok in {:?}", start.elapsed());
                FetchOutcome::Ok
            }
            Ok(Some(output)) => {
                let sanitized =
                    sanitize_remote_credentials(String::from_utf8_lossy(&output.stderr).trim());
                tracing::warn!(target: "git.worktree", "git fetch {remote}/{branch} failed: {sanitized}");
                FetchOutcome::Failed(if sanitized.is_empty() {
                    format!("git fetch exited with {}", output.status)
                } else {
                    sanitized
                })
            }
            Ok(None) => {
                tracing::warn!(target: "git.worktree",
                    "git fetch {remote}/{branch} timed out after {}s",
                    timeout.as_secs()
                );
                FetchOutcome::TimedOut
            }
            Err(e) => {
                tracing::warn!(
                    target: "git.command",
                    remote = %remote,
                    branch = %branch,
                    error = %e,
                    "git fetch spawn failed"
                );
                FetchOutcome::Skipped(format!("spawn failed: {e}"))
            }
        }
    }

    /// Fetches `branch` and records a non-`Ok` outcome as a warning.
    fn fetch_with_warning(&self, warnings: &mut Vec<String>, remote: &str, branch: &str) {
        let detail = match self.fetch_branch(remote, branch) {
            FetchOutcome::Ok => return,
            FetchOutcome::Failed(msg) | FetchOutcome::Skipped(msg) => msg,
            FetchOutcome::TimedOut => "timed out after 10s".to_string(),
        };
        warnings.push(format!(
            "git fetch {remote} {branch} failed for {repo}: {detail}",
            repo = self.repo_path.display()
        ));
    }

    /// Create a worktree at `path` checking out `branch`.
    ///
    /// With `create_branch`, the branch starts at `base_branch` (else the
    /// detected default), resolved remote-first then local. Returns non-fatal
    /// warnings (stale fetches, a failed post-checkout hook, a failed lock)
    /// for the caller to show.
    pub fn create_worktree(
        &self,
        branch: &str,
        path: &Path,
        create_branch: bool,
        base_branch: Option<&str>,
    ) -> Result<Vec<String>> {
        let total_start = Instant::now();
        let mut warnings: Vec<String> = Vec::new();
        tracing::info!(target: "git.worktree",
            "worktree create: start branch={} path={}", branch, path.display());

        if path.exists() {
            return Err(GitError::WorktreeAlreadyExists(path.to_path_buf()));
        }

        // A stale entry at exactly this path may be one aoe locked, and prune
        // skips locked entries; siblings keep their locks.
        self.unlock_worktree(path);
        let t = Instant::now();
        self.prune_worktrees()?;
        tracing::info!(target: "git.worktree", "worktree create: prune done in {:?}", t.elapsed());

        let t = Instant::now();
        if create_branch {
            let base = self.fetch_base(&mut warnings, base_branch);
            tracing::info!(target: "git.worktree", "worktree create: fetch step done in {:?}", t.elapsed());
            let t = Instant::now();
            self.create_branch_from_base(branch, base)?;
            tracing::info!(target: "git.worktree", "worktree create: branch resolve done in {:?}", t.elapsed());
        } else {
            self.fetch_with_warning(&mut warnings, FETCH_REMOTE, branch);
            tracing::info!(target: "git.worktree", "worktree create: fetch step done in {:?}", t.elapsed());
            let t = Instant::now();
            self.ensure_branch_exists(branch)?;
            tracing::info!(target: "git.worktree", "worktree create: branch resolve done in {:?}", t.elapsed());
        }

        self.add_worktree(branch, path, &mut warnings)?;

        // Relative, so the checkout resolves when mounted elsewhere.
        let t = Instant::now();
        Self::convert_git_file_to_relative(path)?;
        tracing::info!(target: "git.worktree",
            "worktree create: convert .git file done in {:?}", t.elapsed());

        let t = Instant::now();
        let submodule_status = if self.init_submodules {
            self.initialize_submodules(path)?
        } else {
            "disabled-by-config".to_string()
        };
        tracing::info!(target: "git.worktree",
            "worktree create: submodules ({}) done in {:?}", submodule_status, t.elapsed());

        if let Err(e) = self.lock_worktree(path) {
            let warning = format!(
                "could not lock worktree {} (cross-boundary prune protection unavailable): {}",
                path.display(),
                e
            );
            tracing::warn!(target: "git.worktree", "worktree create: {}", warning);
            warnings.push(warning);
        }

        tracing::info!(target: "git.worktree",
            "worktree create: TOTAL {:?} branch={} path={} warnings={}",
            total_start.elapsed(),
            branch,
            path.display(),
            warnings.len()
        );
        Ok(warnings)
    }

    /// Fetches the base for a new branch. An explicit base uses the freshest
    /// remote that has it (fork plus `upstream`, #1511, #1029); otherwise the
    /// detected default. Returns the base, its remote, and whether it was explicit.
    fn fetch_base(
        &self,
        warnings: &mut Vec<String>,
        base_branch: Option<&str>,
    ) -> (String, Option<String>, bool) {
        let (base, remote, explicit) = match base_branch.map(str::trim) {
            Some(base) if !base.is_empty() => {
                (base.to_string(), self.pick_remote_for_branch(base), true)
            }
            _ => {
                let info = self
                    .detect_default_branch_info()
                    .unwrap_or(DefaultBranchInfo {
                        name: "main".to_string(),
                        remote: None,
                    });
                (info.name, info.remote, false)
            }
        };
        self.fetch_with_warning(warnings, remote.as_deref().unwrap_or(FETCH_REMOTE), &base);
        (base, remote, explicit)
    }

    /// Branches from `<remote>/<base>`, then `origin/<base>`, then local
    /// `<base>`. An explicit base that resolves to none of them is
    /// `BranchNotFound` so a typo cannot anchor a session to a bystander
    /// commit; an autodetected one falls back to HEAD, then any local branch.
    fn create_branch_from_base(
        &self,
        branch: &str,
        (base, base_remote, explicit): (String, Option<String>, bool),
    ) -> Result<()> {
        let repo = open_repo_at(&self.repo_path)?;
        let branch_tip = |name: &str, kind| {
            repo.find_branch(name, kind)
                .ok()
                .and_then(|b| b.get().target())
        };
        let primary_remote = base_remote.as_deref().unwrap_or(FETCH_REMOTE);
        let direct_match = branch_tip(
            &format!("{primary_remote}/{base}"),
            git2::BranchType::Remote,
        )
        .or_else(|| {
            (primary_remote != FETCH_REMOTE)
                .then(|| branch_tip(&format!("{FETCH_REMOTE}/{base}"), git2::BranchType::Remote))
                .flatten()
        })
        .or_else(|| branch_tip(&base, git2::BranchType::Local));

        let commit_oid = match direct_match {
            Some(oid) => oid,
            None if explicit => return Err(GitError::BranchNotFound(base)),
            None => repo
                .head()
                .ok()
                .and_then(|h| h.peel_to_commit().ok())
                .map(|c| c.id())
                .or_else(|| {
                    repo.branches(Some(git2::BranchType::Local))
                        .ok()?
                        .find_map(|b| b.ok().and_then(|(b, _)| b.get().target()))
                })
                .ok_or_else(|| {
                    GitError::WorktreeCommandFailed("No commits found to branch from".to_string())
                })?,
        };
        repo.branch(branch, &repo.find_commit(commit_oid)?, false)?;
        Ok(())
    }

    /// An existing branch must be local or on some remote.
    fn ensure_branch_exists(&self, branch: &str) -> Result<()> {
        let repo = open_repo_at(&self.repo_path)?;
        if repo.find_branch(branch, git2::BranchType::Local).is_ok() {
            return Ok(());
        }
        let suffix = format!("/{branch}");
        let has_remote = repo
            .branches(Some(git2::BranchType::Remote))
            .ok()
            .is_some_and(|branches| {
                branches.filter_map(|b| b.ok()).any(|(b, _)| {
                    b.name()
                        .ok()
                        .flatten()
                        .is_some_and(|name| name.ends_with(&suffix) || name == branch)
                })
            });
        if has_remote {
            Ok(())
        } else {
            Err(GitError::BranchNotFound(branch.to_string()))
        }
    }

    /// `git worktree add`. A post-checkout hook can fail after the checkout
    /// exists; that worktree is usable, so the failure becomes a warning.
    fn add_worktree(&self, branch: &str, path: &Path, warnings: &mut Vec<String>) -> Result<()> {
        let t = Instant::now();
        let output = crate::git::command::run_git(
            &self.repo_path,
            ["worktree", "add", path_str(path)?, branch],
        )?;
        let add_elapsed = t.elapsed();

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let combined = match (stdout.is_empty(), stderr.is_empty()) {
                (true, true) => "git worktree add failed".to_string(),
                (false, true) => stdout,
                (true, false) => stderr,
                (false, false) => format!("{stdout}\n{stderr}"),
            };
            let combined = sanitize_remote_credentials(&combined);
            if path.exists() && path.join(".git").exists() {
                let warning = format!(
                    "post-checkout hook failed for {} (worktree created, hook output below):\n{}",
                    path.display(),
                    combined.trim()
                );
                tracing::warn!(target: "git.worktree", "worktree create: {}", warning);
                warnings.push(warning);
            } else if self.worktree_path_for_branch(branch)?.is_some() {
                // Porcelain rather than git's localized diagnostic.
                return Err(GitError::BranchAlreadyCheckedOut(branch.to_string()));
            } else {
                return Err(classify_worktree_add_failure(&combined, branch));
            }
        }

        // The stats walk covers the whole checkout, so only pay for it when logged.
        if tracing::enabled!(tracing::Level::INFO) {
            let stats = walk_worktree_stats(path);
            tracing::info!(target: "git.worktree",
                "worktree create: git worktree add done in {:?} ({} files, {} bytes checked out{})",
                add_elapsed,
                stats.file_count,
                stats.total_bytes,
                if stats.capped { ", walk capped" } else { "" }
            );
        }
        Ok(())
    }

    fn initialize_submodules(&self, worktree_path: &Path) -> Result<String> {
        let gitmodules_path = worktree_path.join(".gitmodules");
        if !gitmodules_path.is_file() {
            return Ok("none".to_string());
        }
        let submodule_count = std::fs::read_to_string(&gitmodules_path)
            .map(|s| {
                s.lines()
                    .filter(|l| l.trim_start().starts_with("[submodule"))
                    .count()
            })
            .unwrap_or(0);

        // `-c` flags reach the child clones through `GIT_CONFIG_PARAMETERS`.
        let mut args: Vec<String> = Vec::new();
        #[cfg(test)]
        for config in &self.submodule_config {
            args.push("-c".to_string());
            args.push(config.clone());
        }
        args.extend(["submodule", "update", "--init", "--recursive"].map(String::from));

        let output = crate::git::command::run_git(worktree_path, &args)?;
        if output.status.success() {
            return Ok(format!("initialized count={}", submodule_count));
        }
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        } else {
            stderr
        };
        let lower = message.to_ascii_lowercase();
        if lower.contains("transport 'file' not allowed")
            || lower.contains("transport \"file\" not allowed")
            || lower.contains("disallowed by protocol.file.allow")
        {
            tracing::warn!(target: "git.worktree",
                "skipping submodule initialization in {} because git blocked local file transport: {}",
                worktree_path.display(),
                message
            );
            return Ok(format!(
                "skipped:file-transport-blocked count={}",
                submodule_count
            ));
        }
        Err(GitError::WorktreeCommandFailed(if message.is_empty() {
            "git submodule update --init --recursive failed".to_string()
        } else {
            message
        }))
    }
}

#[derive(Default)]
struct WorktreeWalkStats {
    file_count: u64,
    total_bytes: u64,
    capped: bool,
}

/// File count and size of a checkout, for logging only: skips the root `.git`,
/// does not follow symlinks, and stops below depth 6.
fn walk_worktree_stats(root: &Path) -> WorktreeWalkStats {
    const MAX_DEPTH: usize = 6;

    fn visit(dir: &Path, depth: usize, stats: &mut WorktreeWalkStats) {
        if depth > MAX_DEPTH {
            stats.capped = true;
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            if depth == 0 && entry.file_name() == ".git" {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                visit(&entry.path(), depth + 1, stats);
            } else if metadata.is_file() {
                stats.file_count += 1;
                stats.total_bytes += metadata.len();
            }
        }
    }

    let mut stats = WorktreeWalkStats::default();
    visit(root, 0, &mut stats);
    stats
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::git::test_support::{commit, init_repo, run_git};
    use git2::{BranchType, Oid, Repository};
    use std::path::PathBuf;
    use tempfile::TempDir;

    fn branch_tip(repo: &Repository, name: &str) -> Oid {
        repo.find_branch(name, BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id()
    }

    /// `origin` (a fork) and `upstream` both carry `branch`; upstream is one
    /// commit ahead. Returns the dirs to keep alive, the local clone, and the
    /// upstream and origin tips.
    pub(in crate::git::worktree) fn fork_upstream_layout(
        branch: &str,
    ) -> (Vec<TempDir>, PathBuf, Oid, Oid) {
        let refname = format!("refs/heads/{branch}");
        let seed = |dir: &TempDir| {
            let repo = Repository::init_bare(dir.path()).unwrap();
            repo.set_head(&refname).unwrap();
            let a = commit(
                &repo,
                Some(&refname),
                &[("file.txt", b"hello")],
                &[],
                Some(1_700_000_000),
            );
            (repo, a)
        };
        let upstream_dir = TempDir::new().unwrap();
        let (upstream, a) = seed(&upstream_dir);
        let upstream_tip = commit(
            &upstream,
            Some(&refname),
            &[("file2.txt", b"world")],
            &[a],
            Some(1_700_001_000),
        );
        let origin_dir = TempDir::new().unwrap();
        let (_, origin_tip) = seed(&origin_dir);

        let local_dir = TempDir::new().unwrap();
        Repository::clone(origin_dir.path().to_str().unwrap(), local_dir.path()).unwrap();
        run_git(
            local_dir.path(),
            &[
                "remote",
                "add",
                "upstream",
                upstream_dir.path().to_str().unwrap(),
            ],
        );
        run_git(local_dir.path(), &["fetch", "upstream"]);
        let local = local_dir.path().to_path_buf();
        (
            vec![upstream_dir, origin_dir, local_dir],
            local,
            upstream_tip,
            origin_tip,
        )
    }

    #[test]
    fn remote_credentials_are_redacted() {
        for (input, want) in [
            (
                "fatal: unable to access 'https://alice:supersecret@github.com/foo/bar.git/': 404",
                "fatal: unable to access 'https://<redacted>@github.com/foo/bar.git/': 404",
            ),
            (
                "Could not read from remote ssh://git:tokenval@example.com:22/foo",
                "Could not read from remote ssh://<redacted>@example.com:22/foo",
            ),
            (
                "fatal: 'origin' does not appear to be a git repository",
                "fatal: 'origin' does not appear to be a git repository",
            ),
            (
                "fatal: unable to access 'https://github.com/foo/bar.git/'",
                "fatal: unable to access 'https://github.com/foo/bar.git/'",
            ),
            // SCP-style has no URL userinfo to redact.
            (
                "fatal: Could not read from remote: git@github.com:foo/bar.git",
                "fatal: Could not read from remote: git@github.com:foo/bar.git",
            ),
        ] {
            assert_eq!(sanitize_remote_credentials(input), want);
        }

        for wording in [
            "fatal: 'feature/foo' is already used by worktree at '/tmp/repo-worktrees/feature-foo'",
            "fatal: 'feature/foo' is already checked out at '/tmp/other'",
        ] {
            assert!(matches!(
                classify_worktree_add_failure(wording, "feature/foo"),
                GitError::BranchAlreadyCheckedOut(b) if b == "feature/foo"
            ));
        }
        match classify_worktree_add_failure(
            "fatal: unable to access 'https://alice:supersecret@github.com/foo/bar.git/': 403",
            "feature/foo",
        ) {
            GitError::WorktreeCommandFailed(msg) => assert!(!msg.contains("supersecret"), "{msg}"),
            other => panic!("expected WorktreeCommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn fetch_branch_reports_outcome() {
        let (no_remote, _repo) = init_repo();
        let git_wt = GitWorktree::new(no_remote.path().to_path_buf()).unwrap();
        assert!(matches!(
            git_wt.fetch_branch("origin", "main"),
            FetchOutcome::Failed(_)
        ));

        let (_dirs, local, _, _) = fork_upstream_layout("main");
        let git_wt = GitWorktree::new(local).unwrap();
        assert_eq!(git_wt.fetch_branch("origin", "main"), FetchOutcome::Ok);
        assert!(matches!(
            git_wt.fetch_branch("origin", "nonexistent-branch"),
            FetchOutcome::Failed(_)
        ));
    }

    #[test]
    fn create_worktree_resolves_branch_bases() {
        let (dirs, local, upstream_tip, origin_tip) = fork_upstream_layout("main");
        let repo = Repository::open(&local).unwrap();
        let git_wt = GitWorktree::new(local.clone()).unwrap();
        let parent = TempDir::new().unwrap();

        // #1511: an explicit base comes from the freshest remote.
        git_wt
            .create_worktree("hotfix", &parent.path().join("hotfix"), true, Some("main"))
            .unwrap();
        assert_eq!(branch_tip(&repo, "hotfix"), upstream_tip);
        assert_ne!(upstream_tip, origin_tip);

        // #948: a local-only base is honored rather than the default tip.
        repo.branch("release", &repo.find_commit(origin_tip).unwrap(), false)
            .unwrap();
        git_wt
            .create_worktree(
                "from-release",
                &parent.path().join("rel"),
                true,
                Some("release"),
            )
            .unwrap();
        assert_eq!(branch_tip(&repo, "from-release"), origin_tip);

        // A typo'd explicit base is an error, not a worktree off HEAD.
        let typo = parent.path().join("typo");
        assert!(matches!(
            git_wt.create_worktree("typo", &typo, true, Some("maain")),
            Err(GitError::BranchNotFound(name)) if name == "maain"
        ));
        assert!(!typo.exists());

        // An existing branch only on a remote is checked out.
        let upstream = Repository::open_bare(dirs[0].path()).unwrap();
        upstream
            .branch(
                "remote-only",
                &upstream.find_commit(upstream_tip).unwrap(),
                false,
            )
            .unwrap();
        run_git(&local, &["fetch", "upstream"]);
        let remote_only = parent.path().join("remote-only");
        git_wt
            .create_worktree("remote-only", &remote_only, false, None)
            .unwrap();
        assert!(remote_only.join(".git").is_file());
        assert!(matches!(
            git_wt.create_worktree("missing", &parent.path().join("missing"), false, None),
            Err(GitError::BranchNotFound(_))
        ));

        // The same branch cannot be checked out twice.
        assert!(matches!(
            git_wt.create_worktree("main", &parent.path().join("again"), false, None),
            Err(GitError::BranchAlreadyCheckedOut(b)) if b == "main"
        ));
    }

    #[test]
    fn create_worktree_branches_from_remote_after_fetch() {
        let remote_dir = TempDir::new().unwrap();
        let remote = Repository::init_bare(remote_dir.path()).unwrap();
        remote.set_head("refs/heads/main").unwrap();
        let initial = commit(
            &remote,
            Some("refs/heads/main"),
            &[("file.txt", b"hello")],
            &[],
            None,
        );
        let local_dir = TempDir::new().unwrap();
        Repository::clone(remote_dir.path().to_str().unwrap(), local_dir.path()).unwrap();
        let remote_head = commit(
            &remote,
            Some("refs/heads/main"),
            &[("file2.txt", b"world")],
            &[initial],
            None,
        );

        let local = Repository::open(local_dir.path()).unwrap();
        assert_eq!(branch_tip(&local, "main"), initial);
        let wt_parent = TempDir::new().unwrap();
        GitWorktree::new(local_dir.path().to_path_buf())
            .unwrap()
            .create_worktree("new-feature", &wt_parent.path().join("wt"), true, None)
            .unwrap();
        assert_eq!(branch_tip(&local, "new-feature"), remote_head);
    }

    #[cfg(unix)]
    #[test]
    fn create_worktree_turns_fetch_and_hook_failures_into_warnings() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _repo) = init_repo();
        let bogus = dir.path().join("does-not-exist.git");
        run_git(
            dir.path(),
            &["remote", "add", "origin", bogus.to_str().unwrap()],
        );
        let hook = dir.path().join(".git/hooks/post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(
            &hook,
            "#!/bin/sh\necho 'simulated hook failure' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let wt_parent = TempDir::new().unwrap();
        let wt_path = wt_parent.path().join("wt");
        let warnings = GitWorktree::new(dir.path().to_path_buf())
            .unwrap()
            .create_worktree("new-feature", &wt_path, true, None)
            .unwrap()
            .join("\n");
        assert!(wt_path.join(".git").exists());
        for fragment in [
            "git fetch",
            "failed for",
            "post-checkout hook failed",
            "simulated hook failure",
        ] {
            assert!(
                warnings.contains(fragment),
                "missing {fragment:?}: {warnings}"
            );
        }
    }

    /// A repo whose `.claude` submodule holds `skill.md`, served over a
    /// `file://` bare clone or a plain local path, with `branch` on HEAD.
    fn repo_with_submodule(branch: &str, file_url: bool) -> Vec<TempDir> {
        let sub = TempDir::new().unwrap();
        run_git(sub.path(), &["init", "-q", "."]);
        std::fs::write(sub.path().join("skill.md"), "hello\n").unwrap();
        run_git(sub.path(), &["add", "-A"]);
        run_git(
            sub.path(),
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-qm",
                "init",
            ],
        );
        let bare = TempDir::new().unwrap();
        let url = if file_url {
            let path = bare.path().join("sub.git");
            run_git(
                bare.path(),
                &[
                    "clone",
                    "--bare",
                    "-q",
                    sub.path().to_str().unwrap(),
                    path.to_str().unwrap(),
                ],
            );
            format!("file://{}", path.display())
        } else {
            sub.path().display().to_string()
        };
        let repo = TempDir::new().unwrap();
        run_git(repo.path(), &["init", "-q", "."]);
        run_git(
            repo.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                &url,
                ".claude",
            ],
        );
        run_git(
            repo.path(),
            &[
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@e",
                "commit",
                "-qm",
                "add submodule",
            ],
        );
        run_git(repo.path(), &["branch", branch]);
        vec![repo, sub, bare]
    }

    #[test]
    #[serial_test::serial]
    fn create_worktree_submodule_handling() {
        // Ambient git config (e.g. a global excludesFile ignoring `.claude`)
        // breaks the submodule fixtures, so anchor HOME.
        let home_dir = TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(home_dir.path());

        // (file url, allow file transport, init submodules, populated)
        for (file_url, allow, init, populated) in [
            (true, true, true, true),
            (true, false, false, false),
            // Blocked by git's default; the worktree is still created.
            (false, false, true, false),
        ] {
            let dirs = repo_with_submodule("test-feature", file_url);
            let mut git_wt = GitWorktree::new(dirs[0].path().to_path_buf())
                .unwrap()
                .with_init_submodules(init);
            if allow {
                git_wt = git_wt.allow_submodule_file_transport();
            }
            let parent = TempDir::new().unwrap();
            let wt_path = parent.path().join("wt");
            git_wt
                .create_worktree("test-feature", &wt_path, false, None)
                .unwrap();
            assert!(wt_path.join(".gitmodules").is_file());
            assert_eq!(wt_path.join(".claude/skill.md").is_file(), populated);
        }
    }

    #[test]
    fn walk_worktree_stats_counts_checked_out_files() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".git"), "gitdir: /somewhere/else").unwrap();
        std::fs::write(root.join("a.txt"), "hello").unwrap();
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/c.txt"), "xy").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("a.txt"), root.join("link.txt")).unwrap();
        let stats = walk_worktree_stats(root);
        assert_eq!(
            (stats.file_count, stats.total_bytes, stats.capped),
            (2, 7, false)
        );

        let mut deep = root.to_path_buf();
        for i in 0..10 {
            deep = deep.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(deep.join("deep.txt"), "z").unwrap();
        let stats = walk_worktree_stats(root);
        assert!(stats.capped);
        assert_eq!(stats.file_count, 2, "files past the cap are not counted");
    }
}
