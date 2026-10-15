//! `session/delete`: its wire form, the outcomes aoe distinguishes, and
//! the dispatch that sends it.

use agent_client_protocol::schema::v1::ErrorCode;
use agent_client_protocol::{Agent, ConnectionTo, JsonRpcRequest, JsonRpcResponse};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use super::spawn::scrub_stderr_secrets;

/// Wire request not yet exposed by the Rust ACP schema. Unsupported adapters
/// return `method_not_found`, after which normal process cleanup continues.
#[derive(Debug, Clone, Serialize, Deserialize, JsonRpcRequest)]
#[request(method = "session/delete", response = DeleteSessionResponse)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeleteSessionRequest {
    session_id: agent_client_protocol::schema::v1::SessionId,
    /// Optional in the TS schema, but sent so a strict
    /// `unstable_session_delete` validator cannot answer `-32602`.
    #[serde(rename = "_meta")]
    meta: serde_json::Value,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, JsonRpcResponse)]
pub(super) struct DeleteSessionResponse {}

/// Outcome of an experimental `session/delete` call. Every variant is
/// non-fatal; the supervisor logs and proceeds to SIGTERM regardless.
#[derive(Debug)]
pub enum DeleteSessionOutcome {
    /// Adapter accepted the request and returned a successful response.
    Deleted,
    /// `-32601`, expected from any adapter that does not advertise
    /// `sessionCapabilities.delete`.
    UnsupportedMethod,
    /// The bounded wait elapsed before the adapter responded.
    TimedOut,
    /// Any other failure, carrying the reason for the log line.
    Failed(String),
}

/// Adapter error messages reach `debug.log` verbatim, so cap them. 256 leaves
/// the error code and a useful slice of the message.
pub(super) const ACP_DELETE_ERROR_MSG_MAX: usize = 256;

/// A succeeding adapter completes in tens of ms; this bounds a wedged one.
/// Deliberately not an `AcpConfig` field: best-effort cleanup with no
/// operator-visible failure mode.
pub(super) const ACP_SESSION_DELETE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2);

/// Dispatched from the connect task's cmd_rx arm. The wait is spawned so the
/// select arm keeps polling other commands, and bounded so a wedged adapter
/// still resolves the oneshot before the caller's outer guard (#1404).
pub(super) fn handle_delete_session_cmd(
    connection: &ConnectionTo<Agent>,
    acp_session_id: String,
    respond_to: oneshot::Sender<DeleteSessionOutcome>,
) {
    let target = agent_client_protocol::schema::v1::SessionId::from(acp_session_id);
    // `block_task()` is safe to await from a spawned task: it waits on the
    // per-request oneshot the main connection task feeds, so the dispatch loop
    // keeps running while this future is parked.
    let sent = connection.send_request(DeleteSessionRequest {
        session_id: target,
        meta: serde_json::Value::Object(serde_json::Map::new()),
    });
    tokio::spawn(async move {
        let outcome =
            match tokio::time::timeout(ACP_SESSION_DELETE_TIMEOUT, sent.block_task()).await {
                Ok(Ok(_resp)) => DeleteSessionOutcome::Deleted,
                Ok(Err(err)) => {
                    if err.code == ErrorCode::MethodNotFound {
                        DeleteSessionOutcome::UnsupportedMethod
                    } else {
                        // Scrub first: a leaked `sk-...` or PAT in the
                        // adapter's own error string must not reach the log.
                        let scrubbed = scrub_stderr_secrets(&err.message);
                        DeleteSessionOutcome::Failed(format!(
                            "acp error {}: {}",
                            i32::from(err.code),
                            truncate_for_log(&scrubbed, ACP_DELETE_ERROR_MSG_MAX)
                        ))
                    }
                }
                Err(_) => DeleteSessionOutcome::TimedOut,
            };
        let _ = respond_to.send(outcome);
    });
}

/// Caps an adapter-provided string on a UTF-8 boundary so a multi-megabyte
/// message cannot reach `debug.log`.
pub(super) fn truncate_for_log(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 3);
    out.push_str(&s[..end]);
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cut must never land mid-codepoint: "ééé" is 6 bytes, so a cap of 5
    /// has to rewind to byte 4 rather than slice a char in half.
    #[test]
    fn truncate_for_log_cuts_on_a_utf8_boundary() {
        for (input, cap, want) in [
            ("hello", 64, "hello"),
            ("hello", 5, "hello"),
            ("ééé", 5, "éé..."),
        ] {
            assert_eq!(truncate_for_log(input, cap), want, "{input} at {cap}");
        }
    }
}
