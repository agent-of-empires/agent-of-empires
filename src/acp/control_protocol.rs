//! Control protocol between `aoe serve` and `aoe __acp-runner`, carried
//! over `<id>.control.sock`.

use crate::acp::establishment::NativeReplayClosure;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Current wire generation.
pub const CONTROL_PROTOCOL_VERSION: u32 = 6;

/// Maximum NDJSON frame accepted from the ACP agent.
pub const MAX_AGENT_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Hard cap on a single control frame.
pub const MAX_CONTROL_FRAME_BYTES: u32 = MAX_AGENT_FRAME_BYTES as u32 + 64 * 1024;

/// Detached ordinary queue bound; SDK replay also covers bounded bootstrap
/// slots and retained completion/loss items. Original wire bytes, not expanded
/// typed JSON, determine its byte charge.
pub const MAX_CONTROL_QUEUE_FRAMES: usize = 4096;
pub const MAX_CONTROL_QUEUE_BYTES: usize = 128 * 1024 * 1024;

/// Actual cached handshake replies and replay fences may borrow these slots.
/// Further management waits for real writes; the byte bound is unchanged.
pub(crate) const CONTROL_BOOTSTRAP_RESERVED_FRAMES: usize = 2;

/// Retained completion and sticky loss each add one replay item outside the
/// ordinary queue. Completion bytes use the actual cached wire length; only
/// the fixed-size native-loss receipt has a constant byte reservation.
pub(crate) const CONTROL_REPLAY_RESERVED_FRAMES: usize = 2;
pub(super) const MAX_COMPLETION_MARKER_BYTES: usize = 16 * 1024;
pub(crate) const MAX_NATIVE_LOSS_FRAME_BYTES: usize = 256;

pub(crate) fn session_scoped_callback(method: &str, params: &serde_json::Value) -> bool {
    params.get("sessionId").is_some()
        || (params.get(0).is_some()
            && matches!(
                method,
                "session/request_permission"
                    | "fs/read_text_file"
                    | "fs/write_text_file"
                    | "terminal/create"
                    | "terminal/output"
                    | "terminal/release"
                    | "terminal/wait_for_exit"
                    | "terminal/kill"
            ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandshakeFailureOrigin {
    NativeResponse,
    LocalTransport,
    LocalResult,
    LocalState,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandshakeFailure {
    pub origin: HandshakeFailureOrigin,
    pub error: serde_json::Value,
}

impl HandshakeFailure {
    pub(crate) fn new(origin: HandshakeFailureOrigin, error: serde_json::Value) -> Self {
        Self { origin, error }
    }

    pub(crate) fn into_frame(self, method: &str) -> ControlBody {
        ControlBody::HandshakeFailed {
            method: method.to_owned(),
            origin: self.origin,
            error: self.error,
        }
    }
}

/// A local observation by the runner that owns native stdin/stdout.
/// These causes say nothing about whether the native process is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeChannelLossCause {
    StdoutEof,
    ReadFailed,
    WriteFailed,
    ProtocolEnded,
}

/// This receipt is produced from the runner's genuine in-memory native
/// establishment and channel state, never from a registry session ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NativeChannelAvailability {
    Available,
    Unavailable { cause: NativeChannelLossCause },
}

#[derive(
    Debug, Clone, Default, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification,
)]
#[notification(method = "_aoe/native_channel_lost")]
pub(crate) struct NativeChannelLostMarker {}

/// Runner-minted [`ControlBody::Notify`] queued right after an established
/// session's [`ControlBody::SessionReady`]. Frames go out handshake first, so
/// it follows the updates the agent sent before its reply; the daemon handles
/// it only once that replay has been applied.
#[derive(Debug, Clone, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification)]
#[notification(method = "_aoe/session_replayed")]
pub struct SessionReplayed {
    pub attempt_id: crate::acp::establishment::AttemptId,
    pub closure: crate::acp::establishment::NativeReplayClosure,
    pub delivery: NativeReplayDelivery,
}

/// The fixed portion of the original notification range this attachment owes.
/// It is not a replacement for the original closure or its historical commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReplayDelivery {
    pub after: u64,
    pub through: u64,
    pub lost: bool,
}

impl NativeReplayDelivery {
    pub(crate) fn original(closure: NativeReplayClosure) -> Self {
        Self {
            after: 0,
            through: closure.notifications,
            lost: closure.lost,
        }
    }

    pub(crate) fn matches(self, closure: NativeReplayClosure) -> bool {
        self.after <= self.through
            && self.through == closure.notifications
            && !self.lost
            && closure.closed
            && !closure.lost
    }
}

/// Original native notification ordinal, private to the owned control stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeReplayFrame {
    pub attempt_id: crate::acp::establishment::AttemptId,
    pub ordinal: u64,
}

/// Native ingress evidence, captured before any control or SDK queue wait.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeNotifyEvidence {
    /// Physical transport/row provenance, never content authorship or an ACK.
    pub(crate) physical_source: Option<crate::acp::state::NativePhysicalSource>,
    pub(crate) session_stamp: Option<crate::acp::establishment::NativeSessionStamp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) original_request_stamp: Option<crate::acp::establishment::OriginalRequestStamp>,
    /// Sparse resource START authority only; never public Session scope or RPC correlation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) resource_start_stamp: Option<crate::acp::establishment::NativeSessionStamp>,
    pub(crate) prompt_observation: Option<crate::acp::protocol::AcpPromptObservation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePromptHead {
    pub(crate) prompt_req_id: i64,
    pub(crate) scope: crate::acp::establishment::NativeRequestScope,
    pub(crate) window_id: crate::acp::protocol::AcpPromptWindowId,
}

/// Exact original RPC evidence, not permission for another business request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativePromptCompletionEvidence {
    pub(crate) origin: crate::acp::state::NativeEventOrigin,
    pub(crate) prompt_req_id: i64,
    pub(crate) scope: crate::acp::establishment::NativeRequestScope,
    pub(crate) receipt: std::sync::Arc<crate::acp::establishment::NativeEstablishmentReceipt>,
    pub(crate) window_id: crate::acp::protocol::AcpPromptWindowId,
    pub(crate) uncertain_output: bool,
}

/// Daemon-minted completion barrier, ordered behind preceding SDK notifications.
/// Adopted payloads contain the bounded public diagnostic, never raw error envelopes.
#[derive(
    Debug, Clone, Default, Serialize, Deserialize, agent_client_protocol::JsonRpcNotification,
)]
#[notification(method = "_aoe/prompt_completed")]
pub(crate) struct PromptCompletedMarker {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adopted: Option<Box<AdoptedPromptCompletion>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_req_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AdoptedPromptCompletion {
    pub turn: Option<u64>,
    pub prompt_req_id: i64,
    pub outcome_kind: AdoptedPromptOutcomeKind,
    pub diagnostic: Option<AdoptedPromptDiagnostic>,
    pub reason: String,
    pub evidence: Option<Box<NativePromptCompletionEvidence>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AdoptedPromptOutcomeKind {
    Completed,
    Error,
    Aborted,
}

/// Private ingress markers admit only prompt diagnostics, never arbitrary
/// events with private native-subject qualifications.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub(crate) struct AdoptedPromptDiagnostic(crate::acp::state::Event);

impl AdoptedPromptDiagnostic {
    pub(crate) fn new(event: crate::acp::state::Event) -> Option<Self> {
        matches!(
            event,
            crate::acp::state::Event::PromptRuntimeError { .. }
                | crate::acp::state::Event::RateLimit { .. }
        )
        .then_some(Self(event))
    }

    pub(crate) fn inherit_rate_limit_hint(
        &mut self,
        hint: impl FnOnce() -> Option<chrono::DateTime<chrono::Utc>>,
    ) {
        if let crate::acp::state::Event::RateLimit { info } = &mut self.0 {
            if info.resets_at.is_none() {
                info.resets_at = hint();
            }
        }
    }

    pub(crate) fn into_event(self) -> crate::acp::state::Event {
        self.0
    }
}

impl<'de> Deserialize<'de> for AdoptedPromptDiagnostic {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let event = crate::acp::state::Event::deserialize(deserializer)?;
        Self::new(event).ok_or_else(|| serde::de::Error::custom("not a prompt diagnostic"))
    }
}

/// A single control frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ControlBody {
    // ---- runner -> daemon ----
    /// First frame the runner sends on a fresh control connection.
    Hello {
        control_protocol_version: u32,
        session_id: String,
        prompt_head: Option<NativePromptHead>,
    },
    /// Runner's answer to [`ControlBody::Initialize`]: the raw ACP
    /// `initialize` result (an `InitializeResponse` serialized to JSON).
    Initialized { result: serde_json::Value },
    /// Observational custody of the one original invocation, not its outcome.
    OriginalEstablishment {
        invocation: std::sync::Arc<crate::acp::establishment::NativeEstablishmentReceipt>,
    },
    SessionReady {
        acp_session_id: String,
        result: std::sync::Arc<serde_json::Value>,
        native_channel: NativeChannelAvailability,
        receipt: std::sync::Arc<crate::acp::establishment::NativeEstablishmentReceipt>,
        postimage: Option<std::sync::Arc<crate::acp::establishment::EstablishmentPostimage>>,
        original_delivery: bool,
        delivery: NativeReplayDelivery,
        local_failure: Option<std::sync::Arc<HandshakeFailure>>,
    },
    /// The runner-owned handshake failed (agent incompatible, `session/new`
    /// error, transport failure).
    HandshakeFailed {
        method: String,
        origin: HandshakeFailureOrigin,
        error: serde_json::Value,
    },
    EstablishmentFailed {
        receipt: std::sync::Arc<crate::acp::establishment::NativeEstablishmentReceipt>,
        error: std::sync::Arc<serde_json::Value>,
        origin: HandshakeFailureOrigin,
        original_delivery: bool,
        delivery: NativeReplayDelivery,
    },
    /// The runner assigned the canonical JSON-RPC id for this attachment's
    /// [`ControlBody::Prompt`].
    PromptStarted {
        prompt_req_id: i64,
        correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
        window_id: crate::acp::protocol::AcpPromptWindowId,
    },
    /// The runner observed the agent's response to the `session/prompt`
    /// request it issued.
    PromptCompleted {
        prompt_req_id: i64,
        outcome: PromptOutcome,
        evidence: Option<Box<NativePromptCompletionEvidence>>,
    },
    /// Sticky runner-owned native I/O loss. Management and process ownership
    /// survive; this is neither a peer JSON-RPC error nor a prompt outcome.
    NativeChannelLost {
        cause: NativeChannelLossCause,
        /// Genuine completed native cache exists; routing only, not usability
        /// or permission to issue another native request.
        cached_session: bool,
    },
    /// An agent-to-client request the daemon must service (permission,
    /// elicitation, fs, terminal).
    ServerCall {
        call_id: u64,
        method: String,
        params: serde_json::Value,
        evidence: NativeNotifyEvidence,
    },
    /// The attachment's reverse-call write attempt has terminated locally.
    /// This does not claim Native delivery, success, or a peer response.
    ServerCallSettled { call_id: u64 },
    /// A fire-and-forget agent notification, forwarded verbatim.
    Notify {
        method: String,
        params: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        replay: Option<NativeReplayFrame>,
        evidence: NativeNotifyEvidence,
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
        attempt: std::sync::Arc<crate::acp::establishment::EstablishmentAttempt>,
        store: Option<crate::session::ExecutionBinding>,
    },
    /// Return the established session without sending an ACP load/new request.
    ResumeSession,
    /// Run a turn.
    Prompt {
        request: serde_json::Value,
        scope: crate::acp::establishment::NativeRequestScope,
    },
    /// Cancel the in-flight turn (maps to a `session/cancel` notification).
    Cancel {
        scope: crate::acp::establishment::NativeRequestScope,
    },
    /// A non-establishing client-to-agent request. `session/new`, load and fork
    /// must use the correlated EstablishSession operation.
    AgentCall {
        call_id: u64,
        method: String,
        params: serde_json::Value,
        scope: crate::acp::establishment::NativeRequestScope,
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

/// JSON-RPC invalid parameters, including missing establishment authority.
pub const INVALID_PARAMS: i64 = -32602;

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
    use agent_client_protocol::JsonRpcMessage;
    use std::io::Cursor;

    #[tokio::test]
    #[serial_test::serial]
    async fn original_replay_closes_on_a_native_result_or_error() {
        use crate::process::runner::test_support::OwnedNativeFixture;
        {
            let mut native = OwnedNativeFixture::load("sess-1");
            native.begin().await;
            let request = native.read_request().await;
            let descriptor = native.original_descriptor().await;
            assert!(matches!(descriptor,
                ControlBody::OriginalEstablishment { invocation }
                    if invocation.attempt.as_ref() == native.attempt.as_ref()
                        && !invocation.replay.closed
            ));
            native
                .deliver(&serde_json::json!({
                    "jsonrpc":"2.0","method":"session/update",
                    "params":{"sessionId":"sess-1","update":{
                        "sessionUpdate":"agent_message_chunk",
                        "content":{"type":"text","text":"original native bootstrap"}
                    }}
                }))
                .await;
            native
                .deliver(&serde_json::json!({
                    "jsonrpc":"2.0","id":request["id"],
                    "result":{"sessionId":"sess-1"}
                }))
                .await;
            native.wait_for_frame(|frame| matches!(frame,
                ControlBody::Notify { method, .. } if SessionReplayed::matches_method(method)
            )).await;
            native.join_observer().await;
            let frames = native.frames().await;
            assert!(frames.iter().any(|frame| matches!(frame,
                ControlBody::SessionReady { receipt, postimage: Some(_), .. }
                    if receipt.attempt.as_ref() == native.attempt.as_ref()
                        && receipt.replay.closed && receipt.replay.notifications == 1
            )));
            assert!(frames.iter().any(|frame| matches!(frame,
                ControlBody::Notify { replay: Some(replay), .. }
                    if replay.attempt_id == native.attempt.id && replay.ordinal == 1
            )));
        }
        {
            let mut native = OwnedNativeFixture::fork("original-parent");
            native.begin().await;
            let request = native.read_request().await;
            native
                .deliver(&serde_json::json!({
                    "jsonrpc":"2.0","id":request["id"],
                    "error":{"code":-32603,"message":"native fork rejected"}
                }))
                .await;
            native.wait_for_frame(|frame| matches!(frame,
                ControlBody::Notify { method, .. } if SessionReplayed::matches_method(method)
            )).await;
            let frames = native.frames().await;
            assert!(frames.iter().any(|frame| matches!(frame,
                ControlBody::EstablishmentFailed {
                    origin: HandshakeFailureOrigin::NativeResponse, receipt, ..
                } if receipt.attempt.as_ref() == native.attempt.as_ref()
            )));
        }
    }

    #[tokio::test]
    async fn frames_read_in_order_within_bounds() {
        let body = ControlBody::Notify {
            method: "session/update".into(),
            params: serde_json::json!({"blob": "x".repeat(17 * 1024 * 1024)}),
            replay: None,
            evidence: NativeNotifyEvidence::default(),
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
            prompt_head: None,
        };
        let b = ControlBody::PromptCompleted {
            prompt_req_id: 1,
            outcome: PromptOutcome::Completed {
                stop_reason: Some("cancelled".into()),
            },
            evidence: None,
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
