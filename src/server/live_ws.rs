//! Capture-snapshot live view for the web dashboard (mobile).
//!
//! Mirrors the TUI's live-send architecture instead of the PTY attach
//! relay: the server polls `tmux capture-pane` (cursor folded into the
//! same fork) and pushes ANSI snapshot frames over the WebSocket;
//! browser input comes back as raw bytes and is delivered via
//! `tmux send-keys -H`. No PTY, no `tmux attach`, no SIGSTOP pause:
//! scrollback is just a bigger capture window the client renders and
//! scrolls natively, and the agent keeps running while the user reads.
//!
//! Protocol (one WS per viewer, route `/sessions/{id}/live-ws`):
//!
//! Server -> client, JSON text frames:
//!   `{"type":"frame","content":"<ANSI text>","cursor":{"x":..,"y":..,
//!     "visible":..,"screen_rows":..}|null}`
//!   `content` is verbatim `capture-pane -e` output for the requested
//!   window: history lines first, the live screen as the last
//!   `screen_rows` lines (trailing blank screen rows preserved).
//!
//! Client -> server:
//!   Binary frames: raw bytes for the pane (keystrokes, escape
//!     sequences, bracketed paste). Dropped in read-only mode.
//!   `{"type":"resize","cols":..,"rows":..}`: resize the (detached)
//!     tmux window to the client's grid. Restored to `window-size
//!     latest` when the last live client that resized disconnects.
//!   `{"type":"window","lines":N}`: total capture window (history +
//!     screen). Clamped to [screen rows, MAX_WINDOW_LINES].
//!   `{"type":"cadence","fast":bool}`: capture cadence. Fast while the
//!     client is at the live edge and visible; idle while scrolled up
//!     or backgrounded.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tracing::{debug, warn};

use super::ws::{
    close_early, wait_for_tmux_ready, PaneReadiness, CLOSE_CODE_GOING_AWAY, CLOSE_CODE_PTY_DEAD,
    CLOSE_CODE_TRY_AGAIN_LATER,
};
use super::AppState;

/// Capture cadence while the client is at the live edge. Matches the
/// TUI's live-send fast interval: tight enough that typed echo feels
/// attach-like, while the content dedup keeps idle panes free.
const CAPTURE_INTERVAL_FAST_MS: u64 = 50;
/// Cadence while the client reads scrollback or is backgrounded. The
/// scrolled-up window can be thousands of lines, so frames are big;
/// at this rate a streaming agent costs at most a few frames per second.
const CAPTURE_INTERVAL_IDLE_MS: u64 = 250;
/// Upper bound on the capture window. tmux history defaults to 2000
/// lines per pane; this leaves headroom for raised limits without
/// letting a client demand unbounded captures.
const MAX_WINDOW_LINES: usize = 4000;
/// Floor for the capture window when the client hasn't sized yet.
const DEFAULT_WINDOW_LINES: usize = 50;
/// Keepalive ping interval; the recv side relies on the browser's pong.
const PING_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
#[serde(tag = "type")]
enum LiveControlMessage {
    #[serde(rename = "resize")]
    Resize { cols: u16, rows: u16 },
    #[serde(rename = "window")]
    Window { lines: usize },
    #[serde(rename = "cadence")]
    Cadence { fast: bool },
}

/// Shared per-connection knobs the recv loop writes and the capture
/// loop reads.
struct LiveSettings {
    window_lines: AtomicUsize,
    fast: AtomicBool,
    /// Rows from the latest client resize; used as the window floor so a
    /// shrunk window can never clip the live screen.
    screen_rows: AtomicU64,
}

pub async fn live_terminal_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    debug!(target: "terminal.ws", session = %id, kind = "live", "ws route entered");
    let instances = state.instances.read().await;
    let tmux_name = instances
        .iter()
        .find(|i| i.id == id)
        .map(|inst| crate::tmux::Session::generate_name(&inst.id, &inst.title));
    drop(instances);

    let read_only = state.read_only;
    let shutdown = state.shutdown.clone();

    match tmux_name {
        Some(tmux_name) => ws
            .protocols(["aoe-auth"])
            .on_upgrade(move |socket| handle_live_ws(socket, tmux_name, read_only, shutdown))
            .into_response(),
        None => {
            warn!(target: "terminal.ws", session = %id, kind = "live", "session not found, returning 404");
            (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response()
        }
    }
}

async fn handle_live_ws(
    mut socket: WebSocket,
    tmux_name: String,
    read_only: bool,
    shutdown: tokio_util::sync::CancellationToken,
) {
    match wait_for_tmux_ready(&tmux_name).await {
        PaneReadiness::Ready => {}
        PaneReadiness::Dead => {
            warn!(target: "terminal.ws", tmux = %tmux_name, kind = "live", "pane dead, closing 4001");
            close_early(&mut socket, CLOSE_CODE_PTY_DEAD, "pty_dead").await;
            return;
        }
        PaneReadiness::NotReady => {
            warn!(target: "terminal.ws", tmux = %tmux_name, kind = "live", "tmux not ready, closing 1013");
            close_early(&mut socket, CLOSE_CODE_TRY_AGAIN_LATER, "tmux_not_ready").await;
            return;
        }
    }

    let settings = Arc::new(LiveSettings {
        window_lines: AtomicUsize::new(DEFAULT_WINDOW_LINES),
        fast: AtomicBool::new(true),
        screen_rows: AtomicU64::new(0),
    });
    // Wakes the capture loop out of its inter-capture sleep: after
    // dispatched input (echo latency) and after cadence/window changes.
    let nudge = Arc::new(tokio::sync::Notify::new());

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Frames and pings funnel through one channel so the sender task is
    // the only writer on the socket.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(8);

    // Capture loop: fork capture-pane (+cursor) off the async runtime,
    // dedup, publish.
    let capture_settings = Arc::clone(&settings);
    let capture_nudge = Arc::clone(&nudge);
    let capture_tx = out_tx.clone();
    let capture_tmux = tmux_name.clone();
    let capture_task = tokio::spawn(async move {
        let mut last_published: Option<(String, Option<crate::tmux::PaneCursor>)> = None;
        let mut dead_probes: u32 = 0;
        loop {
            let lines = capture_settings.window_lines.load(Ordering::Relaxed);
            let name = capture_tmux.clone();
            let captured = tokio::task::spawn_blocking(move || {
                let session = crate::tmux::Session::from_name(&name);
                session.capture_pane_with_cursor(lines)
            })
            .await;

            match captured {
                Ok(Ok((content, cursor))) if !content.is_empty() || cursor.is_some() => {
                    dead_probes = 0;
                    let frame = (content, cursor);
                    if last_published.as_ref() != Some(&frame) {
                        let json = frame_json(&frame.0, frame.1.as_ref());
                        if capture_tx.send(Message::Text(json.into())).await.is_err() {
                            break; // socket gone
                        }
                        last_published = Some(frame);
                    }
                }
                Ok(Ok(_)) => {
                    // Empty capture AND no cursor: the session is most
                    // likely gone (capture helpers return empty on a
                    // missing session). Require a few consecutive misses
                    // before declaring the pane dead so a transient tmux
                    // hiccup doesn't kill the connection.
                    dead_probes += 1;
                    if dead_probes >= 3 {
                        let _ = capture_tx
                            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: CLOSE_CODE_PTY_DEAD,
                                reason: "pty_dead".into(),
                            })))
                            .await;
                        break;
                    }
                }
                _ => break, // join error / capture error: bail quietly
            }

            let ms = if capture_settings.fast.load(Ordering::Relaxed) {
                CAPTURE_INTERVAL_FAST_MS
            } else {
                CAPTURE_INTERVAL_IDLE_MS
            };
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                _ = capture_nudge.notified() => {}
            }
        }
    });

    // Sender task: sole socket writer; also emits keepalive pings.
    let send_task = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // arm: first tick fires immediately otherwise
        loop {
            tokio::select! {
                msg = out_rx.recv() => {
                    match msg {
                        Some(Message::Close(frame)) => {
                            let _ = ws_sender.send(Message::Close(frame)).await;
                            break;
                        }
                        Some(msg) => {
                            if ws_sender.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping.tick() => {
                    if ws_sender.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Recv loop: input bytes + control messages, until close/shutdown.
    let mut resized = false;
    loop {
        tokio::select! {
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        if read_only || data.is_empty() {
                            continue;
                        }
                        let name = tmux_name.clone();
                        let bytes = data.to_vec();
                        let send_nudge = Arc::clone(&nudge);
                        // Off-runtime: send-keys forks a subprocess.
                        let _ = tokio::task::spawn_blocking(move || {
                            let session = crate::tmux::Session::from_name(&name);
                            if let Err(e) = session.send_raw_bytes(&bytes) {
                                warn!(target: "terminal.ws", tmux = %name, kind = "live", "send_raw_bytes failed: {}", e);
                            }
                        })
                        .await;
                        // Capture the echo promptly rather than waiting out
                        // the current sleep.
                        send_nudge.notify_one();
                    }
                    Some(Ok(Message::Text(text))) => {
                        let Ok(control) = serde_json::from_str::<LiveControlMessage>(&text) else {
                            continue;
                        };
                        match control {
                            LiveControlMessage::Resize { cols, rows } => {
                                if read_only || cols == 0 || rows == 0 {
                                    continue;
                                }
                                settings.screen_rows.store(rows as u64, Ordering::Relaxed);
                                // Never let the capture window clip the screen.
                                let floor = rows as usize;
                                if settings.window_lines.load(Ordering::Relaxed) < floor {
                                    settings.window_lines.store(floor, Ordering::Relaxed);
                                }
                                let name = tmux_name.clone();
                                let _ = tokio::task::spawn_blocking(move || {
                                    crate::tmux::Session::from_name(&name).resize_window(cols, rows);
                                })
                                .await;
                                resized = true;
                                nudge.notify_one();
                            }
                            LiveControlMessage::Window { lines } => {
                                let floor = (settings.screen_rows.load(Ordering::Relaxed) as usize)
                                    .max(DEFAULT_WINDOW_LINES);
                                let clamped = lines.clamp(floor, MAX_WINDOW_LINES);
                                settings.window_lines.store(clamped, Ordering::Relaxed);
                                nudge.notify_one();
                            }
                            LiveControlMessage::Cadence { fast } => {
                                settings.fast.store(fast, Ordering::Relaxed);
                                if fast {
                                    nudge.notify_one();
                                }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // Ping/Pong handled by axum
                    Some(Err(e)) => {
                        debug!(target: "terminal.ws", tmux = %tmux_name, kind = "live", "ws recv error: {}", e);
                        break;
                    }
                }
            }
            _ = shutdown.cancelled() => {
                let _ = out_tx
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: CLOSE_CODE_GOING_AWAY,
                        reason: "server shutdown".into(),
                    })))
                    .await;
                break;
            }
        }
    }

    capture_task.abort();
    drop(out_tx);
    let _ = send_task.await;

    // Live-view resizes flip the window-size option to manual (tmux
    // behavior); restore automatic sizing so a later full-size attach
    // isn't pinned at phone dimensions. Mirrors the TUI's live-send exit.
    if resized {
        let name = tmux_name.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::tmux::Session::from_name(&name).reset_size_to_latest_client();
        })
        .await;
    }
    debug!(target: "terminal.ws", tmux = %tmux_name, kind = "live", "live ws closed");
}

/// Serialize one snapshot frame. `rows` (pane height) and `history`
/// (scrollback line count) ride at the top level: the client sizes its
/// virtual scroll spacer off `history` and slices the live screen off
/// the content's last `rows` lines, independent of cursor visibility.
fn frame_json(content: &str, cursor: Option<&crate::tmux::PaneCursor>) -> String {
    let cursor_value = match cursor {
        Some(c) if c.visible => serde_json::json!({
            "x": c.x,
            "y": c.y,
        }),
        _ => serde_json::Value::Null,
    };
    serde_json::json!({
        "type": "frame",
        "content": content,
        "rows": cursor.map(|c| c.pane_height).unwrap_or(0),
        "history": cursor.map(|c| c.history_size).unwrap_or(0),
        "cursor": cursor_value,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_json_includes_geometry_and_cursor() {
        let cursor = crate::tmux::PaneCursor {
            x: 3,
            y: 7,
            visible: true,
            pane_height: 46,
            history_size: 1200,
        };
        let json = frame_json("hello\nworld", Some(&cursor));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "frame");
        assert_eq!(v["content"], "hello\nworld");
        assert_eq!(v["rows"], 46);
        assert_eq!(v["history"], 1200);
        assert_eq!(v["cursor"]["x"], 3);
        assert_eq!(v["cursor"]["y"], 7);
    }

    #[test]
    fn frame_json_hides_cursor_when_dectcem_off() {
        let cursor = crate::tmux::PaneCursor {
            x: 3,
            y: 7,
            visible: false,
            pane_height: 46,
            history_size: 0,
        };
        let v: serde_json::Value = serde_json::from_str(&frame_json("x", Some(&cursor))).unwrap();
        assert!(v["cursor"].is_null());
        assert_eq!(v["rows"], 46);
    }

    #[test]
    fn frame_json_null_cursor() {
        let v: serde_json::Value = serde_json::from_str(&frame_json("x", None)).unwrap();
        assert!(v["cursor"].is_null());
        assert_eq!(v["rows"], 0);
    }

    #[test]
    fn control_messages_parse() {
        let m: LiveControlMessage =
            serde_json::from_str(r#"{"type":"resize","cols":74,"rows":46}"#).unwrap();
        assert!(matches!(
            m,
            LiveControlMessage::Resize { cols: 74, rows: 46 }
        ));
        let m: LiveControlMessage =
            serde_json::from_str(r#"{"type":"window","lines":800}"#).unwrap();
        assert!(matches!(m, LiveControlMessage::Window { lines: 800 }));
        let m: LiveControlMessage =
            serde_json::from_str(r#"{"type":"cadence","fast":false}"#).unwrap();
        assert!(matches!(m, LiveControlMessage::Cadence { fast: false }));
    }
}
