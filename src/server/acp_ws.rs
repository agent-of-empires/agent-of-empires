//! Acp WebSocket fanout.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{
    ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    Path, Query, State,
};
use axum::response::IntoResponse;
use serde::{Deserialize, Serialize};
use tokio::select;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;
use tracing::{debug, warn};

/// WebSocket close code 1001 ("going away").
const CLOSE_CODE_GOING_AWAY: u16 = 1001;

use super::{AcpBroadcastFrame, AppState};
use crate::acp::state::{AcpSessionId, AcpState, AgentName, Event};
use crate::acp::transcript::TranscriptModel;

/// Cadence at which the server emits an application-level Ping.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum gap allowed between Pongs before we tear down a stuck socket.
const PONG_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// The app-level keepalive frame emitted on every ping tick.
fn heartbeat_frame() -> String {
    r#"{"kind":"heartbeat"}"#.to_string()
}

/// Query parameters for the structured view WS upgrade.
#[derive(Debug, Default, Deserialize)]
pub struct AcpWsQuery {
    #[serde(default)]
    pub since: Option<u64>,
    /// Set `frames=0` to receive only the folded projections (`reduced_state` +
    /// `transcript_snapshot` / `transcript_delta`) and none of the raw event frames they
    /// are built from.
    #[serde(default)]
    pub frames: Option<u8>,
}

/// Public route handler for the structured view WebSocket.
pub async fn acp_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<AcpWsQuery>,
) -> impl IntoResponse {
    // Logged at DEBUG so we can prove the route was reached even when the upgrade fails.
    let since = q.since.unwrap_or(0);
    let forward_frames = q.frames.unwrap_or(1) != 0;
    debug!(
        target: "acp.ws",
        session = %id,
        since,
        forward_frames,
        "agent ws route entered, beginning upgrade"
    );
    let session_for_handler = id.clone();
    ws.protocols(["aoe-auth"])
        .on_upgrade(move |socket| async move {
            debug!(target: "acp.ws", session = %session_for_handler, "agent ws upgrade complete");
            handle(socket, session_for_handler, state, since, forward_frames).await
        })
}

async fn handle(
    mut socket: WebSocket,
    session_id: String,
    state: Arc<AppState>,
    since: u64,
    forward_frames: bool,
) {
    // Clone the shutdown token so this handler exits promptly when the daemon receives
    // SIGINT/SIGTERM/SIGHUP, instead of holding axum's graceful drain open until the
    // browser tab decides to disconnect.
    let shutdown = state.shutdown.clone();

    // Subscribe BEFORE the replay snapshot so events published in the window between
    // snapshot and live-loop entry land in `rx`.
    let mut rx = state.acp_events_tx.subscribe();

    // Each connection deterministically reduces the ordered event stream into
    // control state. Agent and model seed identity until an event changes it.
    let (agent, model) = seed_identity(&state, &session_id).await;
    // Kept so a lag can rebuild the fold from the same identity seed.
    let seed = (agent.clone(), model.clone());
    let mut reduced = AcpState::new(AcpSessionId(session_id.clone()), agent, model);

    // Fold the same stream into the transcript snapshot and deltas.
    let mut transcript = TranscriptModel::new();
    // Per-connection memory of the cold state fields already delivered.
    let mut cold = ColdFieldCache::default();
    let mut folds = ConnectionFolds {
        reduced: &mut reduced,
        transcript: &mut transcript,
        cold: &mut cold,
        last_applied_seq: 0,
    };

    // Replay events newer than `since` immediately on connect.
    let replay_count = drain_replay_into_socket(
        &mut socket,
        &state,
        &session_id,
        since,
        forward_frames,
        &mut folds,
    )
    .await;
    // Carried out of `folds` so the live loop can keep the control fold
    // idempotent against the drain/broadcast overlap.
    let mut last_applied_seq = folds.last_applied_seq;
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
                        // Browser ack of our keepalive Ping.
                        last_pong_at = Instant::now();
                        continue;
                    }
                    // Inbound messages from the client are not used today.
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
                // App-level heartbeat the browser can actually see.
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
                        if forward_frames {
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
                        // Reduce this event into the connection's control state and push
                        // the updated snapshot.
                        if frame.seq > last_applied_seq {
                            last_applied_seq = frame.seq;
                            let _ = reduced.apply_event((*frame.event).clone());
                        }
                        if !send_reduced_state(&mut socket, &session_id, frame.seq, &reduced, &mut cold).await {
                            break;
                        }
                        // Fold the same event into the transcript render model and push
                        // each resulting row change as a `transcript_delta`.
                        let deltas = transcript.apply_event(frame.seq, &frame.event);
                        let mut socket_dead = false;
                        for delta in &deltas {
                            if !send_transcript_delta(&mut socket, &session_id, frame.seq, delta)
                                .await
                            {
                                socket_dead = true;
                                break;
                            }
                        }
                        if socket_dead {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(skipped)) => {
                        // Tell the client they missed events so they can request a
                        // snapshot+replay rather than silently diverging.
                        let gap = serde_json::json!({
                            "kind": "lagged",
                            "skipped": skipped,
                        });
                        let _ = socket
                            .send(Message::Text(gap.to_string().into()))
                            .await;
                        // The skipped events never reached this connection's control fold,
                        // and nothing else would ever repair it.
                        let mut rebuilt = AcpState::new(
                            AcpSessionId(session_id.clone()),
                            seed.0.clone(),
                            seed.1.clone(),
                        );
                        let store = Arc::clone(&state.acp_event_store);
                        let session_for_read = session_id.clone();
                        let entries = tokio::task::spawn_blocking(move || {
                            store.replay_from(&session_for_read, 0)
                        })
                        .await
                        .unwrap_or_default();
                        let mut highest = 0;
                        for (seq, event) in entries {
                            let _ = rebuilt.apply_event(event);
                            highest = seq;
                        }
                        reduced = rebuilt;
                        last_applied_seq = highest;
                        // The cold-field cache still describes what this socket
                        // holds, so an unchanged command list stays omitted.
                        if !send_reduced_state(
                            &mut socket,
                            &session_id,
                            highest,
                            &reduced,
                            &mut cold,
                        )
                        .await
                        {
                            break;
                        }
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

/// Read every stored event for `session_id` with `seq > since` out of the disk-backed event
/// store, fold it into both projections, and (unless the client opted out with `frames=0`)
/// forward it to the socket as an `AcpBroadcastFrame`.
async fn drain_replay_into_socket(
    socket: &mut WebSocket,
    state: &AppState,
    session_id: &str,
    since: u64,
    forward_frames: bool,
    folds: &mut ConnectionFolds<'_>,
) -> usize {
    // Offload the rusqlite read to the blocking pool.
    let store = Arc::clone(&state.acp_event_store);
    let session_id_owned = session_id.to_string();
    // Read from seq 0, not from `since`.
    let entries =
        match tokio::task::spawn_blocking(move || store.replay_from(&session_id_owned, 0)).await {
            Ok(rows) => rows,
            Err(e) => {
                // Blocking task panicked or was cancelled.
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
    let to_forward = fold_connect_history(entries, since, folds);
    let snapshot_seq = folds.last_applied_seq.max(since);
    for (seq, event) in to_forward {
        if !forward_frames {
            continue;
        }
        let frame = AcpBroadcastFrame {
            session_id: session_id.to_string(),
            seq,
            event: Arc::new(event),
            worker_generation: None,
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
    // Connect snapshot.
    let _ = send_reduced_state(socket, session_id, snapshot_seq, folds.reduced, folds.cold).await;
    // Transcript connect snapshot.
    let _ = send_transcript_snapshot(socket, session_id, snapshot_seq, folds.transcript).await;
    sent
}

/// State fields large enough, and static enough, to be worth suppressing when they have not
/// changed since the last frame on this connection.
const COLD_STATE_FIELDS: [&str; 5] = [
    "available_commands",
    "available_modes",
    "config_options",
    // Not static, but big and bursty.
    "recent_diffs",
    "background_agents",
];

/// Fold a session's stored history into the connection's projections and return the entries
/// the client still needs as raw frames.
async fn seed_identity(state: &AppState, session_id: &str) -> (AgentName, Option<String>) {
    let instances = state.instances.read().await;
    instances
        .iter()
        .find(|i| i.id == session_id)
        .map(|i| {
            (
                AgentName(i.agent_name.clone().unwrap_or_else(|| i.tool.clone())),
                i.agent_model.clone(),
            )
        })
        .unwrap_or_else(|| (AgentName(String::new()), None))
}

fn fold_connect_history(
    entries: Vec<(u64, Event)>,
    since: u64,
    folds: &mut ConnectionFolds<'_>,
) -> Vec<(u64, Event)> {
    let mut to_forward = Vec::new();
    for (seq, event) in entries {
        let _ = folds.reduced.apply_event(event.clone());
        folds.last_applied_seq = seq;
        if seq <= since {
            continue;
        }
        folds.transcript.apply_event(seq, &event);
        to_forward.push((seq, event));
    }
    to_forward
}

/// The three folds a connection maintains over the event stream.
struct ConnectionFolds<'a> {
    reduced: &'a mut AcpState,
    transcript: &'a mut TranscriptModel,
    cold: &'a mut ColdFieldCache,
    /// Highest seq already folded into `reduced`.
    last_applied_seq: u64,
}

/// Per-connection memory of the cold fields already sent, so an unchanged one can be
/// omitted.
#[derive(Default)]
struct ColdFieldCache {
    hashes: std::collections::HashMap<&'static str, u64>,
}

impl ColdFieldCache {
    /// Strip the cold fields whose value this connection already has, and
    /// return their names so the client knows to keep what it holds rather
    /// than read the absence as "now empty".
    fn strip_unchanged(&mut self, state: &mut serde_json::Value) -> Vec<&'static str> {
        use std::hash::{Hash, Hasher};
        let Some(obj) = state.as_object_mut() else {
            return Vec::new();
        };
        let mut unchanged = Vec::new();
        for field in COLD_STATE_FIELDS {
            let Some(value) = obj.get(field) else {
                continue;
            };
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            value.to_string().hash(&mut hasher);
            let digest = hasher.finish();
            if self.hashes.get(field) == Some(&digest) {
                obj.remove(field);
                unchanged.push(field);
            } else {
                self.hashes.insert(field, digest);
            }
        }
        unchanged
    }
}

/// Serialize and send the reduced control state as a `kind`-tagged `reduced_state` frame.
async fn send_reduced_state(
    socket: &mut WebSocket,
    session_id: &str,
    seq: u64,
    reduced: &AcpState,
    cold: &mut ColdFieldCache,
) -> bool {
    let mut state = match serde_json::to_value(reduced) {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "acp.ws", "serialise reduced_state: {e}");
            return true;
        }
    };
    let unchanged = cold.strip_unchanged(&mut state);
    let frame = serde_json::json!({
        "kind": "reduced_state",
        "session_id": session_id,
        "seq": seq,
        "state": state,
        "unchanged": unchanged,
    });
    match serde_json::to_string(&frame) {
        Ok(payload) => socket.send(Message::Text(payload.into())).await.is_ok(),
        Err(e) => {
            warn!(target: "acp.ws", "serialise reduced_state: {e}");
            true
        }
    }
}

/// Serialize and send the full transcript row list as a `kind`-tagged `transcript_snapshot`
/// frame on connect.
async fn send_transcript_snapshot(
    socket: &mut WebSocket,
    session_id: &str,
    seq: u64,
    transcript: &TranscriptModel,
) -> bool {
    let frame = serde_json::json!({
        "kind": "transcript_snapshot",
        "session_id": session_id,
        "seq": seq,
        "rows": transcript.rows(),
    });
    match serde_json::to_string(&frame) {
        Ok(payload) => socket.send(Message::Text(payload.into())).await.is_ok(),
        Err(e) => {
            warn!(target: "acp.ws", "serialise transcript_snapshot: {e}");
            true
        }
    }
}

/// Serialize and send one incremental transcript row change as a `kind`-tagged
/// `transcript_delta` frame.
async fn send_transcript_delta(
    socket: &mut WebSocket,
    session_id: &str,
    seq: u64,
    delta: &crate::acp::transcript::TranscriptDelta,
) -> bool {
    let frame = serde_json::json!({
        "kind": "transcript_delta",
        "session_id": session_id,
        "seq": seq,
        "delta": delta,
    });
    match serde_json::to_string(&frame) {
        Ok(payload) => socket.send(Message::Text(payload.into())).await.is_ok(),
        Err(e) => {
            warn!(target: "acp.ws", "serialise transcript_delta: {e}");
            true
        }
    }
}

/// Helper used by the worker supervisor (and integration tests) to publish a frame.
pub fn publish(state: &AppState, frame: AcpBroadcastFrame) {
    // Discard the receiver count; broadcast::Sender::send is best-effort
    // and ignores send-with-no-receivers.
    let _ = state.acp_events_tx.send(frame);
}

/// Push-notification trigger for "agent needs your approval." Called by the worker
/// supervisor when it observes an `ApprovalRequested` structured view event.
pub async fn trigger_approval_push(
    state: &AppState,
    session_id: &str,
    approval_title: &str,
    destructive: bool,
    seq: u64,
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
    let tag = approval_tag(session_id);
    send_acp_push(state, session_id, |url| AcpNotifyPayload {
        kind: "notify",
        title: title.clone(),
        body: body.clone(),
        url,
        tag: tag.clone(),
        session_id: session_id.to_string(),
        seq,
    })
    .await;
}

/// Retract a previously shown approval notification on every device once the approval is
/// handled.
pub async fn trigger_approval_clear_push(state: &AppState, session_id: &str, seq: u64) {
    let tag = approval_tag(session_id);
    send_acp_push(state, session_id, |url| AcpClearPayload {
        kind: "clear",
        title: "Resolved",
        body: "Handled on another device",
        url,
        tag: tag.clone(),
        session_id: session_id.to_string(),
        seq,
    })
    .await;
}

/// Tag shared by the approval show and clear pushes for a session.
fn approval_tag(session_id: &str) -> String {
    format!("acp-approval-{session_id}")
}

/// Tag shared by the question show and clear pushes for a session.
fn question_tag(session_id: &str) -> String {
    format!("acp-question-{session_id}")
}

/// Push-notification trigger for "agent asked you a question." Called by the worker
/// supervisor when it observes an `ElicitationRequested` (`AskUserQuestion`) structured
/// view event.
pub async fn trigger_question_push(state: &AppState, session_id: &str, question: &str, seq: u64) {
    let title = format!("{} has a question", session_id);
    let body = push_body_snippet(question);
    let tag = question_tag(session_id);
    send_acp_push(state, session_id, |url| AcpNotifyPayload {
        kind: "notify",
        title: title.clone(),
        body: body.clone(),
        url,
        tag: tag.clone(),
        session_id: session_id.to_string(),
        seq,
    })
    .await;
}

/// Retract a previously shown question notification once the question is answered.
pub async fn trigger_question_clear_push(state: &AppState, session_id: &str, seq: u64) {
    let tag = question_tag(session_id);
    send_acp_push(state, session_id, |url| AcpClearPayload {
        kind: "clear",
        title: "Resolved",
        body: "Handled on another device",
        url,
        tag: tag.clone(),
        session_id: session_id.to_string(),
        seq,
    })
    .await;
}

/// Payload for a dedicated ACP attention push (approval / question).
#[derive(Serialize)]
struct AcpNotifyPayload {
    kind: &'static str,
    title: String,
    body: String,
    url: String,
    tag: String,
    session_id: String,
    seq: u64,
}

/// Payload telling the service worker to retract a shown ACP attention notification once
/// the request is handled.
#[derive(Serialize)]
struct AcpClearPayload {
    kind: &'static str,
    title: &'static str,
    body: &'static str,
    url: String,
    tag: String,
    session_id: String,
    seq: u64,
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

/// Shared sender for the dedicated ACP "needs your attention" pushes (approval and
/// question) and their matching clear pushes.
async fn send_acp_push<T, F>(state: &AppState, session_id: &str, make_payload: F)
where
    T: Serialize,
    F: Fn(String) -> T,
{
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
        let payload = make_payload(url);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The connect snapshot is a whole-state frame the clients adopt verbatim, and every
    /// client dials with a non-zero `since` after its first connect (the web seeds
    /// `lastSeq` from the tail before opening the socket; the TUI reconnects from
    /// `last_seq`).
    #[test]
    fn connect_fold_covers_all_history_while_frames_stay_scoped_to_since() {
        let approval = crate::acp::approvals::Approval {
            nonce: crate::acp::approvals::Nonce("n-1".into()),
            tool_call: crate::acp::state::ToolCall {
                id: "t-1".into(),
                name: "Edit".into(),
                kind: "edit".into(),
                args_preview: "{}".into(),
                started_at: chrono::Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
            destructive: false,
            options: Vec::new(),
            choice: false,
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        let history = vec![
            (
                1,
                Event::AvailableCommandsUpdated {
                    commands: vec![crate::acp::state::AvailableCommand {
                        name: "review".into(),
                        description: "Review".into(),
                        accepts_input: false,
                    }],
                },
            ),
            (
                2,
                Event::ModesAvailable {
                    current_mode_id: "plan".into(),
                    modes: vec![crate::acp::state::ModeInfo {
                        id: "plan".into(),
                        name: "Plan".into(),
                        description: None,
                    }],
                },
            ),
            (3, Event::ApprovalRequested { approval }),
            (
                4,
                Event::AgentMessageChunk {
                    text: "hello".into(),
                },
            ),
        ];

        // A reconnect: the client already has everything through seq 4.
        let mut reduced =
            AcpState::new(AcpSessionId("s-1".into()), AgentName("claude".into()), None);
        let mut transcript = TranscriptModel::new();
        let mut cold = ColdFieldCache::default();
        let mut folds = ConnectionFolds {
            reduced: &mut reduced,
            transcript: &mut transcript,
            cold: &mut cold,
            last_applied_seq: 0,
        };
        let forwarded = fold_connect_history(history.clone(), 4, &mut folds);

        assert!(forwarded.is_empty(), "nothing new to forward");
        assert_eq!(folds.last_applied_seq, 4);
        assert!(
            folds.transcript.rows().is_empty(),
            "transcript stays scoped to since; the client holds those rows"
        );
        // The control state is whole-session regardless of the cursor.
        let reduced = &folds.reduced;
        assert_eq!(
            reduced.available_commands.len(),
            1,
            "slash palette survives"
        );
        assert_eq!(reduced.available_modes.len(), 1, "mode picker survives");
        assert_eq!(reduced.current_mode_id.as_deref(), Some("plan"));
        assert_eq!(
            reduced.pending_approvals.len(),
            1,
            "a pending approval must still render after a reconnect"
        );

        // A cold connect gets the same control state plus every row.
        let mut cold_state =
            AcpState::new(AcpSessionId("s-1".into()), AgentName("claude".into()), None);
        let mut cold_transcript = TranscriptModel::new();
        let mut cold_cache = ColdFieldCache::default();
        let mut cold_folds = ConnectionFolds {
            reduced: &mut cold_state,
            transcript: &mut cold_transcript,
            cold: &mut cold_cache,
            last_applied_seq: 0,
        };
        let forwarded = fold_connect_history(history, 0, &mut cold_folds);
        assert_eq!(forwarded.len(), 4);
        assert!(!cold_folds.transcript.rows().is_empty());
        assert_eq!(cold_folds.reduced.available_commands.len(), 1);
        assert_eq!(cold_folds.reduced.pending_approvals.len(), 1);
    }

    /// Prompt dispatch (Tier 3) reads the daemon's own control state through
    /// `fold_control_state`, so the whole decision is only as good as this fold.
    #[tokio::test]
    async fn fold_control_state_tracks_the_turn_flags_dispatch_reads() {
        let mut inst = crate::session::Instance::new("t", "/tmp/aoe-fold-control");
        inst.id = "s-fold".to_string();
        inst.agent_name = Some("claude".to_string());
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        // Publish through the real choke point rather than writing straight to the store.
        use crate::acp::supervisor::BroadcastSink;
        let sink = crate::acp::supervisor::ChannelSink {
            tx: state.acp_events_tx.clone(),
            event_store: Arc::clone(&state.acp_event_store),
            control_cache: Arc::clone(&state.acp_control_cache),
        };
        let record = |seq: u64, event: Event| {
            assert!(
                sink.publish_persisted("s-fold", seq, &event),
                "publish must reach the event store"
            );
        };
        // A live steerable turn.
        record(
            1,
            Event::PromptCapabilities {
                image: false,
                audio: false,
                embedded_context: false,
                load_session: None,
                steering: true,
            },
        );
        record(
            2,
            Event::UserPromptSent {
                text: "go".into(),
                attachments: Vec::new(),
                prompt_id: None,
                synthesized: false,
            },
        );
        let folded = state.session_service.fold_control_state("s-fold").await;
        assert!(folded.turn_active, "the prompt opened a turn");
        assert!(folded.steering, "capabilities survive the fold");
        assert!(!folded.cancelling);
        assert_eq!(
            crate::acp::dispatch::decide(
                &folded,
                crate::acp::dispatch::WorkerLiveness {
                    running: true,
                    idle_dormant: false,
                    rate_limit_parked: false,
                },
            ),
            crate::acp::dispatch::PromptDispatch::Steered
        );

        // A pending cancel flips the same live turn to "park", which is the
        // gate that keeps Stop-then-type from restarting the runner (#1727).
        record(
            3,
            Event::CancelRequested {
                escalates_at: chrono::Utc::now(),
            },
        );
        let folded = state.session_service.fold_control_state("s-fold").await;
        assert!(folded.cancelling);
        assert_eq!(
            crate::acp::dispatch::decide(
                &folded,
                crate::acp::dispatch::WorkerLiveness {
                    running: true,
                    idle_dormant: false,
                    rate_limit_parked: false,
                },
            ),
            crate::acp::dispatch::PromptDispatch::Queued {
                reason: crate::acp::dispatch::QueueReason::Cancelling,
            }
        );

        // Turn end reopens the send path.
        record(
            4,
            Event::Stopped {
                reason: "cancelled".into(),
            },
        );
        let folded = state.session_service.fold_control_state("s-fold").await;
        assert!(!folded.turn_active, "Stopped closed the turn");
        assert!(!folded.cancelling, "and cleared the pending cancel");
        assert_eq!(
            crate::acp::dispatch::decide(
                &folded,
                crate::acp::dispatch::WorkerLiveness {
                    running: true,
                    idle_dormant: false,
                    rate_limit_parked: false,
                },
            ),
            crate::acp::dispatch::PromptDispatch::Sent
        );

        // An unknown session folds to a default (idle) state rather than
        // erroring, so a prompt for a session the daemon has not seen is not
        // parked forever on a phantom turn.
        let unknown = state.session_service.fold_control_state("s-missing").await;
        assert!(!unknown.turn_active);
    }

    /// `AcpState::apply_event` takes no seq and is not idempotent, and the
    /// drain overlaps the live broadcast by design, so a duplicated event
    /// would leave a second, unresolvable approval card in the shelf.
    type TestSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn connect_test_socket(
        state: Arc<AppState>,
    ) -> (TestSocket, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route("/{id}", axum::routing::get(acp_ws))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (socket, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/s-1?frames=0"))
            .await
            .unwrap();
        (socket, server)
    }

    async fn receive_kind(socket: &mut TestSocket, kind: &str) -> serde_json::Value {
        use futures_util::StreamExt;
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let message = socket.next().await.expect("socket remains open").unwrap();
                if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                    let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                    if value["kind"] == kind {
                        return value;
                    }
                }
            }
        })
        .await
        .expect("expected websocket frame")
    }

    #[tokio::test]
    async fn control_fold_skips_events_the_drain_already_applied() {
        let _home = crate::session::test_support::isolate_app_dir();
        let approval = |nonce: &str| crate::acp::approvals::Approval {
            nonce: crate::acp::approvals::Nonce(nonce.into()),
            tool_call: crate::acp::state::ToolCall {
                id: "t-1".into(),
                name: "Edit".into(),
                kind: "edit".into(),
                args_preview: "{}".into(),
                started_at: chrono::Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
            destructive: false,
            options: Vec::new(),
            choice: false,
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let event = Event::ApprovalRequested {
            approval: approval("n-1"),
        };
        state.acp_event_store.record("s-1", 7, &event).unwrap();
        let (mut socket, server) = connect_test_socket(state.clone()).await;
        let initial = receive_kind(&mut socket, "reduced_state").await;
        assert_eq!(
            initial["state"]["pending_approvals"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        receive_kind(&mut socket, "transcript_snapshot").await;
        publish(
            &state,
            AcpBroadcastFrame {
                session_id: "s-1".into(),
                seq: 7,
                event: Arc::new(event),
                worker_generation: None,
            },
        );
        let repeated = receive_kind(&mut socket, "reduced_state").await;
        assert_eq!(repeated["seq"], 7);
        assert_eq!(
            repeated["state"]["pending_approvals"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        state.shutdown.cancel();
        drop(socket);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn lagged_broadcast_reports_gap_and_rebuilds_control_state() {
        let _home = crate::session::test_support::isolate_app_dir();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        state
            .acp_event_store
            .record("s-1", 1, &Event::ThinkingStarted)
            .unwrap();
        let (mut socket, server) = connect_test_socket(state.clone()).await;
        let initial = receive_kind(&mut socket, "reduced_state").await;
        assert_eq!(initial["state"]["turn_active"], true);
        receive_kind(&mut socket, "transcript_snapshot").await;
        // No await in this burst: the current-thread receiver cannot drain its eight slots.
        for seq in 2..=17 {
            let event = if seq == 2 {
                Event::Stopped {
                    reason: "done".into(),
                }
            } else {
                Event::ThinkingEnded
            };
            state.acp_event_store.record("s-1", seq, &event).unwrap();
            publish(
                &state,
                AcpBroadcastFrame {
                    session_id: "s-1".into(),
                    seq,
                    event: Arc::new(event),
                    worker_generation: None,
                },
            );
        }
        let gap = receive_kind(&mut socket, "lagged").await;
        assert_eq!(gap["skipped"], 8);
        let rebuilt = receive_kind(&mut socket, "reduced_state").await;
        assert_eq!(rebuilt["seq"], 17);
        assert_eq!(
            rebuilt["state"]["turn_active"], false,
            "missed Stop must be recovered from durable history"
        );
        state.shutdown.cancel();
        drop(socket);
        server.abort();
        let _ = server.await;
    }

    /// `frames` gates only the raw-frame forwarding, and its default has to stay "send
    /// them".
    #[test]
    fn ws_query_frames_flag_defaults_to_forwarding() {
        let cases = [
            ("", true),
            ("since=7", true),
            ("frames=1", true),
            ("frames=0", false),
            ("since=7&frames=0", false),
        ];
        for (query, expected) in cases {
            let uri: axum::http::Uri = format!("/sessions/s-1/acp/ws?{query}").parse().unwrap();
            let Query(q) = Query::<AcpWsQuery>::try_from_uri(&uri).expect("parse query");
            assert_eq!(q.frames.unwrap_or(1) != 0, expected, "{query:?}");
        }
    }

    #[test]
    fn push_body_snippet_collapses_whitespace_and_caps_length() {
        // Short text passes through with whitespace collapsed.
        assert_eq!(
            push_body_snippet("Which   env?\n staging\tor prod"),
            "Which env? staging or prod"
        );
        // Long text is truncated and gets an ellipsis.
        let long = "word ".repeat(100);
        let snippet = push_body_snippet(&long);
        assert!(snippet.ends_with('…'));
        assert_eq!(snippet.chars().count(), 120 + 1);
    }

    /// The clear path reuses the tag helpers, so a drift here would silently fail to
    /// close the matching notification (#2491). Both payloads carry `kind` and `seq`,
    /// and a clear keeps title/body so a not-yet-updated service worker degrades to a
    /// benign notification rather than a blank one.
    #[test]
    fn attention_payloads_are_session_scoped_and_kind_distinct() {
        assert_eq!(approval_tag("s1"), "acp-approval-s1");
        assert_eq!(question_tag("s1"), "acp-question-s1");

        let clear = serde_json::to_value(AcpClearPayload {
            kind: "clear",
            title: "Resolved",
            body: "Handled on another device",
            url: "/sessions/s1/acp".into(),
            tag: approval_tag("s1"),
            session_id: "s1".into(),
            seq: 7,
        })
        .unwrap();
        assert_eq!(clear["kind"], "clear");
        assert_eq!(clear["tag"], "acp-approval-s1");
        assert_eq!(clear["seq"], 7);
        assert_eq!(clear["title"], "Resolved");

        let notify = serde_json::to_value(AcpNotifyPayload {
            kind: "notify",
            title: "t".into(),
            body: "b".into(),
            url: "/sessions/s1/acp".into(),
            tag: question_tag("s1"),
            session_id: "s1".into(),
            seq: 3,
        })
        .unwrap();
        assert_eq!(notify["kind"], "notify");
        assert_eq!(notify["tag"], "acp-question-s1");
        assert_eq!(notify["seq"], 3);
    }

    #[tokio::test]
    async fn publish_with_no_receivers_does_not_panic() {
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let frame = AcpBroadcastFrame {
            session_id: "s".into(),
            seq: 1,
            event: Arc::new(Event::ThinkingStarted),
            worker_generation: None,
        };
        publish(&state, frame);
        let mut receiver = state.acp_events_tx.subscribe();
        publish(
            &state,
            AcpBroadcastFrame {
                session_id: "s".into(),
                seq: 2,
                event: Arc::new(Event::ThinkingEnded),
                worker_generation: None,
            },
        );
        let delivered = receiver
            .try_recv()
            .expect("publisher remains usable after a disconnected publish");
        assert_eq!(delivered.seq, 2);
        assert!(matches!(*delivered.event, Event::ThinkingEnded));
    }

    /// The keepalive has to survive one missed round-trip and still tick well inside
    /// Cloudflare's documented 100s WebSocket idle cap. The client staleness watchdog
    /// matches the heartbeat frame byte for byte.
    #[test]
    fn keepalive_intervals_and_heartbeat_frame_are_stable() {
        assert!(
            PONG_IDLE_TIMEOUT >= PING_INTERVAL * 2,
            "PONG_IDLE_TIMEOUT ({PONG_IDLE_TIMEOUT:?}) must tolerate two missed pings at \
             PING_INTERVAL ({PING_INTERVAL:?})"
        );
        assert!(
            PING_INTERVAL < Duration::from_secs(100),
            "Cloudflare idle cap"
        );
        assert_eq!(heartbeat_frame(), r#"{"kind":"heartbeat"}"#);
    }

    /// Pins the transcript wire contract that the live loop emits.
    #[test]
    fn transcript_frames_carry_kind_seq_and_payload() {
        use crate::acp::transcript::{TranscriptModel, TranscriptRowKind};

        let mut transcript = TranscriptModel::new();
        // A prompt then a tool start: two live events, each yielding one Append.
        let script = [
            (
                7u64,
                crate::acp::Event::UserPromptSent {
                    prompt_id: None,
                    text: "hi".into(),
                    attachments: Vec::new(),
                    synthesized: false,
                },
            ),
            (
                8u64,
                crate::acp::Event::ToolCallStarted {
                    tool_call: crate::acp::state::ToolCall {
                        id: "t-1".into(),
                        name: "Bash".into(),
                        kind: "execute".into(),
                        args_preview: "{}".into(),
                        started_at: chrono::Utc::now(),
                        parent_tool_call_id: None,
                        memory_recall: None,
                        diffs: Vec::new(),
                    },
                },
            ),
        ];

        // Mirror the drain.
        let mut last_deltas = Vec::new();
        let mut last_seq = 0u64;
        for (seq, ev) in &script {
            last_deltas = transcript.apply_event(*seq, ev);
            last_seq = *seq;
        }

        // The connect snapshot envelope carries the built rows under `rows`.
        let snapshot = serde_json::json!({
            "kind": "transcript_snapshot",
            "session_id": "s1",
            "seq": last_seq,
            "rows": transcript.rows(),
        });
        assert_eq!(snapshot["kind"], "transcript_snapshot");
        assert_eq!(snapshot["seq"], 8);
        assert_eq!(snapshot["rows"].as_array().unwrap().len(), 2);
        assert_eq!(transcript.rows()[0].kind, TranscriptRowKind::UserPrompt);
        assert_eq!(transcript.rows()[1].kind, TranscriptRowKind::ToolStart);

        // The last live event produced exactly one Append delta; its envelope
        // tags `kind`, `seq`, and nests the serialized delta under `delta`.
        assert_eq!(last_deltas.len(), 1);
        let delta_frame = serde_json::json!({
            "kind": "transcript_delta",
            "session_id": "s1",
            "seq": last_seq,
            "delta": &last_deltas[0],
        });
        assert_eq!(delta_frame["kind"], "transcript_delta");
        assert_eq!(delta_frame["seq"], 8);
        // TranscriptDelta serializes as an externally tagged enum, so an Append
        // is `{"Append": {..row..}}`; clients switch on that key.
        assert_eq!(delta_frame["delta"]["Append"]["id"], "start-t-1");
    }
}
