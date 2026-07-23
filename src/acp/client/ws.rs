//! WebSocket client for the structured view broadcast stream.
//!
//! Subscribes to `/sessions/{id}/acp/ws?since=N` and yields a
//! stream of decoded events. The daemon may push two shapes:
//!
//! - `{"kind":"frame", ...AcpBroadcastFrame}`: the next replayed
//!   or live event.
//! - `{"kind":"lagged"}`: the in-memory ring buffer evicted events
//!   the client hadn't acked yet. The consumer must drop its local
//!   state and rehydrate via [`super::http::HttpClient::replay`].
//!
//! Auth: the bearer token is sent as a `?token=<>` query string on the
//! WebSocket URL. Most WS clients do not surface custom headers cleanly,
//! and the daemon's auth middleware already accepts the query-param
//! form (see `src/server/auth.rs`). The token is *not* logged anywhere
//! the URL string is exposed (we log only the base URL).

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
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
}

/// One message off the structured view WebSocket.
#[derive(Debug, Clone)]
pub enum WsMessage {
    /// A normal structured view event frame.
    Frame(Arc<AcpBroadcastFrame>),
    /// Daemon's in-memory ring evicted events the client missed.
    /// Consumer should drop local reducer state and call
    /// `HttpClient::replay(since=last_seq)` to rehydrate.
    Lagged,
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
    let url = ws_url(endpoint, session_id, since);
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
                        let msg = parse_text(&text);
                        if tx.send(msg).await.is_err() {
                            return; // consumer dropped
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

fn parse_text(raw: &str) -> Result<WsMessage, WsError> {
    // The daemon sends either a `AcpBroadcastFrame` JSON object or
    // a `{ "kind": "lagged" }` sentinel. We try the sentinel first
    // (cheap discriminant probe) and fall back to a full frame parse.
    #[derive(serde::Deserialize)]
    struct KindProbe<'a> {
        kind: Option<&'a str>,
    }
    if let Ok(probe) = serde_json::from_str::<KindProbe>(raw) {
        if probe.kind == Some("lagged") {
            return Ok(WsMessage::Lagged);
        }
    }
    let frame: AcpBroadcastFrame = serde_json::from_str(raw).map_err(|e| {
        warn!(target: "acp.client.ws", error = %e, "ws frame parse failed");
        WsError::Parse(e.to_string())
    })?;
    Ok(WsMessage::Frame(Arc::new(frame)))
}

fn ws_url(endpoint: &DaemonEndpoint, session_id: &str, since: u64) -> String {
    let base = endpoint.ws_base_url();
    let path = format!("/sessions/{session_id}/acp/ws");
    let mut params: Vec<String> = Vec::new();
    if since > 0 {
        params.push(format!("since={since}"));
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
        let url = ws_url(&e, "s-1", 42);
        assert_eq!(
            url,
            "ws://127.0.0.1:8080/sessions/s-1/acp/ws?since=42&token=abc"
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
            ws_url(&endpoint, "s-1", 0),
            format!("ws://127.0.0.1:8080/sessions/s-1/acp/ws?token={rotated}")
        );
    }

    #[test]
    fn ws_url_omits_since_when_zero() {
        let e = endpoint("http://127.0.0.1:8080", None);
        assert_eq!(
            ws_url(&e, "s-1", 0),
            "ws://127.0.0.1:8080/sessions/s-1/acp/ws"
        );
    }

    #[test]
    fn ws_url_uses_wss_for_https_endpoint() {
        let e = endpoint("https://remote.example.com", Some("t"));
        assert!(ws_url(&e, "s-1", 0).starts_with("wss://"));
    }

    #[test]
    fn parse_text_lagged_sentinel() {
        let m = parse_text(r#"{"kind":"lagged"}"#).unwrap();
        assert!(matches!(m, WsMessage::Lagged));
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
            WsMessage::Frame(f) => {
                assert_eq!(f.session_id, "s-1");
                assert_eq!(f.seq, 7);
                assert!(matches!(*f.event, Event::ThinkingStarted));
            }
            WsMessage::Lagged => panic!("expected frame"),
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
