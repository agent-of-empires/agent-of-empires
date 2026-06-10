//! Git remote operations: repo cloning and origin-URL parsing.

use std::path::Path;

use super::error::{GitError, Result};
use super::open_repo_at;

/// Clone a git repository as a bare repo with worktree setup, following the
/// workflow-guide structure. Returns the path to the created worktree
/// (`<destination>/main`). Cleans up `<destination>` on failure.
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
    let mut child = std::process::Command::new("git")
        .args(["clone", "--bare", url, bare_str])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| GitError::CloneFailed(format!("Failed to run git clone --bare: {e}")))?;

    let timeout = std::time::Duration::from_secs(300);
    let poll_interval = std::time::Duration::from_millis(200);
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => {
                let stderr = child
                    .stderr
                    .take()
                    .and_then(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok()?;
                        Some(buf)
                    })
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let _ = std::fs::remove_dir_all(destination);
                return Err(GitError::CloneFailed(stderr));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(destination);
                    return Err(GitError::CloneFailed(
                        "Bare clone timed out after 5 minutes".to_string(),
                    ));
                }
                std::thread::sleep(poll_interval);
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(destination);
                return Err(GitError::CloneFailed(format!(
                    "Failed waiting for git clone --bare: {e}"
                )));
            }
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

    // Detect the default branch. `git clone --bare` points the bare repo's
    // own HEAD at the remote's default branch, which works on every git
    // version. `refs/remotes/origin/HEAD` is only populated by `git fetch`
    // on git >= 2.45 (followRemoteHEAD), so it can't be relied on; try it
    // and then main/master as fallbacks. These probes must tolerate a
    // non-zero exit (the ref simply not existing), so they don't go through
    // `run_in_bare`, which treats failure as fatal and wipes the clone.
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

    tracing::info!(
        target: "git.fetch",
        "Bare clone complete: {} -> {}",
        redacted_url,
        worktree_path.display()
    );

    Ok(worktree_path.display().to_string())
}

/// Clone a git repository from a URL into the given destination directory.
///
/// The destination must not already exist. If `shallow` is true, only the
/// latest commit is fetched (`--depth 1`). The clone is killed after 5
/// minutes to prevent indefinite hangs (unresponsive remotes, SSH prompts).
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

    // Pipe stdin to /dev/null so SSH passphrase prompts fail immediately
    // instead of hanging the blocking thread.
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
    let mut child = std::process::Command::new("git")
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| GitError::CloneFailed(format!("Failed to run git clone: {e}")))?;

    // Poll with a 5-minute timeout to avoid blocking the thread pool forever.
    let timeout = std::time::Duration::from_secs(300);
    let poll_interval = std::time::Duration::from_millis(200);
    let start = std::time::Instant::now();

    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => {
                let stderr = child
                    .stderr
                    .take()
                    .and_then(|mut s| {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut s, &mut buf).ok()?;
                        Some(buf)
                    })
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                return Err(GitError::CloneFailed(stderr));
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    if destination.exists() {
                        let _ = std::fs::remove_dir_all(destination);
                    }
                    return Err(GitError::CloneFailed(
                        "Clone timed out after 5 minutes".to_string(),
                    ));
                }
                std::thread::sleep(poll_interval);
            }
            Err(e) => {
                return Err(GitError::CloneFailed(format!(
                    "Failed waiting for git clone: {e}"
                )));
            }
        }
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

/// Extract the owner (first path segment) from a git remote URL.
///
/// Handles common formats:
/// - SSH shorthand: `git@github.com:owner/repo.git`
/// - HTTPS: `https://github.com/owner/repo.git`
/// - SSH URL: `ssh://git@github.com/owner/repo.git`
pub(crate) fn parse_owner_from_remote_url(url: &str) -> Option<String> {
    // SSH shorthand: git@host:owner/repo.git
    // Detect by presence of '@' before ':' and no "://" scheme prefix.
    if !url.contains("://") {
        if let Some(colon_pos) = url.find(':') {
            if url[..colon_pos].contains('@') {
                let after = &url[colon_pos + 1..];
                let owner = after.split('/').next()?;
                return (!owner.is_empty()).then(|| owner.to_string());
            }
        }
    }

    // URL format: scheme://[user@]host/owner/repo.git
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    let after_host = &without_scheme[without_scheme.find('/')? + 1..];
    let owner = after_host.split('/').next()?;
    (!owner.is_empty()).then(|| owner.to_string())
}

/// Look up the owner of a git repository by reading the `origin` remote URL.
/// Returns `None` if the path is not a git repo, has no origin remote, or the
/// URL cannot be parsed.
pub fn get_remote_owner(path: &Path) -> Option<String> {
    let repo = open_repo_at(path).ok()?;
    let remote = repo.find_remote("origin").ok()?;
    let url = remote.url().ok()?;
    parse_owner_from_remote_url(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_owner_ssh_shorthand() {
        assert_eq!(
            parse_owner_from_remote_url("git@github.com:agent-of-empires/agent-of-empires.git"),
            Some("agent-of-empires".to_string()),
        );
    }

    #[test]
    fn test_parse_owner_https() {
        assert_eq!(
            parse_owner_from_remote_url("https://github.com/agent-of-empires/agent-of-empires.git"),
            Some("agent-of-empires".to_string()),
        );
    }

    #[test]
    fn test_parse_owner_ssh_url() {
        assert_eq!(
            parse_owner_from_remote_url(
                "ssh://git@github.com/agent-of-empires/agent-of-empires.git"
            ),
            Some("agent-of-empires".to_string()),
        );
    }

    #[test]
    fn test_parse_owner_http() {
        assert_eq!(
            parse_owner_from_remote_url("http://github.com/mozilla-ai/lumigator.git"),
            Some("mozilla-ai".to_string()),
        );
    }

    #[test]
    fn test_parse_owner_no_dotgit_suffix() {
        assert_eq!(
            parse_owner_from_remote_url("https://github.com/agent-of-empires/agent-of-empires"),
            Some("agent-of-empires".to_string()),
        );
    }

    #[test]
    fn test_parse_owner_empty_url() {
        assert_eq!(parse_owner_from_remote_url(""), None);
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
