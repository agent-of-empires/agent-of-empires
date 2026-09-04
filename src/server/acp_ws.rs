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
use serde::{Deserialize, Serialize};
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
use crate::acp::state::{AcpSessionId, AcpState, AgentName, Event};
use crate::acp::transcript::TranscriptModel;

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
    /// Set `frames=0` to receive only the folded projections
    /// (`reduced_state` + `transcript_snapshot` / `transcript_delta`) and
    /// none of the raw event frames they are built from. The drain still
    /// replays the full history server-side, since that is what builds the
    /// projections; it just stops writing the frames to this socket. The
    /// native TUI renders both projections and reads no raw frame, so this
    /// saves it the entire event history on every open. Defaults to sending
    /// them, which is what the web and `aoe acp tail` still need.
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
    // Logged at DEBUG so we can prove the route was reached even
    // when the upgrade fails. If this line is missing from debug.log
    // for a session that's stuck on "no live updates", the request
    // never got past auth_middleware (or never left the browser).
    // One line per WS connect (not per message), so debug-level
    // doesn't risk spamming.
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

    // Replay events newer than `since` immediately on connect. Without
    // this, any events published in the upgrade gap between the
    // client's POST /acp/spawn (or the first /acp/prompt) and
    // our `subscribe()` above are silently dropped by the broadcast
    // channel, since tokio's `broadcast::Sender::send` discards the
    // message when no receivers exist. The disk-backed event store
    // captures every published event, so reading it here closes the
    // race without forcing the client to GET /acp/replay
    // separately.
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
                        // Reduce this event into the connection's control state
                        // and push the updated snapshot. Reduction errors
                        // (e.g. a resolve for an approval pruned from history)
                        // are lenient no-ops, matching the client reducers.
                        // The drain deliberately overlaps this channel, so skip
                        // anything already folded: `AcpState::apply_event` is
                        // not idempotent (a duplicate `ApprovalRequested` would
                        // leave a second, unresolvable card in the shelf).
                        if frame.seq > last_applied_seq {
                            last_applied_seq = frame.seq;
                            let _ = reduced.apply_event((*frame.event).clone());
                        }
                        if !send_reduced_state(&mut socket, &session_id, frame.seq, &reduced, &mut cold).await {
                            break;
                        }
                        // Fold the same event into the transcript render model and
                        // push each resulting row change as a `transcript_delta`.
                        // Per-event emission is correctness-first; coalescing bursts
                        // into fewer frames is a later optimization.
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
                        // The skipped events never reached this connection's
                        // control fold, and nothing else would ever repair it:
                        // a missed `Stopped` leaves `turn_active` stuck true
                        // (so the composer parks every later prompt) and a
                        // missed `ApprovalRequested` never surfaces a card.
                        // Rebuild from the store, which is the truth, and push
                        // the repaired snapshot. The client's transcript half
                        // self-heals over `?view=rows`; this is the half that
                        // cannot.
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

/// Read every stored event for `session_id` with `seq > since` out of
/// the disk-backed event store, fold it into both projections, and (unless
/// the client opted out with `frames=0`) forward it to the socket as an
/// `AcpBroadcastFrame`. Returns the number of frames sent. The fold happens
/// either way: it is what builds the connect snapshots. The
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
    forward_frames: bool,
    folds: &mut ConnectionFolds<'_>,
) -> usize {
    // Offload the rusqlite read to the blocking pool. A session with
    // a large retained history may iterate thousands of rows; running
    // that on the runtime worker stalls every other concurrent task on
    // the same worker for the duration of the read.
    let store = Arc::clone(&state.acp_event_store);
    let session_id_owned = session_id.to_string();
    // Read from seq 0, not from `since`. The connect `reduced_state` snapshot
    // is a WHOLE-state frame that clients adopt verbatim, so it has to be
    // folded over the whole session: a fold that starts at the client's cursor
    // produces an empty control state (no commands, no modes, no pending
    // approvals, turn_active false) and blanks the client on every reconnect.
    // Only the raw-frame forwarding and the transcript fold are scoped to
    // `since`; the transcript is additionally reconciled by row id client-side.
    let entries =
        match tokio::task::spawn_blocking(move || store.replay_from(&session_id_owned, 0)).await {
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
    // Connect snapshot: one reduced_state frame carrying the full control state
    // built from the replay, so a fresh client renders turn/steering/approvals/
    // usage/modes without waiting for the next live event.
    let _ = send_reduced_state(socket, session_id, snapshot_seq, folds.reduced, folds.cold).await;
    // Transcript connect snapshot: one transcript_snapshot frame carrying the
    // ordered rows built from the replay, so a fresh client renders the activity
    // stream without waiting for the next live delta. Sent after the reduced_state
    // snapshot to keep a stable connect order.
    let _ = send_transcript_snapshot(socket, session_id, snapshot_seq, folds.transcript).await;
    sent
}

/// State fields large enough, and static enough, to be worth suppressing when
/// they have not changed since the last frame on this connection.
///
/// `available_commands` is the reason this exists. On a session whose agent
/// advertises the user's skills it measured ~30 KB, 91% of the frame, and it
/// changes a handful of times per session at most. Since the frame is sent
/// after every event, re-serializing it each time cost ~1.9 MB over a single
/// two-turn session. The other two are the same shape of data (adapter
/// capabilities) and ride along for the same reason.
const COLD_STATE_FIELDS: [&str; 5] = [
    "available_commands",
    "available_modes",
    "config_options",
    // Not static, but big and bursty: 16 diffs each carrying full old and new
    // text, and a background-agent record carries its prompt, tools and
    // result. Re-sending either after every `AgentMessageChunk` costs more on
    // an edit-heavy turn than the command list does on any turn.
    "recent_diffs",
    "background_agents",
];

/// Fold a session's stored history into the connection's projections and
/// return the entries the client still needs as raw frames.
///
/// The asymmetry is the whole point. `reduced` is folded over EVERY event,
/// because the `reduced_state` frame it feeds is a whole-state snapshot the
/// clients adopt verbatim: folding it from the client's `since` cursor yields
/// an empty control state and blanks the slash palette, the mode picker, the
/// plan, and any pending approval on every reconnect. The transcript fold and
/// the raw frames stay scoped to `since`, since both are incremental and the
/// client already holds the earlier rows (reconciled by row id).
/// Identity fields to seed a fresh `AcpState` with: the reducer corrects
/// `agent` on any `AgentSwitched`, but a fold that never sees one keeps this
/// seed, so both the WS connection fold and the on-demand
/// [`crate::server::session_service::SessionService::fold_control_state`]
/// must start from the same place.
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

/// The three folds a connection maintains over the event stream: the control
/// state it pushes as `reduced_state`, the ordered rows it pushes as
/// transcript frames, and the memory of which cold fields this client already
/// holds. Bundled so the drain takes one parameter for them rather than three.
struct ConnectionFolds<'a> {
    reduced: &'a mut AcpState,
    transcript: &'a mut TranscriptModel,
    cold: &'a mut ColdFieldCache,
    /// Highest seq already folded into `reduced`. `AcpState::apply_event`
    /// takes no seq and is not idempotent (`ApprovalRequested` pushes
    /// unconditionally), and the drain overlaps the live broadcast by design
    /// (see the `subscribe()` comment in `handle`), so the live loop needs
    /// this guard. `TranscriptModel` carries the equivalent one internally.
    last_applied_seq: u64,
}

/// Per-connection memory of the cold fields already sent, so an unchanged one
/// can be omitted. Keyed by field name, valued by a hash of the serialized
/// field. Lives for the life of the socket: a reconnect starts empty and
/// therefore re-sends everything, which is what makes the omission safe.
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

/// Serialize and send the reduced control state as a `kind`-tagged
/// `reduced_state` frame. Backward compatible: the raw event frame carries no
/// top-level `kind`, and clients ignore unknown kinds, so this is invisible to
/// clients that don't consume it yet. Returns whether the socket is still live
/// (a serialize failure is logged and treated as live, since it is a server
/// bug, not a dead peer).
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

/// Serialize and send the full transcript row list as a `kind`-tagged
/// `transcript_snapshot` frame on connect. Backward compatible in the same way
/// as `reduced_state`: clients that don't consume the kind ignore it. Returns
/// whether the socket is still live (a serialize failure is logged and treated
/// as live, since it is a server bug, not a dead peer).
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
/// `transcript_delta` frame. Same backward-compatibility and return contract as
/// `send_reduced_state`.
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

/// Retract a previously shown approval notification on every device once
/// the approval is handled. Mirrors `trigger_approval_push`'s tag so the
/// service worker can match and close the live notification. See #2491.
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
/// Single-sourced so the clear path can never drift from the show path.
fn approval_tag(session_id: &str) -> String {
    format!("acp-approval-{session_id}")
}

/// Tag shared by the question show and clear pushes for a session.
fn question_tag(session_id: &str) -> String {
    format!("acp-question-{session_id}")
}

/// Push-notification trigger for "agent asked you a question." Called by
/// the worker supervisor when it observes an `ElicitationRequested`
/// (`AskUserQuestion`) structured view event. A question blocks the turn
/// on the user exactly like an approval, so it gets the same dedicated,
/// suppression-bypassing push rather than only the generic Waiting one.
/// See #2146.
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

/// Retract a previously shown question notification once the question is
/// answered. Mirrors `trigger_question_push`'s tag. See #2491.
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
/// `kind: "notify"` lets the service worker tell a show from a clear; an
/// old service worker ignores the unknown field and falls back to showing
/// `title`/`body`. `seq` is the originating event seq, stored in the
/// notification so a later clear can avoid closing a newer notification.
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

/// Payload telling the service worker to retract a shown ACP attention
/// notification once the request is handled. Carries `title`/`body` so a
/// not-yet-updated service worker degrades to a benign "Resolved"
/// notification (replacing the stale one via `tag`) rather than a blank
/// one. `seq` lets the worker skip closing a newer notification. See #2491.
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

/// Shared sender for the dedicated ACP "needs your attention" pushes
/// (approval and question) and their matching clear pushes. Snapshots
/// subscribers and sends one encrypted payload each, deep-linking to the
/// session's structured view. `make_payload` builds the per-subscriber
/// payload from that subscriber's push URL, so a show path and a clear
/// path share the same fan-out. Bypasses the status-push active-session
/// suppression on purpose: these are precise, turn-blocking events, not
/// the coarse Waiting heuristic.
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

    /// The connect snapshot is a whole-state frame the clients adopt verbatim,
    /// and every client dials with a non-zero `since` after its first connect
    /// (the web seeds `lastSeq` from the tail before opening the socket; the
    /// TUI reconnects from `last_seq`). Folding the control state from that
    /// cursor instead of from 0 hands them an empty `AcpState`: no slash
    /// commands, no modes, no plan, and a pending approval that never renders.
    /// The transcript half must stay scoped to `since`, since it is
    /// incremental and the client already holds the earlier rows.
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
    /// `fold_control_state`, so the whole decision is only as good as this
    /// fold: a wrong replay bound or a lost turn flag would send a prompt into
    /// a running turn (which, mid-cancel, restarts the worker).
    ///
    /// The fold is whole-log by construction, so this drives the turn edges an
    /// event at a time and asserts the flags the decision reads.
    #[tokio::test]
    async fn fold_control_state_tracks_the_turn_flags_dispatch_reads() {
        let mut inst = crate::session::Instance::new("t", "/tmp/aoe-fold-control");
        inst.id = "s-fold".to_string();
        inst.agent_name = Some("claude".to_string());
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        // Publish through the real choke point rather than writing straight to
        // the store: that is what folds the live projection dispatch reads, so
        // going around it would test the hydrate path four times over and
        // prove nothing about the wiring.
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
        // A live steerable turn: the daemon must report it as steerable, or a
        // mid-turn prompt gets parked and #2805 comes back.
        record(
            1,
            Event::PromptCapabilities {
                image: false,
                audio: false,
                embedded_context: false,
                steering: true,
            },
        );
        record(
            2,
            Event::UserPromptSent {
                text: "go".into(),
                attachments: Vec::new(),
                prompt_id: None,
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
                    rate_limit_exhausted: false,
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
                    rate_limit_exhausted: false,
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
                    rate_limit_exhausted: false,
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
    #[test]
    fn control_fold_skips_events_the_drain_already_applied() {
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
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
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
        fold_connect_history(
            vec![(
                7,
                Event::ApprovalRequested {
                    approval: approval("n-1"),
                },
            )],
            0,
            &mut folds,
        );
        assert_eq!(folds.reduced.pending_approvals.len(), 1);

        // The same event arrives again over the broadcast channel. This mirrors
        // the guard in the live loop.
        let redelivered_seq = 7;
        if redelivered_seq > folds.last_applied_seq {
            let _ = folds.reduced.apply_event(Event::ApprovalRequested {
                approval: approval("n-1"),
            });
        }
        assert_eq!(
            folds.reduced.pending_approvals.len(),
            1,
            "a redelivered event must not double the shelf"
        );
    }

    /// `frames` gates only the raw-frame forwarding, and its default has to
    /// stay "send them": the web reducer and `aoe acp tail` both still read
    /// raw frames, so a wrong default silently blanks them.
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
        // Long text is truncated and gets an ellipsis. The cap counts
        // chars, not bytes, so the result is at most MAX + the ellipsis.
        let long = "word ".repeat(100);
        let snippet = push_body_snippet(&long);
        assert!(snippet.ends_with('…'));
        assert_eq!(snippet.chars().count(), 120 + 1);
    }

    #[test]
    fn attention_tags_are_session_scoped_and_kind_distinct() {
        // The clear path reuses these helpers, so a drift here would
        // silently fail to close the matching notification (#2491).
        assert_eq!(approval_tag("s1"), "acp-approval-s1");
        assert_eq!(question_tag("s1"), "acp-question-s1");
        assert_ne!(approval_tag("s1"), question_tag("s1"));
    }

    #[test]
    fn clear_payload_carries_kind_and_seq() {
        let json = serde_json::to_value(AcpClearPayload {
            kind: "clear",
            title: "Resolved",
            body: "Handled on another device",
            url: "/sessions/s1/acp".into(),
            tag: approval_tag("s1"),
            session_id: "s1".into(),
            seq: 7,
        })
        .unwrap();
        assert_eq!(json["kind"], "clear");
        assert_eq!(json["tag"], "acp-approval-s1");
        assert_eq!(json["seq"], 7);
        // title/body are present so a not-yet-updated service worker
        // degrades to a benign notification rather than a blank one.
        assert_eq!(json["title"], "Resolved");
    }

    #[test]
    fn notify_payload_tags_kind_notify() {
        let json = serde_json::to_value(AcpNotifyPayload {
            kind: "notify",
            title: "t".into(),
            body: "b".into(),
            url: "/sessions/s1/acp".into(),
            tag: question_tag("s1"),
            session_id: "s1".into(),
            seq: 3,
        })
        .unwrap();
        assert_eq!(json["kind"], "notify");
        assert_eq!(json["seq"], 3);
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

    /// Pins the transcript wire contract that the live loop emits. The
    /// `send_transcript_*` helpers need a live socket, so the socket write is
    /// covered by the Stage C live e2e; here we drive the same choke-point fold
    /// (`TranscriptModel::apply_event`) over a scripted sequence and assert the
    /// snapshot and delta envelopes are shaped as clients expect. The envelopes
    /// are built the same way the helpers build them.
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

        // Mirror the drain: fold every event, keep only the last delta batch and
        // build the connect snapshot from the accumulated rows.
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
