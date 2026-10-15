//! GitHub client and error taxonomy; no other module calls `api.github.com` directly.

pub mod client;
pub mod error;

pub use client::{
    GitHubAsset, GitHubClient, GitHubClientConfig, GitHubCompare, GitHubCompareCommit,
    GitHubRelease, GitHubRepo,
};
pub use error::{GitHubError, Result};

pub const DEFAULT_GITHUB_API_BASE: &str = "https://api.github.com";
pub const DEFAULT_USER_AGENT: &str = "agent-of-empires";
