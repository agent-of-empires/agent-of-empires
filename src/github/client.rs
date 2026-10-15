//! Typed GitHub HTTP client: the single surface for `api.github.com`.

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};
use reqwest::StatusCode;

const TAG_SEGMENT: &AsciiSet = &CONTROLS
    .add(b'/')
    .add(b' ')
    .add(b'?')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+');
/// Encode everything non-alphanumeric so qualifier syntax survives in the query string.
const QUERY_VALUE: &AsciiSet = NON_ALPHANUMERIC;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::time::Duration;

use crate::github::error::{GitHubError, Result};

#[derive(Debug, Clone)]
pub struct GitHubClientConfig {
    pub api_base: String,
    pub user_agent: String,
    pub timeout: Duration,
}

pub struct GitHubClient {
    http: reqwest::Client,
    api_base: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub body: Option<String>,
    pub published_at: Option<String>,
    #[serde(default)]
    pub draft: bool,
    /// The list endpoint includes prereleases; `releases/latest` does not.
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubCompare {
    pub status: String,
    /// The compare endpoint caps `commits` at 250, so this may exceed its length.
    #[serde(default)]
    pub total_commits: u64,
    #[serde(default)]
    pub commits: Vec<GitHubCompareCommit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubCompareCommit {
    pub sha: String,
    #[serde(default)]
    pub html_url: String,
    pub commit: GitHubCommitInner,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubCommitInner {
    #[serde(default)]
    pub message: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRepo {
    pub full_name: String,
    #[serde(default)]
    pub html_url: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub stargazers_count: u64,
    #[serde(default)]
    pub topics: Vec<String>,
}

#[derive(Deserialize)]
struct SearchReposResponse {
    #[serde(default)]
    items: Vec<GitHubRepo>,
}

#[derive(Deserialize)]
struct ApiErrorBody {
    message: Option<String>,
}

impl GitHubClient {
    pub fn unauthenticated(config: GitHubClientConfig) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static("2022-11-28"),
        );

        let http = reqwest::Client::builder()
            .user_agent(config.user_agent)
            .timeout(config.timeout)
            .default_headers(headers)
            .build()
            .map_err(GitHubError::Http)?;

        Ok(Self {
            http,
            api_base: config.api_base.trim_end_matches('/').to_string(),
        })
    }

    pub async fn list_releases(
        &self,
        owner: &str,
        repo: &str,
        per_page: u8,
    ) -> Result<Vec<GitHubRelease>> {
        let url = format!(
            "{}/repos/{}/{}/releases?per_page={}",
            self.api_base, owner, repo, per_page
        );
        self.send_json(self.http.get(url)).await
    }

    pub async fn compare_commits(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
        head: &str,
    ) -> Result<GitHubCompare> {
        let base = utf8_percent_encode(base, TAG_SEGMENT);
        let head = utf8_percent_encode(head, TAG_SEGMENT);
        let url = format!(
            "{}/repos/{}/{}/compare/{}...{}",
            self.api_base, owner, repo, base, head
        );
        self.send_json(self.http.get(url)).await
    }

    pub async fn latest_release(&self, owner: &str, repo: &str) -> Result<GitHubRelease> {
        let url = format!("{}/repos/{}/{}/releases/latest", self.api_base, owner, repo);
        self.send_json(self.http.get(url)).await
    }

    pub async fn release_by_tag(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<GitHubRelease> {
        // A tag like `release/1.2.3` must stay one path segment.
        let tag = utf8_percent_encode(tag, TAG_SEGMENT);
        let url = format!(
            "{}/repos/{}/{}/releases/tags/{}",
            self.api_base, owner, repo, tag
        );
        self.send_json(self.http.get(url)).await
    }

    pub async fn search_repositories(&self, query: &str, per_page: u8) -> Result<Vec<GitHubRepo>> {
        let q = utf8_percent_encode(query, QUERY_VALUE);
        let url = format!(
            "{}/search/repositories?q={q}&sort=stars&order=desc&per_page={per_page}",
            self.api_base
        );
        let response: SearchReposResponse = self.send_json(self.http.get(url)).await?;
        Ok(response.items)
    }

    pub async fn get_repo_file(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        reference: Option<&str>,
    ) -> Result<String> {
        let path = utf8_percent_encode(path, TAG_SEGMENT);
        let mut url = format!(
            "{}/repos/{}/{}/contents/{}",
            self.api_base, owner, repo, path
        );
        if let Some(reference) = reference {
            url.push_str("?ref=");
            url.extend(utf8_percent_encode(reference, TAG_SEGMENT));
        }
        self.send_text(self.http.get(url).header(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github.raw"),
        ))
        .await
    }

    async fn send_json<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request.send().await.map_err(classify_transport_error)?;
        let status = response.status();
        if status.is_success() {
            return response.json::<T>().await.map_err(GitHubError::Decode);
        }
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        Err(classify_status(status, &headers, &body))
    }

    async fn send_text(&self, request: reqwest::RequestBuilder) -> Result<String> {
        let response = request.send().await.map_err(classify_transport_error)?;
        let status = response.status();
        if status.is_success() {
            return response.text().await.map_err(GitHubError::Http);
        }
        let headers = response.headers().clone();
        let body = response.text().await.unwrap_or_default();
        Err(classify_status(status, &headers, &body))
    }
}

fn classify_transport_error(error: reqwest::Error) -> GitHubError {
    if error.is_timeout() || error.is_connect() {
        GitHubError::Network { source: error }
    } else {
        GitHubError::Http(error)
    }
}

fn classify_status(status: StatusCode, headers: &HeaderMap, body: &str) -> GitHubError {
    match status {
        StatusCode::UNAUTHORIZED => GitHubError::Unauthorized,
        StatusCode::TOO_MANY_REQUESTS => GitHubError::RateLimited,
        StatusCode::FORBIDDEN => {
            if is_rate_limited(headers) {
                GitHubError::RateLimited
            } else if let Some(scopes) = missing_scope(headers, body) {
                GitHubError::InsufficientScope { scopes }
            } else {
                GitHubError::Api {
                    status,
                    message: api_message(body),
                }
            }
        }
        StatusCode::NOT_FOUND => GitHubError::NotFound {
            resource: api_message(body),
        },
        _ => GitHubError::Api {
            status,
            message: api_message(body),
        },
    }
}

fn is_rate_limited(headers: &HeaderMap) -> bool {
    let remaining_zero = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim() == "0")
        .unwrap_or(false);
    remaining_zero || headers.contains_key("retry-after")
}

/// Missing-scope only when the body says so; many unrelated 403s carry the header.
fn missing_scope(headers: &HeaderMap, body: &str) -> Option<String> {
    if !body.to_lowercase().contains("scope") {
        return None;
    }
    accepted_scopes(headers)
}

fn accepted_scopes(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-accepted-oauth-scopes")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn api_message(body: &str) -> String {
    if let Ok(parsed) = serde_json::from_str::<ApiErrorBody>(body) {
        if let Some(message) = parsed.message {
            return message;
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "no response body".to_string()
    } else {
        trimmed.chars().take(200).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> GitHubClientConfig {
        GitHubClientConfig {
            api_base: "https://api.github.com".to_string(),
            user_agent: "agent-of-empires-test".to_string(),
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn api_base_trailing_slash_is_trimmed() {
        let mut cfg = config();
        cfg.api_base = "https://example.test/".to_string();
        let client = GitHubClient::unauthenticated(cfg).unwrap();
        assert_eq!(client.api_base, "https://example.test");
    }

    fn headers_with(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn classify_status_separates_scope_rate_limit_and_plain_api_errors() {
        let scope_header = &[("x-accepted-oauth-scopes", "repo")][..];
        type ErrorCase = (
            StatusCode,
            &'static [(&'static str, &'static str)],
            &'static str,
            fn(GitHubError),
        );
        let cases: [ErrorCase; 9] = [
            (
                StatusCode::NOT_FOUND,
                &[],
                r#"{"message":"Not Found"}"#,
                |err| match err {
                    GitHubError::NotFound { resource } => assert_eq!(resource, "Not Found"),
                    other => panic!("expected NotFound, got {other:?}"),
                },
            ),
            (StatusCode::UNAUTHORIZED, &[], "", |err| {
                assert!(matches!(err, GitHubError::Unauthorized))
            }),
            (
                StatusCode::FORBIDDEN,
                scope_header,
                r#"{"message":"requires the repo scope"}"#,
                |err| match err {
                    GitHubError::InsufficientScope { scopes } => assert_eq!(scopes, "repo"),
                    other => panic!("expected InsufficientScope, got {other:?}"),
                },
            ),
            (
                StatusCode::FORBIDDEN,
                &[("x-accepted-oauth-scopes", "repo, workflow")],
                r#"{"message":"missing the workflow scope"}"#,
                |err| match err {
                    GitHubError::InsufficientScope { scopes } => {
                        assert!(scopes.contains("workflow"))
                    }
                    other => panic!("expected InsufficientScope, got {other:?}"),
                },
            ),
            (
                StatusCode::FORBIDDEN,
                scope_header,
                r#"{"message":"Resource not accessible by integration"}"#,
                |err| assert!(matches!(err, GitHubError::Api { .. })),
            ),
            (
                StatusCode::FORBIDDEN,
                &[("x-ratelimit-remaining", "0")],
                "",
                |err| assert!(matches!(err, GitHubError::RateLimited)),
            ),
            (StatusCode::TOO_MANY_REQUESTS, &[], "", |err| {
                assert!(matches!(err, GitHubError::RateLimited))
            }),
            (
                StatusCode::FORBIDDEN,
                &[],
                r#"{"message":"Resource protected"}"#,
                |err| match err {
                    GitHubError::Api { status, message } => {
                        assert_eq!(status, StatusCode::FORBIDDEN);
                        assert_eq!(message, "Resource protected");
                    }
                    other => panic!("expected Api, got {other:?}"),
                },
            ),
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                &[],
                "",
                |err| match err {
                    GitHubError::Api { status, message } => {
                        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                        assert_eq!(message, "no response body");
                    }
                    other => panic!("expected Api, got {other:?}"),
                },
            ),
        ];
        for (status, headers, body, check) in cases {
            check(classify_status(status, &headers_with(headers), body));
        }
        assert_eq!(api_message("plain text error"), "plain text error");
    }

    #[test]
    fn tag_segment_encodes_slash_but_keeps_dots() {
        assert_eq!(
            utf8_percent_encode("release/1.2.3", TAG_SEGMENT).to_string(),
            "release%2F1.2.3"
        );
        assert_eq!(
            utf8_percent_encode("v1.2.3", TAG_SEGMENT).to_string(),
            "v1.2.3"
        );
    }
}
