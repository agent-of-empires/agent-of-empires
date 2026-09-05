//! `AcpError`, its classifiers, and the ACP wire errors aoe synthesizes.

use crate::acp::approvals::ApprovalDecision;
use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AcpError {
    #[error("agent spawn failed: {0}")]
    Spawn(String),
    /// The session's working directory does not exist on disk. Distinct
    /// from a generic spawn ENOENT (which on POSIX is indistinguishable
    /// at the libc level between missing binary, missing interpreter, and
    /// missing cwd). Surfaced as its own variant so the UI can render a
    /// targeted remediation banner instead of the default "install the
    /// adapter" copy. See issue #1089.
    #[error("project path no longer exists: {path}")]
    ProjectPathMissing { path: PathBuf },
    /// The ACP `initialize` handshake completed but the adapter failed
    /// the per-adapter compatibility policy (see
    /// `src/acp/agent_compat.rs`). Carries the structured detail so
    /// the supervisor can publish a matching `Event::IncompatibleAgent`
    /// through the broadcast sink (the in-process event_tx the failed
    /// `AcpClient::spawn` opened is never delivered, so the structured
    /// payload has to ride out of band on the typed error). The payload
    /// is boxed to keep `AcpError` small on the Ok hot path (clippy's
    /// `result_large_err`).
    #[error("incompatible agent: {0}")]
    IncompatibleAgent(Box<IncompatibleAgentError>),
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
    /// A submitted elicitation answer failed server-side validation. The
    /// pending elicitation is left intact so the client can correct the
    /// answer and resubmit (rather than the question aborting). See #2100.
    #[error("submitted answer is invalid: {0}")]
    InvalidAnswer(String),
    /// A driven conversation reset (`session/new` on the live worker for
    /// a clear command with no native adapter reset, #2979) failed; the
    /// conversation keeps its prior context.
    #[error("conversation reset failed: {0}")]
    ResetFailed(String),
}

/// Boxed payload for `AcpError::IncompatibleAgent`. Carries the
/// structured `StartupErrorDetail` plus a pre-formatted free-form
/// summary the supervisor mirrors into the legacy
/// `Event::AgentStartupError { message }` channel for status-derivation
/// callers that don't yet read the structured detail.
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
    /// Inspect a `std::io::Error` returned by `Command::spawn` against
    /// the spawn site's cwd + (resolved) command. POSIX returns ENOENT
    /// for both "binary not on PATH" and "cwd does not exist", so the
    /// disambiguation has to happen via filesystem stat. Stats only on
    /// the ENOENT branch to keep the hot path free.
    ///
    /// Belt-and-suspenders for the cwd-missing case: the supervisor
    /// pre-flights `cwd.exists()` before spawning, but the directory
    /// can race-disappear between pre-flight and exec. Without this
    /// classifier the bare ENOENT bubbles up as a generic spawn error
    /// and the UI lands on the wrong remediation banner. See #1089.
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

    /// Build the enriched "binary not found" spawn error for a bare-command
    /// ENOENT (no PATH resolution, cwd present). Appends the exact install
    /// command when the binary is a known ACP adapter so the web banner can
    /// show a copyable line instead of making the user guess. See #2109.
    pub(super) fn missing_binary_spawn_error(err: &std::io::Error, command: &str) -> Self {
        let hint = crate::acp::install_hints::install_hint_for(command)
            .map(|cmd| format!(". Install with: {cmd}"))
            .unwrap_or_default();
        AcpError::Spawn(format!(
            "{err} (binary `{command}` not found on the daemon's PATH or in \
             any known node-manager bin dir; install it where the daemon can \
             see it, or restart `aoe serve` from a shell where `which \
             {command}` resolves){hint}"
        ))
    }
}

/// Build a crate `Error` carrying `message`, for the v2 control path where
/// the daemon's handshake round-trip fails at the control channel rather
/// than at a crate `send_request`.
pub(super) fn acp_internal_error(message: String) -> agent_client_protocol::Error {
    let mut err = agent_client_protocol::Error::internal_error();
    err.message = message;
    err
}

/// Reconstruct a crate `Error` from the raw JSON-RPC error object the
/// runner forwarded in `HandshakeFailed`. Preserves `code` / `message` /
/// `data` so the downstream `AgentStartupError` surfaces the same
/// `data.details` remediation the byte-relay handshake did; falls back to a
/// generic internal error if the object is malformed.
pub(super) fn acp_error_from_value(error: serde_json::Value) -> agent_client_protocol::Error {
    serde_json::from_value(error.clone())
        .unwrap_or_else(|_| acp_internal_error(format!("runner handshake failed: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Belt-and-suspenders: even if the pre-flight raced (cwd vanishes
    /// between `cwd.exists()` and `Command::spawn`), the classifier turns
    /// the raw ENOENT into `ProjectPathMissing` rather than the generic
    /// install-the-adapter message.
    #[test]
    fn classify_spawn_error_routes_missing_cwd_to_project_path_missing() {
        let missing =
            std::env::temp_dir().join(format!("aoe-test-classify-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        match AcpError::classify_spawn_error(io_err, &missing, "/bin/true") {
            AcpError::ProjectPathMissing { path } => assert_eq!(path, missing),
            other => panic!("expected ProjectPathMissing, got {other:?}"),
        }
    }

    #[test]
    fn classify_spawn_error_keeps_spawn_when_cwd_exists() {
        let cwd = std::env::temp_dir();
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        match AcpError::classify_spawn_error(io_err, &cwd, "/nonexistent/bin/foo") {
            AcpError::Spawn(msg) => {
                assert!(
                    msg.contains("/nonexistent/bin/foo"),
                    "spawn message should echo command: {msg}"
                );
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn missing_binary_spawn_error_appends_install_hint_for_known_agent() {
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        match AcpError::missing_binary_spawn_error(&io_err, "codex-acp") {
            AcpError::Spawn(msg) => {
                assert!(msg.contains("codex-acp"), "should echo the binary: {msg}");
                assert!(
                    msg.contains(
                        "Install with: npm install -g @agentclientprotocol/codex-acp@latest"
                    ),
                    "should append the exact install command: {msg}"
                );
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn missing_binary_spawn_error_omits_hint_for_unknown_binary() {
        let io_err = std::io::Error::from(std::io::ErrorKind::NotFound);
        match AcpError::missing_binary_spawn_error(&io_err, "totally-unknown-bin") {
            AcpError::Spawn(msg) => {
                assert!(
                    msg.contains("totally-unknown-bin"),
                    "should echo binary: {msg}"
                );
                assert!(!msg.contains("Install with:"), "no hint for unknown: {msg}");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn classify_spawn_error_passes_through_non_enoent() {
        let cwd = std::env::temp_dir();
        let io_err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        match AcpError::classify_spawn_error(io_err, &cwd, "/bin/true") {
            AcpError::Spawn(_) => {}
            other => panic!("expected Spawn for non-ENOENT, got {other:?}"),
        }
    }
}
