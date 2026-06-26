//! `GitWorktree` — worktree lifecycle, branch detection, and template-based
//! path computation. Split out from `mod.rs` as part of the code-quality
//! consolidation pass; the public API is unchanged.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use super::error::{GitError, Result};
use super::open_repo_at;
use super::template::{resolve_template, TemplateVars};

/// Strip embedded credentials from URL-style substrings before stderr
/// gets logged or surfaced to the user. Git fetch errors typically echo
/// the remote URL, so a misconfigured `https://user:token@host/repo`
/// would otherwise leak the token into `debug.log` and into the wizard
/// toast.
///
/// Matches `<scheme>://<userinfo>@<host>` patterns and replaces the
/// userinfo (everything between `://` and `@`) with `<redacted>`.
/// Schemes other than the common `http(s)`/`ssh`/`git+ssh` still match
/// because git accepts arbitrary URL schemes.
fn sanitize_remote_credentials(s: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"([a-zA-Z][a-zA-Z0-9+\-.]*://)[^/\s@]+@").expect("static regex always compiles")
    });
    re.replace_all(s, "${1}<redacted>@").into_owned()
}

/// Classify a failed `git worktree add` into a typed error.
///
/// The branch-already-checked-out case (git prints `'<branch>' is already
/// used by worktree at '<path>'`, or `already checked out at '<path>'` on
/// older git) becomes a clean `BranchAlreadyCheckedOut` that names the
/// branch and drops the raw stderr (including the other worktree's path).
/// Everything else stays `WorktreeCommandFailed`, with any credentialed
/// remote URL redacted before it can be surfaced.
fn classify_worktree_add_failure(combined: &str, branch: &str) -> GitError {
    let lower = combined.to_ascii_lowercase();
    if lower.contains("already used by worktree") || lower.contains("already checked out at") {
        GitError::BranchAlreadyCheckedOut(branch.to_string())
    } else {
        GitError::WorktreeCommandFailed(sanitize_remote_credentials(combined))
    }
}

/// Remote name used as a fallback when no candidate remote can be picked
/// from the local refs. Real selection happens in
/// `detect_default_branch_info`, which walks every configured remote and
/// scores its tracking refs by ancestry to HEAD and commit recency. This
/// constant only matters for the legacy single-remote / "no refs in repo
/// yet" path.
const FETCH_REMOTE: &str = "origin";

/// Reason recorded on every aoe-created worktree's `git worktree lock`. The
/// lock makes `git worktree prune` skip the admin entry, so a prune run from a
/// context that cannot see this checkout (a sibling sandbox, or the host when
/// the worktree lives inside a container mount) cannot reap its
/// `<main>/.git/worktrees/<name>` entry and break the git linkage (#2414 root
/// cause). aoe unlocks again before every intentional remove/move.
const WORKTREE_LOCK_REASON: &str = "aoe-managed worktree (prevents cross-boundary prune)";

pub struct WorktreeEntry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub is_detached: bool,
}

/// Result of a `git fetch` attempt initiated by `fetch_branch`. `Ok`
/// indicates the fetch ran to completion (the remote tip may or may
/// not have advanced; we do not parse fetch output to distinguish).
/// The non-`Ok` variants carry a short description that callers
/// surface to the user as a warning so they know the new worktree may
/// be starting from a stale local ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchOutcome {
    Ok,
    Failed(String),
    Skipped(String),
    TimedOut,
}

/// Resolved default branch with the remote it came from, if any.
///
/// Returned by `GitWorktree::detect_default_branch_info`. Callers that
/// need to fetch / branch off / diff against the canonical default
/// should use this rather than just the branch name, because the same
/// short name (e.g. `main`) can refer to different commits depending on
/// which remote the canonical version lives at. See issue #1029.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefaultBranchInfo {
    /// Short branch name (e.g. `"main"`).
    pub name: String,
    /// Remote name (`"origin"`, `"upstream"`, ...) when the chosen ref
    /// is a remote tracking branch. `None` when the chosen ref is a
    /// purely local branch.
    pub remote: Option<String>,
}

impl DefaultBranchInfo {
    /// Resolvable ref name for git2's `find_branch` (remote variant)
    /// and `revparse_single`. Remote refs become `"<remote>/<name>"`;
    /// local refs are returned unchanged.
    pub fn qualified_ref(&self) -> String {
        match &self.remote {
            Some(remote) => format!("{remote}/{name}", name = self.name),
            None => self.name.clone(),
        }
    }
}

pub struct GitWorktree {
    pub repo_path: PathBuf,
    /// Whether `create_worktree` should run `git submodule update --init
    /// --recursive` when the new checkout contains a `.gitmodules` file.
    /// Defaults to true to preserve the behavior introduced in #942; callers
    /// that respect a user-facing setting (see `WorktreeConfig::init_submodules`)
    /// should call `with_init_submodules` to override it per session.
    init_submodules: bool,
}

impl GitWorktree {
    pub fn new(repo_path: PathBuf) -> Result<Self> {
        if !Self::is_git_repo(&repo_path) {
            return Err(GitError::NotAGitRepo);
        }
        Ok(Self {
            repo_path,
            init_submodules: true,
        })
    }

    /// Configure whether `create_worktree` recursively initializes submodules
    /// for the new checkout. Defaults to true.
    pub fn with_init_submodules(mut self, init_submodules: bool) -> Self {
        self.init_submodules = init_submodules;
        self
    }

    pub fn is_git_repo(path: &Path) -> bool {
        open_repo_at(path).is_ok()
            || Self::find_main_repo_from_linked_worktree_gitfile(path).is_some()
    }

    /// Returns true if the repository is a bare repo (including linked worktree bare repo setups).
    /// This is useful for choosing appropriate worktree path templates.
    pub fn is_bare_repo(path: &Path) -> bool {
        open_repo_at(path)
            .map(|repo| repo.is_bare())
            .unwrap_or(false)
    }

    pub fn find_main_repo(path: &Path) -> Result<PathBuf> {
        if let Ok(repo) = open_repo_at(path) {
            if let Some(main_repo) = Self::find_main_repo_from_worktree_gitdir(repo.path()) {
                return Ok(main_repo);
            }

            // For regular repos with a working directory, return it
            if let Some(workdir) = repo.workdir() {
                return Ok(workdir.to_path_buf());
            }

            let bare_repo_path = repo.path().to_path_buf();
            let parent_dir = bare_repo_path.parent().ok_or(GitError::NotAGitRepo)?;

            // For linked setups where the parent has a `.git` file (not a directory)
            // pointing to the bare repo (e.g. `/repo/.git` containing `gitdir: ./.bare`),
            // use the parent as the main repo path.  We check `is_file()` rather than
            // `exists()` to avoid being fooled by spurious `.git/` directories created by
            // external tools (e.g. opencode drops a state file in `.git/` wherever it runs,
            // including inside bare-repo parent directories).
            if parent_dir.join(".git").is_file() {
                return Ok(parent_dir.to_path_buf());
            }

            // For direct bare repos (e.g. `/repo/foo.git`), use the bare repo itself.
            return Ok(bare_repo_path);
        }

        // Fallback for linked worktree layouts that open_repo_at doesn't handle.
        Self::find_main_repo_from_linked_worktree_gitfile(path).ok_or(GitError::NotAGitRepo)
    }

    /// For linked worktrees, `.git` is a file containing `gitdir: <path>`.
    /// If that path points to `.../worktrees/<name>`, return the repository root.
    ///
    /// Only checks the given path directly (does not walk up parent directories).
    fn find_main_repo_from_linked_worktree_gitfile(path: &Path) -> Option<PathBuf> {
        let dir = if path.is_file() { path.parent()? } else { path };
        let git_entry = dir.join(".git");
        if git_entry.is_file() {
            let gitdir = Self::read_gitdir_from_file(&git_entry)?;
            return Self::find_main_repo_from_worktree_gitdir(&gitdir);
        }
        None
    }

    fn read_gitdir_from_file(git_file: &Path) -> Option<PathBuf> {
        let content = std::fs::read_to_string(git_file).ok()?;
        let gitdir = content
            .lines()
            .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))?;
        let gitdir_path = PathBuf::from(gitdir);
        let resolved = if gitdir_path.is_absolute() {
            gitdir_path
        } else {
            git_file.parent()?.join(gitdir_path)
        };
        resolved.canonicalize().ok()
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

    /// Fetch a specific branch from a remote. Returns a `FetchOutcome`
    /// describing whether the fetch ran successfully, failed, was
    /// skipped (e.g. spawn error), or timed out. Callers that build a
    /// warnings vector for the user (see `create_worktree`) should
    /// route non-`Ok` outcomes into it so the user knows the new
    /// worktree may be starting from a stale local ref.
    ///
    /// Stdin is piped to null to prevent SSH passphrase prompts from
    /// hanging. Times out after 10 seconds.
    pub fn fetch_branch(&self, remote: &str, branch: &str) -> FetchOutcome {
        let mut child = match std::process::Command::new("git")
            .args(["fetch", remote, branch])
            .current_dir(&self.repo_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                tracing::warn!(
                    target: "git.command",
                    remote = %remote,
                    branch = %branch,
                    error = %e,
                    "git fetch spawn failed"
                );
                return FetchOutcome::Skipped(format!("spawn failed: {e}"));
            }
        };

        let timeout = std::time::Duration::from_secs(10);
        let start = std::time::Instant::now();
        let poll_interval = std::time::Duration::from_millis(100);

        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let elapsed = start.elapsed();
                    if !status.success() {
                        let mut msg = String::new();
                        if let Some(mut stderr) = child.stderr.take() {
                            let _ = std::io::Read::read_to_string(&mut stderr, &mut msg);
                        }
                        let sanitized = sanitize_remote_credentials(msg.trim());
                        tracing::warn!(target: "git.worktree", "git fetch {remote}/{branch} failed: {sanitized}");
                        let detail = if sanitized.is_empty() {
                            format!("git fetch exited with {status}")
                        } else {
                            sanitized
                        };
                        return FetchOutcome::Failed(detail);
                    }
                    tracing::info!(target: "git.worktree", "git fetch {remote}/{branch} ok in {:?}", elapsed);
                    return FetchOutcome::Ok;
                }
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        tracing::warn!(target: "git.worktree",
                            "git fetch {remote}/{branch} timed out after {}s",
                            timeout.as_secs()
                        );
                        return FetchOutcome::TimedOut;
                    }
                    std::thread::sleep(poll_interval);
                }
                Err(e) => {
                    tracing::warn!(target: "git.worktree", "git fetch {remote}/{branch} error: {e}");
                    return FetchOutcome::Failed(format!("wait error: {e}"));
                }
            }
        }
    }

    /// Format a user-visible warning describing a non-`Ok`
    /// `FetchOutcome` and push it onto `warnings`. No-op for
    /// `FetchOutcome::Ok`. Used by `create_worktree` so fetch failures
    /// reach the wizard toast / CLI stderr / TUI dialog via the
    /// existing post-checkout warning surface.
    fn record_fetch_warning(
        &self,
        warnings: &mut Vec<String>,
        outcome: &FetchOutcome,
        remote: &str,
        branch: &str,
    ) {
        let detail = match outcome {
            FetchOutcome::Ok => return,
            FetchOutcome::Failed(msg) => msg.clone(),
            FetchOutcome::Skipped(msg) => msg.clone(),
            FetchOutcome::TimedOut => "timed out after 10s".to_string(),
        };
        warnings.push(format!(
            "git fetch {remote} {branch} failed for {repo}: {detail}",
            repo = self.repo_path.display()
        ));
    }

    /// Pick the remote with the freshest copy of `branch_name`. Walks
    /// every configured remote, looks up
    /// `refs/remotes/<remote>/<branch_name>`, and returns the remote
    /// whose ref has the most recent commit time. Ties break in favor
    /// of `origin`, then alphabetically by remote name.
    ///
    /// Returns `None` when no configured remote tracks `branch_name`.
    ///
    /// Used by `create_worktree` for the explicit-base-branch arm so a
    /// fork plus `upstream` layout (issue #1029) fetches the user's
    /// requested base from the canonical remote rather than a stale
    /// `origin`. The autodetect arm continues to use
    /// `detect_default_branch_info`, which scores `main`/`master`
    /// across remotes with HEAD-ancestry filtering.
    pub fn pick_remote_for_branch(&self, branch_name: &str) -> Option<String> {
        let repo = open_repo_at(&self.repo_path).ok()?;
        let remotes = repo.remotes().ok()?;
        let mut best: Option<(String, i64)> = None;
        for remote in remotes.iter().filter_map(|r| r.ok().flatten()) {
            let full = format!("{remote}/{branch_name}");
            let Ok(b) = repo.find_branch(&full, git2::BranchType::Remote) else {
                continue;
            };
            let Ok(commit) = b.get().peel_to_commit() else {
                continue;
            };
            let t = commit.time().seconds();
            let take = match &best {
                None => true,
                Some((cur_name, cur_t)) => {
                    if t != *cur_t {
                        t > *cur_t
                    } else if cur_name == FETCH_REMOTE {
                        false
                    } else if remote == FETCH_REMOTE {
                        true
                    } else {
                        remote < cur_name.as_str()
                    }
                }
            };
            if take {
                best = Some((remote.to_string(), t));
            }
        }
        best.map(|(name, _)| name)
    }

    /// Detect the default branch name by short name only. Wraps
    /// `detect_default_branch_info`; callers that need the remote (for
    /// fetching / building a tracking-ref string) should use the info
    /// variant directly.
    pub fn detect_default_branch(&self) -> Result<String> {
        self.detect_default_branch_info().map(|i| i.name)
    }

    /// Detect the default branch and the remote that owns the freshest
    /// copy of it, scored against HEAD.
    ///
    /// Selection considers every configured remote (not just `origin`),
    /// so a fork + `upstream` layout picks `upstream/main` over a stale
    /// local `main` or fork `origin/main` (issue #1029).
    ///
    /// Candidate pool: every remote's `refs/remotes/<r>/HEAD` symbolic
    /// target, plus `refs/remotes/<r>/main` / `refs/remotes/<r>/master`
    /// for every remote, plus local `main` / `master`. Among candidates
    /// that HEAD descends from (`merge_base(HEAD, candidate) == candidate`),
    /// the one with the most recent commit time wins; ties break in
    /// favor of a remote's stated default (`refs/remotes/*/HEAD`),
    /// then remote over local, then `origin` over other remotes.
    ///
    /// If HEAD doesn't descend from any candidate (unrelated history),
    /// the same scoring runs over all candidates without the ancestry
    /// filter. With no candidates at all (no main/master, no remote
    /// HEAD), falls back to the first local branch like the previous
    /// implementation.
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

        let head_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());

        let pool: Vec<&Candidate> = if let Some(head_commit) = head_commit.as_ref() {
            let ancestors: Vec<&Candidate> = candidates
                .iter()
                .filter(|c| {
                    repo.merge_base(head_commit.id(), c.oid)
                        .map(|mb| mb == c.oid)
                        .unwrap_or(false)
                })
                .collect();
            if ancestors.is_empty() {
                candidates.iter().collect()
            } else {
                ancestors
            }
        } else {
            candidates.iter().collect()
        };

        let pick = pool
            .into_iter()
            .max_by(|a, b| {
                a.commit_time
                    .cmp(&b.commit_time)
                    .then_with(|| a.is_stated_default.cmp(&b.is_stated_default))
                    .then_with(|| a.remote.is_some().cmp(&b.remote.is_some()))
                    .then_with(|| {
                        let a_origin = a.remote.as_deref() == Some(FETCH_REMOTE);
                        let b_origin = b.remote.as_deref() == Some(FETCH_REMOTE);
                        a_origin.cmp(&b_origin)
                    })
                    .then_with(|| b.name.cmp(&a.name))
            })
            .expect("pool is non-empty when candidates is non-empty");

        Ok(DefaultBranchInfo {
            name: pick.name.clone(),
            remote: pick.remote.clone(),
        })
    }

    /// Create a new worktree at `path` checking out `branch`.
    ///
    /// When `create_branch` is true, the new branch is based on `base_branch`
    /// if provided, otherwise on the repository's detected default branch
    /// (`main`/`master`). The base is resolved against the remote first
    /// (`origin/<base>`) then against a local branch with that name, so
    /// passing a teammate's branch works without manually fetching it.
    ///
    /// When `create_branch` is false, `base_branch` is ignored (the
    /// worktree checks out the already-existing `branch`).
    ///
    /// Returns a list of non-fatal warnings (e.g. post-checkout hook failures
    /// where the worktree directory was successfully created anyway). Callers
    /// should surface these to the user (CLI/TUI/web) so they know about the
    /// hook failure without aborting session creation.
    pub fn create_worktree(
        &self,
        branch: &str,
        path: &Path,
        create_branch: bool,
        base_branch: Option<&str>,
    ) -> Result<Vec<String>> {
        let total_start = std::time::Instant::now();
        let mut warnings: Vec<String> = Vec::new();
        tracing::info!(target: "git.worktree",
            "worktree create: start branch={} path={}",
            branch,
            path.display()
        );

        if path.exists() {
            return Err(GitError::WorktreeAlreadyExists(path.to_path_buf()));
        }

        // Prune stale worktree entries so git doesn't reject a path that was
        // previously used by a now-deleted worktree directory. A stale entry at
        // exactly this path may be one aoe locked (the directory was removed
        // out-of-band, bypassing the unlock-on-remove path); `prune` skips
        // locked entries, so unlock this path first (best-effort, resolves from
        // the admin side even with the checkout gone) so its stale entry is
        // reaped. Sibling locks stay intact: only the colliding path is touched.
        self.unlock_worktree(path);
        let t = std::time::Instant::now();
        self.prune_worktrees()?;
        tracing::info!(target: "git.worktree", "worktree create: prune done in {:?}", t.elapsed());

        // Fetch from remote so the worktree starts from the latest state.
        // For new branches, fetch the base branch (default branch unless the
        // caller specified one) to use as the base. For existing branches,
        // fetch the branch itself. Fetch failures are surfaced as warnings
        // via `record_fetch_warning` so the user knows the new worktree may
        // be starting from a stale local ref instead of failing silently.
        let t = std::time::Instant::now();
        // (name, remote, explicit) — `explicit` flips the lookup chain
        // below from "fall through to HEAD" to "return BranchNotFound"
        // when neither a remote-tracking ref nor a local branch with the
        // user-typed name exists. A typo in `--base-branch` should
        // surface as an error, not as a session quietly anchored to a
        // bystander commit.
        let resolved_base: Option<(String, Option<String>, bool)> = if create_branch {
            match base_branch {
                Some(b) if !b.trim().is_empty() => {
                    let base = b.trim().to_string();
                    // Apply canonical-remote scoring to explicit base
                    // branches too. On a fork plus `upstream` layout,
                    // `upstream/<base>` is usually the freshest copy;
                    // hardcoding `origin/<base>` lands new branches on
                    // the stale fork tip (issue #1511 / #1029).
                    let remote = self.pick_remote_for_branch(&base);
                    let fetch_remote = remote.as_deref().unwrap_or(FETCH_REMOTE);
                    let outcome = self.fetch_branch(fetch_remote, &base);
                    self.record_fetch_warning(&mut warnings, &outcome, fetch_remote, &base);
                    Some((base, remote, true))
                }
                _ => {
                    let info = self
                        .detect_default_branch_info()
                        .unwrap_or(DefaultBranchInfo {
                            name: "main".to_string(),
                            remote: None,
                        });
                    let fetch_remote = info.remote.as_deref().unwrap_or(FETCH_REMOTE);
                    let outcome = self.fetch_branch(fetch_remote, &info.name);
                    self.record_fetch_warning(&mut warnings, &outcome, fetch_remote, &info.name);
                    Some((info.name, info.remote, false))
                }
            }
        } else {
            let outcome = self.fetch_branch(FETCH_REMOTE, branch);
            self.record_fetch_warning(&mut warnings, &outcome, FETCH_REMOTE, branch);
            None
        };
        tracing::info!(target: "git.worktree", "worktree create: fetch step done in {:?}", t.elapsed());

        let t = std::time::Instant::now();
        let repo = open_repo_at(&self.repo_path)?;

        if let Some((base, base_remote, explicit)) = resolved_base {
            // Branch from the picked remote's tip when the auto-detected
            // canonical remote isn't `origin` (issue #1029: fork+upstream
            // layouts). When the caller passed an explicit `--base-branch`,
            // try `origin/<base>` first to preserve historical behavior.
            // Falls back to a local branch with the same name (lets users
            // base off a teammate's local-only branch), then to HEAD,
            // then to any local branch (bare repo with broken HEAD), but
            // only when the base came from autodetection. An explicit
            // `--base-branch` that resolves to none of the above is a
            // user-visible error: surfacing it as BranchNotFound keeps
            // typos from silently producing a session anchored to a
            // bystander commit.
            let primary_remote = base_remote.as_deref().unwrap_or(FETCH_REMOTE);
            let remote_ref = format!("{primary_remote}/{base}");
            let direct_match = repo
                .find_branch(&remote_ref, git2::BranchType::Remote)
                .ok()
                .and_then(|b| b.get().target())
                .or_else(|| {
                    // Secondary fallback to `origin/<base>` when the
                    // primary remote was something other than `origin`.
                    if primary_remote == FETCH_REMOTE {
                        None
                    } else {
                        let origin_ref = format!("{FETCH_REMOTE}/{base}");
                        repo.find_branch(&origin_ref, git2::BranchType::Remote)
                            .ok()
                            .and_then(|b| b.get().target())
                    }
                })
                .or_else(|| {
                    repo.find_branch(&base, git2::BranchType::Local)
                        .ok()
                        .and_then(|b| b.get().target())
                });

            let commit_oid = if let Some(oid) = direct_match {
                oid
            } else if explicit {
                return Err(GitError::BranchNotFound(base));
            } else {
                repo.head()
                    .ok()
                    .and_then(|h| h.peel_to_commit().ok())
                    .map(|c| c.id())
                    .or_else(|| {
                        repo.branches(Some(git2::BranchType::Local)).ok().and_then(
                            |mut branches| {
                                branches.find_map(|b| b.ok().and_then(|(b, _)| b.get().target()))
                            },
                        )
                    })
                    .ok_or_else(|| {
                        GitError::WorktreeCommandFailed(
                            "No commits found to branch from".to_string(),
                        )
                    })?
            };

            let commit = repo.find_commit(commit_oid)?;
            repo.branch(branch, &commit, false)?;
        } else {
            let has_local = repo.find_branch(branch, git2::BranchType::Local).is_ok();
            if !has_local {
                let has_remote = repo
                    .branches(Some(git2::BranchType::Remote))
                    .ok()
                    .map(|branches| {
                        branches.filter_map(|b| b.ok()).any(|(b, _)| {
                            b.name()
                                .ok()
                                .flatten()
                                .map(|name| {
                                    name.ends_with(&format!("/{}", branch)) || name == branch
                                })
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false);
                if !has_remote {
                    return Err(GitError::BranchNotFound(branch.to_string()));
                }
            }
        }

        tracing::info!(target: "git.worktree", "worktree create: branch resolve done in {:?}", t.elapsed());

        let path_str = path
            .to_str()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid path"))?;

        let t = std::time::Instant::now();
        let output =
            super::command::run_git(&self.repo_path, ["worktree", "add", path_str, branch])?;
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
            // Redact any credentialed remote URL before this text can reach
            // the debug log or the client (via the warnings channel below or
            // the error message). Idempotent, so the second pass inside
            // `classify_worktree_add_failure` is a no-op.
            let combined = sanitize_remote_credentials(&combined);

            // post-checkout hooks (e.g. pre-commit's hook-type=post-checkout
            // running uv-sync, npm install, etc.) can fail after git has
            // already created the worktree directory. When that happens, the
            // worktree IS usable; we record a warning instead of aborting
            // session creation.
            let worktree_created = path.exists() && path.join(".git").exists();
            if worktree_created {
                let warning = format!(
                    "post-checkout hook failed for {} (worktree created, hook output below):\n{}",
                    path.display(),
                    combined.trim()
                );
                tracing::warn!(target: "git.worktree", "worktree create: {}", warning);
                warnings.push(warning);
            } else {
                return Err(classify_worktree_add_failure(&combined, branch));
            }
        }

        // `walk_worktree_stats` walks the entire checked-out tree, which is
        // unwanted overhead on the hot path when no one is listening. Only
        // pay the cost when the matching tracing target is actually emitting
        // at INFO (i.e. AOE_LOG_LEVEL=info|debug|trace or the legacy
        // AGENT_OF_EMPIRES_DEBUG=1).
        if tracing::enabled!(tracing::Level::INFO) {
            let WorktreeWalkStats {
                file_count,
                total_bytes,
                capped,
            } = walk_worktree_stats(path);
            tracing::info!(target: "git.worktree",
                "worktree create: git worktree add done in {:?} ({} files, {} bytes checked out{})",
                add_elapsed,
                file_count,
                total_bytes,
                if capped { ", walk capped" } else { "" }
            );
        }

        // Convert the .git file from absolute to relative path.
        // Git always writes absolute paths, but relative paths work better when
        // the repo is mounted at different locations (e.g., in Docker containers).
        let t = std::time::Instant::now();
        Self::convert_git_file_to_relative(path)?;
        tracing::info!(target: "git.worktree",
            "worktree create: convert .git file done in {:?}",
            t.elapsed()
        );

        let t = std::time::Instant::now();
        let submodule_status = if self.init_submodules {
            Self::initialize_submodules(path)?
        } else {
            "disabled-by-config".to_string()
        };
        tracing::info!(target: "git.worktree",
            "worktree create: submodules ({}) done in {:?}",
            submodule_status,
            t.elapsed()
        );

        // Lock the freshly-created worktree so a `git worktree prune` issued
        // from a sandbox/host context that cannot see this checkout can no
        // longer reap its admin entry and break the git linkage (#2414). The
        // intentional remove/move paths unlock first. Best-effort: a lock
        // failure is surfaced as a warning, never an abort, so session
        // creation still succeeds without prune protection.
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

    /// Prune stale worktree entries whose directories no longer exist on disk.
    pub fn prune_worktrees(&self) -> Result<()> {
        let output = super::command::run_git(&self.repo_path, ["worktree", "prune"])?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(GitError::WorktreeCommandFailed(stderr));
        }

        Ok(())
    }

    /// Lock `path`'s linked-worktree admin entry so `git worktree prune` skips
    /// it (see [`WORKTREE_LOCK_REASON`]). Called once at the end of
    /// `create_worktree`. Returns an error the caller can surface as a
    /// non-fatal warning; a missing lock only forfeits prune protection, it
    /// does not make the worktree unusable.
    pub fn lock_worktree(&self, path: &Path) -> Result<()> {
        let path_str = path
            .to_str()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid path"))?;
        let output = super::command::run_git(
            &self.repo_path,
            [
                "worktree",
                "lock",
                "--reason",
                WORKTREE_LOCK_REASON,
                path_str,
            ],
        )?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(GitError::WorktreeCommandFailed(stderr));
        }
        Ok(())
    }

    /// Unlock `path`'s admin entry, idempotently. MUST run before any
    /// `git worktree remove`/`move` (git refuses both on a locked worktree even
    /// with a single `--force`) and before any cleanup path that reaps the
    /// entry via `prune` (prune skips locked entries). Best-effort by design:
    /// `git worktree unlock` resolves the entry from the admin side, so it
    /// still clears the lock when the checkout directory is already gone, and a
    /// non-zero exit (already unlocked, or no such worktree) is a harmless
    /// no-op rather than an error.
    pub fn unlock_worktree(&self, path: &Path) {
        let Some(path_str) = path.to_str() else {
            return;
        };
        let _ = super::command::run_git(&self.repo_path, ["worktree", "unlock", path_str]);
    }

    /// Convert a worktree's .git file from absolute to relative path.
    ///
    /// Git worktrees contain a `.git` file (not directory) with content like:
    /// `gitdir: /absolute/path/to/.bare/worktrees/name`
    ///
    /// This converts it to a relative path like:
    /// `gitdir: ../.bare/worktrees/name`
    ///
    /// Relative paths work when the repo is mounted at different locations.
    fn convert_git_file_to_relative(worktree_path: &Path) -> Result<()> {
        let git_file = worktree_path.join(".git");
        if !git_file.exists() || !git_file.is_file() {
            return Ok(()); // Not a worktree or already a directory
        }

        let content = std::fs::read_to_string(&git_file)?;
        let Some(gitdir_line) = content.lines().find(|l| l.starts_with("gitdir:")) else {
            return Ok(()); // No gitdir line found
        };

        let absolute_path = gitdir_line.trim_start_matches("gitdir:").trim();
        let absolute_path = Path::new(absolute_path);

        if absolute_path.is_relative() {
            return Ok(()); // Already relative
        }

        // Calculate relative path from worktree to gitdir
        let worktree_canonical = worktree_path.canonicalize()?;
        let gitdir_canonical = absolute_path.canonicalize()?;

        if let Some(relative) = Self::diff_paths(&gitdir_canonical, &worktree_canonical) {
            let new_content = format!("gitdir: {}\n", relative.display());
            std::fs::write(&git_file, new_content)?;
        }

        Ok(())
    }

    fn initialize_submodules(worktree_path: &Path) -> Result<String> {
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

        let output = super::command::run_git(
            worktree_path,
            ["submodule", "update", "--init", "--recursive"],
        )?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            let message = if stderr.is_empty() { stdout } else { stderr };
            if Self::is_file_transport_blocked(&message) {
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
            return Err(GitError::WorktreeCommandFailed(if message.is_empty() {
                "git submodule update --init --recursive failed".to_string()
            } else {
                message
            }));
        }

        Ok(format!("initialized count={}", submodule_count))
    }

    fn is_file_transport_blocked(message: &str) -> bool {
        let message = message.to_ascii_lowercase();
        message.contains("transport 'file' not allowed")
            || message.contains("transport \"file\" not allowed")
            || message.contains("disallowed by protocol.file.allow")
    }

    /// Calculate a relative path from `base` to `target`.
    /// Returns None if the paths have no common ancestor.
    pub(crate) fn diff_paths(target: &Path, base: &Path) -> Option<PathBuf> {
        let mut target_components = target.components().peekable();
        let mut base_components = base.components().peekable();

        // Skip common prefix
        while let (Some(t), Some(b)) = (target_components.peek(), base_components.peek()) {
            if t != b {
                break;
            }
            target_components.next();
            base_components.next();
        }

        // Count remaining base components (need ".." for each)
        let up_count = base_components.count();

        // Build relative path: "../" for each remaining base component + remaining target
        let mut result = PathBuf::new();
        for _ in 0..up_count {
            result.push("..");
        }
        for component in target_components {
            result.push(component);
        }

        Some(result)
    }

    pub fn list_worktrees(&self) -> Result<Vec<WorktreeEntry>> {
        let repo = open_repo_at(&self.repo_path)?;
        let worktrees = repo.worktrees()?;

        let mut entries = vec![];

        // For non-bare repos, add the main worktree entry
        // Bare repos don't have a main worktree, only linked worktrees
        if !repo.is_bare() {
            entries.push(WorktreeEntry {
                path: self.repo_path.clone(),
                branch: Self::get_current_branch(&self.repo_path).ok(),
                is_detached: repo.head_detached()?,
            });
        }

        for name_str in worktrees.iter().filter_map(|r| r.ok().flatten()) {
            if let Ok(wt) = repo.find_worktree(name_str) {
                if let Ok(path) = wt.path().canonicalize() {
                    entries.push(WorktreeEntry {
                        path: path.clone(),
                        branch: Self::get_current_branch(&path).ok(),
                        is_detached: false,
                    });
                }
            }
        }

        Ok(entries)
    }

    pub fn remove_worktree(&self, path: &Path, force: bool) -> Result<()> {
        if !path.exists() {
            return Err(GitError::WorktreeNotFound(path.to_path_buf()));
        }

        // aoe locks every worktree it creates; `git worktree remove` refuses a
        // locked tree even with a single `--force` (it wants `-f -f`). Unlock
        // first so the existing single-`--force` semantics (dirty-tree
        // override) are preserved unchanged.
        self.unlock_worktree(path);

        let path_str = path
            .to_str()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid path"))?;

        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.push(path_str);

        let output = super::command::run_git(&self.repo_path, &args)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(GitError::WorktreeCommandFailed(stderr));
        }

        Ok(())
    }

    /// Delete a local git branch.
    ///
    /// Idempotent: if the branch does not exist, returns Ok(()) — the caller
    /// asked for the branch to be gone, and it is. This matters for session
    /// deletion where the branch may never have been created (e.g. session
    /// metadata stamped with a stale value, or worktree creation aborted
    /// mid-flight). Returns an error only when git rejects the operation
    /// for a reason other than "not found".
    pub fn delete_branch(&self, branch: &str) -> Result<()> {
        tracing::debug!(target: "git.worktree",
            branch,
            repo = %self.repo_path.display(),
            "delete_branch: invoking `git branch -d`"
        );
        let output = super::command::run_git(&self.repo_path, ["branch", "-d", branch])?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            tracing::debug!(target: "git.worktree",
                branch,
                exit = ?output.status.code(),
                stderr = %stderr,
                stdout = %stdout,
                "delete_branch: `git branch -d` failed"
            );
            // Branch already gone: treat as success. Git emits
            // "error: branch '<name>' not found" in this case.
            if stderr.contains("not found") {
                tracing::debug!(target: "git.worktree",
                    branch,
                    "delete_branch: branch already absent, treating as success"
                );
                return Ok(());
            }
            // If the branch has unmerged changes, try force delete
            if stderr.contains("not fully merged") {
                let force_output =
                    super::command::run_git(&self.repo_path, ["branch", "-D", branch])?;

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
            } else {
                return Err(GitError::WorktreeCommandFailed(format!(
                    "git branch -d {}: {}",
                    branch,
                    stderr.trim()
                )));
            }
        }

        Ok(())
    }

    /// Whether a local branch `refs/heads/<branch>` exists in this repo.
    pub fn branch_exists(&self, branch: &str) -> bool {
        let refname = format!("refs/heads/{branch}");
        match super::command::run_git(
            &self.repo_path,
            ["show-ref", "--verify", "--quiet", &refname],
        ) {
            Ok(output) => output.status.success(),
            Err(_) => false,
        }
    }

    /// The short upstream tracking ref for a local branch (e.g. `origin/hi`),
    /// or `None` when the branch has no upstream configured.
    ///
    /// Used to warn before a branch rename: `git branch -m` only touches the
    /// local ref, so a branch that tracks a remote leaves that remote branch
    /// behind (and any open PR pinned to it) until the new name is pushed and
    /// the old one deleted. `for-each-ref` is quiet, it prints an empty line
    /// rather than erroring when there is no upstream.
    pub fn branch_upstream(&self, branch: &str) -> Option<String> {
        let refname = format!("refs/heads/{branch}");
        let output = super::command::run_git(
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

    /// Move a managed worktree directory via `git worktree move`, which
    /// relocates the checkout and updates the linked-worktree gitdir
    /// metadata. A plain filesystem rename would leave git's bookkeeping
    /// pointing at the old path, so always go through git here.
    pub fn move_worktree(&self, from: &Path, to: &Path) -> Result<()> {
        if !from.exists() {
            return Err(GitError::WorktreeNotFound(from.to_path_buf()));
        }
        if to.exists() {
            return Err(GitError::WorktreeAlreadyExists(to.to_path_buf()));
        }
        let from_str = from
            .to_str()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid path"))?;
        let to_str = to
            .to_str()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "Invalid path"))?;

        tracing::info!(target: "git.worktree",
            from = %from.display(),
            to = %to.display(),
            "move_worktree: invoking `git worktree move`"
        );
        // `git worktree move` also refuses a locked tree; unlock, move, then
        // re-lock at the destination so the relocated checkout keeps its prune
        // protection.
        self.unlock_worktree(from);
        let output =
            super::command::run_git(&self.repo_path, ["worktree", "move", from_str, to_str])?;
        if !output.status.success() {
            // Restore the lock on the unmoved source so a failed move does not
            // silently leave it unprotected.
            let _ = self.lock_worktree(from);
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(GitError::WorktreeCommandFailed(stderr));
        }
        if let Err(e) = self.lock_worktree(to) {
            tracing::warn!(target: "git.worktree",
                to = %to.display(),
                error = %e,
                "move_worktree: could not re-lock worktree at new path"
            );
        }
        Ok(())
    }

    /// Rename a local branch via `git branch -m`. Works while the branch is
    /// checked out in a worktree (git updates that worktree's HEAD). Errors
    /// if `old` does not exist or `new` already exists.
    pub fn rename_branch(&self, old: &str, new: &str) -> Result<()> {
        tracing::info!(target: "git.worktree",
            old, new,
            repo = %self.repo_path.display(),
            "rename_branch: invoking `git branch -m`"
        );
        let output = super::command::run_git(&self.repo_path, ["branch", "-m", old, new])?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            return Err(GitError::WorktreeCommandFailed(stderr));
        }
        Ok(())
    }

    pub fn compute_path(&self, branch: &str, template: &str, session_id: &str) -> Result<PathBuf> {
        let repo_name = self
            .repo_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("repo")
            .to_string();

        let vars = TemplateVars {
            repo_name,
            branch: branch.to_string(),
            session_id: session_id.to_string(),
            base_path: self.repo_path.clone(),
        };

        resolve_template(template, &vars)
    }

    pub fn get_current_branch(path: &Path) -> Result<String> {
        let repo = open_repo_at(path)?;
        let head = repo.head()?;

        if let Ok(branch_name) = head.shorthand() {
            Ok(branch_name.to_string())
        } else {
            Err(GitError::NotAGitRepo)
        }
    }
}

/// One default-branch candidate considered by
/// `detect_default_branch_info`. Scored by `commit_time` first, with
/// `is_stated_default`/remote/origin used as tiebreakers.
struct Candidate {
    name: String,
    remote: Option<String>,
    /// True when this candidate came from a `refs/remotes/<r>/HEAD`
    /// symbolic target rather than the `main`/`master` convention.
    is_stated_default: bool,
    oid: git2::Oid,
    commit_time: i64,
}

fn collect_default_branch_candidates(repo: &git2::Repository) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen: HashSet<(String, Option<String>)> = HashSet::new();

    let remote_names: Vec<String> = repo
        .remotes()
        .ok()
        .as_ref()
        .map(|rs| {
            rs.iter()
                .filter_map(|r| r.ok().flatten())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();

    let mut stated_defaults: Vec<(String, String)> = Vec::new();
    for remote in &remote_names {
        let head_ref_name = format!("refs/remotes/{remote}/HEAD");
        let Ok(reference) = repo.find_reference(&head_ref_name) else {
            continue;
        };
        let Ok(Some(target)) = reference.symbolic_target() else {
            continue;
        };
        let prefix = format!("refs/remotes/{remote}/");
        if let Some(branch_name) = target.strip_prefix(&prefix) {
            if branch_name != "HEAD" && !branch_name.is_empty() {
                stated_defaults.push((remote.clone(), branch_name.to_string()));
            }
        }
    }

    let push = |out: &mut Vec<Candidate>,
                seen: &mut HashSet<(String, Option<String>)>,
                name: String,
                remote: Option<String>,
                is_stated_default: bool| {
        let key = (name.clone(), remote.clone());
        if seen.contains(&key) {
            return;
        }
        let commit = match &remote {
            Some(r) => {
                let full = format!("{r}/{name}");
                repo.find_branch(&full, git2::BranchType::Remote)
                    .ok()
                    .and_then(|b| b.get().peel_to_commit().ok())
            }
            None => repo
                .find_branch(&name, git2::BranchType::Local)
                .ok()
                .and_then(|b| b.get().peel_to_commit().ok()),
        };
        if let Some(commit) = commit {
            seen.insert(key);
            out.push(Candidate {
                name,
                remote,
                is_stated_default,
                oid: commit.id(),
                commit_time: commit.time().seconds(),
            });
        }
    };

    for (remote, name) in &stated_defaults {
        push(
            &mut out,
            &mut seen,
            name.clone(),
            Some(remote.clone()),
            true,
        );
    }

    for remote in &remote_names {
        for name in ["main", "master"] {
            push(
                &mut out,
                &mut seen,
                name.to_string(),
                Some(remote.clone()),
                false,
            );
        }
    }

    for name in ["main", "master"] {
        push(&mut out, &mut seen, name.to_string(), None, false);
    }

    out
}

struct WorktreeWalkStats {
    file_count: u64,
    total_bytes: u64,
    capped: bool,
}

/// Walk a freshly created worktree to report file count and byte size of the
/// checked-out tracked files. Used purely for instrumentation/logging.
///
/// - Skips the `.git` entry (worktree pointer file or shared dir).
/// - Does not follow symlinks.
/// - Caps recursion depth at 6 to bound the cost on pathological trees.
fn walk_worktree_stats(root: &Path) -> WorktreeWalkStats {
    const MAX_DEPTH: usize = 6;

    fn visit(dir: &Path, depth: usize, files: &mut u64, bytes: &mut u64, capped: &mut bool) {
        if depth > MAX_DEPTH {
            *capped = true;
            return;
        }
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if depth == 0 && name == ".git" {
                continue;
            }
            let metadata = match entry.metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                visit(&entry.path(), depth + 1, files, bytes, capped);
            } else if metadata.is_file() {
                *files += 1;
                *bytes += metadata.len();
            }
        }
    }

    let mut stats = WorktreeWalkStats {
        file_count: 0,
        total_bytes: 0,
        capped: false,
    };
    visit(
        root,
        0,
        &mut stats.file_count,
        &mut stats.total_bytes,
        &mut stats.capped,
    );
    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use tempfile::TempDir;

    /// RAII guard that allows git's `file://` transport for this process and
    /// any `git` subprocess it spawns, via `GIT_CONFIG_*` environment
    /// injection. `git submodule update` blocks the file transport by
    /// default (CVE-2022-39253), and `create_worktree` intentionally does
    /// not override that, so the submodule fixture tests opt in here for the
    /// production-side update. Restores the prior environment on drop.
    ///
    /// Every caller is `#[serial(submodule)]` so this permissive setting
    /// never overlaps `test_create_worktree_skips_blocked_local_submodules`,
    /// which depends on the default block.
    struct AllowFileTransport {
        prev_count: Option<String>,
        prev_key: Option<String>,
        prev_value: Option<String>,
    }

    impl AllowFileTransport {
        fn set() -> Self {
            let guard = Self {
                prev_count: std::env::var("GIT_CONFIG_COUNT").ok(),
                prev_key: std::env::var("GIT_CONFIG_KEY_0").ok(),
                prev_value: std::env::var("GIT_CONFIG_VALUE_0").ok(),
            };
            // SAFETY: mutating the environment is unsafe in the 2024 edition
            // because it can race with concurrent reads on other threads.
            // The `#[serial(submodule)]` annotation on every caller, plus the
            // fact that no other test reads these `GIT_CONFIG_*` vars, keeps
            // this race-free in practice.
            unsafe {
                std::env::set_var("GIT_CONFIG_COUNT", "1");
                std::env::set_var("GIT_CONFIG_KEY_0", "protocol.file.allow");
                std::env::set_var("GIT_CONFIG_VALUE_0", "always");
            }
            guard
        }
    }

    impl Drop for AllowFileTransport {
        fn drop(&mut self) {
            // SAFETY: see `AllowFileTransport::set`.
            unsafe {
                match &self.prev_count {
                    Some(v) => std::env::set_var("GIT_CONFIG_COUNT", v),
                    None => std::env::remove_var("GIT_CONFIG_COUNT"),
                }
                match &self.prev_key {
                    Some(v) => std::env::set_var("GIT_CONFIG_KEY_0", v),
                    None => std::env::remove_var("GIT_CONFIG_KEY_0"),
                }
                match &self.prev_value {
                    Some(v) => std::env::set_var("GIT_CONFIG_VALUE_0", v),
                    None => std::env::remove_var("GIT_CONFIG_VALUE_0"),
                }
            }
        }
    }

    fn run_git(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed:\nstdout: {}\nstderr: {}",
            args,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn setup_test_repo() -> (TempDir, git2::Repository) {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.write_tree().unwrap()
        };
        {
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
                .unwrap();
        }

        (dir, repo)
    }

    #[test]
    fn test_is_git_repo_returns_true_for_git_directory() {
        let (_dir, repo) = setup_test_repo();
        assert!(GitWorktree::is_git_repo(repo.path().parent().unwrap()));
    }

    #[test]
    fn test_is_git_repo_returns_false_for_non_git_directory() {
        let dir = TempDir::new().unwrap();
        assert!(!GitWorktree::is_git_repo(dir.path()));
    }

    #[test]
    fn test_find_main_repo_returns_repo_root() {
        let (_dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();
        let result = GitWorktree::find_main_repo(repo_path).unwrap();
        assert_eq!(result, repo_path);
    }

    #[test]
    fn test_find_main_repo_fails_for_non_git_directory() {
        let dir = TempDir::new().unwrap();
        assert!(GitWorktree::find_main_repo(dir.path()).is_err());
    }

    #[test]
    fn test_list_worktrees_returns_main_and_additional() {
        let (dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("feature", &commit, false).unwrap();

        let wt_path = dir.path().join("feature-worktree");
        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        git_wt
            .create_worktree("feature", &wt_path, false, None)
            .unwrap();

        let worktrees = git_wt.list_worktrees().unwrap();
        assert!(worktrees.len() >= 2);
    }

    #[test]
    fn test_remove_worktree_deletes_worktree() {
        let (_dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("removable", &commit, false).unwrap();

        let wt_path = repo_path.parent().unwrap().join("removable-wt");
        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        git_wt
            .create_worktree("removable", &wt_path, false, None)
            .unwrap();

        assert!(wt_path.exists());

        git_wt.remove_worktree(&wt_path, false).unwrap();
        assert!(!wt_path.exists());
    }

    #[test]
    fn test_compute_path_with_template() {
        let (_dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();
        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();

        let template = "../{repo-name}-worktrees/{branch}";
        let path = git_wt
            .compute_path("feat/test", template, "abc123")
            .unwrap();

        assert!(path.to_string_lossy().contains("feat-test"));
        assert!(path.to_string_lossy().contains("-worktrees"));
    }

    /// Sets up a linked worktree bare repo structure:
    /// /tmp/xxx/
    ///   .bare/           <- bare git repository
    ///   .git             <- file containing "gitdir: ./.bare"
    ///   main/            <- worktree for main branch
    fn setup_linked_worktree_bare_repo() -> TempDir {
        let dir = TempDir::new().unwrap();
        let bare_path = dir.path().join(".bare");

        // Create a bare repository
        let repo = git2::Repository::init_bare(&bare_path).unwrap();

        // Create initial commit so we have a valid HEAD
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();

        // Create the .git file pointing to the bare repo
        let git_file_path = dir.path().join(".git");
        std::fs::write(&git_file_path, "gitdir: ./.bare\n").unwrap();

        // Create a worktree using git command (git2 worktree API is limited for bare repos)
        let main_wt_path = dir.path().join("main");
        std::process::Command::new("git")
            .args(["worktree", "add", main_wt_path.to_str().unwrap(), "HEAD"])
            .current_dir(&bare_path)
            .output()
            .unwrap();

        dir
    }

    fn convert_worktree_gitfile_to_relative(worktree_path: &Path) {
        let git_file = worktree_path.join(".git");
        let content = std::fs::read_to_string(&git_file).unwrap();
        let gitdir_line = content
            .lines()
            .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))
            .unwrap();
        let gitdir_path = Path::new(gitdir_line);

        if gitdir_path.is_relative() {
            return;
        }

        let worktree_canonical = worktree_path.canonicalize().unwrap();
        let gitdir_canonical = gitdir_path.canonicalize().unwrap();
        let relative = GitWorktree::diff_paths(&gitdir_canonical, &worktree_canonical).unwrap();
        std::fs::write(git_file, format!("gitdir: {}\n", relative.display())).unwrap();
    }

    fn setup_linked_worktree_with_worktrees_gitfile() -> Option<(TempDir, PathBuf)> {
        let dir = setup_linked_worktree_bare_repo();
        let worktree_path = dir.path().join("main");
        if !worktree_path.exists() {
            return None;
        }

        convert_worktree_gitfile_to_relative(&worktree_path);
        let git_file_content = std::fs::read_to_string(worktree_path.join(".git")).unwrap();
        assert!(
            git_file_content.contains("worktrees"),
            ".git file should point to worktrees/<name>, got: {git_file_content}"
        );

        Some((dir, worktree_path))
    }

    fn setup_sibling_bare_repo_worktree() -> Option<(TempDir, PathBuf, PathBuf)> {
        let dir = TempDir::new().unwrap();
        let repo_root = dir.path().join("fe");
        std::fs::create_dir_all(&repo_root).unwrap();
        let bare_repo_path = repo_root.join("foo.git");

        let init = std::process::Command::new("git")
            .args(["init", "--bare", bare_repo_path.to_str().unwrap()])
            .output()
            .ok()?;
        if !init.status.success() {
            return None;
        }

        let seed_path = repo_root.join("seed");
        let clone = std::process::Command::new("git")
            .args([
                "clone",
                bare_repo_path.to_str().unwrap(),
                seed_path.to_str().unwrap(),
            ])
            .output()
            .ok()?;
        if !clone.status.success() {
            return None;
        }

        let config_name = std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(&seed_path)
            .output()
            .ok()?;
        if !config_name.status.success() {
            return None;
        }
        let config_email = std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&seed_path)
            .output()
            .ok()?;
        if !config_email.status.success() {
            return None;
        }

        std::fs::write(seed_path.join("README.md"), "hello\n").ok()?;
        let add = std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(&seed_path)
            .output()
            .ok()?;
        if !add.status.success() {
            return None;
        }
        let commit = std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&seed_path)
            .output()
            .ok()?;
        if !commit.status.success() {
            return None;
        }
        let push = std::process::Command::new("git")
            .args(["push", "origin", "HEAD:main"])
            .current_dir(&seed_path)
            .output()
            .ok()?;
        if !push.status.success() {
            return None;
        }

        std::fs::remove_dir_all(&seed_path).ok()?;

        let worktree_path = repo_root.join("master");
        let add_worktree = std::process::Command::new("git")
            .args([
                "--git-dir",
                bare_repo_path.to_str().unwrap(),
                "worktree",
                "add",
                worktree_path.to_str().unwrap(),
                "main",
            ])
            .output()
            .ok()?;
        if !add_worktree.status.success() || !worktree_path.exists() {
            return None;
        }

        Some((dir, bare_repo_path, worktree_path))
    }

    #[test]
    fn test_is_git_repo_recognizes_worktree_gitfile_pointing_to_worktrees() {
        let Some((_dir, worktree_path)) = setup_linked_worktree_with_worktrees_gitfile() else {
            return;
        };

        assert!(
            GitWorktree::is_git_repo(&worktree_path),
            "Worktree path with .git -> <bare>/worktrees/<name> should be recognized"
        );

        // Nested subdirectories should NOT be recognized as git repos (no walk-up)
        let nested_path = worktree_path.join("nested");
        std::fs::create_dir_all(&nested_path).unwrap();
        assert!(
            !GitWorktree::is_git_repo(&nested_path),
            "Nested paths should not walk up to find ancestor repos"
        );
    }

    #[test]
    fn test_find_main_repo_from_worktree_gitfile_pointing_to_worktrees_returns_root() {
        let Some((dir, worktree_path)) = setup_linked_worktree_with_worktrees_gitfile() else {
            return;
        };

        let expected = dir.path().canonicalize().unwrap();
        assert_eq!(
            GitWorktree::find_main_repo(&worktree_path).unwrap(),
            expected,
            "find_main_repo should resolve linked worktree path back to bare repo root"
        );

        // Nested subdirectories should NOT resolve (no walk-up)
        let nested_path = worktree_path.join("nested");
        std::fs::create_dir_all(&nested_path).unwrap();
        assert!(
            GitWorktree::find_main_repo(&nested_path).is_err(),
            "find_main_repo should not walk up from nested paths"
        );
    }

    #[test]
    fn test_find_main_repo_from_sibling_bare_repo_worktree_returns_bare_repo_path() {
        let Some((_dir, bare_repo_path, worktree_path)) = setup_sibling_bare_repo_worktree() else {
            return;
        };

        let expected = bare_repo_path.canonicalize().unwrap();
        assert_eq!(
            GitWorktree::find_main_repo(&worktree_path).unwrap(),
            expected,
            "find_main_repo should resolve sibling bare-repo worktree to bare repo path"
        );
        assert!(
            GitWorktree::new(expected).is_ok(),
            "resolved bare repo path should be accepted by GitWorktree::new"
        );

        // Nested subdirectories should NOT resolve (no walk-up)
        let nested_path = worktree_path.join("nested");
        std::fs::create_dir_all(&nested_path).unwrap();
        assert!(
            GitWorktree::find_main_repo(&nested_path).is_err(),
            "find_main_repo should not walk up from nested paths"
        );
    }

    #[test]
    fn test_find_main_repo_from_direct_bare_repo_path_returns_bare_repo_path() {
        let Some((_dir, bare_repo_path, _worktree_path)) = setup_sibling_bare_repo_worktree()
        else {
            return;
        };

        let expected = bare_repo_path.canonicalize().unwrap();
        assert_eq!(
            GitWorktree::find_main_repo(&bare_repo_path).unwrap(),
            expected,
            "find_main_repo should keep direct bare repo paths instead of returning their parent"
        );
        assert!(
            GitWorktree::new(expected).is_ok(),
            "direct bare repo path should be accepted by GitWorktree::new"
        );
    }

    #[test]
    fn test_list_worktrees_works_with_linked_worktree_bare_repo() {
        let dir = setup_linked_worktree_bare_repo();

        let main_repo_path = GitWorktree::find_main_repo(dir.path()).unwrap();
        let git_wt = GitWorktree::new(main_repo_path).unwrap();

        let worktrees = git_wt.list_worktrees();
        assert!(
            worktrees.is_ok(),
            "list_worktrees should succeed for linked worktree setup"
        );

        let worktrees = worktrees.unwrap();
        // Should have at least the main worktree
        assert!(!worktrees.is_empty(), "Should list at least one worktree");
    }

    #[test]
    fn test_delete_branch_deletes_local_branch() {
        let (_dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        // Create a new branch
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("to-delete", &commit, false).unwrap();

        // Verify branch exists
        assert!(repo
            .find_branch("to-delete", git2::BranchType::Local)
            .is_ok());

        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        git_wt.delete_branch("to-delete").unwrap();

        // Verify branch no longer exists
        assert!(repo
            .find_branch("to-delete", git2::BranchType::Local)
            .is_err());
    }

    #[test]
    fn test_create_worktree_from_remote_branch() {
        let dir = TempDir::new().unwrap();

        // Create the "remote" repo with a branch
        let remote_path = dir.path().join("remote");
        std::fs::create_dir(&remote_path).unwrap();
        let remote_repo = git2::Repository::init(&remote_path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = remote_repo.index().unwrap().write_tree().unwrap();
        let tree = remote_repo.find_tree(tree_id).unwrap();
        let commit_oid = remote_repo
            .commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
            .unwrap();
        let commit = remote_repo.find_commit(commit_oid).unwrap();
        remote_repo
            .branch("remote-only-branch", &commit, false)
            .unwrap();

        // Clone it as the "local" repo
        let local_path = dir.path().join("local");
        std::process::Command::new("git")
            .args([
                "clone",
                remote_path.to_str().unwrap(),
                local_path.to_str().unwrap(),
            ])
            .output()
            .unwrap();

        // Verify the branch is not local but is remote
        let local_repo = git2::Repository::open(&local_path).unwrap();
        assert!(local_repo
            .find_branch("remote-only-branch", git2::BranchType::Local)
            .is_err());
        assert!(local_repo
            .find_branch("origin/remote-only-branch", git2::BranchType::Remote)
            .is_ok());

        // Create a worktree from the remote branch
        let wt_path = dir.path().join("remote-wt");
        let git_wt = GitWorktree::new(local_path).unwrap();
        git_wt
            .create_worktree("remote-only-branch", &wt_path, false, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    #[test]
    fn test_delete_branch_is_idempotent_for_nonexistent_branch() {
        // Sessions whose metadata stamps a branch that was never actually
        // created (e.g. stale or corrupted session record) should still
        // delete cleanly. The caller asked for the branch to be gone, and
        // it is — that's the intended end state.
        let (_dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        let result = git_wt.delete_branch("nonexistent");

        assert!(
            result.is_ok(),
            "delete_branch should succeed when branch is absent, got {:?}",
            result
        );
    }

    #[test]
    fn test_delete_branch_idempotent_for_branch_with_spaces_in_name() {
        // Regression for sessions whose branch field was populated with a
        // free-form string ("too many open files"). Git rejects such a name
        // with "branch '<name>' not found"; treat as success.
        let (_dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        let result = git_wt.delete_branch("too many open files");

        assert!(
            result.is_ok(),
            "delete_branch should be idempotent for never-created branch names, got {:?}",
            result
        );
    }

    #[test]
    fn test_delete_branch_returns_error_for_other_failures() {
        // Trying to delete the currently checked-out branch should still
        // fail — that's a real error, not a "branch doesn't exist" case.
        let (_dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        // The setup_test_repo helper checks out a branch named "main" or
        // "master" depending on git defaults. Either way, it's the active
        // branch and cannot be deleted.
        let head_branch = repo
            .head()
            .ok()
            .and_then(|h| h.shorthand().ok().map(String::from))
            .expect("HEAD should be a branch");

        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        let result = git_wt.delete_branch(&head_branch);

        assert!(
            result.is_err(),
            "delete_branch should fail for the checked-out branch, got {:?}",
            result
        );
    }

    #[test]
    fn test_create_worktree_succeeds_after_stale_directory_deleted() {
        let (dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("stale-branch", &commit, false).unwrap();

        let wt_path = dir.path().join("stale-worktree");
        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        git_wt
            .create_worktree("stale-branch", &wt_path, false, None)
            .unwrap();
        assert!(wt_path.exists());

        // Simulate external deletion (e.g., container rebuild) by removing the
        // worktree directory without going through `git worktree remove`.
        std::fs::remove_dir_all(&wt_path).unwrap();
        assert!(!wt_path.exists());

        // Creating a worktree at the same path should succeed because
        // create_worktree prunes stale entries first.
        git_wt
            .create_worktree("stale-branch", &wt_path, false, None)
            .unwrap();
        assert!(wt_path.exists());
    }

    #[test]
    fn test_created_worktree_is_locked_and_survives_prune_when_unreachable() {
        // Regression for #2414: a `git worktree prune` issued from a context
        // that cannot see a sibling's checkout (a separate sandbox, or the host
        // when the worktree lives in a container mount) must not reap that
        // sibling's admin entry. aoe locks every worktree it creates; prune
        // skips locked entries.
        let (dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();
        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("locked-branch", &commit, false).unwrap();

        let wt_path = dir.path().join("locked-worktree");
        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        git_wt
            .create_worktree("locked-branch", &wt_path, false, None)
            .unwrap();

        let admin_dir = repo_path.join(".git/worktrees").join("locked-worktree");
        assert!(
            admin_dir.join("locked").exists(),
            "create_worktree should lock the new worktree"
        );

        // Simulate the pruning process being unable to see the checkout by
        // moving it aside, then prune. The locked admin entry must survive.
        let hidden = dir.path().join("locked-worktree-HIDDEN");
        std::fs::rename(&wt_path, &hidden).unwrap();
        git_wt.prune_worktrees().unwrap();
        assert!(
            admin_dir.exists(),
            "locked worktree admin entry must survive a prune while its checkout is unreachable"
        );

        // Restore the checkout; the git linkage is intact, and the managed
        // remove path still works because it unlocks first.
        std::fs::rename(&hidden, &wt_path).unwrap();
        git_wt.remove_worktree(&wt_path, true).unwrap();
        assert!(!wt_path.exists());
        assert!(
            !admin_dir.exists(),
            "remove_worktree should unlock and reap the admin entry"
        );
    }

    #[test]
    fn test_create_worktree_returns_error_on_git_failure() {
        let (dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("fail-branch", &commit, false).unwrap();

        let wt_path = dir.path().join("fail-worktree");
        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        git_wt
            .create_worktree("fail-branch", &wt_path, false, None)
            .unwrap();

        // Try creating again at a different path but same branch - git won't
        // allow two worktrees to check out the same branch. This is the
        // branch-already-checked-out case, so it maps to the typed
        // BranchAlreadyCheckedOut error naming the branch.
        let wt_path2 = dir.path().join("fail-worktree-2");
        let result = git_wt.create_worktree("fail-branch", &wt_path2, false, None);
        match result {
            Err(GitError::BranchAlreadyCheckedOut(branch)) => assert_eq!(branch, "fail-branch"),
            other => panic!("Expected BranchAlreadyCheckedOut error, got: {other:?}"),
        }
    }

    // ---- Full worktree creation flow tests for regular repos ----

    #[test]
    fn test_regular_repo_create_worktree_existing_branch() {
        let (dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        let head = repo.head().unwrap();
        let commit = head.peel_to_commit().unwrap();
        repo.branch("feature-a", &commit, false).unwrap();

        let main_repo = GitWorktree::find_main_repo(repo_path).unwrap();
        assert!(!GitWorktree::is_bare_repo(&main_repo));
        let git_wt = GitWorktree::new(main_repo).unwrap();

        let wt_path = dir.path().join("feature-a-wt");
        git_wt
            .create_worktree("feature-a", &wt_path, false, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    #[test]
    fn test_regular_repo_create_worktree_new_branch() {
        let (dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        let main_repo = GitWorktree::find_main_repo(repo_path).unwrap();
        let git_wt = GitWorktree::new(main_repo).unwrap();

        let wt_path = dir.path().join("new-feat-wt");
        git_wt
            .create_worktree("new-feat", &wt_path, true, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(repo
            .find_branch("new-feat", git2::BranchType::Local)
            .is_ok());
    }

    /// Regression for #948: when an explicit `base_branch` is passed,
    /// the new worktree branch must point at the base branch's commit,
    /// not the repo's default branch tip.
    #[test]
    fn test_create_worktree_uses_explicit_base_branch() {
        let (dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        // Add a second commit on the default branch (HEAD), then branch
        // "release-1" off the first commit. The new worktree branch
        // based on "release-1" should land at the first commit, not the
        // second.
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let first_commit_oid = repo.head().unwrap().peel_to_commit().unwrap().id();
        std::fs::write(repo_path.join("post.txt"), b"second").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(std::path::Path::new("post.txt")).unwrap();
        let tree_id = index.write_tree().unwrap();
        index.write().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.find_commit(first_commit_oid).unwrap();
        let second_commit_oid = repo
            .commit(Some("HEAD"), &sig, &sig, "second", &tree, &[&parent])
            .unwrap();
        assert_ne!(first_commit_oid, second_commit_oid);

        // Create "release-1" pointing at the first commit (pre-second).
        repo.branch(
            "release-1",
            &repo.find_commit(first_commit_oid).unwrap(),
            false,
        )
        .unwrap();

        let main_repo = GitWorktree::find_main_repo(repo_path).unwrap();
        let git_wt = GitWorktree::new(main_repo).unwrap();

        let wt_path = dir.path().join("hotfix-wt");
        git_wt
            .create_worktree("hotfix-1", &wt_path, true, Some("release-1"))
            .unwrap();

        let hotfix = repo
            .find_branch("hotfix-1", git2::BranchType::Local)
            .unwrap();
        let hotfix_target = hotfix.get().target().unwrap();
        assert_eq!(
            hotfix_target, first_commit_oid,
            "branch based on release-1 should sit at the first commit, not HEAD"
        );
    }

    #[test]
    fn test_regular_repo_full_builder_flow() {
        let (_dir, repo) = setup_test_repo();
        let repo_path = repo.path().parent().unwrap();

        // Mimic what builder.rs does
        assert!(GitWorktree::is_git_repo(repo_path));

        let main_repo_path = GitWorktree::find_main_repo(repo_path).unwrap();
        let git_wt = GitWorktree::new(main_repo_path.clone()).unwrap();

        let is_bare = GitWorktree::is_bare_repo(&main_repo_path);
        assert!(!is_bare);

        let template = if is_bare {
            "./{branch}"
        } else {
            "../{repo-name}-worktrees/{branch}"
        };
        let wt_path = git_wt
            .compute_path("my-branch", template, "abc12345")
            .unwrap();

        git_wt
            .create_worktree("my-branch", &wt_path, true, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());

        // Cleanup
        let _ = std::fs::remove_dir_all(&wt_path);
    }

    // ---- Full worktree creation flow tests for linked bare repos ----

    #[test]
    fn test_linked_bare_repo_create_worktree_new_branch() {
        let dir = setup_linked_worktree_bare_repo();
        let main_wt = dir.path().join("main");
        if !main_wt.exists() {
            return; // git not available
        }

        let main_repo_path = GitWorktree::find_main_repo(dir.path()).unwrap();
        let git_wt = GitWorktree::new(main_repo_path).unwrap();

        let wt_path = dir.path().join("new-feature");
        git_wt
            .create_worktree("new-feature", &wt_path, true, None)
            .unwrap();

        assert!(wt_path.exists(), "Worktree directory should be created");
        assert!(
            wt_path.join(".git").exists(),
            "Worktree should have .git file"
        );
    }

    #[test]
    fn test_linked_bare_repo_create_worktree_existing_branch() {
        let dir = setup_linked_worktree_bare_repo();
        let main_wt = dir.path().join("main");
        if !main_wt.exists() {
            return;
        }

        let main_repo_path = GitWorktree::find_main_repo(dir.path()).unwrap();
        let git_wt = GitWorktree::new(main_repo_path.clone()).unwrap();

        // Create a branch first
        {
            let repo = open_repo_at(&main_repo_path).unwrap();
            let head = repo.head().unwrap();
            let commit = head.peel_to_commit().unwrap();
            repo.branch("existing-branch", &commit, false).unwrap();
        }

        let wt_path = dir.path().join("existing-branch-wt");
        git_wt
            .create_worktree("existing-branch", &wt_path, false, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    #[test]
    fn test_linked_bare_repo_create_worktree_from_worktree_dir() {
        let dir = setup_linked_worktree_bare_repo();
        let main_wt = dir.path().join("main");
        if !main_wt.exists() {
            return;
        }

        // Start from the worktree directory (as a user would)
        let main_repo_path = GitWorktree::find_main_repo(&main_wt).unwrap();
        let git_wt = GitWorktree::new(main_repo_path.clone()).unwrap();

        assert!(
            GitWorktree::is_bare_repo(&main_repo_path),
            "Should detect bare repo when resolved from worktree"
        );

        let wt_path = dir.path().join("from-wt-branch");
        git_wt
            .create_worktree("from-wt-branch", &wt_path, true, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    #[test]
    fn test_linked_bare_repo_full_builder_flow() {
        let dir = setup_linked_worktree_bare_repo();
        let main_wt = dir.path().join("main");
        if !main_wt.exists() {
            return;
        }

        // Mimic the exact flow from builder.rs, starting from a worktree dir
        let path = &main_wt;
        assert!(
            GitWorktree::is_git_repo(path),
            "Worktree should be recognized as git repo"
        );

        let main_repo_path = GitWorktree::find_main_repo(path).unwrap();
        let git_wt = GitWorktree::new(main_repo_path.clone()).unwrap();

        let is_bare = GitWorktree::is_bare_repo(&main_repo_path);
        assert!(is_bare, "Should detect bare repo");

        let template = if is_bare {
            "./{branch}"
        } else {
            "../{repo-name}-worktrees/{branch}"
        };
        let wt_path = git_wt
            .compute_path("builder-branch", template, "abc12345")
            .unwrap();

        git_wt
            .create_worktree("builder-branch", &wt_path, true, None)
            .unwrap();

        assert!(
            wt_path.exists(),
            "Worktree should be created at computed path"
        );
        assert!(wt_path.join(".git").exists());

        // Verify the worktree is a sibling inside the project dir (bare repo template)
        assert_eq!(
            wt_path.parent().unwrap().canonicalize().unwrap(),
            dir.path().canonicalize().unwrap(),
            "Worktree should be a sibling directory in bare repo layout"
        );
    }

    #[test]
    fn test_linked_bare_repo_full_builder_flow_from_root() {
        let dir = setup_linked_worktree_bare_repo();
        let main_wt = dir.path().join("main");
        if !main_wt.exists() {
            return;
        }

        // Same as above, but starting from the root directory (not a worktree)
        let path = dir.path();
        assert!(
            GitWorktree::is_git_repo(path),
            "Bare repo root should be recognized as git repo"
        );

        let main_repo_path = GitWorktree::find_main_repo(path).unwrap();
        let git_wt = GitWorktree::new(main_repo_path.clone()).unwrap();

        let is_bare = GitWorktree::is_bare_repo(&main_repo_path);
        assert!(is_bare, "Should detect bare repo from root");

        let template = "./{branch}";
        let wt_path = git_wt
            .compute_path("root-branch", template, "xyz99999")
            .unwrap();

        git_wt
            .create_worktree("root-branch", &wt_path, true, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    // ---- Sibling bare repo worktree creation tests ----

    #[test]
    fn test_sibling_bare_repo_create_worktree_new_branch() {
        let Some((_dir, bare_repo_path, _worktree_path)) = setup_sibling_bare_repo_worktree()
        else {
            return;
        };

        let main_repo_path = GitWorktree::find_main_repo(&bare_repo_path).unwrap();
        let git_wt = GitWorktree::new(main_repo_path).unwrap();

        let wt_path = bare_repo_path.parent().unwrap().join("new-sibling-wt");
        git_wt
            .create_worktree("new-sibling", &wt_path, true, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    #[test]
    fn test_sibling_bare_repo_create_worktree_existing_branch() {
        let Some((_dir, bare_repo_path, _worktree_path)) = setup_sibling_bare_repo_worktree()
        else {
            return;
        };

        let main_repo_path = GitWorktree::find_main_repo(&bare_repo_path).unwrap();
        let git_wt = GitWorktree::new(main_repo_path.clone()).unwrap();

        // Create a branch to check out
        {
            let repo = open_repo_at(&main_repo_path).unwrap();
            // Find any existing commit to branch from
            let oid = repo
                .reflog("HEAD")
                .ok()
                .and_then(|reflog| reflog.get(0).map(|e| e.id_new()))
                .or_else(|| {
                    repo.branches(Some(git2::BranchType::Local))
                        .ok()
                        .and_then(|mut branches| {
                            branches.find_map(|b| b.ok().and_then(|(b, _)| b.get().target()))
                        })
                })
                .expect("bare repo should have at least one commit");
            let commit = repo.find_commit(oid).unwrap();
            repo.branch("existing-sibling", &commit, false).unwrap();
        }

        let wt_path = bare_repo_path.parent().unwrap().join("existing-sibling-wt");
        git_wt
            .create_worktree("existing-sibling", &wt_path, false, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    #[test]
    fn test_sibling_bare_repo_create_worktree_from_worktree_dir() {
        let Some((_dir, _bare_repo_path, worktree_path)) = setup_sibling_bare_repo_worktree()
        else {
            return;
        };

        // Start from an existing worktree
        let main_repo_path = GitWorktree::find_main_repo(&worktree_path).unwrap();
        let git_wt = GitWorktree::new(main_repo_path).unwrap();

        let wt_path = worktree_path.parent().unwrap().join("from-wt-sibling");
        git_wt
            .create_worktree("from-wt-sibling", &wt_path, true, None)
            .unwrap();

        assert!(wt_path.exists());
        assert!(wt_path.join(".git").exists());
    }

    /// Sets up a bare repo whose parent directory contains a spurious `.git/` directory
    /// (e.g. created by an external tool storing state files there).
    fn setup_bare_repo_whose_parent_has_spurious_git_dir() -> Option<(TempDir, PathBuf)> {
        let dir = TempDir::new().unwrap();

        // Simulate what opencode does: create a `.git/` directory in the parent and
        // drop a state file into it.  This is the exact scenario seen in production.
        let spurious_git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&spurious_git_dir).unwrap();
        std::fs::write(spurious_git_dir.join("opencode"), "some-sha\n").unwrap();

        // Create the actual bare repo as a subdirectory of the parent.
        let bare_path = dir.path().join("bare");
        let init = std::process::Command::new("git")
            .args(["init", "--bare", bare_path.to_str().unwrap()])
            .output()
            .ok()?;
        if !init.status.success() {
            return None;
        }

        // Give the bare repo an initial commit so HEAD resolves.
        {
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let repo = git2::Repository::open_bare(&bare_path).unwrap();
            let tree_id = repo.treebuilder(None).unwrap().write().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "Initial commit", &tree, &[])
                .unwrap();
        }

        // Create a linked worktree (this is what creates bare/worktrees/ and bare/main/).
        let main_wt = bare_path.join("main");
        let add = std::process::Command::new("git")
            .args([
                "--git-dir",
                bare_path.to_str().unwrap(),
                "worktree",
                "add",
                main_wt.to_str().unwrap(),
                "HEAD",
            ])
            .output()
            .ok()?;
        if !add.status.success() || !main_wt.exists() {
            return None;
        }

        Some((dir, bare_path))
    }

    #[test]
    fn test_find_main_repo_for_bare_repo_whose_parent_has_spurious_git_dir() {
        let Some((_dir, bare_path)) = setup_bare_repo_whose_parent_has_spurious_git_dir() else {
            return;
        };

        let result = GitWorktree::find_main_repo(&bare_path);
        assert!(
            result.is_ok(),
            "find_main_repo should succeed for a bare repo whose parent has a \
             spurious .git/ directory, got: {:?}",
            result.err()
        );
        assert_eq!(
            result.unwrap().canonicalize().unwrap(),
            bare_path.canonicalize().unwrap(),
            "find_main_repo should return the bare repo root itself, not its parent"
        );
    }

    #[test]
    fn test_find_main_repo_from_worktree_with_spurious_parent_git_dir() {
        let Some((_dir, bare_path)) = setup_bare_repo_whose_parent_has_spurious_git_dir() else {
            return;
        };

        // find_main_repo from a linked worktree inside the bare repo should resolve
        // back to the bare repo, not to the parent with the spurious .git/ dir.
        let worktree_path = bare_path.join("main");
        let result = GitWorktree::find_main_repo(&worktree_path);
        assert!(
            result.is_ok(),
            "find_main_repo from worktree should succeed, got: {:?}",
            result.err()
        );
        let resolved = result.unwrap().canonicalize().unwrap();
        let expected = bare_path.canonicalize().unwrap();
        assert_eq!(
            resolved, expected,
            "should resolve to bare repo, not parent"
        );
    }

    #[test]
    fn test_full_builder_flow_for_bare_repo_whose_parent_has_spurious_git_dir() {
        let Some((_dir, bare_path)) = setup_bare_repo_whose_parent_has_spurious_git_dir() else {
            return;
        };

        // Mimic the exact flow executed by builder.rs / cli/add.rs.
        assert!(
            GitWorktree::is_git_repo(&bare_path),
            "is_git_repo must return true before the builder proceeds"
        );

        let main_repo_path = GitWorktree::find_main_repo(&bare_path).unwrap();

        // This is the call that fails before the fix: find_main_repo returns the
        // parent directory, and GitWorktree::new then rejects it as NotAGitRepo.
        let git_wt = GitWorktree::new(main_repo_path.clone())
            .expect("GitWorktree::new should succeed with the resolved bare repo path");

        assert!(
            GitWorktree::is_bare_repo(&main_repo_path),
            "Should be detected as a bare repo"
        );

        let wt_path = bare_path.join("new-feature");
        git_wt
            .create_worktree("new-feature", &wt_path, true, None)
            .unwrap();

        assert!(wt_path.exists(), "Worktree directory should be created");
        assert!(
            wt_path.join(".git").is_file(),
            "Worktree should have a .git pointer file"
        );
    }

    // --- detect_default_branch tests ---

    #[test]
    fn test_detect_default_branch_returns_main() {
        // setup_test_repo creates a repo with HEAD on main (git2 default)
        let (dir, _repo) = setup_test_repo();
        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        // git2::Repository::init creates an initial branch based on init.defaultBranch
        // or "master". Either way, it should be detected.
        let result = git_wt.detect_default_branch().unwrap();
        assert!(
            result == "main" || result == "master",
            "expected main or master, got: {result}"
        );
    }

    #[test]
    fn test_detect_default_branch_finds_master_when_no_main() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Create a commit on a "master" branch explicitly
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("refs/heads/master"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        // If there's also a "main" from init, remove it
        if let Ok(mut main_branch) = repo.find_branch("main", git2::BranchType::Local) {
            let _ = main_branch.delete();
        }

        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        assert_eq!(git_wt.detect_default_branch().unwrap(), "master");
    }

    #[test]
    fn test_detect_default_branch_falls_back_to_first_branch() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("refs/heads/develop"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        // Remove main and master if they exist
        if let Ok(mut b) = repo.find_branch("main", git2::BranchType::Local) {
            let _ = b.delete();
        }
        if let Ok(mut b) = repo.find_branch("master", git2::BranchType::Local) {
            let _ = b.delete();
        }

        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        let result = git_wt.detect_default_branch().unwrap();
        assert_eq!(result, "develop");
    }

    #[test]
    fn test_detect_default_branch_prefers_remote_head_over_local_main() {
        let remote_dir = TempDir::new().unwrap();
        let remote = git2::Repository::init_bare(remote_dir.path()).unwrap();

        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let blob_oid = remote.blob(b"hello").unwrap();
            let mut tb = remote.treebuilder(None).unwrap();
            tb.insert("file.txt", blob_oid, 0o100644).unwrap();
            tb.write().unwrap()
        };
        let tree = remote.find_tree(tree_id).unwrap();
        let develop_oid = remote
            .commit(
                Some("refs/heads/develop"),
                &sig,
                &sig,
                "initial",
                &tree,
                &[],
            )
            .unwrap();
        let develop_commit = remote.find_commit(develop_oid).unwrap();
        remote.branch("main", &develop_commit, true).unwrap();
        remote.set_head("refs/heads/develop").unwrap();

        let local_dir = TempDir::new().unwrap();
        git2::Repository::clone(remote_dir.path().to_str().unwrap(), local_dir.path()).unwrap();

        let local_repo = git2::Repository::open(local_dir.path()).unwrap();
        let remote_main_commit = local_repo
            .find_branch("origin/main", git2::BranchType::Remote)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        local_repo
            .branch("main", &remote_main_commit, true)
            .unwrap();

        let git_wt = GitWorktree::new(local_dir.path().to_path_buf()).unwrap();
        assert_eq!(git_wt.detect_default_branch().unwrap(), "develop");
    }

    /// Fork + upstream layout (issue #1029): when `origin` is the user's
    /// stale fork and `upstream` is the canonical remote, the freshest
    /// `main` lives at `upstream/main`. Detection must pick it over a
    /// stale local `main` and stale `origin/main`.
    #[test]
    fn test_detect_default_branch_picks_upstream_over_stale_origin() {
        let upstream_dir = TempDir::new().unwrap();
        let upstream = git2::Repository::init_bare(upstream_dir.path()).unwrap();
        upstream.set_head("refs/heads/main").unwrap();

        // Use explicit timestamps so commit B is detectably newer than
        // commit A. git2 commit times are second-precision; without
        // this the in-test commits all collide at the same second and
        // commit-time scoring degenerates into tiebreak territory.
        let sig_a = git2::Signature::new(
            "Test",
            "test@example.com",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let sig_b = git2::Signature::new(
            "Test",
            "test@example.com",
            &git2::Time::new(1_700_001_000, 0),
        )
        .unwrap();
        let tree_a_id = {
            let blob = upstream.blob(b"hello").unwrap();
            let mut tb = upstream.treebuilder(None).unwrap();
            tb.insert("file.txt", blob, 0o100644).unwrap();
            tb.write().unwrap()
        };
        let tree_a = upstream.find_tree(tree_a_id).unwrap();
        let commit_a = upstream
            .commit(
                Some("refs/heads/main"),
                &sig_a,
                &sig_a,
                "commit A",
                &tree_a,
                &[],
            )
            .unwrap();

        let origin_dir = TempDir::new().unwrap();
        let origin = git2::Repository::init_bare(origin_dir.path()).unwrap();
        origin.set_head("refs/heads/main").unwrap();
        let origin_tree = {
            let blob = origin.blob(b"hello").unwrap();
            let mut tb = origin.treebuilder(None).unwrap();
            tb.insert("file.txt", blob, 0o100644).unwrap();
            let id = tb.write().unwrap();
            origin.find_tree(id).unwrap()
        };
        origin
            .commit(
                Some("refs/heads/main"),
                &sig_a,
                &sig_a,
                "commit A",
                &origin_tree,
                &[],
            )
            .unwrap();

        let commit_a_obj = upstream.find_commit(commit_a).unwrap();
        let tree_b = {
            let blob = upstream.blob(b"world").unwrap();
            let mut tb = upstream.treebuilder(Some(&tree_a)).unwrap();
            tb.insert("file2.txt", blob, 0o100644).unwrap();
            let id = tb.write().unwrap();
            upstream.find_tree(id).unwrap()
        };
        let commit_b = upstream
            .commit(
                Some("refs/heads/main"),
                &sig_b,
                &sig_b,
                "commit B",
                &tree_b,
                &[&commit_a_obj],
            )
            .unwrap();

        let local_dir = TempDir::new().unwrap();
        git2::Repository::clone(origin_dir.path().to_str().unwrap(), local_dir.path()).unwrap();
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

        let local_repo = git2::Repository::open(local_dir.path()).unwrap();
        let upstream_tip = local_repo
            .find_branch("upstream/main", git2::BranchType::Remote)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap();
        local_repo.branch("feature", &upstream_tip, false).unwrap();
        local_repo.set_head("refs/heads/feature").unwrap();

        // Sanity: HEAD is at commit B, local main and origin/main at commit A.
        assert_eq!(
            local_repo.head().unwrap().peel_to_commit().unwrap().id(),
            commit_b
        );
        let local_main_oid = local_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(local_main_oid, commit_a);

        let git_wt = GitWorktree::new(local_dir.path().to_path_buf()).unwrap();
        let info = git_wt.detect_default_branch_info().unwrap();
        assert_eq!(info.name, "main");
        assert_eq!(
            info.remote.as_deref(),
            Some("upstream"),
            "should pick upstream/main over stale origin/main and local main; got {info:?}"
        );
        assert_eq!(info.qualified_ref(), "upstream/main");
        assert_eq!(git_wt.detect_default_branch().unwrap(), "main");
    }

    /// HEAD doesn't descend from any candidate (unrelated history /
    /// fresh detached state). The non-ancestor pool is still scored,
    /// so something sensible comes back rather than an error.
    #[test]
    fn test_detect_default_branch_info_falls_back_when_no_ancestor() {
        let dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let main_commit = repo
            .commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        // Force HEAD onto an unrelated history by detaching to an
        // orphan commit (no parents, different tree).
        let blob = repo.blob(b"orphan").unwrap();
        let mut tb = repo.treebuilder(None).unwrap();
        tb.insert("orphan.txt", blob, 0o100644).unwrap();
        let orphan_tree = repo.find_tree(tb.write().unwrap()).unwrap();
        let orphan_commit = repo
            .commit(None, &sig, &sig, "orphan", &orphan_tree, &[])
            .unwrap();
        repo.set_head_detached(orphan_commit).unwrap();

        // Sanity: HEAD is orphan, main exists at main_commit.
        let head_oid = repo.head().unwrap().peel_to_commit().unwrap().id();
        assert_eq!(head_oid, orphan_commit);
        assert_ne!(head_oid, main_commit);

        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        let info = git_wt.detect_default_branch_info().unwrap();
        assert_eq!(info.name, "main");
        assert!(info.remote.is_none());
    }

    /// Qualified ref helper round-trips remote vs local correctly.
    #[test]
    fn test_default_branch_info_qualified_ref() {
        let remote = DefaultBranchInfo {
            name: "main".to_string(),
            remote: Some("upstream".to_string()),
        };
        assert_eq!(remote.qualified_ref(), "upstream/main");

        let local = DefaultBranchInfo {
            name: "develop".to_string(),
            remote: None,
        };
        assert_eq!(local.qualified_ref(), "develop");
    }

    // --- fetch_branch tests ---

    #[test]
    fn test_sanitize_remote_credentials_redacts_https_userinfo() {
        let s = "fatal: unable to access 'https://alice:supersecret@github.com/foo/bar.git/': 404";
        let out = sanitize_remote_credentials(s);
        assert!(!out.contains("supersecret"), "token leaked: {out}");
        assert!(!out.contains("alice:"), "username leaked: {out}");
        assert!(
            out.contains("https://<redacted>@github.com/foo/bar.git/"),
            "expected redacted URL, got: {out}"
        );
    }

    #[test]
    fn test_sanitize_remote_credentials_redacts_ssh_userinfo() {
        let s = "Could not read from remote ssh://git:tokenval@example.com:22/foo";
        let out = sanitize_remote_credentials(s);
        assert!(!out.contains("tokenval"), "token leaked: {out}");
        assert!(out.contains("ssh://<redacted>@example.com:22/foo"));
    }

    #[test]
    fn test_sanitize_remote_credentials_passes_through_when_no_userinfo() {
        let s = "fatal: 'origin' does not appear to be a git repository";
        assert_eq!(sanitize_remote_credentials(s), s);
        let s2 = "fatal: unable to access 'https://github.com/foo/bar.git/'";
        assert_eq!(sanitize_remote_credentials(s2), s2);
    }

    #[test]
    fn test_sanitize_remote_credentials_does_not_touch_scp_style() {
        // `git@host:path` is the SCP-style ref, not a URL with userinfo.
        // Without a `scheme://` prefix the regex does not match, which
        // is correct: there is no embedded password to redact.
        let s = "fatal: Could not read from remote: git@github.com:foo/bar.git";
        assert_eq!(sanitize_remote_credentials(s), s);
    }

    #[test]
    fn test_classify_worktree_add_failure_branch_in_use() {
        // Current git wording.
        let combined =
            "fatal: 'feature/foo' is already used by worktree at '/tmp/repo-worktrees/feature-foo'";
        match classify_worktree_add_failure(combined, "feature/foo") {
            GitError::BranchAlreadyCheckedOut(b) => assert_eq!(b, "feature/foo"),
            other => panic!("expected BranchAlreadyCheckedOut, got {other:?}"),
        }
        // Older git wording.
        let combined_old = "fatal: 'feature/foo' is already checked out at '/tmp/other'";
        assert!(matches!(
            classify_worktree_add_failure(combined_old, "feature/foo"),
            GitError::BranchAlreadyCheckedOut(_)
        ));
    }

    #[test]
    fn test_classify_worktree_add_failure_redacts_credentials() {
        let combined =
            "fatal: unable to access 'https://alice:supersecret@github.com/foo/bar.git/': 403";
        match classify_worktree_add_failure(combined, "feature/foo") {
            GitError::WorktreeCommandFailed(msg) => {
                assert!(!msg.contains("supersecret"), "token leaked: {msg}");
                assert!(!msg.contains("alice:"), "username leaked: {msg}");
            }
            other => panic!("expected WorktreeCommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn test_fetch_branch_returns_failed_when_no_remote() {
        let (dir, _repo) = setup_test_repo();
        let git_wt = GitWorktree::new(dir.path().to_path_buf()).unwrap();
        // No remote configured: `git fetch origin main` exits non-zero
        // with a "remote does not exist" message. Surface as Failed so
        // callers can write a warning.
        let outcome = git_wt.fetch_branch("origin", "main");
        assert!(
            matches!(outcome, FetchOutcome::Failed(_)),
            "expected Failed, got {outcome:?}"
        );
    }

    #[test]
    fn test_fetch_branch_returns_failed_when_branch_missing_on_remote() {
        let remote_dir = TempDir::new().unwrap();
        let _remote = git2::Repository::init_bare(remote_dir.path()).unwrap();

        // Clone to get a local repo with an "origin" remote
        let local_dir = TempDir::new().unwrap();
        let _local =
            git2::Repository::clone(remote_dir.path().to_str().unwrap(), local_dir.path()).unwrap();

        let git_wt = GitWorktree::new(local_dir.path().to_path_buf()).unwrap();
        // Fetching a branch that doesn't exist on the remote should
        // surface as Failed (not Ok), so create_worktree can record it
        // as a warning for the user.
        let outcome = git_wt.fetch_branch("origin", "nonexistent-branch");
        assert!(
            matches!(outcome, FetchOutcome::Failed(_)),
            "expected Failed, got {outcome:?}"
        );
    }

    #[test]
    fn test_fetch_branch_returns_ok_when_branch_exists_on_remote() {
        // Sanity: real fetch path returns Ok. Sets up a tiny "remote"
        // repo with a single commit on main, clones it locally, and
        // re-runs fetch.
        let remote_dir = TempDir::new().unwrap();
        let remote = git2::Repository::init_bare(remote_dir.path()).unwrap();
        remote.set_head("refs/heads/main").unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let blob = remote.blob(b"hello").unwrap();
            let mut tb = remote.treebuilder(None).unwrap();
            tb.insert("file.txt", blob, 0o100644).unwrap();
            tb.write().unwrap()
        };
        let tree = remote.find_tree(tree_id).unwrap();
        remote
            .commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let local_dir = TempDir::new().unwrap();
        git2::Repository::clone(remote_dir.path().to_str().unwrap(), local_dir.path()).unwrap();

        let git_wt = GitWorktree::new(local_dir.path().to_path_buf()).unwrap();
        assert_eq!(git_wt.fetch_branch("origin", "main"), FetchOutcome::Ok);
    }

    // --- create_worktree fetch integration test ---

    #[test]
    fn test_create_worktree_branches_from_remote_after_fetch() {
        // Create a bare "remote" repo with an initial commit on main
        let remote_dir = TempDir::new().unwrap();
        let remote = git2::Repository::init_bare(remote_dir.path()).unwrap();

        // Point HEAD at refs/heads/main so clone creates a local "main" branch
        remote.set_head("refs/heads/main").unwrap();

        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let blob_oid = remote.blob(b"hello").unwrap();
            let mut tb = remote.treebuilder(None).unwrap();
            tb.insert("file.txt", blob_oid, 0o100644).unwrap();
            tb.write().unwrap()
        };
        let tree = remote.find_tree(tree_id).unwrap();
        let initial_oid = remote
            .commit(Some("refs/heads/main"), &sig, &sig, "initial", &tree, &[])
            .unwrap();

        // Clone it locally
        let local_dir = TempDir::new().unwrap();
        git2::Repository::clone(remote_dir.path().to_str().unwrap(), local_dir.path()).unwrap();

        // Add a second commit to the remote (local is now 1 commit behind)
        let blob2 = remote.blob(b"world").unwrap();
        let mut tb2 = remote.treebuilder(Some(&tree)).unwrap();
        tb2.insert("file2.txt", blob2, 0o100644).unwrap();
        let tree2_id = tb2.write().unwrap();
        let tree2 = remote.find_tree(tree2_id).unwrap();
        let initial_commit = remote.find_commit(initial_oid).unwrap();
        let remote_head_oid = remote
            .commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "second commit",
                &tree2,
                &[&initial_commit],
            )
            .unwrap();

        // Verify local is behind: local main should still be at initial_oid
        let local_repo = git2::Repository::open(local_dir.path()).unwrap();
        let local_main = local_repo
            .find_branch("main", git2::BranchType::Local)
            .unwrap();
        assert_eq!(local_main.get().peel_to_commit().unwrap().id(), initial_oid);

        // Create a new worktree branch via create_worktree
        let wt_parent = TempDir::new().unwrap();
        let wt_path = wt_parent.path().join("test-fetch-wt");
        let git_wt = GitWorktree::new(local_dir.path().to_path_buf()).unwrap();
        git_wt
            .create_worktree("new-feature", &wt_path, true, None)
            .unwrap();

        // The new branch should be based on the remote's latest commit,
        // not the stale local HEAD
        let new_branch = local_repo
            .find_branch("new-feature", git2::BranchType::Local)
            .unwrap();
        let branch_commit_id = new_branch.get().peel_to_commit().unwrap().id();
        assert_eq!(
            branch_commit_id, remote_head_oid,
            "new branch should be based on remote HEAD ({remote_head_oid}), \
             not stale local HEAD ({initial_oid})"
        );
    }

    /// Build a main repo with a single submodule served from a local
    /// `file://` bare repo, then create a `test-feature` branch on its
    /// current HEAD. Returns the `TempDir` containers (which must stay
    /// alive for `git submodule update` to reach the URL) and the repo
    /// path.
    ///
    /// Uses a local `file://` transport rather than a `git daemon`: the
    /// daemon variant cloned over `git://` with no timeout, so a stalled
    /// clone could hang the whole test run indefinitely on slower CI
    /// runners (observed on macOS). `file://` is deterministic and
    /// offline. git blocks the file transport for submodules by default
    /// (the CVE-2022-39253 mitigation); the `submodule add` here opts in
    /// per-command with `-c protocol.file.allow=always`, and the caller
    /// installs an [`AllowFileTransport`] env guard so the production-side
    /// `git submodule update` (which inherits the test process env) can
    /// clone too.
    ///
    /// The submodule is added at `.claude/` and contains a `skill.md` file,
    /// so tests can assert on `wt_path.join(".claude").join("skill.md")` to
    /// distinguish "submodule initialized" from "only .gitmodules checked
    /// out".
    fn build_repo_with_submodule_and_branch(branch: &str) -> (TempDir, TempDir, TempDir) {
        let submodule_src_dir = TempDir::new().unwrap();
        let submodule_repo = git2::Repository::init(submodule_src_dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        std::fs::write(
            submodule_src_dir.path().join("skill.md"),
            "hello from submodule\n",
        )
        .unwrap();
        let submodule_tree_id = {
            let mut index = submodule_repo.index().unwrap();
            index.add_path(Path::new("skill.md")).unwrap();
            index.write_tree().unwrap()
        };
        let submodule_tree = submodule_repo.find_tree(submodule_tree_id).unwrap();
        submodule_repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "Initial submodule commit",
                &submodule_tree,
                &[],
            )
            .unwrap();

        let submodule_bare_dir = TempDir::new().unwrap();
        run_git(
            submodule_bare_dir.path(),
            &[
                "clone",
                "--bare",
                submodule_src_dir.path().to_str().unwrap(),
                "submodule.git",
            ],
        );
        let submodule_url = format!(
            "file://{}",
            submodule_bare_dir.path().join("submodule.git").display()
        );

        let repo_dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(repo_dir.path()).unwrap();
        std::fs::write(repo_dir.path().join("README.md"), "main repo\n").unwrap();
        let initial_tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("README.md")).unwrap();
            index.write_tree().unwrap()
        };
        let initial_tree = repo.find_tree(initial_tree_id).unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Initial commit",
            &initial_tree,
            &[],
        )
        .unwrap();

        run_git(repo_dir.path(), &["config", "user.name", "Test"]);
        run_git(
            repo_dir.path(),
            &["config", "user.email", "test@example.com"],
        );
        // `git submodule add` blocks the `file://` transport by default
        // (CVE-2022-39253); allow it for just this command. The later
        // production-side `git submodule update` is allowed via the
        // `AllowFileTransport` env guard the calling test installs.
        run_git(
            repo_dir.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                &submodule_url,
                ".claude",
            ],
        );
        run_git(repo_dir.path(), &["commit", "-am", "Add submodule"]);

        let repo = git2::Repository::open(repo_dir.path()).unwrap();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch(branch, &head_commit, false).unwrap();

        (submodule_src_dir, submodule_bare_dir, repo_dir)
    }

    #[test]
    #[serial(submodule)]
    fn test_create_worktree_initializes_submodules() {
        let _git_allow = AllowFileTransport::set();
        let (_submodule_src, _submodule_bare, repo_dir) =
            build_repo_with_submodule_and_branch("test-feature");

        let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
        let worktree_parent = TempDir::new().unwrap();
        let wt_path = worktree_parent.path().join("submodule-worktree");
        git_wt
            .create_worktree("test-feature", &wt_path, false, None)
            .unwrap();

        assert!(
            wt_path.join(".claude").join("skill.md").is_file(),
            "submodule contents should be initialized in the new worktree"
        );
    }

    #[test]
    #[serial(submodule)]
    fn test_create_worktree_skips_submodules_when_disabled() {
        // with_init_submodules(false) must skip the `git submodule update`
        // step entirely so the worktree shows up before submodules clone.
        // The guard only covers the fixture's `submodule add`; init is off.
        let _git_allow = AllowFileTransport::set();
        let (_submodule_src, _submodule_bare, repo_dir) =
            build_repo_with_submodule_and_branch("test-feature");

        let git_wt = GitWorktree::new(repo_dir.path().to_path_buf())
            .unwrap()
            .with_init_submodules(false);
        let worktree_parent = TempDir::new().unwrap();
        let wt_path = worktree_parent.path().join("submodule-worktree");
        git_wt
            .create_worktree("test-feature", &wt_path, false, None)
            .unwrap();

        assert!(
            wt_path.join(".git").exists(),
            "worktree itself should still be created"
        );
        assert!(
            wt_path.join(".gitmodules").is_file(),
            ".gitmodules should be checked out from the parent commit"
        );
        assert!(
            !wt_path.join(".claude").join("skill.md").is_file(),
            "submodule contents must NOT be initialized when init_submodules=false"
        );
    }

    #[test]
    #[serial(submodule)]
    fn test_create_worktree_skips_blocked_local_submodules() {
        // Serial with the other submodule tests (but installs no
        // `AllowFileTransport` guard) so it always observes git's default
        // file-transport block that this test asserts on.
        let submodule_dir = TempDir::new().unwrap();
        let submodule_repo = git2::Repository::init(submodule_dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        std::fs::write(
            submodule_dir.path().join("skill.md"),
            "hello from local submodule\n",
        )
        .unwrap();
        let submodule_tree_id = {
            let mut index = submodule_repo.index().unwrap();
            index.add_path(Path::new("skill.md")).unwrap();
            index.write_tree().unwrap()
        };
        let submodule_tree = submodule_repo.find_tree(submodule_tree_id).unwrap();
        submodule_repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "Initial submodule commit",
                &submodule_tree,
                &[],
            )
            .unwrap();

        let repo_dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(repo_dir.path()).unwrap();
        std::fs::write(repo_dir.path().join("README.md"), "main repo\n").unwrap();
        let initial_tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("README.md")).unwrap();
            index.write_tree().unwrap()
        };
        let initial_tree = repo.find_tree(initial_tree_id).unwrap();
        repo.commit(
            Some("HEAD"),
            &sig,
            &sig,
            "Initial commit",
            &initial_tree,
            &[],
        )
        .unwrap();

        run_git(repo_dir.path(), &["config", "user.name", "Test"]);
        run_git(
            repo_dir.path(),
            &["config", "user.email", "test@example.com"],
        );
        run_git(
            repo_dir.path(),
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                submodule_dir.path().to_str().unwrap(),
                ".claude",
            ],
        );
        run_git(repo_dir.path(), &["commit", "-am", "Add submodule"]);

        let repo = git2::Repository::open(repo_dir.path()).unwrap();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("test-feature", &head_commit, false).unwrap();

        let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
        let worktree_parent = TempDir::new().unwrap();
        let wt_path = worktree_parent.path().join("submodule-worktree");
        git_wt
            .create_worktree("test-feature", &wt_path, false, None)
            .unwrap();

        assert!(
            wt_path.join(".git").exists(),
            "worktree creation should succeed even when git blocks local-path submodules"
        );
        assert!(
            !wt_path.join(".claude").join("skill.md").is_file(),
            "blocked local submodules should be left uninitialized rather than forcing file transport"
        );
    }

    #[test]
    fn test_walk_worktree_stats_counts_files_and_bytes() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        std::fs::write(root.join("a.txt"), "hello").unwrap(); // 5 bytes
        std::fs::write(root.join("b.txt"), "world!").unwrap(); // 6 bytes
        std::fs::create_dir(root.join("sub")).unwrap();
        std::fs::write(root.join("sub").join("c.txt"), "xy").unwrap(); // 2 bytes

        let stats = walk_worktree_stats(root);
        assert_eq!(stats.file_count, 3);
        assert_eq!(stats.total_bytes, 13);
        assert!(!stats.capped);
    }

    #[test]
    fn test_walk_worktree_stats_skips_dot_git_at_root() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Worktree pointer file: a regular file named ".git" at the root.
        std::fs::write(root.join(".git"), "gitdir: /somewhere/else").unwrap();
        std::fs::write(root.join("kept.txt"), "x").unwrap();

        let stats = walk_worktree_stats(root);
        assert_eq!(stats.file_count, 1, "should skip the root .git entry");
        assert_eq!(stats.total_bytes, 1);
    }

    #[test]
    fn test_walk_worktree_stats_caps_deep_trees() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        // Build a path that exceeds the internal MAX_DEPTH (6). Visit caps
        // when depth > MAX_DEPTH, so we need >7 nested levels to trigger it.
        let mut cur = root.to_path_buf();
        for i in 0..10 {
            cur = cur.join(format!("d{i}"));
            std::fs::create_dir(&cur).unwrap();
        }
        std::fs::write(cur.join("deep.txt"), "z").unwrap();

        let stats = walk_worktree_stats(root);
        assert!(
            stats.capped,
            "deeply nested trees should set the capped flag"
        );
        assert_eq!(
            stats.file_count, 0,
            "files past the depth cap should not be counted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_create_worktree_tolerates_failing_post_checkout_hook() {
        use std::os::unix::fs::PermissionsExt;

        // Initialize a normal repo with one commit and the test-feature branch.
        let repo_dir = TempDir::new().unwrap();
        let repo = git2::Repository::init(repo_dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        std::fs::write(repo_dir.path().join("README.md"), "hi\n").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("README.md")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        let head_commit = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("test-feature", &head_commit, false).unwrap();

        // Drop a post-checkout hook that always fails. `git worktree add`
        // creates the worktree directory and the .git pointer BEFORE running
        // hooks, so this is exactly the partial-success state the new
        // tolerance branch is meant to surface as a warning.
        let hooks_dir = repo_dir.path().join(".git").join("hooks");
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let hook_path = hooks_dir.join("post-checkout");
        std::fs::write(
            &hook_path,
            "#!/bin/sh\necho 'simulated hook failure' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();

        let git_wt = GitWorktree::new(repo_dir.path().to_path_buf()).unwrap();
        let wt_parent = TempDir::new().unwrap();
        let wt_path = wt_parent.path().join("hook-failure");

        let warnings = git_wt
            .create_worktree("test-feature", &wt_path, false, None)
            .expect("create_worktree should succeed when only the post-checkout hook failed");

        assert!(
            wt_path.exists() && wt_path.join(".git").exists(),
            "worktree should have been created"
        );
        assert!(
            !warnings.is_empty(),
            "hook failure should produce at least one warning"
        );
        let combined = warnings.join("\n");
        assert!(
            combined.contains("post-checkout hook failed"),
            "warning should mention the post-checkout hook: {combined}"
        );
        assert!(
            combined.contains("simulated hook failure"),
            "warning should include the hook's stderr: {combined}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_walk_worktree_stats_skips_symlinks() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();

        std::fs::write(root.join("real.txt"), "abc").unwrap();
        std::os::unix::fs::symlink(root.join("real.txt"), root.join("link.txt")).unwrap();

        let stats = walk_worktree_stats(root);
        assert_eq!(
            stats.file_count, 1,
            "symlinks should not be counted alongside their target"
        );
        assert_eq!(stats.total_bytes, 3);
    }

    // --- pick_remote_for_branch + fork+upstream explicit-base tests ---

    /// Build a fork plus upstream layout where `upstream/<branch>` is
    /// strictly ahead of `origin/<branch>`. Returns the temp dirs that
    /// must outlive the test (else the remotes vanish), the local clone
    /// path, the commit OID at the upstream tip, and the commit OID at
    /// the origin tip.
    ///
    /// The local clone has `origin` configured (the fork) plus an
    /// `upstream` remote pointing at the canonical repo, mirroring the
    /// fork-plus-upstream developer setup.
    fn setup_fork_upstream_layout(
        branch: &str,
    ) -> (TempDir, TempDir, TempDir, git2::Oid, git2::Oid) {
        let sig_a = git2::Signature::new(
            "Test",
            "test@example.com",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();
        let sig_b = git2::Signature::new(
            "Test",
            "test@example.com",
            &git2::Time::new(1_700_001_000, 0),
        )
        .unwrap();

        let upstream_dir = TempDir::new().unwrap();
        let upstream = git2::Repository::init_bare(upstream_dir.path()).unwrap();
        upstream.set_head(&format!("refs/heads/{branch}")).unwrap();
        let tree_a_id = {
            let blob = upstream.blob(b"hello").unwrap();
            let mut tb = upstream.treebuilder(None).unwrap();
            tb.insert("file.txt", blob, 0o100644).unwrap();
            tb.write().unwrap()
        };
        let tree_a = upstream.find_tree(tree_a_id).unwrap();
        let commit_a = upstream
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &sig_a,
                &sig_a,
                "commit A",
                &tree_a,
                &[],
            )
            .unwrap();

        let origin_dir = TempDir::new().unwrap();
        let origin = git2::Repository::init_bare(origin_dir.path()).unwrap();
        origin.set_head(&format!("refs/heads/{branch}")).unwrap();
        let origin_tree_id = {
            let blob = origin.blob(b"hello").unwrap();
            let mut tb = origin.treebuilder(None).unwrap();
            tb.insert("file.txt", blob, 0o100644).unwrap();
            tb.write().unwrap()
        };
        let origin_tree = origin.find_tree(origin_tree_id).unwrap();
        let origin_tip = origin
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &sig_a,
                &sig_a,
                "commit A",
                &origin_tree,
                &[],
            )
            .unwrap();

        let commit_a_obj = upstream.find_commit(commit_a).unwrap();
        let tree_b = {
            let blob = upstream.blob(b"world").unwrap();
            let mut tb = upstream.treebuilder(Some(&tree_a)).unwrap();
            tb.insert("file2.txt", blob, 0o100644).unwrap();
            let id = tb.write().unwrap();
            upstream.find_tree(id).unwrap()
        };
        let commit_b = upstream
            .commit(
                Some(&format!("refs/heads/{branch}")),
                &sig_b,
                &sig_b,
                "commit B",
                &tree_b,
                &[&commit_a_obj],
            )
            .unwrap();

        let local_dir = TempDir::new().unwrap();
        git2::Repository::clone(origin_dir.path().to_str().unwrap(), local_dir.path()).unwrap();
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

        (upstream_dir, origin_dir, local_dir, commit_b, origin_tip)
    }

    #[test]
    fn test_pick_remote_for_branch_picks_freshest_remote() {
        let (_upstream, _origin, local, _upstream_tip, _origin_tip) =
            setup_fork_upstream_layout("release-1");
        let git_wt = GitWorktree::new(local.path().to_path_buf()).unwrap();
        let picked = git_wt.pick_remote_for_branch("release-1");
        assert_eq!(
            picked.as_deref(),
            Some("upstream"),
            "upstream/release-1 is newer than origin/release-1; should pick upstream"
        );
    }

    #[test]
    fn test_pick_remote_for_branch_returns_none_when_branch_not_on_any_remote() {
        let (_upstream, _origin, local, _upstream_tip, _origin_tip) =
            setup_fork_upstream_layout("release-1");
        let git_wt = GitWorktree::new(local.path().to_path_buf()).unwrap();
        assert_eq!(git_wt.pick_remote_for_branch("does-not-exist"), None);
    }

    #[test]
    fn test_pick_remote_for_branch_prefers_origin_on_commit_time_tie() {
        // Both remotes carry an identical commit (same time, same
        // tree). Tiebreak rule keeps `origin` as the conservative
        // default — matches historical behavior before fork+upstream
        // scoring landed.
        let sig = git2::Signature::new(
            "Test",
            "test@example.com",
            &git2::Time::new(1_700_000_000, 0),
        )
        .unwrap();

        fn seed_branch(
            sig: &git2::Signature,
            branch: &str,
            content: &[u8],
        ) -> (TempDir, git2::Oid) {
            let dir = TempDir::new().unwrap();
            let repo = git2::Repository::init_bare(dir.path()).unwrap();
            repo.set_head(&format!("refs/heads/{branch}")).unwrap();
            let tree_id = {
                let blob = repo.blob(content).unwrap();
                let mut tb = repo.treebuilder(None).unwrap();
                tb.insert("file.txt", blob, 0o100644).unwrap();
                tb.write().unwrap()
            };
            let tree = repo.find_tree(tree_id).unwrap();
            let oid = repo
                .commit(
                    Some(&format!("refs/heads/{branch}")),
                    sig,
                    sig,
                    "init",
                    &tree,
                    &[],
                )
                .unwrap();
            (dir, oid)
        }

        let (a_dir, _a_oid) = seed_branch(&sig, "main", b"hello");
        let (b_dir, _b_oid) = seed_branch(&sig, "main", b"hello");

        let local_dir = TempDir::new().unwrap();
        git2::Repository::clone(a_dir.path().to_str().unwrap(), local_dir.path()).unwrap();
        run_git(
            local_dir.path(),
            &["remote", "add", "upstream", b_dir.path().to_str().unwrap()],
        );
        run_git(local_dir.path(), &["fetch", "upstream"]);

        let git_wt = GitWorktree::new(local_dir.path().to_path_buf()).unwrap();
        assert_eq!(
            git_wt.pick_remote_for_branch("main").as_deref(),
            Some("origin"),
            "ties on commit_time should fall back to origin"
        );
    }

    #[test]
    fn test_create_worktree_explicit_base_branches_off_freshest_remote() {
        // Fork plus upstream layout, user passes `--base-branch main`.
        // The new branch must land on upstream/main (the fresh tip),
        // not origin/main (the stale fork tip). Regression for #1511.
        let (_upstream, _origin, local, upstream_tip, origin_tip) =
            setup_fork_upstream_layout("main");
        assert_ne!(upstream_tip, origin_tip, "test fixture mis-set up");

        let git_wt = GitWorktree::new(local.path().to_path_buf()).unwrap();
        let wt_parent = TempDir::new().unwrap();
        let wt_path = wt_parent.path().join("hotfix-wt");
        git_wt
            .create_worktree("hotfix-1", &wt_path, true, Some("main"))
            .unwrap();

        let local_repo = git2::Repository::open(local.path()).unwrap();
        let new_branch_tip = local_repo
            .find_branch("hotfix-1", git2::BranchType::Local)
            .unwrap()
            .get()
            .peel_to_commit()
            .unwrap()
            .id();
        assert_eq!(
            new_branch_tip, upstream_tip,
            "hotfix-1 should branch off upstream/main ({upstream_tip}), \
             not stale origin/main ({origin_tip})"
        );
    }

    #[test]
    fn test_create_worktree_explicit_base_typo_returns_branch_not_found() {
        // A typo in `--base-branch` must not silently produce a worktree
        // anchored to HEAD. With no remote-tracking ref and no local
        // branch by the typo'd name, create_worktree must surface
        // BranchNotFound so the caller can show the user the typo.
        let (dir, _repo) = setup_test_repo();
        let repo_path = dir.path();
        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        let wt_parent = TempDir::new().unwrap();
        let wt_path = wt_parent.path().join("hotfix-wt");
        let result = git_wt.create_worktree("hotfix-1", &wt_path, true, Some("maain"));
        match result {
            Err(GitError::BranchNotFound(name)) => {
                assert_eq!(name, "maain", "error should name the typo'd base");
            }
            other => panic!("expected BranchNotFound, got {other:?}"),
        }
        assert!(
            !wt_path.exists(),
            "no worktree should be created when base resolution fails"
        );
    }

    #[test]
    fn test_create_worktree_autodetected_base_still_falls_through_to_head() {
        // When no explicit base is passed, autodetect runs and may yield
        // a name that ultimately can't be resolved (e.g. a brand-new
        // local-only repo where `main` exists but the lookup chain
        // somehow misses it). The fallback to HEAD must still kick in
        // so the historical behavior of "create the worktree off HEAD"
        // is preserved. This is the autodetect twin of the typo test.
        let (dir, repo) = setup_test_repo();
        let repo_path = dir.path();
        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        let wt_parent = TempDir::new().unwrap();
        let wt_path = wt_parent.path().join("autodetected-wt");
        git_wt
            .create_worktree("new-feature", &wt_path, true, None)
            .expect("autodetect path should still succeed via HEAD fallback");
        assert!(wt_path.exists(), "worktree should be created");
        assert!(repo
            .find_branch("new-feature", git2::BranchType::Local)
            .is_ok());
    }

    #[test]
    fn test_create_worktree_surfaces_fetch_failure_as_warning() {
        // Configure a remote pointing at a non-existent path. The
        // fetch call inside create_worktree exits non-zero, but the
        // worktree itself should still get created (from the local
        // initial commit). The non-fatal failure should land in the
        // returned warnings vector so the wizard / CLI can show it.
        let (dir, _repo) = setup_test_repo();
        let repo_path = dir.path();

        // Point `origin` at a path that doesn't exist; `git fetch`
        // will exit non-zero with a "Could not read from remote" style
        // error.
        let bogus_remote = dir.path().join("does-not-exist.git");
        run_git(
            repo_path,
            &["remote", "add", "origin", bogus_remote.to_str().unwrap()],
        );

        let git_wt = GitWorktree::new(repo_path.to_path_buf()).unwrap();
        let wt_parent = TempDir::new().unwrap();
        let wt_path = wt_parent.path().join("new-feature");
        let warnings = git_wt
            .create_worktree("new-feature", &wt_path, true, None)
            .expect("create_worktree should still succeed when fetch fails");

        assert!(
            wt_path.exists() && wt_path.join(".git").exists(),
            "worktree should be created even when fetch fails"
        );
        let joined = warnings.join("\n");
        assert!(
            joined.contains("git fetch") && joined.contains("failed for"),
            "warnings should mention the fetch failure; got: {joined}"
        );
    }
}
