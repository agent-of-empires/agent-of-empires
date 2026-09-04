//! The `initialize` request aoe sends and the wait for the agent's reply.

use agent_client_protocol::schema::v1::{
    ClientCapabilities, ElicitationCapabilities, ElicitationFormCapabilities,
    FileSystemCapabilities, Implementation, InitializeRequest,
};
use agent_client_protocol::schema::ProtocolVersion;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};
use tracing::warn;

use super::errors::AcpError;

/// Whether to issue ACP `session/fork` on this connect: only when a fork was
/// requested AND the agent advertised the (unstable) fork capability. Falls
/// back to the normal new/load handshake otherwise (which, for a fork that
/// can't run, surfaces as an empty new session rather than corrupting the
/// parent).
pub(crate) fn should_fork(fork_from: Option<&str>, agent_advertises_fork: bool) -> bool {
    fork_from.is_some_and(|s| !s.is_empty()) && agent_advertises_fork
}

/// Build the ACP `initialize` request AoE sends to every agent adapter.
/// `client_info` is mandatory here: strict agent backends (Mistral Vibe's
/// `vibe-acp`) reject an initialize whose `client_name`/`client_version` are
/// empty strings, which is what omitting it serializes to. See issue #2767.
pub(super) fn build_initialize_request() -> InitializeRequest {
    let capabilities = ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(true))
        .terminal(true)
        // Advertise form-mode elicitation so claude-agent-acp
        // (>=0.44) re-enables AskUserQuestion and routes it to us as
        // an `elicitation/create` request. Without this the adapter
        // unconditionally blacklists the tool. See handle_elicitation_request.
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
    InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(capabilities)
        .client_info(
            Implementation::new("agent-of-empires", env!("CARGO_PKG_VERSION"))
                .title("Agent of Empires"),
        )
}

/// Wait for the connection task to finish the ACP handshake (or fail).
/// Bounds the wait so a wedged agent (the classic `npx -y` first-run
/// download stall) returns a clear typed error instead of leaving the
/// supervisor parked indefinitely. Also watches for early child exit
/// and surfaces stderr in the message so callers see why it died.
///
/// `install_binary` is the binary name from `AgentSpec.command` so the
/// timeout message points users at the right install command for the
/// specific agent (codex-acp / opencode / gemini, not always
/// claude-agent-acp).
pub(super) async fn wait_for_handshake(
    session_label: &str,
    ready_rx: oneshot::Receiver<Result<(), AcpError>>,
    child: Option<&Arc<Mutex<tokio::process::Child>>>,
    install_binary: &str,
) -> Result<(), AcpError> {
    let timeout = std::time::Duration::from_secs(30);
    match tokio::time::timeout(timeout, ready_rx).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(e))) => {
            warn!(target: "acp.protocol", session = %session_label, "ACP handshake failed: {e}");
            collect_child_failure(child).await;
            Err(e)
        }
        Ok(Err(_canceled)) => Err(AcpError::Spawn(
            "ACP connection task ended before completing the initialize handshake".into(),
        )),
        Err(_elapsed) => {
            warn!(
                target: "acp.protocol",
                session = %session_label,
                "ACP handshake timed out after {}s",
                timeout.as_secs()
            );
            if let Some(child) = child {
                let mut guard = child.lock().await;
                let _ = guard.kill().await;
            }
            let install_hint = crate::acp::install_hints::install_hint_for(install_binary)
                .unwrap_or("install the adapter for the configured agent and re-run");
            Err(AcpError::Spawn(format!(
                "agent did not complete the ACP initialize handshake within {}s. \
                 Common causes: the adapter is still downloading on first run, \
                 or the configured agent command isn't a real ACP server. \
                 Try `{}` and re-run.",
                timeout.as_secs(),
                install_hint
            )))
        }
    }
}

pub(super) async fn collect_child_failure(child: Option<&Arc<Mutex<tokio::process::Child>>>) {
    if let Some(child) = child {
        let mut guard = child.lock().await;
        if let Ok(Some(status)) = guard.try_wait() {
            warn!(target: "acp.protocol", "agent process exited early: status={status}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_request_carries_non_empty_client_info() {
        // Regression for #2767: strict agent backends (Mistral Vibe) reject an
        // initialize whose client_name/client_version are empty. Our request
        // must always send a populated client_info.
        let req = build_initialize_request();
        let info = req.client_info.expect("client_info must be set");
        assert_eq!(info.name, "agent-of-empires");
        assert!(!info.version.is_empty());
    }

    #[test]
    fn should_fork_requires_capability_and_parent() {
        assert!(should_fork(Some("parent"), true));
        assert!(!should_fork(Some("parent"), false)); // adapter can't fork (e.g. aoe-agent)
        assert!(!should_fork(None, true));
        assert!(!should_fork(Some(""), true));
    }

    /// Pin the ACP fork wire shape our production path reads, against the
    /// `agent_client_protocol` serde derives. `should_fork` keys off
    /// `agent_capabilities.session_capabilities.fork.is_some()`, and the fork
    /// response is read via `resp.session_id`. If upstream renames either key
    /// (e.g. `fork` -> `session_fork`, or `sessionId` casing), these
    /// deserializations flip: the capability would read absent (silent
    /// `session/new` downgrade in production) or the response would fail to
    /// parse. The fake agent (`web/tests/helpers/fakeAcpAgent.mjs`) sends these
    /// exact keys, so pinning them here catches an upstream drift that the fake
    /// would otherwise mask. See PR review.
    #[test]
    fn acp_fork_capability_and_response_wire_keys_are_stable() {
        use agent_client_protocol::schema::v1::{ForkSessionResponse, SessionCapabilities};

        // The fork capability is advertised as a `"fork": {}` object nested in
        // the session capabilities the agent returns from `initialize`.
        let caps: SessionCapabilities =
            serde_json::from_value(serde_json::json!({ "fork": {} })).expect("caps parse");
        assert!(
            caps.fork.is_some(),
            "the `fork` capability key must deserialize into SessionCapabilities.fork"
        );
        // Absent/`null` fork must read as not-forkable (the resume-only shape).
        let no_fork: SessionCapabilities =
            serde_json::from_value(serde_json::json!({})).expect("empty caps parse");
        assert!(no_fork.fork.is_none());

        // The fork response identifies the child session under `sessionId`.
        let resp: ForkSessionResponse =
            serde_json::from_value(serde_json::json!({ "sessionId": "child-123" }))
                .expect("fork response parse");
        assert_eq!(resp.session_id.0.as_ref(), "child-123");
    }
}
