//! Typed GitHub client errors, each with an actionable hint.

use reqwest::StatusCode;
use thiserror::Error;

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
        "GitHub rejected the request (HTTP 401).\n\
         AoE only makes unauthenticated public requests, so this usually means \
         the resource is private or the endpoint requires sign-in."
    )]
    Unauthorized,

    #[error(
        "GitHub refused the request for lack of an authorized scope (HTTP 403): {scopes}.\n\
         AoE makes unauthenticated requests, so it cannot satisfy this; the \
         resource needs a signed-in client."
    )]
    InsufficientScope { scopes: String },

    #[error(
        "GitHub API rate limit exceeded.\n\
         Wait for the limit to reset (see the X-RateLimit-Reset header) and retry."
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

    /// Auth failures must not steer users to a token path, and network
    /// failures must not suggest re-authenticating.
    #[test]
    fn error_hints_match_the_failure() {
        let auth = GitHubError::Unauthorized.to_string();
        assert!(!auth.contains("GITHUB_TOKEN") && !auth.contains("gh auth login"));

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
