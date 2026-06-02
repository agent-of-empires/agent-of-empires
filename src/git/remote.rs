//! Git remote operations: repo cloning and origin-URL parsing.

use std::path::Path;

use super::error::{GitError, Result};
use super::open_repo_at;

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
}
