//! Git remote operations: repo cloning and origin-URL parsing.

use std::path::Path;

use super::error::{GitError, Result};
use super::open_repo_at;

/// Clone as a bare repo with the workflow-guide worktree layout, returning
/// `<destination>/main`. Removes `<destination>` on failure.
#[tracing::instrument(target = "git.fetch", skip_all, fields(url = %redact_url(url)))]
pub fn clone_bare_repo(url: &str, destination: &Path) -> Result<String> {
    if destination.exists() {
        return Err(GitError::CloneFailed(format!(
            "Destination already exists: {}",
            destination.display()
        )));
    }

    let bare_dir = destination.join(".bare");
    let bare_str = bare_dir
        .to_str()
        .ok_or_else(|| GitError::CloneFailed("Invalid bare directory path".to_string()))?;

    let redacted_url = redact_url(url);

    tracing::debug!(
        target: "git.command",
        args = ?["clone", "--bare", &redacted_url, bare_str],
        "spawning git clone --bare"
    );
    // Stdin is null so an SSH passphrase prompt fails instead of hanging.
    // `run_with_timeout` captures output in regular files, so a grandchild
    // inheriting the handles cannot block past the deadline.
    let mut cmd = std::process::Command::new("git");
    cmd.args(["clone", "--bare", url, bare_str])
        .stdin(std::process::Stdio::null());
    let timeout = std::time::Duration::from_secs(300);

    match crate::process::run_with_timeout(&mut cmd, timeout) {
        Ok(Some(output)) if output.status.success() => {}
        Ok(Some(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let _ = std::fs::remove_dir_all(destination);
            return Err(GitError::CloneFailed(stderr));
        }
        Ok(None) => {
            let _ = std::fs::remove_dir_all(destination);
            return Err(GitError::CloneFailed(
                "Bare clone timed out after 5 minutes".to_string(),
            ));
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(destination);
            return Err(GitError::CloneFailed(format!(
                "Failed to run git clone --bare: {e}"
            )));
        }
    }

    let run_in_bare = |args: &[&str]| -> Result<std::process::Output> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&bare_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .map_err(|e| GitError::CloneFailed(format!("Git command failed: {e}")))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let _ = std::fs::remove_dir_all(destination);
            return Err(GitError::CloneFailed(stderr));
        }
        Ok(output)
    };

    let gitfile_path = destination.join(".git");
    if let Err(e) = std::fs::write(&gitfile_path, "gitdir: ./.bare\n") {
        let _ = std::fs::remove_dir_all(destination);
        return Err(GitError::CloneFailed(format!(
            "Failed to create .git file: {e}"
        )));
    }

    run_in_bare(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/*:refs/remotes/origin/*",
    ])?;

    run_in_bare(&["fetch", "origin"])?;

    // The bare repo's own HEAD points at the remote default on every git
    // version; `refs/remotes/origin/HEAD` only exists from git 2.45, so it and
    // main/master are fallbacks. These probes tolerate a missing ref, so they
    // bypass `run_in_bare`, which treats failure as fatal and wipes the clone.
    let probe = |args: &[&str]| -> Option<String> {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&bare_dir)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let out = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!out.is_empty()).then_some(out)
    };
    let branch_from_ref = |full: &str| full.rsplit_once('/').map(|(_, name)| name.to_string());

    let default_branch = probe(&["symbolic-ref", "--short", "HEAD"])
        .or_else(|| {
            probe(&["symbolic-ref", "refs/remotes/origin/HEAD"])
                .as_deref()
                .and_then(branch_from_ref)
        })
        .or_else(|| {
            probe(&["show-ref", "--verify", "refs/remotes/origin/main"]).map(|_| "main".into())
        })
        .or_else(|| {
            probe(&["show-ref", "--verify", "refs/remotes/origin/master"]).map(|_| "master".into())
        });

    let default_branch = match default_branch {
        Some(b) => b,
        None => {
            let _ = std::fs::remove_dir_all(destination);
            return Err(GitError::CloneFailed(
                "Could not detect default branch (tried HEAD, origin/HEAD, main, master)"
                    .to_string(),
            ));
        }
    };

    let worktree_path = destination.join("main");
    let worktree_str = worktree_path
        .to_str()
        .ok_or_else(|| GitError::CloneFailed("Invalid worktree path".to_string()))?;

    let output = std::process::Command::new("git")
        .args(["worktree", "add", worktree_str, &default_branch])
        .current_dir(destination)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| GitError::CloneFailed(format!("Git worktree add failed: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let _ = std::fs::remove_dir_all(destination);
        return Err(GitError::CloneFailed(format!(
            "Failed to create worktree: {stderr}"
        )));
    }

    // `main` is the one aoe-created worktree that does not go through
    // `create_worktree`, so lock it here for the same cross-boundary prune
    // protection (#2414). Best-effort: a failure only forfeits that.
    match super::GitWorktree::new(destination.to_path_buf()) {
        Ok(git_wt) => {
            if let Err(e) = git_wt.lock_worktree(&worktree_path) {
                tracing::warn!(
                    target: "git.fetch",
                    path = %worktree_path.display(),
                    error = %e,
                    "Bare clone: could not lock main worktree (cross-boundary prune protection unavailable)"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "git.fetch",
                path = %destination.display(),
                error = %e,
                "Bare clone: could not open repo to lock main worktree (cross-boundary prune protection unavailable)"
            );
        }
    }

    tracing::info!(
        target: "git.fetch",
        "Bare clone complete: {} -> {}",
        redacted_url,
        worktree_path.display()
    );

    Ok(worktree_path.display().to_string())
}

/// Clone into `destination`, which must not exist. `shallow` fetches only the
/// latest commit. Killed after 5 minutes, so an unresponsive remote or an SSH
/// prompt cannot hang forever.
#[tracing::instrument(target = "git.fetch", skip_all, fields(url = %redact_url(url), shallow))]
pub fn clone_repo(url: &str, destination: &Path, shallow: bool) -> Result<()> {
    if destination.exists() {
        return Err(GitError::CloneFailed(format!(
            "Destination already exists: {}",
            destination.display()
        )));
    }

    let dest_str = destination
        .to_str()
        .ok_or_else(|| GitError::CloneFailed("Invalid destination path".to_string()))?;

    let mut args = vec!["clone"];
    if shallow {
        args.extend(["--depth", "1"]);
    }
    args.extend([url, dest_str]);

    // Null stdin so an SSH passphrase prompt fails instead of hanging.
    let redacted_url = redact_url(url);
    let redacted_args: Vec<&str> = args
        .iter()
        .map(|a| if *a == url { redacted_url.as_str() } else { *a })
        .collect();
    tracing::debug!(
        target: "git.command",
        args = ?redacted_args,
        "spawning git clone"
    );
    let mut cmd = std::process::Command::new("git");
    cmd.args(&args).stdin(std::process::Stdio::null());

    // Output goes to regular files, so a grandchild inheriting the handles
    // cannot hang the wait past the timeout.
    let timeout = std::time::Duration::from_secs(300);

    match crate::process::run_with_timeout(&mut cmd, timeout) {
        Ok(Some(output)) if output.status.success() => Ok(()),
        Ok(Some(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            Err(GitError::CloneFailed(stderr))
        }
        Ok(None) => {
            if destination.exists() {
                let _ = std::fs::remove_dir_all(destination);
            }
            Err(GitError::CloneFailed(
                "Clone timed out after 5 minutes".to_string(),
            ))
        }
        Err(e) => Err(GitError::CloneFailed(format!(
            "Failed to run git clone: {e}"
        ))),
    }
}

/// Strip userinfo (`user:token@`) from a URL so credentials don't reach logs.
fn redact_url(url: &str) -> String {
    if let Some(scheme_end) = url.find("://") {
        let after = &url[scheme_end + 3..];
        if let Some(at_off) = after.find('@') {
            let prefix = &url[..scheme_end + 3];
            let rest = &after[at_off + 1..];
            return format!("{prefix}***@{rest}");
        }
    }
    url.to_string()
}

/// Split a git remote URL into its host and the path segment after it,
/// rejecting any URL that is not a canonical hosted remote: a known scheme
/// (`http`/`https`/`ssh`) or SSH shorthand (`user@host:path`). Local schemes
/// (`file://`), bare/absolute filesystem paths, and relative paths all
/// return `None` so they can never be mistaken for a hosted `owner/repo`.
fn split_host_and_path(url: &str) -> Option<(&str, &str)> {
    if !url.contains("://") {
        // SSH shorthand: git@host:owner/repo.git
        let colon_pos = url.find(':')?;
        let userinfo = &url[..colon_pos];
        if !userinfo.contains('@') {
            return None;
        }
        let path = &url[colon_pos + 1..];
        // An absolute path (`git@host:/foo/bar.git`) is a filesystem path,
        // not a hosted owner/repo slug; reject it.
        if path.starts_with('/') {
            return None;
        }
        let host = userinfo.rsplit_once('@').map_or(userinfo, |(_, h)| h);
        if host.is_empty() {
            return None;
        }
        return Some((host, path));
    }

    let (scheme, without_scheme) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https" | "ssh") {
        return None;
    }
    let slash_pos = without_scheme.find('/')?;
    let host_part = &without_scheme[..slash_pos];
    let host = host_part.rsplit_once('@').map_or(host_part, |(_, h)| h);
    if host.is_empty() {
        return None;
    }
    Some((host, &without_scheme[slash_pos + 1..]))
}

/// The owner segment of a hosted remote URL, via
/// [`parse_slug_from_remote_url`] so a local path's first component can never
/// pass as one.
pub(crate) fn parse_owner_from_remote_url(url: &str) -> Option<String> {
    parse_slug_from_remote_url(url)
        .and_then(|s| s.split_once('/').map(|(owner, _)| owner.to_string()))
}

/// The owner from a repo's `origin` remote, or `None` when there is no repo,
/// no origin, or no parse.
pub fn get_remote_owner(path: &Path) -> Option<String> {
    let repo = open_repo_at(path).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    let url = remote.url().ok()?;
    parse_owner_from_remote_url(url)
}

/// The bare owner for display plus an `owner@host` key for grouping, from one
/// parse, so two same-named owners on different hosts never share a bucket.
/// `None` under the same conditions as [`parse_owner_from_remote_url`].
pub(crate) fn parse_owner_with_key_from_remote_url(url: &str) -> Option<(String, String)> {
    let owner = parse_owner_from_remote_url(url)?;
    let (host, _) = split_host_and_path(url)?;
    // Hostnames are case-insensitive, so two spellings of one host must not
    // produce two buckets.
    let key = format!("{owner}@{}", host.to_ascii_lowercase());
    Some((owner, key))
}

/// `parse_owner_with_key_from_remote_url` against a repo's `origin`, in one
/// lookup. `None` under the same conditions as [`get_remote_owner`].
pub fn get_remote_owner_with_key(path: &Path) -> Option<(String, String)> {
    let repo = open_repo_at(path).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    let url = remote.url().ok()?;
    parse_owner_with_key_from_remote_url(url)
}

/// The `owner/repo` slug of a remote URL, without any `.git` suffix or
/// trailing slash. `None` unless the URL is a canonical hosted repo: an
/// `http`, `https` or `ssh` scheme, or SSH shorthand, with exactly an
/// `owner/repo` path.
pub(crate) fn parse_slug_from_remote_url(url: &str) -> Option<String> {
    let (_, path) = split_host_and_path(url)?;
    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segments = path.split('/').filter(|s| !s.is_empty());
    let owner = segments.next()?;
    let repo = segments.next()?;
    // A hosted repo is exactly `owner/repo`; anything deeper is not one.
    if segments.next().is_some() {
        return None;
    }
    Some(format!("{}/{}", owner, repo))
}

/// A repo's `origin` URL verbatim, unparsed, for the CityHall bundle.
pub fn get_remote_url(path: &Path) -> Option<String> {
    let repo = open_repo_at(path).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    remote.url().ok().map(str::to_string)
}

/// The slug from a repo's `origin` remote, or `None` when there is no repo,
/// no origin, or no parse.
pub fn get_remote_slug(path: &Path) -> Option<String> {
    let repo = open_repo_at(path).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    let url = remote.url().ok()?;
    parse_slug_from_remote_url(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_slug_reads_only_owner_repo_remotes() {
        let slug = Some("mozilla-ai/any-llm");
        for (url, expected) in [
            ("git@github.com:mozilla-ai/any-llm.git", slug),
            ("https://github.com/mozilla-ai/any-llm.git", slug),
            ("ssh://git@github.com/mozilla-ai/any-llm.git", slug),
            ("https://github.com/mozilla-ai/any-llm", slug),
            ("git@github.com:mozilla-ai/any-llm", slug),
            ("", None),
            ("git@github.com:owner", None),
            ("https://github.com/owner", None),
            ("file:///tmp/repo.git", None),
            ("file://host/owner/repo.git", None),
            ("https://example.com/group/sub/repo.git", None),
            ("git@host:/foo/bar.git", None),
        ] {
            assert_eq!(
                parse_slug_from_remote_url(url).as_deref(),
                expected,
                "{url}"
            );
        }
    }

    /// The owner is the first path segment of a canonical hosted remote, in
    /// every spelling git accepts. A local scheme, an absolute SSH path, or a
    /// URL with no real host has no owner at all: taking the first segment
    /// unconditionally gave those a bogus org header.
    #[test]
    fn parse_owner_accepts_only_hosted_remotes() {
        let cases = [
            (
                "git@github.com:agent-of-empires/agent-of-empires.git",
                Some("agent-of-empires"),
            ),
            (
                "https://github.com/agent-of-empires/agent-of-empires.git",
                Some("agent-of-empires"),
            ),
            (
                "ssh://git@github.com/agent-of-empires/agent-of-empires.git",
                Some("agent-of-empires"),
            ),
            (
                "http://github.com/mozilla-ai/lumigator.git",
                Some("mozilla-ai"),
            ),
            (
                "https://github.com/agent-of-empires/agent-of-empires",
                Some("agent-of-empires"),
            ),
            ("", None),
            ("file:///srv/git/foo.git", None),
            ("file://host/myorg/repo.git", None),
            ("git@host:/foo/bar.git", None),
            ("user@:owner/repo.git", None),
            ("https:///owner/repo.git", None),
            ("ssh://@/owner/repo.git", None),
        ];
        for (url, expected) in cases {
            assert_eq!(
                parse_owner_from_remote_url(url),
                expected.map(str::to_string),
                "{url}"
            );
        }
    }

    /// The key scopes the owner by host, lowercased, so one login on two
    /// hosts never shares a bucket and two spellings of one host always do.
    /// A non-hosted remote resolves to no identity at all.
    #[test]
    fn parse_owner_with_key_scopes_identity_by_lowercased_host() {
        let cases = [
            ("git@github.com:acme/x.git", Some("acme@github.com")),
            ("https://gitlab.com/acme/y.git", Some("acme@gitlab.com")),
            ("git@GitHub.COM:acme/x.git", Some("acme@github.com")),
            ("https://GitHub.com/acme/x.git", Some("acme@github.com")),
            ("file:///srv/git/foo.git", None),
        ];
        for (url, key) in cases {
            assert_eq!(
                parse_owner_with_key_from_remote_url(url),
                key.map(|key| ("acme".to_string(), key.to_string())),
                "{url}"
            );
        }
    }

    #[test]
    fn test_clone_bare_repo_creates_structure() {
        use tempfile::TempDir;

        // Create a source repo to clone from
        let source_dir = TempDir::new().unwrap();
        let source_repo = git2::Repository::init(source_dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut index = source_repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = source_repo.find_tree(tree_id).unwrap();
        source_repo
            .commit(Some("HEAD"), &sig, &sig, "Initial", &tree, &[])
            .unwrap();

        // Clone as bare repo
        let dest_dir = TempDir::new().unwrap();
        let dest_path = dest_dir.path().join("test-bare-clone");
        let url = format!("file://{}", source_dir.path().display());

        let result = clone_bare_repo(&url, &dest_path);
        assert!(result.is_ok(), "clone_bare_repo failed: {:?}", result.err());

        let worktree_path = result.unwrap();
        assert!(
            worktree_path.ends_with("/main"),
            "Expected path ending with /main"
        );

        // Verify structure
        assert!(dest_path.join(".bare").exists(), ".bare directory missing");
        assert!(dest_path.join(".git").exists(), ".git file missing");
        assert!(dest_path.join("main").exists(), "main worktree missing");

        // Verify .git file content
        let gitfile = std::fs::read_to_string(dest_path.join(".git")).unwrap();
        assert_eq!(gitfile.trim(), "gitdir: ./.bare");

        // Verify main is a valid worktree
        let main_path = dest_path.join("main");
        assert!(main_path.join(".git").exists(), "worktree .git missing");
    }

    #[test]
    fn test_clone_bare_repo_locks_main_worktree() {
        use tempfile::TempDir;

        // Regression for #2414: the `main` worktree the bare clone creates is
        // the one aoe-created worktree that does not go through
        // `create_worktree`, so it must still be locked or a prune from a
        // context that cannot see it would reap its admin entry.
        let source_dir = TempDir::new().unwrap();
        let source_repo = git2::Repository::init(source_dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = {
            let mut index = source_repo.index().unwrap();
            index.write_tree().unwrap()
        };
        let tree = source_repo.find_tree(tree_id).unwrap();
        source_repo
            .commit(Some("HEAD"), &sig, &sig, "Initial", &tree, &[])
            .unwrap();

        let dest_dir = TempDir::new().unwrap();
        let dest_path = dest_dir.path().join("test-bare-clone");
        let url = format!("file://{}", source_dir.path().display());
        clone_bare_repo(&url, &dest_path).unwrap();

        let admin_dir = dest_path.join(".bare/worktrees/main");
        assert!(
            admin_dir.join("locked").exists(),
            "clone_bare_repo should lock the main worktree"
        );

        // Hide the checkout so a prune sees it as missing; the locked admin
        // entry must survive.
        let hidden = dest_path.join("main-HIDDEN");
        std::fs::rename(dest_path.join("main"), &hidden).unwrap();
        std::process::Command::new("git")
            .args(["worktree", "prune"])
            .current_dir(&dest_path)
            .output()
            .unwrap();
        assert!(
            admin_dir.exists(),
            "locked main worktree admin entry must survive a prune while its checkout is unreachable"
        );
    }

    #[test]
    fn test_clone_bare_repo_destination_exists() {
        use tempfile::TempDir;

        let dest_dir = TempDir::new().unwrap();
        let dest_path = dest_dir.path().join("existing");
        std::fs::create_dir(&dest_path).unwrap();

        let result = clone_bare_repo("https://example.com/repo.git", &dest_path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("already exists"),
            "Expected 'already exists' error, got: {}",
            err
        );
    }
}
