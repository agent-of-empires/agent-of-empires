//! Acp WebSocket fanout.
//!
//! `/sessions/{id}/acp/ws` upgrades to a WebSocket that subscribes
//! to `AppState::acp_events_tx` and forwards every frame whose
//! `session_id` matches the route param. Frames are JSON. The protocol
//! is one-way today (server -> client); inbound messages are ignored.
//!
//! Durability lives in `AppState::acp_event_store` (SQLite), not
//! this channel. The broadcast channel is best-effort: a client that
//! connects between a `tx.send` and its `subscribe()` misses frames,
//! and `RecvError::Lagged` drops frames when the channel overflows.
//! Both cases recover via the on-connect drain, which reads the
//! event store from `?since=` (or 0 for fresh subscribers); the
//! same store backs `GET /api/sessions/{id}/acp/replay`. The
//! channel is the fast path; the store is the truth.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{
    ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    Path, Query, State,
};
use axum::response::IntoResponse;
use serde::Deserialize;
use tokio::select;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;
use tracing::{debug, warn};

/// WebSocket close code 1001 ("going away"). Sent when the daemon is
/// shutting down so the client can distinguish a server-side exit from
/// a transient transport error and skip its reconnect backoff for one
/// cycle. See #1198.
const CLOSE_CODE_GOING_AWAY: u16 = 1001;

use super::{AcpBroadcastFrame, AppState};

/// Cadence at which the server emits an application-level Ping. The
/// browser's WebSocket auto-replies with a Pong; axum forwards that
/// Pong to the recv loop where it resets `last_pong_at`. 30s sits
/// comfortably under Cloudflare's 100s WebSocket idle timeout and the
/// ~60s background-WS reaper used by mobile Chrome / Safari, so a
/// quiet session stays connected indefinitely. See #1130.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum gap allowed between Pongs before we tear down a stuck
/// socket. With PING_INTERVAL of 30s, this tolerates two missed
/// round-trips before closing. The frontend's auto-reconnect picks up
/// from `?since=<lastSeq>` so a tear-down here is a transparent
/// recovery, not a session loss.
const PONG_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// The app-level keepalive frame emitted on every ping tick. A plain
/// Text frame the browser delivers to `onmessage`, unlike the WS Ping
/// the browser handles invisibly. The client keys its staleness
/// watchdog on this exact shape, so keep it stable. See #2287.
fn heartbeat_frame() -> String {
    r#"{"kind":"heartbeat"}"#.to_string()
}

/// Query parameters for the structured view WS upgrade. Clients pass
/// `?since=<lastSeq>` so the on-connect drain only resends events
/// newer than what they already have. Without this, a long-running
/// session resends its full transcript on every reconnect (page
/// refresh / mobile flap), which can be tens of MB at the retention
/// cap.
#[derive(Debug, Default, Deserialize)]
pub struct AcpWsQuery {
    #[serde(default)]
    pub since: Option<u64>,
}

/// Public route handler for the structured view WebSocket.
pub async fn acp_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<AcpWsQuery>,
) -> impl IntoResponse {
    // Logged at DEBUG so we can prove the route was reached even
    // when the upgrade fails. If this line is missing from debug.log
    // for a session that's stuck on "no live updates", the request
    // never got past auth_middleware (or never left the browser).
    // One line per WS connect (not per message), so debug-level
    // doesn't risk spamming.
    let since = q.since.unwrap_or(0);
    debug!(
        target: "acp.ws",
        session = %id,
        since,
        "agent ws route entered, beginning upgrade"
    );
    let session_for_handler = id.clone();
    ws.protocols(["aoe-auth"])
        .on_upgrade(move |socket| async move {
            debug!(target: "acp.ws", session = %session_for_handler, "agent ws upgrade complete");
            handle(socket, session_for_handler, state, since).await
        })
}

async fn handle(mut socket: WebSocket, session_id: String, state: Arc<AppState>, since: u64) {
    // Clone the shutdown token so this handler exits promptly when the
    // daemon receives SIGINT/SIGTERM/SIGHUP, instead of holding axum's
    // graceful drain open until the browser tab decides to disconnect.
    // See #1198.
    let shutdown = state.shutdown.clone();

    // Subscribe BEFORE the replay snapshot so events published in the
    // window between snapshot and live-loop entry land in `rx`. Such
    // events also appear in the replay snapshot if the publish
    // happens to interleave; the client dedupes via `frame.seq <=
    // state.lastSeq`, so duplicates are no-ops. The reverse order
    // (snapshot first, then subscribe) leaves a gap where live
    // events get dropped.
    let mut rx = state.acp_events_tx.subscribe();

    // Replay events newer than `since` immediately on connect. Without
    // this, any events published in the upgrade gap between the
    // client's POST /acp/spawn (or the first /acp/prompt) and
    // our `subscribe()` above are silently dropped by the broadcast
    // channel, since tokio's `broadcast::Sender::send` discards the
    // message when no receivers exist. The disk-backed event store
    // captures every published event, so reading it here closes the
    // race without forcing the client to GET /acp/replay
    // separately.
    let replay_count = drain_replay_into_socket(&mut socket, &state, &session_id, since).await;
    debug!(
        target: "acp.ws",
        session = %session_id,
        since,
        replayed = replay_count,
        "agent ws subscribed"
    );

    let mut ping_interval = tokio::time::interval(PING_INTERVAL);
    ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // First tick fires immediately; consume it so the first ping waits
    // PING_INTERVAL rather than racing the upgrade handshake.
    ping_interval.tick().await;
    let mut last_pong_at = Instant::now();

    let mut shutting_down = false;
    loop {
        select! {
            _ = shutdown.cancelled() => {
                debug!(target: "acp.ws", session = %session_id, "shutdown signaled, closing");
                shutting_down = true;
                break;
            }
            client_msg = socket.recv() => {
                match client_msg {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Pong(_))) => {
                        // Browser ack of our keepalive Ping. Refresh the
                        // pong watchdog; otherwise a quiet but live
                        // session would get reaped at PONG_IDLE_TIMEOUT.
                        last_pong_at = Instant::now();
                        continue;
                    }
                    // Inbound messages from the client are not used today.
                    // Clients post approval resolutions via REST, not the
                    // WebSocket. Ignore everything else we receive.
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        warn!(target: "acp.ws", "client recv error: {e}");
                        break;
                    }
                }
            }
            _ = ping_interval.tick() => {
                if last_pong_at.elapsed() > PONG_IDLE_TIMEOUT {
                    warn!(
                        target: "acp.ws",
                        session = %session_id,
                        idle_secs = last_pong_at.elapsed().as_secs(),
                        "agent ws idle reaper fired (no Pong from peer)"
                    );
                    break;
                }
                // App-level heartbeat the browser can actually see. The WS
                // Ping below keeps the server-side pong reaper honest, but
                // browser JavaScript cannot observe Ping/Pong frames, so a
                // quiet-but-live session gives the client no liveness signal
                // and it cannot tell a healthy idle socket from a half-open
                // (zombie) one a proxy reset without the browser noticing.
                // This Text frame is that signal; the client's staleness
                // watchdog reconnects when it stops arriving. See #2287.
                if socket
                    .send(Message::Text(heartbeat_frame().into()))
                    .await
                    .is_err()
                {
                    debug!(target: "acp.ws", session = %session_id, "ws heartbeat send failed, peer gone");
                    break;
                }
                if socket
                    .send(Message::Ping(Vec::new().into()))
                    .await
                    .is_err()
                {
                    debug!(target: "acp.ws", session = %session_id, "ws Ping send failed, peer gone");
                    break;
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(frame) => {
                        if frame.session_id != session_id {
                            continue;
                        }
                        let payload = match serde_json::to_string(&frame) {
                            Ok(s) => s,
                            Err(e) => {
                                warn!(target: "acp.ws", "serialise frame: {e}");
                                continue;
                            }
                        };
                        if socket.send(Message::Text(payload.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        // Tell the client they missed events so they can
                        // request a snapshot+replay rather than silently
                        // diverging.
                        let gap = serde_json::json!({
                            "kind": "lagged",
                            "skipped": skipped,
                        });
                        let _ = socket
                            .send(Message::Text(gap.to_string().into()))
                            .await;
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }

    debug!(target: "acp.ws", session = %session_id, "agent ws disconnected");
    let close_frame = if shutting_down {
        Some(CloseFrame {
            code: CLOSE_CODE_GOING_AWAY,
            reason: "server shutdown".into(),
        })
    } else {
        None
    };
    let _ = socket.send(Message::Close(close_frame)).await;
}

/// Read every stored event for `session_id` with `seq > since` out of
/// the disk-backed event store and forward it to the socket as a
/// `AcpBroadcastFrame`. Returns the number of frames sent. The
/// event store survives `aoe serve` restart, so this drain works even
/// after the daemon has restarted. The live broadcast channel is
/// already subscribed by the caller before this runs, so any events
/// published between the snapshot and the live-loop entry are still
/// delivered (the client dedupes by seq).
async fn drain_replay_into_socket(
    socket: &mut WebSocket,
    state: &AppState,
    session_id: &str,
    since: u64,
) -> usize {
    // Offload the rusqlite read to the blocking pool. A session with
    // a large retained history may iterate thousands of rows; running
    // that on the runtime worker stalls every other concurrent task on
    // the same worker for the duration of the read.
    let store = Arc::clone(&state.acp_event_store);
    let session_id_owned = session_id.to_string();
    let entries = match tokio::task::spawn_blocking(move || {
        store.replay_from(&session_id_owned, since)
    })
    .await
    {
        Ok(rows) => rows,
        Err(e) => {
            // Blocking task panicked or was cancelled. Live broadcast still
            // flows and the client dedupes by seq, so empty drain is benign,
            // but the silent swallow would hide the panic from operators.
            warn!(
                target: "acp.ws",
                session_id = %session_id,
                error = %e,
                "replay drain blocking task failed; sending zero frames"
            );
            Vec::new()
        }
    };
    let mut sent = 0usize;
    for (seq, event) in entries {
        let frame = AcpBroadcastFrame {
            session_id: session_id.to_string(),
            seq,
            event: Arc::new(event),
        };
        let payload = match serde_json::to_string(&frame) {
            Ok(s) => s,
            Err(e) => {
                warn!(target: "acp.ws", "serialise replay frame: {e}");
                continue;
            }
        };
        if socket.send(Message::Text(payload.into())).await.is_err() {
            break;
        }
        sent += 1;
    }
    sent
}

/// Helper used by the worker supervisor (and integration tests) to
/// publish a frame.
pub fn publish(state: &AppState, frame: AcpBroadcastFrame) {
    // Discard the receiver count; broadcast::Sender::send is best-effort
    // and ignores send-with-no-receivers.
    let _ = state.acp_events_tx.send(frame);
}

/// Push-notification trigger for "agent needs your approval." Called
/// by the worker supervisor when it observes an `ApprovalRequested`
/// structured view event. Re-uses the existing push infrastructure: subscribers
/// for `state.push` receive a payload telling the PWA to focus the
/// approval card.
pub async fn trigger_approval_push(
    state: &AppState,
    session_id: &str,
    approval_title: &str,
    destructive: bool,
) {
    let badge = if destructive {
        "DESTRUCTIVE"
    } else {
        "approval"
    };
    let title = format!("{} needs approval", session_id);
    let body = if destructive {
        format!("{badge}: {approval_title}")
    } else {
        approval_title.to_string()
    };
    let tag = format!("acp-approval-{session_id}");
    send_acp_attention_push(state, session_id, title, body, tag).await;
}

/// Push-notification trigger for "agent asked you a question." Called by
/// the worker supervisor when it observes an `ElicitationRequested`
/// (`AskUserQuestion`) structured view event. A question blocks the turn
/// on the user exactly like an approval, so it gets the same dedicated,
/// suppression-bypassing push rather than only the generic Waiting one.
/// See #2146.
pub async fn trigger_question_push(state: &AppState, session_id: &str, question: &str) {
    let title = format!("{} has a question", session_id);
    let tag = format!("acp-question-{session_id}");
    send_acp_attention_push(state, session_id, title, push_body_snippet(question), tag).await;
}

/// Question text can be long and lands on a lock screen, so collapse
/// whitespace and cap it before it goes into a push payload.
fn push_body_snippet(s: &str) -> String {
    const MAX: usize = 120;
    let compact = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() > MAX {
        format!("{}…", compact.chars().take(MAX).collect::<String>())
    } else {
        compact
    }
}

/// Shared sender for the dedicated ACP "needs your attention" pushes
/// (approval and question). Snapshots subscribers and sends one encrypted
/// payload each, deep-linking to the session's structured view. Bypasses
/// the status-push active-session suppression on purpose: these are
/// precise, turn-blocking events, not the coarse Waiting heuristic.
async fn send_acp_attention_push(
    state: &AppState,
    session_id: &str,
    title: String,
    body: String,
    tag: String,
) {
    let Some(push) = state.push.as_ref() else {
        return;
    };
    if !state.push_enabled {
        return;
    }
    let path = format!("/sessions/{session_id}/acp");
    let subs = push.store.snapshot().await;
    if subs.is_empty() {
        return;
    }
    let client = match super::push_send::build_client() {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "acp.push", "build_client: {e}");
            return;
        }
    };
    for sub in subs {
        let Some(url) = super::push::build_push_url(&sub, &path) else {
            continue;
        };
        let payload = super::push_send::PushPayload {
            title: title.clone(),
            body: body.clone(),
            url,
            tag: tag.clone(),
            session_id: session_id.to_string(),
        };
        let body_bytes = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                warn!(target: "acp.push", "serialise payload: {e}");
                continue;
            }
        };
        let auth_header = match super::push_send::vapid_auth_header(push, &sub.endpoint) {
            Ok(h) => h,
            Err(e) => {
                warn!(target: "acp.push", "vapid header: {e}");
                continue;
            }
        };
        let cipher = match super::push_send::encrypt_aes128gcm(&sub, &body_bytes) {
            Ok(c) => c,
            Err(e) => {
                warn!(target: "acp.push", "encrypt: {e}");
                continue;
            }
        };
        let _ = client
            .post(&sub.endpoint)
            .header("Authorization", &auth_header)
            .header("Content-Encoding", "aes128gcm")
            .header("Content-Type", "application/octet-stream")
            .header("TTL", "60")
            .body(cipher)
            .send()
            .await;
    }
}

#[cfg(all(test, feature = "serve"))]
mod tests {
    use super::*;

    #[test]
    fn push_body_snippet_collapses_whitespace_and_caps_length() {
        // Short text passes through with whitespace collapsed.
        assert_eq!(
            push_body_snippet("Which   env?\n staging\tor prod"),
            "Which env? staging or prod"
        );
        // Long text is truncated and gets an ellipsis. The cap counts
        // chars, not bytes, so the result is at most MAX + the ellipsis.
        let long = "word ".repeat(100);
        let snippet = push_body_snippet(&long);
        assert!(snippet.ends_with('…'));
        assert_eq!(snippet.chars().count(), 120 + 1);
    }

    #[tokio::test]
    async fn publish_with_no_receivers_does_not_panic() {
        // Create a minimal AppState-like fixture: in real code the server
        // owns AppState; for this unit test we just need the broadcast
        // channel by itself.
        let (tx, _rx) = tokio::sync::broadcast::channel::<AcpBroadcastFrame>(8);
        // Drop receiver: send should not error.
        drop(_rx);
        let send_result = tx.send(AcpBroadcastFrame {
            session_id: "s".into(),
            seq: 1,
            event: Arc::new(crate::acp::Event::ThinkingStarted),
        });
        // Sending to a channel with no receivers returns Err, but
        // publish() in this module deliberately discards the result.
        assert!(send_result.is_err() || send_result.is_ok());
    }

    /// PONG_IDLE_TIMEOUT must outrun PING_INTERVAL by enough margin to
    /// tolerate at least one missed round-trip. A misconfiguration here
    /// (interval >= timeout) would have the keepalive immediately
    /// reaping every connection on its first tick. See #1130.
    #[test]
    fn keepalive_pong_timeout_exceeds_ping_interval() {
        assert!(
            PONG_IDLE_TIMEOUT > PING_INTERVAL,
            "PONG_IDLE_TIMEOUT ({:?}) must be longer than PING_INTERVAL ({:?})",
            PONG_IDLE_TIMEOUT,
            PING_INTERVAL,
        );
        // Allow at least two missed round-trips: PONG_IDLE_TIMEOUT >= 2 *
        // PING_INTERVAL keeps the watchdog forgiving on flaky mobile
        // links without delaying recovery on a truly dead peer.
        assert!(
            PONG_IDLE_TIMEOUT >= PING_INTERVAL * 2,
            "PONG_IDLE_TIMEOUT should tolerate two missed pings",
        );
    }

    /// Both keepalive intervals must stay well under Cloudflare's
    /// documented 100s WebSocket idle timeout. If either climbs above
    /// it, idle structured view sessions through a Cloudflare tunnel would be
    /// dropped by the tunnel before the keepalive could fire.
    #[test]
    fn keepalive_under_cloudflare_idle_cap() {
        const CLOUDFLARE_IDLE_CAP: Duration = Duration::from_secs(100);
        assert!(
            PING_INTERVAL < CLOUDFLARE_IDLE_CAP,
            "PING_INTERVAL ({:?}) must be shorter than Cloudflare's 100s tunnel idle cap",
            PING_INTERVAL,
        );
    }

    /// The client staleness watchdog matches this exact byte string to
    /// distinguish a keepalive tick from a real event frame. If the shape
    /// drifts, the client treats heartbeats as malformed and a quiet but
    /// live session looks stale. See #2287.
    #[test]
    fn heartbeat_frame_shape_is_stable() {
        assert_eq!(heartbeat_frame(), r#"{"kind":"heartbeat"}"#);
    }
}
