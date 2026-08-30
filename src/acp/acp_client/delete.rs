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
    /// Emit `_meta: {}` so adapters that validate against the strict
    /// `unstable_session_delete` schema accept the request. Optional
    /// in the TS schema, but a defensive default avoids `-32602
    /// invalid_params` from future adapter validators.
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
    /// Adapter returned JSON-RPC `-32601 method_not_found`. Expected on
    /// adapters that don't advertise `sessionCapabilities.delete`
    /// (`aoe-agent`, `codex`, `opencode`, older `claude-agent-acp`).
    UnsupportedMethod,
    /// The bounded wait elapsed before the adapter responded.
    TimedOut,
    /// Any other failure (non-`-32601` JSON-RPC error, transport drop,
    /// dispatch channel closed). Carries the reason for the log line.
    Failed(String),
}

/// Adapter error messages flow through here verbatim. Cap at this many
/// bytes before logging so a chatty or malicious adapter cannot bloat
/// `debug.log`. 256 leaves room for the prefix while preserving the
/// JSON-RPC error code and a useful slice of the message.
pub(super) const ACP_DELETE_ERROR_MSG_MAX: usize = 256;

/// Hard cap on the wait for `session/delete`. Adapters that succeed
/// (claude-agent-acp clears a local file) complete in tens of ms; the
/// timeout protects the delete path from a wedged adapter. Not a
/// `AcpConfig` field on purpose: this is best-effort experimental
/// cleanup with no operator-visible failure mode.
pub(super) const ACP_SESSION_DELETE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(2);

/// Dispatch the experimental `session/delete` RPC from the connect
/// task's cmd_rx arm. The wait is detached via `tokio::spawn` so the
/// cmd_rx select arm keeps polling other commands during the
/// round-trip; the outcome is delivered to the caller via the
/// `respond_to` oneshot. Bounded by `ACP_SESSION_DELETE_TIMEOUT` so a
/// wedged adapter still resolves the oneshot in time for the caller's
/// outer guard. See #1404.
pub(super) fn handle_delete_session_cmd(
    connection: &ConnectionTo<Agent>,
    acp_session_id: String,
    respond_to: oneshot::Sender<DeleteSessionOutcome>,
) {
    let target = agent_client_protocol::schema::v1::SessionId::from(acp_session_id);
    // `block_task()` is documented as safe to await from a spawned
    // task: it waits on the per-request oneshot the main connection
    // task feeds via its inbound pump, so the dispatch loop keeps
    // running while this future is parked. The spawn here keeps the
    // cmd_rx select arm responsive to other commands during the
    // round-trip, mirroring the pattern used by SetMode /
    // SetConfigOption.
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
                        // Adapter error messages reach debug.log
                        // verbatim. Run them through the existing
                        // stderr secret scrubber so a leaked
                        // `sk-...` / `Bearer ...` / GitHub PAT in
                        // the adapter's own error string doesn't
                        // land in operator logs, then cap length.
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

/// Defensive truncation of adapter-provided strings before they land
/// in `debug.log`. A malformed or malicious adapter could emit a
/// multi-megabyte message; this caps the allocation while preserving
/// a useful prefix and respecting UTF-8 boundaries.
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

    // truncate_for_log is the adapter-error sanitizer in the
    // session/delete path: it caps a third-party-controlled string so
    // a chatty adapter can't bloat debug.log, and must never panic on
    // UTF-8 inputs. The cases below lock down the boundary semantics
    // (no-op under cap, exact cap, multibyte cut) so a future change
    // to the helper can't quietly regress the no-panic invariant.
    #[test]
    fn truncate_for_log_below_cap_returns_input() {
        assert_eq!(truncate_for_log("hello", 64), "hello");
    }

    #[test]
    fn truncate_for_log_at_exact_cap_returns_input() {
        assert_eq!(truncate_for_log("hello", 5), "hello");
    }

    #[test]
    fn truncate_for_log_cuts_on_utf8_boundary_without_panic() {
        // "é" is two bytes (0xC3 0xA9). With max_bytes=5 the naive
        // slice would land mid-codepoint; the helper must rewind to
        // the previous char boundary (byte 4) before appending the
        // ellipsis. "ééé" is 6 bytes total, so we expect "éé...".
        let out = truncate_for_log("ééé", 5);
        assert_eq!(out, "éé...");
    }
}
