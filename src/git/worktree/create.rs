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

/// Execution mode is chosen by the caller before any Git mutation.
#[derive(Clone, Copy)]
enum CreateExecutor<'a> {
    Unmanaged,
    Owned(&'a crate::session::builder::CreationIntent),
}

impl CreateExecutor<'_> {
    fn run<I, S>(&self, cwd: &Path, args: I) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        match self {
            Self::Unmanaged => Ok(crate::git::command::run_git(cwd, args)?),
            Self::Owned(intent) => {
                let mut command = intent
                    .owned_git_command(cwd)
                    .map_err(|e| GitError::WorktreeCommandFailed(format!("{e:#}")))?;
                command.args(args).env("LC_ALL", "C");
                intent
                    .run_owned_output(&mut command, None)
                    .map_err(|e| GitError::WorktreeCommandFailed(format!("{e:#}")))
            }
        }
    }
}

impl GitWorktree {
    fn fetch_owned(
        &self,
        intent: &crate::session::builder::CreationIntent,
        remote: &str,
        branch: &str,
        timeout: std::time::Duration,
    ) -> std::io::Result<Option<std::process::Output>> {
        let mut command = intent
            .owned_git_command(&self.repo_path)
            .map_err(std::io::Error::other)?;
        command.args(["fetch", remote, branch]).env("LC_ALL", "C");
        let cancel = tokio_util::sync::CancellationToken::new();
        let deadline = cancel.clone();
        let (complete, finished) = std::sync::mpsc::channel::<()>();
        let timer = std::thread::Builder::new()
            .name("aoe-owned-fetch-deadline".into())
            .spawn(move || {
                if matches!(
                    finished.recv_timeout(timeout),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    deadline.cancel();
                }
            })?;
        let output = intent.run_owned_output(&mut command, Some(&cancel));
        drop(complete);
        timer
            .join()
            .map_err(|_| std::io::Error::other("owned fetch deadline worker panicked"))?;
        if cancel.is_cancelled() {
            Ok(None)
        } else {
            output.map(Some).map_err(std::io::Error::other)
        }
    }

    /// `git fetch <remote> <branch>` with stdin nulled (no passphrase prompts)
    /// and a 10 second bound.
    fn fetch_branch(
        &self,
        remote: &str,
        branch: &str,
        executor: CreateExecutor<'_>,
    ) -> FetchOutcome {
        let timeout = std::time::Duration::from_secs(10);
        let start = Instant::now();
        let result = match executor {
            CreateExecutor::Unmanaged => {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["fetch", remote, branch])
                    .current_dir(&self.repo_path)
                    .stdin(std::process::Stdio::null());
                crate::process::run_with_timeout(&mut cmd, timeout)
            }
            CreateExecutor::Owned(intent) => self.fetch_owned(intent, remote, branch, timeout),
        };
        match result {
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
    fn fetch_with_warning(
        &self,
        warnings: &mut Vec<String>,
        remote: &str,
        branch: &str,
        executor: CreateExecutor<'_>,
    ) {
        let detail = match self.fetch_branch(remote, branch, executor) {
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
        self.create_worktree_with(
            branch,
            path,
            create_branch,
            base_branch,
            CreateExecutor::Unmanaged,
        )
    }

    pub(crate) fn create_worktree_owned(
        &self,
        branch: &str,
        path: &Path,
        create_branch: bool,
        base_branch: Option<&str>,
        intent: &crate::session::builder::CreationIntent,
    ) -> Result<Vec<String>> {
        intent
            .require_worktree_plan(&self.repo_path, branch, path)
            .map_err(|e| GitError::WorktreeCommandFailed(format!("{e:#}")))?;
        self.create_worktree_with(
            branch,
            path,
            create_branch,
            base_branch,
            CreateExecutor::Owned(intent),
        )
    }

    fn create_worktree_with(
        &self,
        branch: &str,
        path: &Path,
        create_branch: bool,
        base_branch: Option<&str>,
        executor: CreateExecutor<'_>,
    ) -> Result<Vec<String>> {
        let total_start = Instant::now();
        let mut warnings: Vec<String> = Vec::new();
        tracing::info!(target: "git.worktree",
            "worktree create: start branch={} path={}", branch, path.display());

        if path.exists() {
            return Err(GitError::WorktreeAlreadyExists(path.to_path_buf()));
        }

        if let CreateExecutor::Owned(intent) = executor {
            intent
                .allocate_worktree_bootstrap(
                    &self.repo_path,
                    branch,
                    path,
                    super::WORKTREE_LOCK_REASON,
                )
                .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        }

        // A stale entry at exactly this path may be one aoe locked, and prune
        // skips locked entries; siblings keep their locks.
        let t = Instant::now();
        // Creating never prunes unrelated stale entries: those are not in its Undo plan.
        if matches!(executor, CreateExecutor::Unmanaged) {
            self.unlock_worktree(path);
            self.prune_worktrees()?;
        }
        tracing::info!(target: "git.worktree", "worktree create: prune done in {:?}", t.elapsed());

        let t = Instant::now();
        if create_branch {
            let base = self.fetch_base(&mut warnings, base_branch, executor);
            tracing::info!(target: "git.worktree", "worktree create: fetch step done in {:?}", t.elapsed());
            let t = Instant::now();
            if let CreateExecutor::Owned(intent) = executor {
                intent
                    .require_worktree_plan(&self.repo_path, branch, path)
                    .map_err(|e| GitError::WorktreeCommandFailed(format!("{e:#}")))?;
            }
            let produced = self.create_branch_from_base(branch, base, executor)?;
            if let CreateExecutor::Owned(intent) = executor {
                intent
                    .acknowledge_created_branch(&self.repo_path, branch, produced)
                    .map_err(|e| GitError::WorktreeCommandFailed(format!("{e:#}")))?;
            }
            tracing::info!(target: "git.worktree", "worktree create: branch resolve done in {:?}", t.elapsed());
        } else {
            self.fetch_with_warning(&mut warnings, FETCH_REMOTE, branch, executor);
            tracing::info!(target: "git.worktree", "worktree create: fetch step done in {:?}", t.elapsed());
            let t = Instant::now();
            match executor {
                CreateExecutor::Unmanaged => self.ensure_branch_exists(branch)?,
                CreateExecutor::Owned(intent) => {
                    self.ensure_owned_local_branch(branch, path, intent)?
                }
            }
            tracing::info!(target: "git.worktree", "worktree create: branch resolve done in {:?}", t.elapsed());
        }

        if let CreateExecutor::Owned(intent) = executor {
            intent
                .require_worktree_plan(&self.repo_path, branch, path)
                .map_err(|e| GitError::WorktreeCommandFailed(format!("{e:#}")))?;
        }
        match executor {
            CreateExecutor::Unmanaged => {
                self.add_worktree(branch, path, &mut warnings, executor)?;
            }
            CreateExecutor::Owned(intent) => {
                self.add_owned_worktree(branch, path, &mut warnings, intent)?;
            }
        }

        // Relative, so the checkout resolves when mounted elsewhere.
        let t = Instant::now();
        if matches!(executor, CreateExecutor::Unmanaged) {
            Self::convert_git_file_to_relative(path)?;
        }
        // The original issuer already wrote the relative .git link exclusively.
        tracing::info!(target: "git.worktree",
            "worktree create: convert .git file done in {:?}", t.elapsed());

        let t = Instant::now();
        let submodule_status = if self.init_submodules {
            self.initialize_submodules(path, executor)?
        } else {
            "disabled-by-config".to_string()
        };
        tracing::info!(target: "git.worktree",
            "worktree create: submodules ({}) done in {:?}", submodule_status, t.elapsed());

        let lock_result = match executor {
            CreateExecutor::Unmanaged => self.lock_worktree(path),
            CreateExecutor::Owned(intent) => intent
                .original_worktree_lock(path)
                .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}"))),
        };
        if let Err(e) = lock_result {
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
        if let CreateExecutor::Owned(intent) = executor {
            intent
                .acknowledge_worktree(&self.repo_path, branch, path, true)
                .map_err(|e| GitError::WorktreeCommandFailed(format!("{e:#}")))?;
        }
        Ok(warnings)
    }

    /// Fetches the base for a new branch. An explicit base uses the freshest
    /// remote that has it (fork plus `upstream`, #1511, #1029); otherwise the
    /// detected default. Returns the base, its remote, and whether it was explicit.
    fn fetch_base(
        &self,
        warnings: &mut Vec<String>,
        base_branch: Option<&str>,
        executor: CreateExecutor<'_>,
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
        self.fetch_with_warning(
            warnings,
            remote.as_deref().unwrap_or(FETCH_REMOTE),
            &base,
            executor,
        );
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
        executor: CreateExecutor<'_>,
    ) -> Result<git2::Oid> {
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
        if let CreateExecutor::Owned(_) = executor {
            let reference = format!("refs/heads/{branch}");
            let output = executor.run(
                &self.repo_path,
                [
                    "update-ref",
                    &reference,
                    &commit_oid.to_string(),
                    &git2::Oid::ZERO_SHA1.to_string(),
                ],
            )?;
            if !output.status.success() {
                return Err(GitError::WorktreeCommandFailed(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ));
            }
            return Ok(commit_oid);
        }
        let produced = repo.branch(branch, &repo.find_commit(commit_oid)?, false)?;
        produced.get().target().ok_or_else(|| {
            GitError::WorktreeCommandFailed(
                "Git branch producer did not return its actual tip".to_owned(),
            )
        })
    }

    fn ensure_owned_local_branch(
        &self,
        branch: &str,
        path: &Path,
        intent: &crate::session::builder::CreationIntent,
    ) -> Result<()> {
        let repo = open_repo_at(&self.repo_path)?;
        if repo.find_branch(branch, git2::BranchType::Local).is_ok() {
            return Ok(());
        }
        let suffix = format!("/{branch}");
        let mut candidates = Vec::new();
        for entry in repo.branches(Some(git2::BranchType::Remote))? {
            let (remote, _) = entry?;
            let name = remote.name()?.ok_or_else(|| {
                GitError::WorktreeCommandFailed("non-UTF8 remote branch remains protected".into())
            })?;
            if name.ends_with(&suffix) {
                let oid = remote.get().target().ok_or_else(|| {
                    GitError::WorktreeCommandFailed("remote branch has no commit OID".into())
                })?;
                candidates.push((name.to_owned(), oid));
            }
        }
        let preferred = repo.config()?.get_string("checkout.defaultRemote").ok();
        let selected = match candidates.as_slice() {
            [one] => one,
            _ => candidates
                .iter()
                .find(|(name, _)| {
                    preferred
                        .as_ref()
                        .is_some_and(|remote| name == &format!("{remote}/{branch}"))
                })
                .ok_or_else(|| GitError::BranchNotFound(branch.to_owned()))?,
        };
        intent
            .require_worktree_plan(&self.repo_path, branch, path)
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        let reference = format!("refs/heads/{branch}");
        let oid = selected.1;
        let output = CreateExecutor::Owned(intent).run(
            &self.repo_path,
            [
                "update-ref",
                &reference,
                &oid.to_string(),
                &git2::Oid::ZERO_SHA1.to_string(),
            ],
        )?;
        if !output.status.success() {
            return Err(GitError::WorktreeCommandFailed(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        intent
            .acknowledge_created_branch(&self.repo_path, branch, oid)
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        intent
            .prepare_tracking(&self.repo_path, branch)
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        let remote = selected
            .0
            .strip_suffix(&suffix)
            .ok_or_else(|| GitError::BranchNotFound(branch.into()))?;
        let merge = format!("refs/heads/{branch}");
        for (suffix, value) in [("remote", remote), ("merge", merge.as_str())] {
            let key = format!("branch.{branch}.{suffix}");
            let output = CreateExecutor::Owned(intent).run(
                &self.repo_path,
                ["config", "--local", "--replace-all", &key, value],
            )?;
            if !output.status.success() {
                return Err(GitError::WorktreeCommandFailed(
                    String::from_utf8_lossy(&output.stderr).into_owned(),
                ));
            }
        }
        intent
            .acknowledge_tracking(
                &self.repo_path,
                branch,
                remote,
                &format!("refs/heads/{branch}"),
            )
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
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

    fn original_common_hook_path(repository: &git2::Repository) -> std::path::PathBuf {
        // The issuer's linked layout has a literal ../.. commondir. Nonlinked
        // sources already expose their original common Git directory path.
        if repository.is_worktree() {
            repository
                .path()
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_else(|| repository.path().to_path_buf())
                .join("hooks")
        } else {
            repository.path().join("hooks")
        }
    }

    fn run_owned_post_checkout(
        &self,
        intent: &crate::session::builder::CreationIntent,
        path: &Path,
        oid: git2::Oid,
        common: &crate::session::builder::AnchoredDir,
    ) -> Result<Option<std::process::Output>> {
        let executor = CreateExecutor::Owned(intent);
        let version = executor.run(path, ["--version"])?;
        if !version.status.success() {
            return Err(super::command_failed(&version));
        }
        let text = String::from_utf8_lossy(&version.stdout);
        let mut parts = text.split_whitespace().nth(2).unwrap_or("").split('.');
        let major = parts.next().and_then(|part| part.parse::<u32>().ok());
        let minor = parts.next().and_then(|part| part.parse::<u32>().ok());
        let modern = match (major, minor) {
            (Some(major), Some(minor)) => major > 2 || (major == 2 && minor >= 36),
            _ => {
                return Err(GitError::WorktreeCommandFailed(
                    "Git did not acknowledge an identifiable hook interface version".into(),
                ))
            }
        };
        let mut command = if modern {
            let mut command = intent
                .owned_git_command(path)
                .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
            command.args(["hook", "run", "--ignore-missing", "post-checkout", "--"]);
            command
        } else {
            let repository = open_repo_at(path)?;
            let configured = match repository.config()?.get_path("core.hooksPath") {
                Ok(path) => Some(path),
                Err(error) if error.code() == git2::ErrorCode::NotFound => None,
                Err(error) => return Err(error.into()),
            };
            let hooks = configured
                .map(|hooks| {
                    if hooks.is_absolute() {
                        hooks
                    } else {
                        path.join(hooks)
                    }
                })
                .unwrap_or_else(|| Self::original_common_hook_path(&repository));
            let hook = hooks.join("post-checkout");
            match nix::unistd::access(&hook, nix::unistd::AccessFlags::X_OK) {
                Ok(()) => {}
                Err(nix::errno::Errno::ENOENT | nix::errno::Errno::EACCES) => return Ok(None),
                Err(error) => {
                    return Err(GitError::WorktreeCommandFailed(format!(
                        "checking actual post-checkout executable: {error}"
                    )))
                }
            }
            intent
                .owned_hook_command(&hook, path)
                .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?
        };
        command
            .anchored_env_path("GIT_COMMON_DIR", common)
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        command
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("LC_ALL", "C")
            .args([
                "0000000000000000000000000000000000000000",
                &oid.to_string(),
                "1",
            ]);
        intent
            .run_owned_output(&mut command, None)
            .map(Some)
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))
    }

    fn add_owned_worktree(
        &self,
        branch: &str,
        path: &Path,
        warnings: &mut Vec<String>,
        intent: &crate::session::builder::CreationIntent,
    ) -> Result<()> {
        let layout = intent
            .begin_worktree_effect(&self.repo_path, branch, path)
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        let mut checkout = intent
            .owned_command("git")
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        checkout
            .anchored_current_dir(&layout.root)
            .and_then(|command| command.anchored_env_path("GIT_DIR", &layout.admin))
            .and_then(|command| command.anchored_env_path("GIT_COMMON_DIR", &layout.common))
            .and_then(|command| command.anchored_env_path("GIT_WORK_TREE", &layout.root))
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        // read-tree is worktree-local plumbing: it does not move an existing
        // branch or create a second opaque admin allocator. The native checkout
        // uses the frozen OID and respects sparse/config/filter semantics.
        checkout
            .args(["read-tree", "--reset", "-u", &layout.oid.to_string()])
            .env("LC_ALL", "C");
        intent
            .begin_checkout_command(&self.repo_path, branch, path)
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        let output = intent.run_owned_output(&mut checkout, None);
        intent
            .acknowledge_worktree(
                &self.repo_path,
                branch,
                path,
                output.as_ref().is_ok_and(|output| output.status.success()),
            )
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        let output =
            output.map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        if !output.status.success() {
            return Err(super::command_failed(&output));
        }

        // The real Git hook dispatcher preserves executable/core.hooksPath
        // lookup and subprocess semantics. Upstream worktree.c removes these
        // two variables before post-checkout and passes zero, new OID, flag 1.
        if let Some(output) =
            self.run_owned_post_checkout(intent, path, layout.oid, &layout.common)?
        {
            if !output.status.success() {
                let detail = sanitize_remote_credentials(&format!(
                    "{}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                ));
                warnings.push(format!(
                    "post-checkout hook failed for {} ({}, worktree created, hook output below):\n{}",
                    path.display(),
                    output.status,
                    detail.trim()
                ));
            }
        }
        // Hook failure is a distinct actual effect outcome; it cannot erase
        // successful index/layout ACK, nor certify later dirty body changes.
        intent
            .acknowledge_worktree(&self.repo_path, branch, path, true)
            .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        Ok(())
    }

    /// `git worktree add`. A post-checkout hook can fail after the checkout
    /// exists; that worktree is usable, so the failure becomes a warning.
    fn add_worktree(
        &self,
        branch: &str,
        path: &Path,
        warnings: &mut Vec<String>,
        executor: CreateExecutor<'_>,
    ) -> Result<bool> {
        let t = Instant::now();
        let output = executor.run(
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
            } else {
                let output =
                    executor.run(&self.repo_path, ["worktree", "list", "--porcelain", "-z"])?;
                let branch_field = format!("branch refs/heads/{branch}");
                if output.status.success()
                    && output
                        .stdout
                        .split(|b| *b == 0)
                        .any(|field| field == branch_field.as_bytes())
                {
                    return Err(GitError::BranchAlreadyCheckedOut(branch.to_string()));
                }
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
        Ok(output.status.success())
    }

    fn initialize_submodules(
        &self,
        worktree_path: &Path,
        executor: CreateExecutor<'_>,
    ) -> Result<String> {
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

        if let CreateExecutor::Owned(intent) = executor {
            intent
                .retain_submodule_domain(worktree_path)
                .map_err(|error| GitError::WorktreeCommandFailed(format!("{error:#}")))?;
        }
        let output = executor.run(worktree_path, &args)?;
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
            git_wt.fetch_branch("origin", "main", CreateExecutor::Unmanaged),
            FetchOutcome::Failed(_)
        ));

        let (_dirs, local, _, _) = fork_upstream_layout("main");
        let git_wt = GitWorktree::new(local).unwrap();
        assert_eq!(
            git_wt.fetch_branch("origin", "main", CreateExecutor::Unmanaged),
            FetchOutcome::Ok
        );
        assert!(matches!(
            git_wt.fetch_branch("origin", "nonexistent-branch", CreateExecutor::Unmanaged),
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
