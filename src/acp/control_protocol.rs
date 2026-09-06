//! Control protocol v4 between `aoe serve` and `aoe __acp-runner`, carried
//! over `<id>.control.sock`. The runner is the sole ACP protocol terminator.
//!
//! The runner owns initialization, session establishment, prompts, cancellation,
//! and every JSON-RPC id sent to the agent. Notifications and prompt completion
//! are persistent across daemon attachments. Reverse calls, forward calls, and
//! handshake replies are scoped to the attachment that owns their correlation.
//!
//! The runner pre-encodes queued frames and accounts exact wire bytes. Its writer
//! retains queue ownership until write and flush succeed. This is a best-effort
//! detach buffer, not durable or exactly-once delivery: a disconnect after kernel
//! acceptance but before local commit can duplicate a frame on reattach.
//!
//! Notifications and prompt completion share one FIFO socket, so completion
//! cannot overtake preceding `session/update` frames.
//!
//! Frames use a 4-byte big-endian length followed by serialized JSON.
//! Length framing prevents nested payload newlines from becoming delimiters.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Current wire generation. Both peers validate it before transferring queued
/// frames; mixed generations are rejected before their frame sets can diverge.
/// Bump this whenever a wire-incompatible body or semantic contract changes.
pub const CONTROL_PROTOCOL_VERSION: u32 = 4;

/// Maximum NDJSON frame accepted from the ACP agent. The control channel is
/// the agent stream's only destination, so both limits must be derived from
/// one contract rather than accepting payloads the next hop cannot carry.
pub const MAX_AGENT_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Hard cap on a single control frame. A control envelope replaces the ACP
/// JSON-RPC envelope and adds small correlation fields; reserve explicit
/// headroom so every accepted agent frame remains representable.
pub const MAX_CONTROL_FRAME_BYTES: u32 = MAX_AGENT_FRAME_BYTES as u32 + 64 * 1024;

/// A single control frame. `kind` tags the variant so the wire form is
/// self-describing and forward-compatible: an unknown variant fails to
/// deserialize rather than being silently misread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlBody {
    // ---- runner -> daemon ----
    /// First frame the runner sends on a fresh control connection. Lets
    /// the daemon confirm the protocol version and the session identity
    /// it dialed.
    Hello {
        control_protocol_version: u32,
        session_id: String,
    },
    /// Runner's answer to [`ControlBody::Initialize`]: the raw ACP
    /// `initialize` result (an `InitializeResponse` serialized to JSON).
    /// Produced by running `initialize` once. Later attachments receive the
    /// cached result in a new attachment-scoped reply.
    Initialized { result: serde_json::Value },
    /// Runner's answer to [`ControlBody::EstablishSession`]: the
    /// established ACP session id plus the raw session response result
    /// (a `NewSessionResponse` / `LoadSessionResponse` serialized to
    /// JSON) so the daemon can extract modes / config options. Replayed
    /// from cache on later attaches.
    SessionReady {
        acp_session_id: String,
        result: serde_json::Value,
    },
    /// The runner-owned handshake failed (agent incompatible, `session/new`
    /// error, transport failure). Carries the raw JSON-RPC error object
    /// (`{code, message, data?}`) so the daemon can reconstruct the crate
    /// error verbatim and surface the same `AgentStartupError` (including
    /// data.details remediation) it would have on the direct stdio path,
    /// instead of hanging on a handshake that will never complete. A
    /// transport failure with no agent error synthesizes a minimal object.
    HandshakeFailed { error: serde_json::Value },
    /// The runner observed the agent's response to the `session/prompt`
    /// request it issued. `prompt_req_id` is the JSON-RPC id the runner
    /// assigned. `outcome` is the typed turn result.
    PromptCompleted {
        prompt_req_id: i64,
        outcome: PromptOutcome,
    },
    /// An agent-to-client request the daemon must service (permission,
    /// elicitation, fs, terminal). `params` is the raw JSON-RPC params;
    /// the daemon deserializes it into the crate request type keyed by
    /// `method`. `call_id` is allocated by the runner and is the sole
    /// correlation handle: the agent's own JSON-RPC id stays inside the
    /// runner, which alone knows its JSON type (a string id from one
    /// adapter and a numeric id from another must both round-trip).
    ///
    /// Answered by exactly one [`ControlBody::ServerResult`] or
    /// [`ControlBody::ServerError`] carrying the same `call_id`.
    ServerCall {
        call_id: u64,
        method: String,
        params: serde_json::Value,
    },
    /// A fire-and-forget agent notification, forwarded verbatim. Today
    /// that is `session/update` (the entire event stream); an unrecognized
    /// notification method is forwarded too rather than dropped, so a
    /// newly-emitting adapter shows up in the daemon's logs instead of
    /// vanishing.
    Notify {
        method: String,
        params: serde_json::Value,
    },
    /// The runner's answer to a [`ControlBody::AgentCall`]: the agent's
    /// raw JSON-RPC `result`.
    AgentResult {
        call_id: u64,
        result: serde_json::Value,
    },
    /// The agent answered a [`ControlBody::AgentCall`] with an error
    /// envelope, or the runner could not complete it (agent gone, deadline
    /// expired).
    AgentError { call_id: u64, error: JsonRpcError },

    // ---- daemon -> runner ----
    /// First frame the daemon sends after [`ControlBody::Hello`],
    /// acknowledging the version it will speak.
    Attach { control_protocol_version: u32 },
    /// The ACP `initialize` request params (an `InitializeRequest`
    /// serialized to JSON). The runner injects the JSON-RPC envelope + id.
    /// On a runner that already handshook, the params are ignored and the
    /// cached [`ControlBody::Initialized`] is replayed.
    Initialize { request: serde_json::Value },
    /// The session-creation request the runner should issue: `method` is
    /// `session/new`, `session/load`, or `session/fork`, and `request` is
    /// the matching params. Ignored (cache replayed) once the runner has
    /// an established session.
    EstablishSession {
        method: String,
        request: serde_json::Value,
    },
    /// Return the established session without sending an ACP load/new request.
    /// Waits for an already-sent conversation reset to commit first.
    ResumeSession,
    /// Run a turn. `request` is the ACP `session/prompt` params
    /// (`PromptRequest`); the runner assigns the canonical JSON-RPC id and
    /// tracks the response.
    Prompt { request: serde_json::Value },
    /// Cancel the in-flight turn (maps to a `session/cancel` notification).
    Cancel,
    /// A client-to-agent request the runner does not own: `session/set_mode`,
    /// `session/set_config_option`, `session/delete`, `_session/steering`,
    /// or a conversation-reset `session/new`. The runner injects its own
    /// JSON-RPC envelope and id, correlates the agent's response, and
    /// answers with [`ControlBody::AgentResult`] / [`ControlBody::AgentError`]
    /// carrying the same `call_id`. `call_id` is allocated by the daemon;
    /// the two lanes have independent id spaces and never collide because
    /// each is only ever matched against its own pending map.
    AgentCall {
        call_id: u64,
        method: String,
        params: serde_json::Value,
    },
    /// The daemon's answer to a [`ControlBody::ServerCall`]: the JSON-RPC
    /// `result` to hand back to the agent.
    ServerResult {
        call_id: u64,
        result: serde_json::Value,
    },
    /// The daemon could not service a [`ControlBody::ServerCall`]. The
    /// runner forwards this as the request's JSON-RPC error envelope.
    ServerError { call_id: u64, error: JsonRpcError },
}

/// A JSON-RPC error object. Typed rather than a bare `Value` so neither
/// side can emit a malformed envelope that the other has to guess at; the
/// agent-facing wire form is exactly these three fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// JSON-RPC "internal error". Used when the daemon produced an answer this
/// side could not make sense of. Distinct from JSON-RPC's -32601
/// "method not found", which the crate's own dispatch answers for an
/// unhandled method before the shim ever sees it.
pub const INTERNAL_ERROR: i64 = -32603;

/// Reserved-range code for "the daemon went away before answering". Shared
/// by the runner's disconnect sweep and its deadline expiry so an agent
/// sees one consistent code for an unanswerable reverse call. Matches the
/// code the pre-Phase-C relay sweep used, so adapter-side handling of it is
/// unchanged.
pub const DAEMON_GONE: i64 = -32001;

impl JsonRpcError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }
}

/// Typed result of a runner-owned turn, including agent error envelopes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PromptOutcome {
    /// Normal completion. `stop_reason` is the ACP `stopReason` from the
    /// response result when present.
    Completed { stop_reason: Option<String> },
    /// The agent answered the prompt with a JSON-RPC error envelope. The
    /// `data` object is preserved so the daemon can still classify a
    /// rate-limit error (which carries `errorKind` / `resets_at` there).
    Error {
        code: i32,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<serde_json::Value>,
    },
    /// The turn ended because the runner lost the agent (process exit,
    /// transport failure) before a response arrived.
    Aborted,
}

/// Encode a frame: 4-byte big-endian length prefix, then the JSON body.
pub fn encode_frame(body: &ControlBody) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(4096);
    buf.extend_from_slice(&[0; 4]);
    serde_json::to_writer(&mut buf, body)?;
    let len = u32::try_from(buf.len() - 4)
        .map_err(|_| anyhow::anyhow!("control frame exceeds u32 length"))?;
    if len > MAX_CONTROL_FRAME_BYTES {
        bail!("control frame {len} bytes exceeds cap {MAX_CONTROL_FRAME_BYTES}");
    }
    buf[..4].copy_from_slice(&len.to_be_bytes());
    Ok(buf)
}

/// Write a frame that was encoded and size-checked before queue admission.
pub async fn write_encoded_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &[u8]) -> Result<()> {
    w.write_all(frame).await?;
    w.flush().await?;
    Ok(())
}

/// Write one frame and flush.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, body: &ControlBody) -> Result<()> {
    let buf = encode_frame(body)?;
    write_encoded_frame(w, &buf).await
}

/// Read one frame. Returns `Ok(None)` on a clean EOF at a frame boundary
/// (the peer closed the socket), so callers can treat that as a normal
/// disconnect rather than an error.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<ControlBody>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_CONTROL_FRAME_BYTES {
        bail!("control frame length {len} exceeds cap {MAX_CONTROL_FRAME_BYTES}");
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).await?;
    let parsed: ControlBody = serde_json::from_slice(&body)?;
    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn roundtrip(body: ControlBody) -> ControlBody {
        let encoded = encode_frame(&body).expect("encode");
        // Length prefix plus a body that deserializes back to the same value.
        let len = u32::from_be_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(len as usize, encoded.len() - 4);
        serde_json::from_slice(&encoded[4..]).expect("decode")
    }

    #[test]
    fn hello_roundtrips() {
        let body = ControlBody::Hello {
            control_protocol_version: CONTROL_PROTOCOL_VERSION,
            session_id: "abc-123".into(),
        };
        assert_eq!(roundtrip(body.clone()), body);
    }

    #[test]
    fn prompt_completed_roundtrips() {
        let body = ControlBody::PromptCompleted {
            prompt_req_id: 42,
            outcome: PromptOutcome::Completed {
                stop_reason: Some("end_turn".into()),
            },
        };
        assert_eq!(roundtrip(body.clone()), body);
    }

    #[test]
    fn prompt_outcome_variants_roundtrip() {
        for outcome in [
            PromptOutcome::Completed { stop_reason: None },
            PromptOutcome::Error {
                code: -32000,
                message: "boom".into(),
                data: Some(serde_json::json!({"errorKind": "rate_limit"})),
            },
            PromptOutcome::Aborted,
        ] {
            let body = ControlBody::PromptCompleted {
                prompt_req_id: 1,
                outcome: outcome.clone(),
            };
            assert_eq!(roundtrip(body.clone()), body);
        }
    }

    #[test]
    fn handshake_frames_roundtrip() {
        for body in [
            ControlBody::Initialize {
                request: serde_json::json!({"protocolVersion": 1}),
            },
            ControlBody::Initialized {
                result: serde_json::json!({"agentCapabilities": {}}),
            },
            ControlBody::EstablishSession {
                method: "session/new".into(),
                request: serde_json::json!({"cwd": "/tmp"}),
            },
            ControlBody::SessionReady {
                acp_session_id: "sess-1".into(),
                result: serde_json::json!({"sessionId": "sess-1"}),
            },
            ControlBody::HandshakeFailed {
                error: serde_json::json!({"code": -32603, "message": "incompatible"}),
            },
            ControlBody::Prompt {
                request: serde_json::json!({"sessionId": "sess-1", "prompt": []}),
            },
            ControlBody::Cancel,
        ] {
            assert_eq!(roundtrip(body.clone()), body);
        }
    }

    #[test]
    fn v4_lane_frames_roundtrip() {
        for body in [
            ControlBody::ServerCall {
                call_id: 1,
                method: "session/request_permission".into(),
                params: serde_json::json!({"sessionId": "s"}),
            },
            ControlBody::ServerResult {
                call_id: 1,
                result: serde_json::json!({"outcome": {"outcome": "cancelled"}}),
            },
            ControlBody::ServerError {
                call_id: 1,
                error: JsonRpcError::new(INTERNAL_ERROR, "no handler"),
            },
            ControlBody::Notify {
                method: "session/update".into(),
                params: serde_json::json!({"sessionId": "s"}),
            },
            ControlBody::AgentCall {
                call_id: 2,
                method: "session/set_mode".into(),
                params: serde_json::json!({"modeId": "default"}),
            },
            ControlBody::AgentResult {
                call_id: 2,
                result: serde_json::json!({}),
            },
            ControlBody::AgentError {
                call_id: 2,
                error: JsonRpcError {
                    code: DAEMON_GONE,
                    message: "gone".into(),
                    data: Some(serde_json::json!({"errorKind": "rate_limit"})),
                },
            },
        ] {
            assert_eq!(roundtrip(body.clone()), body);
        }
    }

    #[test]
    fn json_rpc_error_omits_absent_data() {
        // The agent-facing envelope must not carry `"data": null`; some
        // adapters treat a present-but-null data field as a payload.
        let encoded = serde_json::to_value(JsonRpcError::new(DAEMON_GONE, "gone")).unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({"code": -32001, "message": "gone"})
        );
    }

    #[tokio::test]
    async fn write_then_read_frame() {
        let body = ControlBody::PromptCompleted {
            prompt_req_id: 7,
            outcome: PromptOutcome::Aborted,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &body).await.expect("write");
        let mut cursor = Cursor::new(buf);
        let got = read_frame(&mut cursor).await.expect("read");
        assert_eq!(got, Some(body));
    }

    #[tokio::test]
    async fn multiple_frames_in_one_stream() {
        let a = ControlBody::Hello {
            control_protocol_version: CONTROL_PROTOCOL_VERSION,
            session_id: "s".into(),
        };
        let b = ControlBody::PromptCompleted {
            prompt_req_id: 1,
            outcome: PromptOutcome::Completed {
                stop_reason: Some("cancelled".into()),
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &a).await.unwrap();
        write_frame(&mut buf, &b).await.unwrap();
        let mut cursor = Cursor::new(buf);
        assert_eq!(read_frame(&mut cursor).await.unwrap(), Some(a));
        assert_eq!(read_frame(&mut cursor).await.unwrap(), Some(b));
        assert_eq!(read_frame(&mut cursor).await.unwrap(), None);
    }

    #[tokio::test]
    async fn clean_eof_returns_none() {
        let mut cursor = Cursor::new(Vec::new());
        assert_eq!(read_frame(&mut cursor).await.unwrap(), None);
    }

    #[tokio::test]
    async fn frame_bounds_accept_large_agent_payload_and_reject_excess_prefix() {
        let body = ControlBody::AgentCall {
            call_id: 1,
            method: "session/prompt".into(),
            params: serde_json::json!({"blob": "x".repeat(17 * 1024 * 1024)}),
        };
        let encoded = encode_frame(&body).expect("17 MiB agent payload fits the shared cap");
        assert!(encoded.len() > 17 * 1024 * 1024);

        let mut buf = Vec::new();
        buf.extend_from_slice(&(MAX_CONTROL_FRAME_BYTES + 1).to_be_bytes());
        let mut cursor = Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }

    #[tokio::test]
    async fn truncated_body_is_error_not_eof() {
        // A full length prefix but a short body is a corrupt frame, not a
        // clean close.
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"only-4"); // fewer than 16 bytes
        let mut cursor = Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());
    }
}
