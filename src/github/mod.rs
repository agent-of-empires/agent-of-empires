//! GitHub client and auth foundation.
//!
//! One typed surface for talking to GitHub, shared by the TUI and the web
//! backend. Token resolution, the HTTP client, and the error taxonomy live
//! here so no other module shells out to `gh` or hits `api.github.com`
//! directly.
//!
//! See `docs/github-integration.md` for the token resolution order, the
//! per-failure hints, and what is deferred to follow-up issues.

pub mod auth;
pub mod client;
pub mod error;

pub use auth::{resolve_token, resolve_token_from_system, ResolvedToken, TokenSource};
pub use client::{GitHubClient, GitHubClientConfig, GitHubRelease};
pub use error::{GitHubAuthError, GitHubError, Result};

/// Default GitHub REST API base.
pub const DEFAULT_GITHUB_API_BASE: &str = "https://api.github.com";
/// User-Agent sent on every GitHub request (GitHub requires one).
pub const DEFAULT_USER_AGENT: &str = "agent-of-empires";
