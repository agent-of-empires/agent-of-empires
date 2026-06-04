//! GitHub token resolution.
//!
//! `gh` is an optional token source, never a hard dependency and never a
//! per-call shell-out. The resolver finds a token once, in a fixed order, and
//! the client then sends it as `Authorization: Bearer <token>`:
//!
//! 1. `GITHUB_TOKEN` / `GH_TOKEN` environment variable.
//! 2. `gh auth token`, only when `gh` is installed and authenticated.
//! 3. Device-flow login as the no-`gh` fallback (deferred, see the GitHub
//!    integration docs for the tracking issue).
//!
//! The environment is abstracted behind [`TokenEnvironment`] so the resolution
//! order and per-case hint selection are unit-testable without touching real
//! process state or requiring `gh` in CI.

use crate::github::error::GitHubAuthError;
use std::process::Command;

/// Whether a client must carry a token or may run unauthenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Public, unauthenticated requests (for example the update check).
    None,
    /// Resolve a token and attach it; fail with a typed hint if none is found.
    Required,
}

/// Where a resolved token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenSource {
    EnvGithubToken,
    EnvGhToken,
    GhCli,
}

/// A resolved token plus its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedToken {
    pub token: String,
    pub source: TokenSource,
}

/// Result of invoking `gh auth token`.
pub struct GhTokenOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Abstraction over the process environment, so token resolution is testable.
pub trait TokenEnvironment {
    fn env_var(&self, key: &str) -> Option<String>;
    fn gh_available(&self) -> bool;
    fn gh_auth_token(&self) -> std::io::Result<GhTokenOutput>;
}

/// Production environment backed by real env vars and the `gh` binary.
pub struct SystemEnvironment;

impl TokenEnvironment for SystemEnvironment {
    fn env_var(&self, key: &str) -> Option<String> {
        std::env::var(key).ok()
    }

    fn gh_available(&self) -> bool {
        which::which("gh").is_ok()
    }

    fn gh_auth_token(&self) -> std::io::Result<GhTokenOutput> {
        let output = Command::new("gh").args(["auth", "token"]).output()?;
        Ok(GhTokenOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Resolve a GitHub token in the fixed order, returning a typed error whose
/// hint matches the exact failure when no token is available.
pub fn resolve_token<E: TokenEnvironment>(
    env: &E,
) -> std::result::Result<ResolvedToken, GitHubAuthError> {
    if let Some(token) = env
        .env_var("GITHUB_TOKEN")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        return Ok(ResolvedToken {
            token,
            source: TokenSource::EnvGithubToken,
        });
    }
    if let Some(token) = env
        .env_var("GH_TOKEN")
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        return Ok(ResolvedToken {
            token,
            source: TokenSource::EnvGhToken,
        });
    }

    if !env.gh_available() {
        return Err(GitHubAuthError::NoTokenNoGh);
    }

    match env.gh_auth_token() {
        Ok(output) if output.success => {
            let token = output.stdout.trim();
            if token.is_empty() {
                Err(GitHubAuthError::GhReturnedEmptyToken)
            } else {
                Ok(ResolvedToken {
                    token: token.to_string(),
                    source: TokenSource::GhCli,
                })
            }
        }
        // A non-zero exit with the canonical "no oauth token" message (or no
        // stderr at all) means the user simply is not signed in. Any other
        // stderr is a real gh failure and must not be mislabeled as that.
        Ok(output) => {
            let stderr = output.stderr.trim();
            if stderr.is_empty() || stderr.to_lowercase().contains("no oauth token") {
                Err(GitHubAuthError::GhNotAuthenticated)
            } else {
                Err(GitHubAuthError::GhCommandFailed(stderr.to_string()))
            }
        }
        Err(e) => Err(GitHubAuthError::GhCommandFailed(e.to_string())),
    }
}

/// Resolve a token from the real process environment.
pub fn resolve_token_from_system() -> std::result::Result<ResolvedToken, GitHubAuthError> {
    resolve_token(&SystemEnvironment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeEnv {
        github_token: Option<String>,
        gh_token: Option<String>,
        gh_available: bool,
        gh_result: Option<std::io::Result<GhTokenOutput>>,
    }

    impl TokenEnvironment for FakeEnv {
        fn env_var(&self, key: &str) -> Option<String> {
            match key {
                "GITHUB_TOKEN" => self.github_token.clone(),
                "GH_TOKEN" => self.gh_token.clone(),
                _ => None,
            }
        }
        fn gh_available(&self) -> bool {
            self.gh_available
        }
        fn gh_auth_token(&self) -> std::io::Result<GhTokenOutput> {
            match &self.gh_result {
                Some(Ok(o)) => Ok(GhTokenOutput {
                    success: o.success,
                    stdout: o.stdout.clone(),
                    stderr: o.stderr.clone(),
                }),
                Some(Err(e)) => Err(std::io::Error::new(e.kind(), e.to_string())),
                None => panic!("gh_auth_token called unexpectedly"),
            }
        }
    }

    fn ok_gh(stdout: &str) -> Option<std::io::Result<GhTokenOutput>> {
        Some(Ok(GhTokenOutput {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        }))
    }

    fn failed_gh(stderr: &str) -> Option<std::io::Result<GhTokenOutput>> {
        Some(Ok(GhTokenOutput {
            success: false,
            stdout: String::new(),
            stderr: stderr.to_string(),
        }))
    }

    #[test]
    fn github_token_env_wins_without_touching_gh() {
        let env = FakeEnv {
            github_token: Some("env-tok".to_string()),
            gh_available: true,
            gh_result: None, // would panic if gh were consulted
            ..Default::default()
        };
        let resolved = resolve_token(&env).unwrap();
        assert_eq!(resolved.token, "env-tok");
        assert_eq!(resolved.source, TokenSource::EnvGithubToken);
    }

    #[test]
    fn gh_token_env_used_when_github_token_absent() {
        let env = FakeEnv {
            gh_token: Some("gh-env-tok".to_string()),
            gh_available: true,
            gh_result: None,
            ..Default::default()
        };
        let resolved = resolve_token(&env).unwrap();
        assert_eq!(resolved.token, "gh-env-tok");
        assert_eq!(resolved.source, TokenSource::EnvGhToken);
    }

    #[test]
    fn empty_env_token_is_skipped() {
        let env = FakeEnv {
            github_token: Some("   ".to_string()),
            gh_available: true,
            gh_result: ok_gh("cli-tok\n"),
            ..Default::default()
        };
        let resolved = resolve_token(&env).unwrap();
        assert_eq!(resolved.token, "cli-tok");
        assert_eq!(resolved.source, TokenSource::GhCli);
    }

    #[test]
    fn gh_authenticated_reuses_token_no_prompt() {
        let env = FakeEnv {
            gh_available: true,
            gh_result: ok_gh("gho_abc123\n"),
            ..Default::default()
        };
        let resolved = resolve_token(&env).unwrap();
        assert_eq!(resolved.token, "gho_abc123");
        assert_eq!(resolved.source, TokenSource::GhCli);
    }

    #[test]
    fn no_token_and_no_gh_yields_install_or_set_token_hint() {
        let env = FakeEnv {
            gh_available: false,
            ..Default::default()
        };
        let err = resolve_token(&env).unwrap_err();
        assert!(matches!(err, GitHubAuthError::NoTokenNoGh));
        let msg = err.to_string();
        assert!(msg.contains("GITHUB_TOKEN"));
        assert!(msg.contains("install the GitHub CLI") || msg.contains("brew install gh"));
    }

    #[test]
    fn gh_installed_but_not_authenticated_says_login_not_install() {
        let env = FakeEnv {
            gh_available: true,
            gh_result: failed_gh("no oauth token found for github.com"),
            ..Default::default()
        };
        let err = resolve_token(&env).unwrap_err();
        assert!(matches!(err, GitHubAuthError::GhNotAuthenticated));
        let msg = err.to_string();
        assert!(msg.contains("gh auth login"));
        assert!(!msg.contains("brew install") && !msg.contains("install the GitHub CLI"));
    }

    #[test]
    fn gh_nonzero_with_empty_stderr_is_not_authenticated() {
        let env = FakeEnv {
            gh_available: true,
            gh_result: failed_gh(""),
            ..Default::default()
        };
        assert!(matches!(
            resolve_token(&env).unwrap_err(),
            GitHubAuthError::GhNotAuthenticated
        ));
    }

    #[test]
    fn gh_nonzero_with_unexpected_stderr_is_command_failed() {
        let env = FakeEnv {
            gh_available: true,
            gh_result: failed_gh("error connecting to api.github.com"),
            ..Default::default()
        };
        match resolve_token(&env).unwrap_err() {
            GitHubAuthError::GhCommandFailed(msg) => assert!(msg.contains("connecting")),
            other => panic!("expected GhCommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn env_token_is_trimmed() {
        let env = FakeEnv {
            github_token: Some("  gho_padded  ".to_string()),
            ..Default::default()
        };
        assert_eq!(resolve_token(&env).unwrap().token, "gho_padded");
    }

    #[test]
    fn gh_success_with_empty_output_is_empty_token_error() {
        let env = FakeEnv {
            gh_available: true,
            gh_result: ok_gh("\n"),
            ..Default::default()
        };
        let err = resolve_token(&env).unwrap_err();
        assert!(matches!(err, GitHubAuthError::GhReturnedEmptyToken));
    }

    #[test]
    fn gh_command_failure_is_reported() {
        let env = FakeEnv {
            gh_available: true,
            gh_result: Some(Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "boom",
            ))),
            ..Default::default()
        };
        let err = resolve_token(&env).unwrap_err();
        assert!(matches!(err, GitHubAuthError::GhCommandFailed(_)));
    }
}
