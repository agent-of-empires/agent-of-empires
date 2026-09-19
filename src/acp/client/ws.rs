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
//! Auth: the bearer token is sent as a `?token=<>` query string on the
//! WebSocket URL. Most WS clients do not surface custom headers cleanly,
//! and the daemon's auth middleware already accepts the query-param
//! form (see `src/server/auth.rs`). The token is *not* logged anywhere
//! the URL string is exposed (we log only the base URL).
//!
//! A `--auth=passphrase` daemon never mints a bearer token, so when only a
//! passphrase resolves the upgrade request instead carries `Cookie:
//! aoe_session=...` and `Sec-WebSocket-Protocol: aoe-device.<secret>`
//! headers, the same passphrase-login session `HttpClient` uses (see
//! `passphrase_session`).

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
use super::http::HttpError;
use super::passphrase_session::{self, PassphraseSessionCache};
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
    /// A daemon frame failed to deserialise. Surfaced to the caller so
    /// a toast like "ws: parse error" carries the real reason instead
    /// of a fabricated transport error.
    #[error("failed to parse websocket frame: {0}")]
    Parse(String),
    /// The passphrase-login handshake (see `passphrase_session::login`)
    /// failed before the WS upgrade was even attempted.
    #[error("passphrase login failed: {0}")]
    Auth(#[from] HttpError),
}

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
    let url = ws_url(endpoint, session_id, since, forward_frames);
    debug!(
        target: "acp.client.ws",
        // Log the path without the token query param.
        url = %sanitize_for_log(&url),
        "connecting to structured view ws"
    );
    let cache = PassphraseSessionCache::default();
    let stream = connect_authed(endpoint, &url, &cache).await?;
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

/// Connect the WS upgrade, retrying once after a fresh passphrase login if a
/// *cached* session was rejected (rotated passphrase, expired or evicted
/// session). A first-ever login failure is not retried: the passphrase
/// itself is wrong, not merely stale.
async fn connect_authed(
    endpoint: &DaemonEndpoint,
    url: &str,
    cache: &PassphraseSessionCache,
) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, WsError> {
    let had_cached_session = cache.get(endpoint).is_some();
    let request = build_authed_request(endpoint, url, cache).await?;
    match connect_async(request).await {
        Ok((stream, _)) => Ok(stream),
        Err(err) if had_cached_session && is_unauthorized(&err) => {
            cache.invalidate(endpoint);
            let request = build_authed_request(endpoint, url, cache).await?;
            let (stream, _) = connect_async(request).await?;
            Ok(stream)
        }
        Err(err) => Err(err.into()),
    }
}

/// Build the WS upgrade request. A resolved bearer token already rides the
/// URL (see [`ws_url`]); a passphrase instead needs the login session's
/// `aoe_session` cookie and device-binding secret attached as headers,
/// logging in on first use.
async fn build_authed_request(
    endpoint: &DaemonEndpoint,
    url: &str,
    cache: &PassphraseSessionCache,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>, WsError> {
    let mut request = url
        .into_client_request()
        .map_err(|e| WsError::InvalidUrl(e.to_string()))?;
    if endpoint.resolved_token().is_none() && endpoint.resolved_passphrase().is_some() {
        let session = match cache.get(endpoint) {
            Some(session) => session,
            None => passphrase_session::login(&reqwest::Client::new(), endpoint, cache).await?,
        };
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::COOKIE,
            session
                .cookie
                .parse()
                .map_err(|_| WsError::InvalidUrl("invalid session cookie".to_string()))?,
        );
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
            format!("aoe-device.{}", session.binding_secret)
                .parse()
                .map_err(|_| WsError::InvalidUrl("invalid device binding secret".to_string()))?,
        );
    }
    Ok(request)
}

fn is_unauthorized(err: &tokio_tungstenite::tungstenite::Error) -> bool {
    matches!(
        err,
        tokio_tungstenite::tungstenite::Error::Http(response) if response.status().as_u16() == 401
    )
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
                    serde_json::from_str(raw).map_err(|e| WsError::Parse(e.to_string()))?;
                return Ok(Some(WsMessage::TranscriptSnapshot(frame.rows)));
            }
            Some("transcript_delta") => {
                let frame: TranscriptDeltaFrame =
                    serde_json::from_str(raw).map_err(|e| WsError::Parse(e.to_string()))?;
                return Ok(Some(WsMessage::TranscriptDelta(Box::new(frame.delta))));
            }
            // Server-folded control state (Tier 1.3), sent on connect and
            // after every event.
            Some("reduced_state") => {
                let frame: ReducedStateFrame =
                    serde_json::from_str(raw).map_err(|e| WsError::Parse(e.to_string()))?;
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
                    kind = %kind,
                    "ignoring unrecognized ws control frame"
                );
                return Ok(None);
            }
        }
    }
    // No `kind` key (or not a JSON object): parse as a raw event frame. A
    // genuinely malformed frame fails here and surfaces as WsError::Parse,
    // which the consumer treats as a dropped socket.
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
    match url.split_once("token=") {
        Some((head, tail)) => {
            let rest = tail.split_once('&').map(|(_, r)| r).unwrap_or("");
            if rest.is_empty() {
                format!("{head}token=<redacted>")
            } else {
                format!("{head}token=<redacted>&{rest}")
            }
        }
        None => url.to_string(),
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

    #[test]
    fn ws_url_appends_since_and_token() {
        let e = endpoint("http://127.0.0.1:8080", Some("abc"));
        let url = ws_url(&e, "s-1", 42, true);
        assert_eq!(
            url,
            "ws://127.0.0.1:8080/sessions/s-1/acp/ws?since=42&token=abc"
        );
        // A projections-only consumer asks the daemon to skip the raw frames.
        assert_eq!(
            ws_url(&e, "s-1", 42, false),
            "ws://127.0.0.1:8080/sessions/s-1/acp/ws?since=42&frames=0&token=abc"
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

    #[test]
    fn ws_url_omits_since_when_zero() {
        let e = endpoint("http://127.0.0.1:8080", None);
        assert_eq!(
            ws_url(&e, "s-1", 0, true),
            "ws://127.0.0.1:8080/sessions/s-1/acp/ws"
        );
    }

    #[test]
    fn ws_url_uses_wss_for_https_endpoint() {
        let e = endpoint("https://remote.example.com", Some("t"));
        assert!(ws_url(&e, "s-1", 0, true).starts_with("wss://"));
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

    #[tokio::test]
    #[serial_test::serial]
    async fn build_authed_request_adds_no_headers_when_bearer_token_resolves() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let e = endpoint("http://127.0.0.1:8080", Some("tok"));
        let cache = PassphraseSessionCache::default();
        let request = build_authed_request(&e, "ws://127.0.0.1:8080/sessions/s-1/acp/ws", &cache)
            .await
            .unwrap();
        assert!(request
            .headers()
            .get(tokio_tungstenite::tungstenite::http::header::COOKIE)
            .is_none());
        assert!(request
            .headers()
            .get(tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL)
            .is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn build_authed_request_adds_no_headers_without_token_or_passphrase() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let e = endpoint("http://127.0.0.1:8080", None);
        let cache = PassphraseSessionCache::default();
        let request = build_authed_request(&e, "ws://127.0.0.1:8080/sessions/s-1/acp/ws", &cache)
            .await
            .unwrap();
        assert!(request
            .headers()
            .get(tokio_tungstenite::tungstenite::http::header::COOKIE)
            .is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn build_authed_request_sends_cached_passphrase_session_as_headers() {
        let _env = crate::session::test_support::EnvGuard::unset(&["AOE_DAEMON_PASSPHRASE"]);
        let dir = tempfile::tempdir().unwrap();
        let passphrase_path = dir.path().join("serve.passphrase");
        std::fs::write(&passphrase_path, "hunter2").unwrap();
        let e = DaemonEndpoint::new("http://127.0.0.1:8080".into(), None, Source::LocalDaemon)
            .with_local_passphrase_path(passphrase_path);
        let cache = PassphraseSessionCache::default();
        cache.set_for_test(passphrase_session::PassphraseSession {
            cookie: "aoe_session=abc123".to_string(),
            binding_secret: "the-binding-secret".to_string(),
        });

        let request = build_authed_request(&e, "ws://127.0.0.1:8080/sessions/s-1/acp/ws", &cache)
            .await
            .unwrap();
        assert_eq!(
            request
                .headers()
                .get(tokio_tungstenite::tungstenite::http::header::COOKIE)
                .unwrap(),
            "aoe_session=abc123"
        );
        assert_eq!(
            request
                .headers()
                .get(tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL)
                .unwrap(),
            "aoe-device.the-binding-secret"
        );
    }

    #[test]
    fn is_unauthorized_matches_only_http_401() {
        use tokio_tungstenite::tungstenite::http::{Response, StatusCode};
        use tokio_tungstenite::tungstenite::Error as TError;

        let unauthorized = Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(None)
            .unwrap();
        assert!(is_unauthorized(&TError::Http(Box::new(unauthorized))));

        let forbidden = Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(None)
            .unwrap();
        assert!(!is_unauthorized(&TError::Http(Box::new(forbidden))));
        assert!(!is_unauthorized(&TError::ConnectionClosed));
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
