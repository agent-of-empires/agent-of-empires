//! `AcpError`, its classifiers, and the ACP wire errors aoe synthesizes.

use crate::acp::approvals::ApprovalDecision;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("agent spawn failed: {0}")]
    Spawn(String),
    /// Its own variant because POSIX cannot tell a missing cwd from a missing
    /// binary at the libc level, and the UI needs a different banner (#1089).
    #[error("project path no longer exists: {path}")]
    ProjectPathMissing { path: PathBuf },
    /// The handshake completed but the adapter failed `agent_compat`'s policy.
    /// The event_tx a failed `AcpClient::spawn` opened is never delivered, so
    /// the structured detail has to ride out of band on the error. Boxed to
    /// keep `AcpError` small on the Ok path.
    #[error("incompatible agent: {0}")]
    IncompatibleAgent(Box<IncompatibleAgentError>),
    /// Typed so the caller parks on `RateLimit` + `Stopped { rate_limited }`
    /// instead of burning the respawn budget against the same limit.
    #[error("agent is rate-limited during startup: {}", .0.status)]
    RateLimited(Box<crate::acp::state::RateLimitInfo>),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("protocol violation: {0}")]
    Protocol(String),
    #[error("agent process exited unexpectedly")]
    AgentExited,
    #[error("client task is not running")]
    NotRunning,
    #[error("no pending approval with that nonce")]
    UnknownNonce,
    #[error("agent did not offer a {0:?} option")]
    NoMatchingOption(ApprovalDecision),
    /// The pending elicitation survives so the client can correct and
    /// resubmit rather than the question aborting (#2100).
    #[error("submitted answer is invalid: {0}")]
    InvalidAnswer(String),
    /// A driven `session/new` reset failed (#2979); the conversation keeps
    /// its prior context.
    #[error("conversation reset failed: {0}")]
    ResetFailed(String),
}

/// The structured detail plus a formatted summary the supervisor mirrors into
/// `Event::AgentStartupError { message }` for callers that read only that.
#[derive(Debug)]
pub struct IncompatibleAgentError {
    pub detail: crate::acp::state::StartupErrorDetail,
    pub message: String,
}

impl std::fmt::Display for IncompatibleAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl AcpError {
    /// POSIX returns ENOENT for both "binary not on PATH" and "cwd is gone",
    /// so only a stat can tell them apart; it runs on the ENOENT branch alone.
    /// The supervisor pre-flights `cwd.exists()`, but the directory can vanish
    /// between that and exec, which would land the UI on the wrong banner
    /// (#1089).
    pub fn classify_spawn_error(
        err: std::io::Error,
        cwd: &std::path::Path,
        spawn_command: &str,
    ) -> Self {
        if err.kind() == std::io::ErrorKind::NotFound && !cwd.exists() {
            return AcpError::ProjectPathMissing {
                path: cwd.to_path_buf(),
            };
        }
        AcpError::Spawn(format!("{err} (command `{spawn_command}`)"))
    }

    /// Why a bare adapter name could not launch. A bundled copy aoe refuses
    /// needs a reinstall or a newer Node, not the adapter's install command.
    pub(super) fn unresolved_adapter_spawn_error(
        err: &std::io::Error,
        config: &super::SpawnConfig,
        reason: &super::resolve_command::Unresolved,
    ) -> Self {
        use super::resolve_command::Unresolved;
        let command = config.spec.command.as_str();
        match reason {
            Unresolved::NotBare | Unresolved::NotFound => {
                Self::missing_binary_spawn_error(err, command, configured_path_source(config))
            }
            Unresolved::StaleBundle => AcpError::Spawn(format!(
                "aoe's bundled ACP adapter `{command}` predates this aoe build, so it is \
                 refused until it is reinstalled. Reinstall it with: \
                 aoe acp doctor --fix --adapter {command}"
            )),
            Unresolved::NodeTooOld(found) => {
                let too_old = crate::acp::adapters::AdapterError::NodeTooOld {
                    adapter: command.to_string(),
                    major: crate::acp::node::MIN_NODE_MAJOR,
                    minor: crate::acp::node::MIN_NODE_MINOR,
                    found: found.clone(),
                };
                AcpError::Spawn(format!(
                    "{too_old}. Upgrade Node, or point AOE_ACP_NODE at a newer one, then retry"
                ))
            }
        }
    }

    /// Appends the exact install command for a known ACP adapter so the web
    /// banner shows a copyable line instead of a guess (#2109). A configured
    /// PATH replaces the daemon's for the agent, so the advice names it.
    fn missing_binary_spawn_error(
        err: &std::io::Error,
        command: &str,
        configured_path: Option<&str>,
    ) -> Self {
        let hint = crate::acp::install_hints::install_hint_for(command)
            .map(|cmd| format!(". Install with: {cmd}"))
            .unwrap_or_default();
        let place = match configured_path {
            Some(source) => format!(
                "on the PATH {source} sets for the agent, which replaces the daemon's; \
                 add the directory that holds it to that PATH, or install it there"
            ),
            None => format!(
                "on the daemon's PATH or in any known node-manager bin dir; install it \
                 where the daemon can see it, or restart `aoe serve` from a shell where \
                 `which {command}` resolves"
            ),
        };
        AcpError::Spawn(format!(
            "{err} (binary `{command}` not found {place}){hint}"
        ))
    }
}

/// What set the PATH the agent is launched with, when it isn't the daemon's:
/// the configured `environment`, or a `before_session` hook overlaying it.
fn configured_path_source(config: &super::SpawnConfig) -> Option<&'static str> {
    fn last_path<'a>(
        pairs: impl DoubleEndedIterator<Item = &'a (String, String)>,
    ) -> Option<&'a String> {
        pairs
            .rev()
            .find(|(key, _)| key == "PATH")
            .map(|(_, value)| value)
    }
    let effective = last_path(
        config
            .host_environment
            .iter()
            .chain(&config.provider_routing),
    )?;
    Some(
        if last_path(config.base_host_environment.iter()) == Some(effective) {
            "`environment`"
        } else {
            "a `before_session` hook"
        },
    )
}

/// Build a crate error for a control v3 handshake failure.
pub(super) fn acp_internal_error(message: String) -> agent_client_protocol::Error {
    let mut err = agent_client_protocol::Error::internal_error();
    err.message = message;
    err
}

/// Preserves `code` / `message` / `data` from the runner's `HandshakeFailed`
/// so `AgentStartupError` surfaces the same `data.details` remediation the
/// byte-relay handshake did. A malformed object falls back to internal error.
pub(super) fn acp_error_from_value(error: serde_json::Value) -> agent_client_protocol::Error {
    serde_json::from_value(error.clone())
        .unwrap_or_else(|_| acp_internal_error(format!("runner handshake failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spawn_message(error: AcpError) -> String {
        match error {
            AcpError::Spawn(msg) => msg,
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    /// Only ENOENT against a vanished cwd becomes `ProjectPathMissing`; the
    /// pre-flight can race, and a bare ENOENT would land the UI on the
    /// install-the-adapter banner instead.
    #[test]
    fn classify_spawn_error_separates_a_missing_cwd_from_a_missing_binary() {
        let missing =
            std::env::temp_dir().join(format!("aoe-test-classify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let enoent = || std::io::Error::from(std::io::ErrorKind::NotFound);

        match AcpError::classify_spawn_error(enoent(), &missing, "/bin/true") {
            AcpError::ProjectPathMissing { path } => assert_eq!(path, missing),
            other => panic!("expected ProjectPathMissing, got {other:?}"),
        }
        let cwd = std::env::temp_dir();
        let msg = spawn_message(AcpError::classify_spawn_error(
            enoent(),
            &cwd,
            "/nonexistent/bin/foo",
        ));
        assert!(msg.contains("/nonexistent/bin/foo"), "{msg}");
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        spawn_message(AcpError::classify_spawn_error(denied, &cwd, "/bin/true"));
    }

    /// A configured PATH replaces the daemon's for the agent, so the
    /// missing-adapter advice names what set it instead of the daemon's PATH.
    #[test]
    fn missing_adapter_advice_names_the_path_the_agent_searched() {
        use super::super::resolve_command::Unresolved;
        let path = |dir: &str| vec![("PATH".to_string(), dir.to_string())];
        let daemon = "restart `aoe serve`";
        let environment = "on the PATH `environment` sets for the agent";
        let hook = "on the PATH a `before_session` hook sets for the agent";
        // (configured `environment`, after the hook overlay, provider routing, expected, unexpected)
        let cases = [
            (vec![], vec![], vec![], daemon, "sets for the agent"),
            (
                path("/opt/acp"),
                path("/opt/acp"),
                vec![],
                environment,
                daemon,
            ),
            (vec![], path("/hook/bin"), vec![], hook, daemon),
            // A hook overlaying the configured PATH is what the agent searches.
            (
                path("/opt/acp"),
                path("/hook/bin"),
                vec![],
                hook,
                environment,
            ),
            // Routing carries no PATH, so it never hides the configured one.
            (
                path("/opt/acp"),
                path("/opt/acp"),
                vec![("CLAUDE_CODE_USE_BEDROCK".to_string(), "1".to_string())],
                environment,
                daemon,
            ),
        ];
        for (base, host, routing, expected, unexpected) in cases {
            let mut config =
                super::super::test_helpers::env_test_spawn_config(std::env::temp_dir());
            config.spec.command = "codex-acp".into();
            config.base_host_environment = base;
            config.host_environment = host;
            config.provider_routing = routing;
            let msg = spawn_message(AcpError::unresolved_adapter_spawn_error(
                &std::io::Error::from(std::io::ErrorKind::NotFound),
                &config,
                &Unresolved::NotFound,
            ));
            let label = format!(
                "base={:?} host={:?} routing={:?}",
                config.base_host_environment, config.host_environment, config.provider_routing
            );
            assert!(msg.contains(expected), "{label}: {msg}");
            assert!(!msg.contains(unexpected), "{label}: {msg}");
            assert!(
                msg.contains("binary `codex-acp` not found"),
                "{label}: {msg}"
            );
            assert!(
                msg.contains("Install with: npm install -g @agentclientprotocol/codex-acp@latest"),
                "{label}: {msg}"
            );
        }
    }
}
