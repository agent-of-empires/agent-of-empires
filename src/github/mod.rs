//! GitHub client foundation.
//!
//! One typed surface for talking to GitHub, shared by the TUI and the web
//! backend. The HTTP client and the error taxonomy live here so no other
//! module hits `api.github.com` directly.
//!
//! See `docs/github-integration.md` for the per-failure hints.

pub mod client;
pub mod error;

pub use client::{GitHubClient, GitHubClientConfig, GitHubRelease, GitHubSearchRepo};
pub use error::{GitHubError, Result};

/// Default GitHub REST API base.
pub const DEFAULT_GITHUB_API_BASE: &str = "https://api.github.com";
/// User-Agent sent on every GitHub request (GitHub requires one).
pub const DEFAULT_USER_AGENT: &str = "agent-of-empires";
