//! Thin typed GitHub HTTP client built on the already-present `reqwest`.
//!
//! This is the single surface for talking to `api.github.com`. It owns the
//! base URL, the standard headers, and the mapping from HTTP responses to the
//! typed [`GitHubError`] taxonomy. Only unauthenticated public reads (such as
//! the update check) are wired up today via [`GitHubClient::unauthenticated`].

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT};
use reqwest::StatusCode;

/// Characters to percent-encode inside a single URL path segment (a release
/// tag). Encodes the path separator and other reserved/query characters while
/// leaving unreserved ones like `.`, `-`, `_` intact.
const TAG_SEGMENT: &AsciiSet = &CONTROLS
    .add(b'/')
    .add(b' ')
    .add(b'?')
    .add(b'#')
    .add(b'%')
    .add(b'&')
    .add(b'+');
/// Encode a search `q` value: encode everything non-alphanumeric (spaces,
/// `:`, etc.) so the qualifier syntax (`topic:aoe-plugin fork:false`) survives
/// into the query string intact.
const QUERY_VALUE: &AsciiSet = NON_ALPHANUMERIC;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::time::Duration;

use crate::github::error::{GitHubError, Result};

/// Configuration for constructing a [`GitHubClient`].
#[derive(Debug, Clone)]
pub struct GitHubClientConfig {
    /// API base, normally `https://api.github.com`. Overridable for tests.
    pub api_base: String,
    pub user_agent: String,
    pub timeout: Duration,
}

/// A configured GitHub HTTP client.
pub struct GitHubClient {
    http: reqwest::Client,
    api_base: String,
}

/// A GitHub release.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRelease {
    pub tag_name: String,
    #[serde(default)]
    pub body: Option<String>,
    pub published_at: Option<String>,
    /// Release assets (downloadable binaries). Empty for the update check; used
    /// by plugin install to fetch a release-binary worker.
    #[serde(default)]
    pub assets: Vec<GitHubAsset>,
}

/// A single downloadable asset attached to a release.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubAsset {
    pub name: String,
    pub browser_download_url: String,
}

/// A repository returned by the search API (the subset plugin discovery shows).
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubRepo {
    /// `owner/repo`.
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
    /// Client for public, unauthenticated requests.
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

    /// `GET /repos/{owner}/{repo}/releases?per_page={per_page}`
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

    /// `GET /repos/{owner}/{repo}/releases/latest`
    pub async fn latest_release(&self, owner: &str, repo: &str) -> Result<GitHubRelease> {
        let url = format!("{}/repos/{}/{}/releases/latest", self.api_base, owner, repo);
        self.send_json(self.http.get(url)).await
    }

    /// `GET /repos/{owner}/{repo}/releases/tags/{tag}`
    pub async fn release_by_tag(
        &self,
        owner: &str,
        repo: &str,
        tag: &str,
    ) -> Result<GitHubRelease> {
        // A tag like `release/1.2.3` is valid and must not split into extra path
        // segments, or the API 404s on a real tag.
        let tag = utf8_percent_encode(tag, TAG_SEGMENT);
        let url = format!(
            "{}/repos/{}/{}/releases/tags/{}",
            self.api_base, owner, repo, tag
        );
        self.send_json(self.http.get(url)).await
    }

    /// `GET /search/repositories?q={query}&sort=stars&order=desc`
    ///
    /// Unauthenticated search is heavily rate limited (about 10 requests per
    /// minute per IP); a 403/429 surfaces as [`GitHubError::RateLimited`] so the
    /// caller can say so plainly rather than reporting a generic API error.
    pub async fn search_repositories(&self, query: &str, per_page: u8) -> Result<Vec<GitHubRepo>> {
        let q = utf8_percent_encode(query, QUERY_VALUE);
        let url = format!(
            "{}/search/repositories?q={q}&sort=stars&order=desc&per_page={per_page}",
            self.api_base
        );
        let response: SearchReposResponse = self.send_json(self.http.get(url)).await?;
        Ok(response.items)
    }

    /// Fetch a single file's raw contents via the contents API (`Accept:
    /// application/vnd.github.raw`). Used to read a plugin's `aoe-plugin.toml`
    /// for the details view without cloning. `reference` pins the branch, tag,
    /// or commit (`?ref=`); `None` reads the repo's default branch.
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

/// Map a non-success HTTP response to the typed error with the right hint.
/// Pure and header-driven so it is unit-testable without a live API.
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

/// A 403 is only treated as a missing-scope failure when the response body
/// actually says so. GitHub sends `X-Accepted-OAuth-Scopes` on many responses,
/// including ones that are forbidden for unrelated reasons, so the header alone
/// is not evidence. The named scope still comes from that header. Precise
/// per-operation scope mapping is tracked in the scope-elevation follow-up.
fn missing_scope(headers: &HeaderMap, body: &str) -> Option<String> {
    if !body.to_lowercase().contains("scope") {
        return None;
    }
    accepted_scopes(headers)
}

/// The scopes GitHub says the endpoint accepts, taken from
/// `X-Accepted-OAuth-Scopes` so the hint names the real missing scope.
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
    fn unauthenticated_client_builds() {
        assert!(GitHubClient::unauthenticated(config()).is_ok());
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
    fn unauthorized_maps_to_unauthorized() {
        let err = classify_status(StatusCode::UNAUTHORIZED, &HeaderMap::new(), "");
        assert!(matches!(err, GitHubError::Unauthorized));
    }

    #[test]
    fn forbidden_with_scope_error_names_the_scope() {
        let headers = headers_with(&[("x-accepted-oauth-scopes", "repo")]);
        let err = classify_status(
            StatusCode::FORBIDDEN,
            &headers,
            r#"{"message":"requires the repo scope"}"#,
        );
        match err {
            GitHubError::InsufficientScope { scopes } => assert_eq!(scopes, "repo"),
            other => panic!("expected InsufficientScope, got {other:?}"),
        }
    }

    #[test]
    fn forbidden_with_workflow_scope_names_workflow() {
        let headers = headers_with(&[("x-accepted-oauth-scopes", "repo, workflow")]);
        let err = classify_status(
            StatusCode::FORBIDDEN,
            &headers,
            r#"{"message":"missing the workflow scope"}"#,
        );
        match err {
            GitHubError::InsufficientScope { scopes } => assert!(scopes.contains("workflow")),
            other => panic!("expected InsufficientScope, got {other:?}"),
        }
    }

    #[test]
    fn forbidden_with_scope_header_but_no_scope_message_is_api() {
        // The header alone is not evidence; many 403s carry it.
        let headers = headers_with(&[("x-accepted-oauth-scopes", "repo")]);
        let err = classify_status(
            StatusCode::FORBIDDEN,
            &headers,
            r#"{"message":"Resource not accessible by integration"}"#,
        );
        assert!(matches!(err, GitHubError::Api { .. }));
    }

    #[test]
    fn forbidden_rate_limited_maps_to_rate_limited() {
        let headers = headers_with(&[("x-ratelimit-remaining", "0")]);
        let err = classify_status(StatusCode::FORBIDDEN, &headers, "");
        assert!(matches!(err, GitHubError::RateLimited));
    }

    #[test]
    fn too_many_requests_maps_to_rate_limited() {
        let err = classify_status(StatusCode::TOO_MANY_REQUESTS, &HeaderMap::new(), "");
        assert!(matches!(err, GitHubError::RateLimited));
    }

    #[test]
    fn plain_forbidden_maps_to_api_error() {
        let err = classify_status(
            StatusCode::FORBIDDEN,
            &HeaderMap::new(),
            r#"{"message":"Resource protected"}"#,
        );
        match err {
            GitHubError::Api { status, message } => {
                assert_eq!(status, StatusCode::FORBIDDEN);
                assert_eq!(message, "Resource protected");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn not_found_carries_message() {
        let err = classify_status(
            StatusCode::NOT_FOUND,
            &HeaderMap::new(),
            r#"{"message":"Not Found"}"#,
        );
        match err {
            GitHubError::NotFound { resource } => assert_eq!(resource, "Not Found"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn server_error_maps_to_api() {
        let err = classify_status(StatusCode::INTERNAL_SERVER_ERROR, &HeaderMap::new(), "");
        match err {
            GitHubError::Api { status, message } => {
                assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
                assert_eq!(message, "no response body");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn api_message_falls_back_to_raw_body() {
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
