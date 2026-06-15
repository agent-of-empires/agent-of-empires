//! Typed errors for the GitHub client.
//!
//! Each failure case carries its own actionable hint so the TUI toast and the
//! web error banner can show the user exactly what to do, never a generic
//! "auth required". The wording mirrors the house convention in
//! `src/git/error.rs` and `src/containers/error.rs`.

use reqwest::StatusCode;
use thiserror::Error;

/// Top-level error for any GitHub client operation.
#[derive(Debug, Error)]
pub enum GitHubError {
    #[error(
        "GitHub API is unreachable.\n\
         Check your network connection or GitHub status: https://www.githubstatus.com/\n\
         Details: {source}"
    )]
    Network {
        #[source]
        source: reqwest::Error,
    },

    #[error(
        "GitHub rejected the credentials (HTTP 401).\n\
         The token is missing, invalid, or expired.\n\
         Re-authenticate with: gh auth login, or set a fresh GITHUB_TOKEN."
    )]
    Unauthorized,

    #[error(
        "GitHub token is missing a required scope (HTTP 403).\n\
         This operation needs one of: {scopes}.\n\
         Re-authenticate with a token that carries it, for example:\n\
           gh auth login --scopes {scopes}\n\
         or set GITHUB_TOKEN to a personal access token with that scope."
    )]
    InsufficientScope { scopes: String },

    #[error(
        "GitHub API rate limit exceeded.\n\
         Wait for the limit to reset (see the X-RateLimit-Reset header) and retry.\n\
         Authenticating raises the limit: set GITHUB_TOKEN or run gh auth login."
    )]
    RateLimited,

    #[error("GitHub resource not found: {resource}")]
    NotFound { resource: String },

    #[error("GitHub API returned HTTP {status}: {message}")]
    Api { status: StatusCode, message: String },

    #[error("Failed to decode GitHub API response: {0}")]
    Decode(#[source] reqwest::Error),

    #[error("GitHub HTTP request failed: {0}")]
    Http(#[source] reqwest::Error),
}

pub type Result<T> = std::result::Result<T, GitHubError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insufficient_scope_names_the_scope() {
        let msg = GitHubError::InsufficientScope {
            scopes: "repo".to_string(),
        }
        .to_string();
        assert!(msg.contains("repo"), "must name the missing scope");
    }

    #[test]
    fn unauthorized_hint_mentions_reauthenticate() {
        let auth = GitHubError::Unauthorized.to_string();
        assert!(auth.contains("Re-authenticate"));
    }

    #[test]
    fn network_hint_does_not_suggest_reauthenticating() {
        // A GitHub outage must not tell the user to re-login. The Network
        // variant needs a real reqwest error, so exercise it via a transport
        // failure to a port that refuses connections.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let err = rt.block_on(async {
            crate::github::GitHubClient::unauthenticated(crate::github::GitHubClientConfig {
                api_base: "http://127.0.0.1:1".to_string(),
                user_agent: "agent-of-empires-test".to_string(),
                timeout: std::time::Duration::from_millis(200),
            })
            .unwrap()
            .latest_release("o", "r")
            .await
            .unwrap_err()
        });
        assert!(!err.to_string().contains("Re-authenticate"));
    }
}
