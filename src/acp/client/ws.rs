//! WebSocket client for the structured view broadcast stream.
//!
//! Subscribes to `/sessions/{id}/acp/ws?since=N` and yields a
//! stream of decoded events. The daemon may push these shapes:
//!
//! - An `AcpBroadcastFrame` (`session_id`, `seq`, `event`, no `kind`):
//!   the next replayed or live event.
//! - `{"kind":"lagged"}`: the in-memory ring buffer evicted events
//!   the client hadn't acked yet. The consumer must drop its local
//!   state and rehydrate via [`super::http::HttpClient::replay`].
//! - `{"kind":"heartbeat"}`: the app-level keepalive the daemon emits
//!   on every ping tick (`PING_INTERVAL` in `src/server/acp_ws.rs`).
//!   Carries no state, so the reader loop drops it without waking the
//!   consumer. A `kind` this build does not recognise is a control
//!   frame from a newer daemon and is dropped the same way, never
//!   parsed as an event frame (#3560). See `parse_text`.
//!
//! Native authentication uses a sensitive Authorization header, never the URL.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use tracing::debug;

use super::discovery::DaemonEndpoint;
use crate::acp::protocol::AcpBroadcastFrame;
use crate::acp::state::AcpState;
use crate::acp::transcript::{TranscriptDelta, TranscriptRow};
use crate::daemon::{
    websocket::{self, NativeSocket},
    WsError,
};

/// One message off the structured view WebSocket.
#[derive(Debug, Clone)]
pub enum WsMessage {
    /// A normal structured view event frame. Consumed by `aoe acp tail`,
    /// which dumps the raw stream; the structured view reads the two folded
    /// projections below instead (control state and transcript rows).
    Frame(Arc<AcpBroadcastFrame>),
    /// The server-folded CONTROL state (turn flags, approvals, elicitations,
    /// usage, modes, commands, plan), sent on connect and after every event.
    /// Boxed because `AcpState` dwarfs the other variants. Tier 1.3.
    ///
    /// `unchanged` names the cold fields the server omitted because this
    /// connection already has them (see `COLD_STATE_FIELDS` in
    /// `src/server/acp_ws.rs`). They deserialize to their empty defaults, so a
    /// consumer must keep what it holds for those rather than adopt the blank.
    ReducedState {
        seq: u64,
        state: Box<AcpState>,
        unchanged: Vec<String>,
    },
    /// Daemon's in-memory ring evicted events the client missed.
    /// Consumer should drop local reducer state and call
    /// `HttpClient::replay(since=last_seq)` to rehydrate.
    Lagged,
    /// Connect (and reconnect) snapshot of the server-folded transcript
    /// rows. The consumer reconciles these into its row buffer by id, so an
    /// overlap with an initial `?view=rows` replay is idempotent.
    TranscriptSnapshot(Vec<TranscriptRow>),
    /// One incremental row change the server folded from a live event.
    /// Boxed: a `Patch` carries a full `TranscriptRow`, which would otherwise
    /// bloat every `WsMessage` (and the `EmbeddedEvent` that wraps it).
    TranscriptDelta(Box<TranscriptDelta>),
}

/// Handle to a running WebSocket reader task. Drop or call
/// [`Self::shutdown`] to close the connection.
pub struct WsHandle {
    rx: mpsc::Receiver<Result<WsMessage, WsError>>,
    task: JoinHandle<()>,
    /// Cancellation signal observed by `reader_loop`. The previous
    /// shape used `mpsc::channel(1)` for a single shot signal; a
    /// `CancellationToken` is the same shape with the rest of the
    /// codebase (`state.shutdown`, tunnel watchdog) and avoids the
    /// `Option<Sender>` dance because cancellation is idempotent.
    shutdown: tokio_util::sync::CancellationToken,
    /// Drop-cancel: restores the prior `mpsc::Sender`-drop semantics
    /// from before #1295. Without this, dropping a `WsHandle` without
    /// an explicit `shutdown().await` would leave `reader_loop` parked
    /// on `stream.next()` instead of sending a Close frame and
    /// exiting. The guard cancels the same token on drop; the
    /// explicit `shutdown()` path's earlier `cancel()` is idempotent
    /// so there is no double-cancel hazard.
    _drop_guard: tokio_util::sync::DropGuard,
}

/// Wait this long for the reader task to send its close frame and
/// exit cleanly before falling back to `abort()`. Picked so a healthy
/// loopback round-trip lands well inside the budget while a stuck
/// task still doesn't block our caller's teardown.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(200);

impl WsHandle {
    pub async fn recv(&mut self) -> Option<Result<WsMessage, WsError>> {
        self.rx.recv().await
    }

    /// Ask the reader task to send a Close frame and finish cleanly.
    /// Falls back to `abort()` if the task doesn't finish within
    /// `SHUTDOWN_GRACE` so a stuck or already-aborted task can't
    /// block teardown.
    pub async fn shutdown(self) {
        self.shutdown.cancel();
        let mut task = self.task;
        match tokio::time::timeout(SHUTDOWN_GRACE, &mut task).await {
            Ok(_) => {}
            Err(_) => task.abort(),
        }
    }
}

/// Connect to the structured view broadcast stream for `session_id` starting
/// after `since` (use `0` for full replay). Returns a handle whose
/// `recv()` yields decoded messages until the stream ends or errors.
pub async fn connect(
    endpoint: &DaemonEndpoint,
    session_id: &str,
    since: u64,
) -> Result<WsHandle, WsError> {
    connect_with(endpoint, session_id, since, true).await
}

/// [`connect`], with control over whether the server forwards the raw event
/// frames. A consumer that renders only the folded projections (the native
/// structured view since Tier 1.3) passes `forward_frames: false` so a long
/// session's whole event history is not shipped on every open; the server
/// still folds it to build the connect snapshots.
pub async fn connect_with(
    endpoint: &DaemonEndpoint,
    session_id: &str,
    since: u64,
    forward_frames: bool,
) -> Result<WsHandle, WsError> {
    let session_id = crate::daemon::transport::path_segment(session_id)?;
    let path = format!("/sessions/{session_id}/acp/ws");
    let query = format!("since={since}&frames={}", u8::from(forward_frames));
    match websocket::connect(endpoint, &path, Some(&query)).await? {
        NativeSocket::Unix(stream) => Ok(start_reader(*stream)),
        NativeSocket::Tcp(stream) => Ok(start_reader(*stream)),
    }
}

fn start_reader<S>(stream: WebSocketStream<S>) -> WsHandle
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (frame_tx, frame_rx) = mpsc::channel(64);
    let shutdown = tokio_util::sync::CancellationToken::new();
    let _drop_guard = shutdown.clone().drop_guard();
    let task = tokio::spawn(reader_loop(stream, frame_tx, shutdown.clone()));
    WsHandle {
        rx: frame_rx,
        task,
        shutdown,
        _drop_guard,
    }
}

async fn reader_loop<S>(
    mut stream: WebSocketStream<S>,
    tx: mpsc::Sender<Result<WsMessage, WsError>>,
    shutdown: tokio_util::sync::CancellationToken,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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
                        match parse_text(&text) {
                            // Keepalive: no consumer-visible state, so
                            // don't wake the consumer at all.
                            Ok(None) => {}
                            Ok(Some(msg)) => {
                                if tx.send(Ok(msg)).await.is_err() {
                                    return; // consumer dropped
                                }
                            }
                            Err(e) => {
                                if tx.send(Err(e)).await.is_err() {
                                    return; // consumer dropped
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        // Daemon never sends binary; ignore defensively.
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        let _ = stream.send(Message::Pong(payload)).await;
                    }
                    Some(Ok(Message::Pong(_))) | Some(Ok(Message::Frame(_))) => {}
                    Some(Ok(Message::Close(frame))) => {
                        let code = frame.as_ref().map(|f| f.code);
                        let _ = tx.send(Err(WsError::UnexpectedClose(code))).await;
                        return;
                    }
                    Some(Err(e)) => {
                        let _ = tx.send(Err(WsError::from(e))).await;
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

/// Decode one text frame. `Ok(None)` means the frame was a sentinel with
/// nothing for the consumer to act on (the daemon's keepalive), which is
/// distinct from `Err` because consumers escalate a parse error to a
/// socket teardown and reconnect.
fn parse_text(raw: &str) -> Result<Option<WsMessage>, WsError> {
    // The daemon sends an `AcpBroadcastFrame` JSON object or a
    // `{ "kind": ... }` control frame. A real frame never carries `kind`
    // (its serializer emits only session_id/seq/event), so the key's
    // presence alone marks a control frame, whatever its value.
    // `Option<Option<_>>` with the helper below tells an absent `kind` apart
    // from a present-but-null one: only absence means "event frame".
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
            // App-level keepalive (#2287). A real frame always carries
            // `session_id`/`seq`/`event` and never a `kind`, so this
            // cannot shadow one.
            Some("heartbeat") => return Ok(None),
            // Server-folded transcript rows (Tier 4). The connect snapshot
            // carries every row; each live event carries its row delta.
            Some("transcript_snapshot") => {
                let frame: TranscriptSnapshotFrame =
                    serde_json::from_str(raw).map_err(|_| WsError::Parse)?;
                return Ok(Some(WsMessage::TranscriptSnapshot(frame.rows)));
            }
            Some("transcript_delta") => {
                let frame: TranscriptDeltaFrame =
                    serde_json::from_str(raw).map_err(|_| WsError::Parse)?;
                return Ok(Some(WsMessage::TranscriptDelta(Box::new(frame.delta))));
            }
            // Server-folded control state (Tier 1.3), sent on connect and
            // after every event.
            Some("reduced_state") => {
                let frame: ReducedStateFrame =
                    serde_json::from_str(raw).map_err(|_| WsError::Parse)?;
                return Ok(Some(WsMessage::ReducedState {
                    seq: frame.seq,
                    state: Box::new(frame.state),
                    unchanged: frame.unchanged,
                }));
            }
            // A control frame this build does not consume, typically a
            // sentinel a newer daemon grew. Dropping it is safe: the
            // projections above are re-sent on every event and on connect
            // (#3560).
            _ => {
                debug!(
                    target: "acp.client.ws",
                    "ignoring unrecognized ws control frame"
                );
                return Ok(None);
            }
        }
    }
    // No `kind` key (or not a JSON object): parse as a raw event frame. A
    // genuinely malformed frame fails here and surfaces as WsError::Parse,
    // which the consumer treats as a dropped socket.
    let frame: AcpBroadcastFrame = serde_json::from_str(raw).map_err(|_| WsError::Parse)?;
    Ok(Some(WsMessage::Frame(Arc::new(frame))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;
    use crate::acp::state::Event;

    #[tokio::test]
    async fn websocket_auth_never_uses_url_credentials() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_hdr_async(
                tcp,
                |request: &tokio_tungstenite::tungstenite::handshake::server::Request, response| {
                    assert_eq!(
                        request.uri().path_and_query().unwrap().as_str(),
                        "/sessions/s%2F1%3Fx%23%25/acp/ws?since=42&frames=0"
                    );
                    assert_eq!(
                        request.headers().get("authorization").unwrap(),
                        format!("Bearer {}", "b".repeat(64)).as_str()
                    );
                    Ok(response)
                },
            )
            .await
            .unwrap();
            let _ = ws.next().await;
        });
        let endpoint = DaemonEndpoint::new(
            format!("http://{address}"),
            Some("b".repeat(64)),
            Source::Env,
        );
        let handle = connect_with(&endpoint, "s/1?x#%", 42, false).await.unwrap();
        handle.shutdown().await;
        server.await.unwrap();
    }

    /// How each `{"kind":...}` frame the daemon can send must classify, and
    /// how a `kind`-less object must classify.
    ///
    /// The heartbeat row is the #3171 regression: the daemon emits
    /// `{"kind":"heartbeat"}` every `PING_INTERVAL` (30s); before it was
    /// handled it fell through to the `AcpBroadcastFrame` parse, failed on a
    /// missing field, and surfaced as `WsError::Parse`, which
    /// `tui::structured_view` treats as a dropped socket: an error toast plus
    /// a full reconnect every 30 seconds on any quiet session.
    ///
    /// The `something_new` rows are the general case: a `frames=0` client
    /// receives only `kind`-tagged control frames, so any `kind` this build
    /// does not recognize (a newer daemon's sentinel) must be ignored rather
    /// than fall through to the event-frame parse. That fall-through failed
    /// with "missing field `event`" and drove acp.tui.ws into a tight
    /// reconnect loop against the connect-snapshot control frame.
    #[derive(Debug)]
    enum Expect {
        Lagged,
        Ignored,
        ParseError,
    }

    #[test]
    fn parse_text_classifies_kind_sentinels() {
        let cases = [
            // Ring buffer evicted events; consumer must rehydrate.
            (r#"{"kind":"lagged"}"#, Expect::Lagged),
            // Keepalive: no consumer-visible state, must not wake the
            // consumer and must not read as a dropped socket.
            (r#"{"kind":"heartbeat"}"#, Expect::Ignored),
            // A sentinel this build does not know is dropped, not escalated
            // to a reconnect (#3560); the daemon re-sends every projection.
            (r#"{"kind":"something_new"}"#, Expect::Ignored),
            (
                r#"{"kind":"something_new","session_id":"s-1","seq":9}"#,
                Expect::Ignored,
            ),
            (r#"{"kind":null}"#, Expect::Ignored),
            // A present `kind` of a non-string JSON type still marks a
            // control frame: it must be ignored, never routed to the
            // event parse.
            (r#"{"kind":42}"#, Expect::Ignored),
            // No `kind` and no event shape: genuinely malformed.
            (r#"{"session_id":"s-1","seq":9}"#, Expect::ParseError),
        ];
        for (raw, expect) in cases {
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
        // The connect snapshot yields the row buffer; a live delta yields
        // one row change. Both are keyed by id so the consumer reconciles
        // idempotently against a `?view=rows` replay overlap.
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
        // The whole control state rides on this frame, and the fields the
        // sender omits must default rather than fail the parse: a parse error
        // reads as a dead socket to the consumer.
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
}
