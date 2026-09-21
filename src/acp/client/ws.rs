//! WebSocket client for the structured view broadcast stream.
//!
//! Subscribes to `/sessions/{id}/acp/ws?since=N` and yields decoded events.
//! `parse_text` documents the frame shapes the daemon pushes.
//!
//! The bearer token rides a `?token=` query param rather than a header, which
//! most WS clients do not surface cleanly; the daemon's auth middleware accepts
//! both. Only the redacted URL is ever logged.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};
use tracing::{debug, warn};

use super::discovery::DaemonEndpoint;
use crate::acp::protocol::AcpBroadcastFrame;
use crate::acp::state::AcpState;
use crate::acp::transcript::{TranscriptDelta, TranscriptRow};

#[derive(Debug, Error)]
pub enum WsError {
    #[error("websocket transport error: {0}")]
    Transport(#[from] tokio_tungstenite::tungstenite::Error),
    #[error("invalid websocket URL: {0}")]
    InvalidUrl(String),
    #[error("websocket closed unexpectedly (code {0:?})")]
    UnexpectedClose(Option<CloseCode>),
    /// Surfaced so a toast carries the real reason, not a fabricated
    /// transport error.
    #[error("failed to parse websocket frame: {0}")]
    Parse(String),
}

/// One message off the structured view WebSocket.
#[derive(Debug, Clone)]
pub enum WsMessage {
    /// A raw event frame, consumed by `aoe acp tail`. The structured view
    /// reads the two folded projections below instead.
    Frame(Arc<AcpBroadcastFrame>),
    /// Server-folded control state (turn flags, approvals, elicitations,
    /// usage, modes, commands, plan), sent on connect and after every event.
    /// Boxed because `AcpState` dwarfs the other variants.
    ///
    /// `unchanged` names the cold fields the server omitted because this
    /// connection already holds them (`COLD_STATE_FIELDS` in
    /// `src/server/acp_ws.rs`). They deserialize to empty defaults, so a
    /// consumer must keep what it has rather than adopt the blank.
    ReducedState {
        seq: u64,
        state: Box<AcpState>,
        unchanged: Vec<String>,
    },
    /// The daemon's ring evicted events this client missed. The consumer must
    /// drop its reducer state and `HttpClient::replay(since=last_seq)`.
    Lagged,
    /// Connect snapshot of the folded transcript rows, reconciled by id so an
    /// overlap with a `?view=rows` replay is idempotent.
    TranscriptSnapshot(Vec<TranscriptRow>),
    /// One folded row change. Boxed: a `Patch` carries a whole
    /// `TranscriptRow`, which would bloat every `WsMessage`.
    TranscriptDelta(Box<TranscriptDelta>),
}

/// Handle to a running WebSocket reader task. Drop or call
/// [`Self::shutdown`] to close the connection.
pub struct WsHandle {
    rx: mpsc::Receiver<Result<WsMessage, WsError>>,
    task: JoinHandle<()>,
    shutdown: tokio_util::sync::CancellationToken,
    /// Cancels the same token, so dropping the handle without an explicit
    /// `shutdown().await` still closes the socket instead of leaving
    /// `reader_loop` parked on `stream.next()`. Cancellation is idempotent.
    _drop_guard: tokio_util::sync::DropGuard,
}

/// Long enough for a healthy loopback close round-trip, short enough that a
/// stuck reader task cannot block our caller's teardown.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(200);

impl WsHandle {
    pub async fn recv(&mut self) -> Option<Result<WsMessage, WsError>> {
        self.rx.recv().await
    }

    /// Close cleanly, falling back to `abort()` past `SHUTDOWN_GRACE`.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let mut task = self.task;
        if tokio::time::timeout(SHUTDOWN_GRACE, &mut task)
            .await
            .is_err()
        {
            task.abort();
        }
    }
}

/// Stream `session_id`'s events after `since` (`0` for a full replay).
pub async fn connect(
    endpoint: &DaemonEndpoint,
    session_id: &str,
    since: u64,
) -> Result<WsHandle, WsError> {
    connect_with(endpoint, session_id, since, true).await
}

/// [`connect`], but a consumer that renders only the folded projections passes
/// `forward_frames: false` so a long session's history is not shipped on every
/// open. The server still folds it to build the connect snapshots.
pub async fn connect_with(
    endpoint: &DaemonEndpoint,
    session_id: &str,
    since: u64,
    forward_frames: bool,
) -> Result<WsHandle, WsError> {
    let url = ws_url(endpoint, session_id, since, forward_frames);
    debug!(
        target: "acp.client.ws",
        // Log the path without the token query param.
        url = %sanitize_for_log(&url),
        "connecting to structured view ws"
    );
    let request = url
        .into_client_request()
        .map_err(|e| WsError::InvalidUrl(e.to_string()))?;
    let (stream, _) = connect_async(request).await?;
    let (frame_tx, frame_rx) = mpsc::channel(64);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let _drop_guard = shutdown.clone().drop_guard();
    let task = tokio::spawn(reader_loop(stream, frame_tx, shutdown.clone()));
    Ok(WsHandle {
        rx: frame_rx,
        task,
        shutdown,
        _drop_guard,
    })
}

async fn reader_loop(
    mut stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    tx: mpsc::Sender<Result<WsMessage, WsError>>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                let _ = stream
                    .send(Message::Close(Some(CloseFrame {
                        code: CloseCode::Normal,
                        reason: "client shutdown".into(),
                    })))
                    .await;
                return;
            }
            next = stream.next() => {
                match next {
                    Some(Ok(Message::Text(text))) => {
                        // `Ok(None)` is a keepalive: nothing to wake the consumer for.
                        let delivery = match parse_text(&text) {
                            Ok(None) => None,
                            Ok(Some(msg)) => Some(Ok(msg)),
                            Err(e) => Some(Err(e)),
                        };
                        if let Some(delivery) = delivery {
                            if tx.send(delivery).await.is_err() {
                                return; // consumer dropped
                            }
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = stream.send(Message::Pong(payload)).await;
                    }
                    // The daemon never sends binary; ignore defensively.
                    Some(Ok(
                        Message::Binary(_) | Message::Pong(_) | Message::Frame(_),
                    )) => {}
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|f| f.code);
                        let _ = tx.send(Err(WsError::UnexpectedClose(code))).await;
                        return;
                    }
                    Some(Err(e)) => {
                        let _ = tx.send(Err(WsError::Transport(e))).await;
                        return;
                    }
                    None => {
                        let _ = tx.send(Err(WsError::UnexpectedClose(None))).await;
                        return;
                    }
                }
            }
        }
    }
}

/// Decode one text frame: either an `AcpBroadcastFrame` event or a
/// `{"kind": ...}` control frame. `Ok(None)` is a frame with nothing for the
/// consumer to act on, distinct from `Err`, which consumers escalate to a
/// socket teardown and reconnect.
fn parse_text(raw: &str) -> Result<Option<WsMessage>, WsError> {
    // An event frame serializes only session_id/seq/event, so the presence of
    // `kind` alone marks a control frame. `Option<Option<_>>` tells an absent
    // `kind` apart from a present-but-null one.
    #[derive(serde::Deserialize)]
    struct KindProbe {
        #[serde(default, deserialize_with = "present")]
        kind: Option<Option<serde_json::Value>>,
    }
    fn present<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Option<serde_json::Value>>, D::Error> {
        Option::<serde_json::Value>::deserialize(d).map(Some)
    }
    #[derive(serde::Deserialize)]
    struct TranscriptSnapshotFrame {
        rows: Vec<TranscriptRow>,
    }
    #[derive(serde::Deserialize)]
    struct TranscriptDeltaFrame {
        delta: TranscriptDelta,
    }
    #[derive(serde::Deserialize)]
    struct ReducedStateFrame {
        seq: u64,
        state: AcpState,
        #[serde(default)]
        unchanged: Vec<String>,
    }
    if let Ok(KindProbe { kind: Some(kind) }) = serde_json::from_str::<KindProbe>(raw) {
        let kind = kind.unwrap_or(serde_json::Value::Null);
        match kind.as_str() {
            Some("lagged") => return Ok(Some(WsMessage::Lagged)),
            // App-level keepalive (#2287).
            Some("heartbeat") => return Ok(None),
            Some("transcript_snapshot") => {
                let frame: TranscriptSnapshotFrame =
                    serde_json::from_str(raw).map_err(|e| WsError::Parse(e.to_string()))?;
                return Ok(Some(WsMessage::TranscriptSnapshot(frame.rows)));
            }
            Some("transcript_delta") => {
                let frame: TranscriptDeltaFrame =
                    serde_json::from_str(raw).map_err(|e| WsError::Parse(e.to_string()))?;
                return Ok(Some(WsMessage::TranscriptDelta(Box::new(frame.delta))));
            }
            Some("reduced_state") => {
                let frame: ReducedStateFrame =
                    serde_json::from_str(raw).map_err(|e| WsError::Parse(e.to_string()))?;
                return Ok(Some(WsMessage::ReducedState {
                    seq: frame.seq,
                    state: Box::new(frame.state),
                    unchanged: frame.unchanged,
                }));
            }
            // A sentinel a newer daemon grew. Dropping it is safe: every
            // projection above is re-sent on connect and on each event (#3560).
            _ => {
                debug!(
                    target: "acp.client.ws",
                    kind = %kind,
                    "ignoring unrecognized ws control frame"
                );
                return Ok(None);
            }
        }
    }
    // No `kind` key, or not a JSON object: an event frame. A malformed one
    // surfaces as `WsError::Parse`, which the consumer treats as a dead socket.
    let frame: AcpBroadcastFrame = serde_json::from_str(raw).map_err(|e| {
        warn!(target: "acp.client.ws", error = %e, "ws frame parse failed");
        WsError::Parse(e.to_string())
    })?;
    Ok(Some(WsMessage::Frame(Arc::new(frame))))
}

fn ws_url(endpoint: &DaemonEndpoint, session_id: &str, since: u64, forward_frames: bool) -> String {
    let base = endpoint.ws_base_url();
    let path = format!("/sessions/{session_id}/acp/ws");
    let mut params: Vec<String> = Vec::new();
    if since > 0 {
        params.push(format!("since={since}"));
    }
    if !forward_frames {
        params.push("frames=0".to_string());
    }
    if let Some(token) = endpoint.resolved_token() {
        params.push(format!("token={token}"));
    }
    if params.is_empty() {
        format!("{base}{path}")
    } else {
        format!("{base}{path}?{}", params.join("&"))
    }
}

fn sanitize_for_log(url: &str) -> String {
    let Some((head, tail)) = url.split_once("token=") else {
        return url.to_string();
    };
    match tail.split_once('&') {
        Some((_, rest)) => format!("{head}token=<redacted>&{rest}"),
        None => format!("{head}token=<redacted>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;
    use crate::acp::state::Event;

    fn endpoint(base: &str, token: Option<&str>) -> DaemonEndpoint {
        DaemonEndpoint::new(base.to_string(), token.map(str::to_string), Source::Env)
    }

    /// `since=0` is omitted, `frames=0` marks a projections-only consumer, and
    /// an https endpoint upgrades to `wss`.
    #[test]
    fn ws_url_query_shape() {
        let e = endpoint("http://127.0.0.1:8080", Some("abc"));
        for (since, frames, want) in [
            (
                42,
                true,
                "ws://127.0.0.1:8080/sessions/s-1/acp/ws?since=42&token=abc",
            ),
            (
                42,
                false,
                "ws://127.0.0.1:8080/sessions/s-1/acp/ws?since=42&frames=0&token=abc",
            ),
            (0, true, "ws://127.0.0.1:8080/sessions/s-1/acp/ws?token=abc"),
        ] {
            assert_eq!(ws_url(&e, "s-1", since, frames), want);
        }
        assert_eq!(
            ws_url(&endpoint("http://127.0.0.1:8080", None), "s-1", 0, true),
            "ws://127.0.0.1:8080/sessions/s-1/acp/ws"
        );
        assert!(
            ws_url(&endpoint("https://remote.test", Some("t")), "s-1", 0, true)
                .starts_with("wss://")
        );
    }

    #[test]
    fn ws_url_uses_rotated_token_for_local_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("serve.token");
        let rotated = "b".repeat(64);
        std::fs::write(&token_path, &rotated).unwrap();
        let endpoint = DaemonEndpoint::new(
            "http://127.0.0.1:8080".into(),
            Some("a".repeat(64)),
            Source::LocalDaemon,
        )
        .with_local_token_path(token_path);

        assert_eq!(
            ws_url(&endpoint, "s-1", 0, true),
            format!("ws://127.0.0.1:8080/sessions/s-1/acp/ws?token={rotated}")
        );
    }

    /// Any present `kind` marks a control frame, whatever its JSON type, and
    /// one this build does not know must be dropped rather than fall through
    /// to the event parse. That fall-through failed on a missing `event`
    /// field and drove a reconnect loop against the heartbeat (#3171) and
    /// against the connect snapshot of a `frames=0` client (#3560).
    #[derive(Debug)]
    enum Expect {
        Lagged,
        Ignored,
        ParseError,
    }

    #[test]
    fn parse_text_classifies_kind_sentinels() {
        for (raw, expect) in [
            (r#"{"kind":"lagged"}"#, Expect::Lagged),
            (r#"{"kind":"heartbeat"}"#, Expect::Ignored),
            (r#"{"kind":"something_new"}"#, Expect::Ignored),
            (
                r#"{"kind":"something_new","session_id":"s-1","seq":9}"#,
                Expect::Ignored,
            ),
            (r#"{"kind":null}"#, Expect::Ignored),
            (r#"{"kind":42}"#, Expect::Ignored),
            // No `kind` and no event shape: genuinely malformed.
            (r#"{"session_id":"s-1","seq":9}"#, Expect::ParseError),
        ] {
            let got = parse_text(raw);
            match expect {
                Expect::Lagged => assert!(
                    matches!(got, Ok(Some(WsMessage::Lagged))),
                    "{raw}: expected Lagged, got {got:?}"
                ),
                Expect::Ignored => assert!(
                    matches!(got, Ok(None)),
                    "{raw}: expected to be ignored, got {got:?}"
                ),
                Expect::ParseError => {
                    assert!(got.is_err(), "{raw}: expected a parse error, got {got:?}")
                }
            }
        }
    }

    #[test]
    fn parse_text_frame() {
        let raw = serde_json::to_string(&serde_json::json!({
            "session_id": "s-1",
            "seq": 7,
            "event": "ThinkingStarted",
        }))
        .unwrap();
        let m = parse_text(&raw).unwrap();
        match m {
            Some(WsMessage::Frame(f)) => {
                assert_eq!(f.session_id, "s-1");
                assert_eq!(f.seq, 7);
                assert!(matches!(*f.event, Event::ThinkingStarted));
            }
            other => panic!("expected frame, got {other:?}"),
        }
    }

    #[test]
    fn parse_text_transcript_snapshot_and_delta() {
        let snapshot = serde_json::json!({
            "kind": "transcript_snapshot",
            "session_id": "s-1",
            "seq": 3,
            "rows": [{
                "id": "msg-1",
                "group_id": "g1",
                "kind": "message",
                "at": "2024-01-01T00:00:00Z",
                "text": "hi",
            }],
        })
        .to_string();
        match parse_text(&snapshot).unwrap() {
            Some(WsMessage::TranscriptSnapshot(rows)) => {
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].id, "msg-1");
                assert_eq!(rows[0].text, "hi");
            }
            other => panic!("expected snapshot, got {other:?}"),
        }

        let delta = serde_json::json!({
            "kind": "transcript_delta",
            "session_id": "s-1",
            "seq": 4,
            "delta": { "Remove": "msg-1" },
        })
        .to_string();
        match parse_text(&delta).unwrap() {
            Some(WsMessage::TranscriptDelta(boxed)) => match *boxed {
                TranscriptDelta::Remove(id) => assert_eq!(id, "msg-1"),
                other => panic!("expected Remove, got {other:?}"),
            },
            other => panic!("expected delta, got {other:?}"),
        }
    }

    #[test]
    fn parse_text_reads_the_reduced_state_frame() {
        // An omitted field must default: a parse error reads as a dead socket.
        let raw = serde_json::json!({
            "kind": "reduced_state",
            "session_id": "s-1",
            "seq": 7,
            "state": {
                "session_id": "s-1",
                "agent": "claude",
                "model": null,
                "mode": "Default",
                "current_plan": null,
                "todos": [],
                "in_flight_tool": null,
                "pending_approvals": [],
                "recent_diffs": [],
                "thinking": null,
                "rate_limit": null,
                "turn_active": true,
                "last_seq": 7,
                "updated_at": "2026-08-16T00:00:00Z",
            },
        })
        .to_string();
        match parse_text(&raw) {
            Ok(Some(WsMessage::ReducedState { seq, state, .. })) => {
                assert_eq!(seq, 7);
                assert!(state.turn_active);
                assert!(state.available_modes.is_empty(), "absent field defaults");
            }
            other => panic!("expected reduced state, got {other:?}"),
        }
    }

    #[test]
    fn sanitize_for_log_redacts_token() {
        assert_eq!(
            sanitize_for_log("ws://127.0.0.1/path?since=1&token=secret"),
            "ws://127.0.0.1/path?since=1&token=<redacted>"
        );
        assert_eq!(
            sanitize_for_log("ws://127.0.0.1/path?token=secret&since=1"),
            "ws://127.0.0.1/path?token=<redacted>&since=1"
        );
    }
}
