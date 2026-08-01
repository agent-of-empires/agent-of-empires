//! Git endpoints: repository cloning and branch listing.

use std::sync::Arc;

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::AppState;

// --- Clone repository ---

#[derive(Deserialize)]
pub struct CloneRepoBody {
    pub url: String,
    pub destination: Option<String>,
    #[serde(default)]
    pub shallow: bool,
    #[serde(default)]
    pub bare: bool,
}

/// Returns true if `url` looks like a git clone URL accepted by this
/// endpoint. Recognised forms: https, http, git, ssh, file schemes, and
/// scp-style (`user@host:path`). Anything else is rejected with a 400
/// before reaching `git clone`.
fn looks_like_git_url(url: &str) -> bool {
    url.starts_with("https://")
        || url.starts_with("http://")
        || url.starts_with("git://")
        || url.starts_with("ssh://")
        || url.starts_with("file://")
        || (url.contains('@') && url.contains(':') && !url.contains(' '))
}

/// Expand a leading `~` or `~/` to the user's home directory.
/// Leaves the path unchanged when no home dir is available.
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    std::path::PathBuf::from(path)
}

/// Extract a repository name from a git URL.
///
/// Handles HTTPS, SSH, and scp-style URLs:
///   https://github.com/user/repo.git       -> repo
///   git@github.com:user/repo.git           -> repo
///   ssh://git@host/user/repo               -> repo
///   ssh://git@host:2222/user/repo.git      -> repo
fn repo_name_from_url(url: &str) -> Option<String> {
    // For scheme-based URLs (https://, ssh://, git://), take the last path segment.
    // For scp-style (git@host:path), split on ':' and take the last segment of the path.
    let last_segment = if url.contains("://") {
        url.rsplit_once('/')?.1
    } else if let Some((_host, path)) = url.split_once(':') {
        // scp-style: git@github.com:user/repo.git
        path.rsplit('/').next().unwrap_or(path)
    } else {
        url.rsplit_once('/')?.1
    };
    let name = last_segment.trim_end_matches(".git");
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub async fn clone_repo(
    State(state): State<Arc<AppState>>,
    body: Result<Json<CloneRepoBody>, axum::extract::rejection::JsonRejection>,
) -> impl IntoResponse {
    if state.read_only {
        return (
            StatusCode::FORBIDDEN,
            Json(
                serde_json::json!({"error": "read_only", "message": "Server is in read-only mode"}),
            ),
        )
            .into_response();
    }
    // Cloning writes to $HOME and fetches from the network; the CityHall "Clone
    // URL" action is hidden in the UI, so close the endpoint too.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let Json(body) = match body {
        Ok(b) => b,
        Err(rej) => return rej.into_response(),
    };

    let url = body.url.trim().to_string();
    if url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(
                serde_json::json!({"error": "validation_failed", "message": "URL cannot be empty"}),
            ),
        )
            .into_response();
    }

    if !looks_like_git_url(&url) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": "URL does not look like a git repository URL"})),
        )
            .into_response();
    }

    if body.bare && body.shallow {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "validation_failed", "message": "Cannot use both bare and shallow options"})),
        )
            .into_response();
    }

    // Resolve destination path
    let destination = if let Some(ref dest) = body.destination {
        let dest = dest.trim();
        if dest.is_empty() {
            None
        } else {
            Some(expand_tilde(dest))
        }
    } else {
        None
    };

    let destination = match destination {
        Some(d) => d,
        None => {
            let repo_name = repo_name_from_url(&url).unwrap_or_else(|| "cloned-repo".to_string());
            let home = dirs::home_dir().unwrap_or_else(|| std::path::PathBuf::from("/tmp"));
            home.join(&repo_name)
        }
    };

    // Security: destination must be within the home directory
    if let Some(home) = dirs::home_dir() {
        let canonical_home = home.canonicalize().unwrap_or(home);
        // For new paths that don't exist yet, check the parent
        let check_path = if destination.exists() {
            destination.canonicalize().unwrap_or(destination.clone())
        } else {
            destination
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p.join(destination.file_name().unwrap_or_default()))
                .unwrap_or(destination.clone())
        };
        if !check_path.starts_with(&canonical_home) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "validation_failed", "message": "Destination must be within the home directory"})),
            )
                .into_response();
        }
    }

    let dest_display = destination.display().to_string();

    // Return an actionable error if the destination already exists, before
    // spawning the blocking task, so the user knows to pick a different name.
    if destination.exists() {
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "destination_exists",
                "message": format!(
                    "Directory '{}' already exists. Choose a different destination or delete it first.",
                    destination.file_name().unwrap_or_default().to_string_lossy()
                ),
                "path": dest_display,
            })),
        )
            .into_response();
    }

    let shallow = body.shallow;
    let bare = body.bare;
    let result = tokio::task::spawn_blocking(move || {
        if bare {
            crate::git::clone_bare_repo(&url, &destination)
        } else {
            crate::git::clone_repo(&url, &destination, shallow)?;
            Ok(destination.display().to_string())
        }
    })
    .await;

    match result {
        Ok(Ok(path)) => {
            (StatusCode::CREATED, Json(serde_json::json!({"path": path}))).into_response()
        }
        Ok(Err(e)) => {
            let msg = e.to_string();
            tracing::warn!(target: "http.api.git", "Clone failed for {dest_display}: {msg}");
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "clone_failed", "message": msg})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!(target: "http.api.git", "Clone task panicked: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({"error": "internal", "message": "Internal server error"})),
            )
                .into_response()
        }
    }
}

#[derive(Deserialize)]
pub struct BranchesQuery {
    pub path: String,
    /// Include remote-only branches alongside local ones. Used by the
    /// new-session wizard's base-branch picker so users can base a new
    /// worktree off a teammate's not-yet-fetched branch. See #948.
    #[serde(default)]
    pub include_remote: bool,
}

#[derive(Serialize)]
pub struct BranchInfo {
    pub name: String,
    pub is_current: bool,
    /// Set when the branch only exists on the remote (no local copy).
    /// Omitted on responses where every branch is local.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub remote_only: bool,
}

pub async fn list_branches(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<BranchesQuery>,
) -> impl IntoResponse {
    // Probes an arbitrary filesystem path for git branches; not reachable from
    // the CityHall name-only wizard, so close it to crafted requests too.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let include_remote = query.include_remote;
    let result = tokio::task::spawn_blocking(move || {
        let path = std::path::Path::new(&query.path);
        if !crate::git::GitWorktree::is_git_repo(path) {
            return Err("Path is not a git repository".to_string());
        }

        let current = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .current_dir(path)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string())
            .unwrap_or_default();

        let mut result: Vec<BranchInfo> = if include_remote {
            crate::git::diff::list_branches_with_remotes(path)
                .map_err(|e| e.to_string())?
                .into_iter()
                .take(400)
                .map(|entry| BranchInfo {
                    is_current: entry.name == current,
                    name: entry.name,
                    remote_only: entry.remote_only,
                })
                .collect()
        } else {
            crate::git::diff::list_branches(path)
                .map_err(|e| e.to_string())?
                .into_iter()
                .take(200)
                .map(|name| BranchInfo {
                    is_current: name == current,
                    name,
                    remote_only: false,
                })
                .collect()
        };

        result.sort_by_key(|b| std::cmp::Reverse(b.is_current));

        Ok(result)
    })
    .await;

    match result {
        Ok(Ok(branches)) => (
            StatusCode::OK,
            Json(serde_json::to_value(branches).unwrap()),
        )
            .into_response(),
        Ok(Err(msg)) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "not_a_repo", "message": msg})),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
pub struct IsRepoQuery {
    pub path: String,
}

/// Reports whether `path` is a git repository, using the same
/// `GitWorktree::is_git_repo` gate the session builder enforces (`builder.rs`
/// single-worktree path). The new-session wizard probes this to disable the
/// worktree toggle for a plain folder. Returns 200 `{ "is_git_repo": bool }`;
/// a transient failure surfaces as a non-200 so the client can stay optimistic
/// rather than misreport a real repo as a non-repo.
pub async fn is_git_repo(
    State(state): State<Arc<AppState>>,
    axum::extract::Query(query): axum::extract::Query<IsRepoQuery>,
) -> impl IntoResponse {
    // Probes an arbitrary filesystem path; not reachable from the CityHall
    // name-only wizard, so close it to crafted requests too.
    if let Some(resp) = super::cityhall_block(&state) {
        return resp;
    }
    let result = tokio::task::spawn_blocking(move || {
        crate::git::GitWorktree::is_git_repo(std::path::Path::new(&query.path))
    })
    .await;

    match result {
        Ok(is_git_repo) => (
            StatusCode::OK,
            Json(serde_json::json!({ "is_git_repo": is_git_repo })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": "internal", "message": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // ── repo_name_from_url tests ─────────────────────────────────────────────

    #[test]
    fn repo_name_from_https_url() {
        assert_eq!(
            repo_name_from_url("https://github.com/user/my-repo.git"),
            Some("my-repo".to_string())
        );
    }

    #[test]
    fn repo_name_from_https_url_no_dotgit() {
        assert_eq!(
            repo_name_from_url("https://github.com/user/my-repo"),
            Some("my-repo".to_string())
        );
    }

    #[test]
    fn repo_name_from_scp_url() {
        assert_eq!(
            repo_name_from_url("git@github.com:user/my-repo.git"),
            Some("my-repo".to_string())
        );
    }

    #[test]
    fn repo_name_from_ssh_url() {
        assert_eq!(
            repo_name_from_url("ssh://git@github.com/user/my-repo.git"),
            Some("my-repo".to_string())
        );
    }

    #[test]
    fn repo_name_from_ssh_url_with_port() {
        assert_eq!(
            repo_name_from_url("ssh://git@host:2222/user/my-repo.git"),
            Some("my-repo".to_string())
        );
    }

    // ── looks_like_git_url tests ──────────────────────────────────────────────

    #[test]
    fn looks_like_git_url_accepts_https() {
        assert!(looks_like_git_url("https://github.com/u/r.git"));
    }

    #[test]
    fn looks_like_git_url_accepts_http() {
        assert!(looks_like_git_url("http://example.com/r.git"));
    }

    #[test]
    fn looks_like_git_url_accepts_git_protocol() {
        assert!(looks_like_git_url("git://example.com/u/r.git"));
    }

    #[test]
    fn looks_like_git_url_accepts_ssh() {
        assert!(looks_like_git_url("ssh://git@github.com/u/r.git"));
    }

    #[test]
    fn looks_like_git_url_accepts_scp_style() {
        assert!(looks_like_git_url("git@github.com:u/r.git"));
    }

    #[test]
    fn looks_like_git_url_accepts_file_scheme() {
        assert!(looks_like_git_url("file:///tmp/bare.git"));
    }

    #[test]
    fn looks_like_git_url_rejects_bare_word() {
        assert!(!looks_like_git_url("not-a-url"));
    }

    #[test]
    fn looks_like_git_url_rejects_empty() {
        assert!(!looks_like_git_url(""));
    }

    #[test]
    fn looks_like_git_url_rejects_scp_style_with_space() {
        assert!(!looks_like_git_url("git@host: /path"));
    }

    #[test]
    fn expand_tilde_expands_home() {
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(expand_tilde("~/foo"), home.join("foo"));
        assert_eq!(expand_tilde("~"), home);
    }

    #[test]
    fn expand_tilde_leaves_absolute_paths_alone() {
        assert_eq!(
            expand_tilde("/tmp/foo"),
            std::path::PathBuf::from("/tmp/foo")
        );
        assert_eq!(expand_tilde("foo/bar"), std::path::PathBuf::from("foo/bar"));
    }
}
