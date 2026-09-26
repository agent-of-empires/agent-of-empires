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

/// A fork needs both a requested parent and the agent's fork capability;
/// otherwise the normal new/load handshake runs, which surfaces an unfulfilled
/// fork as an empty session rather than corrupting the parent.
pub(crate) fn should_fork(fork_from: Option<&str>, agent_advertises_fork: bool) -> bool {
    fork_from.is_some_and(|s| !s.is_empty()) && agent_advertises_fork
}

/// `client_info` is mandatory: a strict backend rejects the empty strings that
/// omitting it serializes to (#2767).
pub(super) fn build_initialize_request() -> InitializeRequest {
    let capabilities = ClientCapabilities::new()
        .fs(FileSystemCapabilities::new()
            .read_text_file(true)
            .write_text_file(true))
        .terminal(true)
        // Form-mode elicitation re-enables claude-agent-acp's AskUserQuestion,
        // which it otherwise blacklists, and routes it to
        // `handle_elicitation_request`.
        .elicitation(ElicitationCapabilities::new().form(ElicitationFormCapabilities::new()));
    InitializeRequest::new(ProtocolVersion::V1)
        .client_capabilities(capabilities)
        .client_info(
            Implementation::new("agent-of-empires", env!("CARGO_PKG_VERSION"))
                .title("Agent of Empires"),
        )
}

/// Bounded so a wedged agent (the `npx -y` first-run download stall) returns a
/// typed error instead of parking the supervisor. `install_binary` points the
/// timeout message at the configured agent's own install command.
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

    /// #2767: a strict backend rejects an empty client_name/client_version.
    #[test]
    fn handshake_wire_shapes() {
        let req = build_initialize_request();
        let info = req.client_info.expect("client_info must be set");
        assert_eq!(info.name, "agent-of-empires");
        assert!(!info.version.is_empty());

        // Pins the fork wire keys against upstream serde drift. An upstream
        // rename would make the capability read absent (a silent `session/new`
        // downgrade) or fail the response parse, and the fake agent sends these
        // exact keys, so it would otherwise mask the drift.
        {
            use agent_client_protocol::schema::v1::{ForkSessionResponse, SessionCapabilities};

            let caps: SessionCapabilities =
                serde_json::from_value(serde_json::json!({ "fork": {} })).expect("caps parse");
            assert!(caps.fork.is_some());
            // Absent fork must read as not-forkable, the resume-only shape.
            let no_fork: SessionCapabilities =
                serde_json::from_value(serde_json::json!({})).expect("empty caps parse");
            assert!(no_fork.fork.is_none());

            let resp: ForkSessionResponse =
                serde_json::from_value(serde_json::json!({ "sessionId": "child-123" }))
                    .expect("fork response parse");
            assert_eq!(resp.session_id.0.as_ref(), "child-123");
        }
    }
}
