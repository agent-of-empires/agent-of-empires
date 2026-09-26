//! Control protocol v3 between `aoe serve` and `aoe __acp-runner`, carried
//! over `<id>.control.sock`.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Current wire generation.
pub const CONTROL_PROTOCOL_VERSION: u32 = 3;

/// Maximum NDJSON frame accepted from the ACP agent.
pub const MAX_AGENT_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Hard cap on a single control frame.
pub const MAX_CONTROL_FRAME_BYTES: u32 = MAX_AGENT_FRAME_BYTES as u32 + 64 * 1024;

/// Bound on the runner-to-daemon control queue. A detached runner buffers
/// outbound frames until a daemon reattaches, so it must never retain an
/// unbounded number of large JSON values, yet one maximum-sized ACP frame
/// must still fit. This backlog is exactly what a reattaching daemon replays,
/// so the daemon's pending-replay buffer is sized against the same contract:
/// a legitimate reattach must be able to accept the whole flushed queue.
pub const MAX_CONTROL_QUEUE_FRAMES: usize = 4096;
pub const MAX_CONTROL_QUEUE_BYTES: usize = 128 * 1024 * 1024;

/// Runner-minted [`ControlBody::Notify`] queued right after an established
/// session's [`ControlBody::SessionReady`]. Frames go out handshake first, so
/// it follows the updates the agent sent before its reply; the daemon handles
/// it only once that replay has been applied.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification,
)]
#[notification(method = "_aoe/session_replayed")]
pub struct SessionReplayed {}

/// A single control frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlBody {
    // ---- runner -> daemon ----
    /// First frame the runner sends on a fresh control connection.
    Hello {
        control_protocol_version: u32,
        session_id: String,
    },
    /// Runner's answer to [`ControlBody::Initialize`]: the raw ACP
    /// `initialize` result (an `InitializeResponse` serialized to JSON).
    Initialized { result: serde_json::Value },
    SessionReady {
        acp_session_id: String,
        result: serde_json::Value,
    },
    /// The runner-owned handshake failed (agent incompatible, `session/new`
    /// error, transport failure).
    HandshakeFailed { error: serde_json::Value },
    /// The runner assigned the canonical JSON-RPC id for this attachment's
    /// [`ControlBody::Prompt`].
    PromptStarted { prompt_req_id: i64 },
    /// The runner observed the agent's response to the `session/prompt`
    /// request it issued.
    PromptCompleted {
        prompt_req_id: i64,
        outcome: PromptOutcome,
    },
    /// An agent-to-client request the daemon must service (permission,
    /// elicitation, fs, terminal).
    ServerCall {
        call_id: u64,
        method: String,
        params: serde_json::Value,
    },
    /// A fire-and-forget agent notification, forwarded verbatim.
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
    /// serialized to JSON).
    Initialize { request: serde_json::Value },
    /// The session-creation request the runner should issue: `method` is
    /// `session/new`, `session/load`, or `session/fork`, and `request` is
    /// the matching params.
    EstablishSession {
        method: String,
        request: serde_json::Value,
    },
    /// Return the established session without sending an ACP load/new request.
    ResumeSession,
    /// Run a turn.
    Prompt { request: serde_json::Value },
    /// Cancel the in-flight turn (maps to a `session/cancel` notification).
    Cancel,
    /// A client-to-agent request the runner does not own: `session/set_mode`,
    /// `session/set_config_option`, `session/delete`, `_session/steering`,
    /// or a conversation-reset `session/new`.
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
    /// The daemon could not service a [`ControlBody::ServerCall`].
    ServerError { call_id: u64, error: JsonRpcError },
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// JSON-RPC "internal error".
pub const INTERNAL_ERROR: i64 = -32603;

/// Reserved-range code for "the daemon went away before answering".
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
    /// Normal completion.
    Completed { stop_reason: Option<String> },
    /// The agent answered the prompt with a JSON-RPC error envelope.
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

/// Read one frame.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<ControlBody>> {
    Ok(read_frame_with_size(r).await?.map(|(body, _)| body))
}

/// Read a frame with its original wire cost, including the length prefix.
pub(crate) async fn read_frame_with_size<R: AsyncRead + Unpin>(
    r: &mut R,
) -> Result<Option<(ControlBody, usize)>> {
    let mut len_buf = [0u8; 4];
    #[cfg(debug_assertions)]
    let length_read = async {
        let mut consumed = 0;
        while consumed < len_buf.len() {
            let n = r.read(&mut len_buf[consumed..]).await?;
            if n == 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            consumed += n;
            if consumed < len_buf.len() {
                if let Some(path) = std::env::var_os("AOE_E2E_PARTIAL_FRAME_FILE") {
                    std::fs::write(path, consumed.to_string())?;
                }
            }
        }
        Ok(consumed)
    }
    .await;
    #[cfg(not(debug_assertions))]
    let length_read = r.read_exact(&mut len_buf).await;
    match length_read {
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
    Ok(Some((parsed, len as usize + len_buf.len())))
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

    /// Every frame the two sides exchange survives the length-prefixed encoding.
    #[test]
    fn control_frames_roundtrip() {
        for body in [
            ControlBody::Hello {
                control_protocol_version: CONTROL_PROTOCOL_VERSION,
                session_id: "abc-123".into(),
            },
            ControlBody::Attach {
                control_protocol_version: CONTROL_PROTOCOL_VERSION,
            },
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
            ControlBody::ResumeSession,
            ControlBody::HandshakeFailed {
                error: serde_json::json!({"code": -32603, "message": "incompatible"}),
            },
            ControlBody::Prompt {
                request: serde_json::json!({"sessionId": "sess-1", "prompt": []}),
            },
            ControlBody::PromptStarted { prompt_req_id: 42 },
            ControlBody::Cancel,
            ControlBody::PromptCompleted {
                prompt_req_id: 42,
                outcome: PromptOutcome::Completed {
                    stop_reason: Some("end_turn".into()),
                },
            },
            ControlBody::PromptCompleted {
                prompt_req_id: 1,
                outcome: PromptOutcome::Completed { stop_reason: None },
            },
            ControlBody::PromptCompleted {
                prompt_req_id: 1,
                outcome: PromptOutcome::Error {
                    code: -32000,
                    message: "boom".into(),
                    data: Some(serde_json::json!({"errorKind": "rate_limit"})),
                },
            },
            ControlBody::PromptCompleted {
                prompt_req_id: 1,
                outcome: PromptOutcome::Aborted,
            },
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

    #[tokio::test]
    async fn frames_read_in_order_within_bounds() {
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

        // A truncated body is an error, not a clean EOF.
        let mut buf = Vec::new();
        buf.extend_from_slice(&16u32.to_be_bytes());
        buf.extend_from_slice(b"only-4");
        let mut cursor = Cursor::new(buf);
        assert!(read_frame(&mut cursor).await.is_err());

        // Frames read back in order until a clean EOF.
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
}
