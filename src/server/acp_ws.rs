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
use crate::acp::state::{AcpSessionId, AcpState, AgentName};
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
///
/// CityHall client mode is refused for any target that is not a structured
/// session it owns: the socket streams the whole transcript and the live ACP
/// event frames, so a foreign or crafted id would hand a locked-down client
/// the same conversation content the gated REST reads refuse.
pub async fn acp_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<AcpWsQuery>,
) -> axum::response::Response {
    if let Some(resp) =
        crate::server::api::sessions::cityhall_block_non_structured(&state, &id).await
    {
        return resp;
    }
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
        .into_response()
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
    let mut runtime_snapshots = state.runtime.subscribe_snapshots();
    let replay_count = drain_replay_into_socket(
        &mut socket,
        &state,
        &session_id,
        since,
        forward_frames,
        &mut folds,
    )
    .await;
    let replay_count = match replay_count {
        Ok(count) => count,
        Err(error) => {
            warn!(target: "acp.ws", session = %session_id, %error, "replay failed; no snapshot published");
            return;
        }
    };
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
            changed = runtime_snapshots.changed() => {
                if changed.is_err() { break; }
                if !send_native_state(
                    &state, &mut socket, &session_id, last_applied_seq, &reduced, &mut cold, true,
                ).await {
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
                        let metadata = frame.view_metadata();
                        // Reduce this event into the connection's control state and push
                        // the updated snapshot.
                        if frame.seq > last_applied_seq {
                            last_applied_seq = frame.seq;
                            if let Err(error) = reduced.apply_scoped_event(frame.seq, &frame.event, &metadata) {
                                warn!(target: "acp.ws", session = %session_id, %error, "live control fold failed");
                                break;
                            }
                        }
                        if !send_reduced_state(&state, &mut socket, &session_id, frame.seq, &reduced, &mut cold).await {
                            break;
                        }
                        // Fold the same event into the transcript render model and push
                        // each resulting row change as a `transcript_delta`.
                        let deltas = transcript.apply_scoped_event(frame.seq, &frame.event, &metadata);
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
                        // Rebuild the retained suffix after this connection misses events.
                        let rebuilt = AcpState::new(
                            AcpSessionId(session_id.clone()),
                            seed.0.clone(),
                            seed.1.clone(),
                        );
                        let store = Arc::clone(&state.acp_event_store);
                        let session_for_read = session_id.clone();
                        let entries = match tokio::task::spawn_blocking(move || {
                            store.replay_from(&session_for_read, 0)
                        }).await {
                            Ok(Ok(entries)) => entries,
                            result => {
                                warn!(target: "acp.ws", session = %session_id, ?result, "replay rebuild failed");
                                break;
                            }
                        };
                        let rebuilt_result = entries.iter().try_fold(rebuilt, |mut fold, stored| {
                            fold.apply_scoped_event(stored.seq, &stored.event, &stored.metadata)?;
                            Ok::<_, crate::acp::state::StateError>(fold)
                        });
                        let rebuilt = match rebuilt_result {
                            Ok(rebuilt) => rebuilt,
                            Err(error) => {
                                warn!(target: "acp.ws", session = %session_id, %error, "replay control fold failed");
                                break;
                            }
                        };
                        let highest = rebuilt.last_seq;
                        reduced = rebuilt;
                        last_applied_seq = highest;
                        // The cold-field cache still describes what this socket
                        // holds, so an unchanged command list stays omitted.
                        if !send_reduced_state(
                            &state,
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
) -> anyhow::Result<usize> {
    // Offload the rusqlite read to the blocking pool.
    let store = Arc::clone(&state.acp_event_store);
    let session_id_owned = session_id.to_string();
    // Read from seq 0, not from `since`.
    let entries =
        tokio::task::spawn_blocking(move || store.replay_from(&session_id_owned, 0)).await??;
    let mut sent = 0usize;
    let to_forward = fold_connect_history(entries, since, folds)?;
    let snapshot_seq = folds.last_applied_seq.max(since);
    for stored in to_forward {
        if !forward_frames {
            continue;
        }
        let frame = AcpBroadcastFrame::from_qualified(
            session_id.to_owned(),
            stored.seq,
            stored.event,
            &stored.metadata,
            None,
            None,
            None,
        );
        let payload = serde_json::to_string(&frame)?;
        socket.send(Message::Text(payload.into())).await?;
        sent += 1;
    }
    // Connect snapshot.
    anyhow::ensure!(
        send_reduced_state(
            state,
            socket,
            session_id,
            snapshot_seq,
            folds.reduced,
            folds.cold,
        )
        .await,
        "control snapshot delivery failed"
    );
    // Transcript connect snapshot.
    anyhow::ensure!(
        send_transcript_snapshot(socket, session_id, snapshot_seq, folds.transcript).await,
        "transcript snapshot delivery failed"
    );
    Ok(sent)
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
    mut entries: Vec<crate::acp::event_store::StoredEvent>,
    since: u64,
    folds: &mut ConnectionFolds<'_>,
) -> anyhow::Result<Vec<crate::acp::event_store::StoredEvent>> {
    for stored in &entries {
        folds
            .reduced
            .apply_scoped_event(stored.seq, &stored.event, &stored.metadata)?;
        folds.last_applied_seq = stored.seq;
        if stored.seq <= since {
            folds.transcript.observe_prior(&stored.event);
        } else {
            folds
                .transcript
                .apply_scoped_event(stored.seq, &stored.event, &stored.metadata);
        }
    }
    entries.retain(|stored| stored.seq > since);
    Ok(entries)
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
    native: Option<crate::acp::state::NativeAvailabilityProjection>,
    recovery: Option<Option<crate::session::SessionRecovery>>,
    rate_limit_park: Option<&'static str>,
    original_decision_nonces: Vec<crate::acp::approvals::Nonce>,
    snapshot_seq: u64,
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
    owner: &AppState,
    socket: &mut WebSocket,
    session_id: &str,
    seq: u64,
    reduced: &AcpState,
    cold: &mut ColdFieldCache,
) -> bool {
    send_native_state(owner, socket, session_id, seq, reduced, cold, false).await
}

async fn send_native_state(
    owner: &AppState,
    socket: &mut WebSocket,
    session_id: &str,
    seq: u64,
    reduced: &AcpState,
    cold: &mut ColdFieldCache,
    only_if_changed: bool,
) -> bool {
    let store = std::sync::Arc::clone(&owner.acp_event_store);
    let id = session_id.to_owned();
    let park = match tokio::task::spawn_blocking(move || store.rate_limit_park_snapshot(&id)).await
    {
        Ok(Ok((frontier, park))) => projected_rate_limit_park(
            frontier,
            seq.max(cold.snapshot_seq).max(reduced.last_seq),
            park,
        ),
        error => {
            warn!(target: "acp.ws", "read reduced_state quota projection: {error:?}");
            return false;
        }
    };
    let observation = owner
        .session_service
        .native_control_observation(
            session_id,
            reduced.native_availability,
            reduced.native_control_seq,
        )
        .await;
    let contended = observation.is_contended();
    let mut sampled_recovery = match current_recovery_projection(
        owner,
        session_id,
        reduced,
        observation.projection,
        !contended && !owner.read_only && !owner.cityhall_mode,
    )
    .await
    {
        Ok(recovery) => recovery,
        Err(error) => {
            warn!(target: "acp.ws", %error, "read recovery projection");
            return false;
        }
    };
    if contended {
        let original = crate::acp::control_cache::OriginalDecisions::default();
        if (cold.native != Some(observation.projection)
            || cold.rate_limit_park != park
            || cold.recovery.as_ref() != Some(&sampled_recovery)
            || !cold.original_decision_nonces.is_empty())
            && !send_projected_reduced_state(
                socket,
                session_id,
                seq,
                reduced,
                cold,
                LiveControlProjection {
                    native: observation.projection,
                    rate_limit_park: park,
                    original: &original,
                    recovery: sampled_recovery.take(),
                },
            )
            .await
        {
            return false;
        }
    }
    let mut original = match owner
        .session_service
        .acp_control_cache
        .original_decisions(session_id, seq.max(cold.snapshot_seq).max(reduced.last_seq))
        .await
    {
        Ok(original) => original,
        Err(error) => {
            warn!(target: "acp.ws", %error, "read original decision projection");
            return false;
        }
    };
    if !contended
        && only_if_changed
        && cold.native == Some(observation.projection)
        && cold.rate_limit_park == park
        && cold.original_decision_nonces == original.nonces
        && cold.recovery.as_ref() == Some(&sampled_recovery)
    {
        return true;
    }
    let native = owner
        .session_service
        .settle_native_control_observation(session_id, observation)
        .await;
    // A native exclusion can outlast a human answer or retirement. Re-read
    // actual responder custody after that wait; never resend a stale capability.
    if contended {
        original = match owner
            .session_service
            .acp_control_cache
            .original_decisions(session_id, seq.max(cold.snapshot_seq).max(reduced.last_seq))
            .await
        {
            Ok(original) => original,
            Err(error) => {
                warn!(target: "acp.ws", %error, "refresh original decision projection");
                return false;
            }
        };
    }
    let recovery = if contended {
        match current_recovery_projection(
            owner,
            session_id,
            reduced,
            native,
            !owner.read_only && !owner.cityhall_mode,
        )
        .await
        {
            Ok(recovery) => recovery,
            Err(error) => {
                warn!(target: "acp.ws", %error, "refresh recovery projection");
                return false;
            }
        }
    } else {
        sampled_recovery
    };
    send_projected_reduced_state(
        socket,
        session_id,
        seq,
        reduced,
        cold,
        LiveControlProjection {
            native,
            rate_limit_park: park,
            original: &original,
            recovery,
        },
    )
    .await
}

async fn current_recovery_projection(
    owner: &AppState,
    session_id: &str,
    reduced: &AcpState,
    native: crate::acp::state::NativeAvailabilityProjection,
    mutations: bool,
) -> anyhow::Result<Option<crate::session::SessionRecovery>> {
    let row = owner
        .instances
        .read()
        .await
        .iter()
        .find(|row| row.id == session_id)
        .cloned();
    match row {
        Some(row) => {
            owner
                .session_service
                .recovery_projection(&row, Some(reduced.clone()), native, mutations)
                .await
        }
        None => Ok(None),
    }
}

fn projected_rate_limit_park(
    frontier: u64,
    snapshot_seq: u64,
    park: crate::acp::event_store::ControlKnowledge<Option<bool>>,
) -> Option<&'static str> {
    use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
    if frontier != snapshot_seq {
        return None;
    }
    match park {
        Known(None) => Some("none"),
        Known(Some(false)) => Some("armed"),
        Known(Some(true)) => Some("exhausted"),
        Indeterminate => None,
    }
}

struct LiveControlProjection<'a> {
    native: crate::acp::state::NativeAvailabilityProjection,
    rate_limit_park: Option<&'static str>,
    original: &'a crate::acp::control_cache::OriginalDecisions,
    recovery: Option<crate::session::SessionRecovery>,
}

async fn send_projected_reduced_state(
    socket: &mut WebSocket,
    session_id: &str,
    seq: u64,
    reduced: &AcpState,
    cold: &mut ColdFieldCache,
    projection: LiveControlProjection<'_>,
) -> bool {
    let LiveControlProjection {
        native,
        rate_limit_park,
        original,
        recovery,
    } = projection;
    let mut state = match serde_json::to_value(reduced) {
        Ok(v) => v,
        Err(e) => {
            warn!(target: "acp.ws", "serialise reduced_state: {e}");
            return true;
        }
    };
    state["connection_ready"] = serde_json::json!(native.ready);
    state["connection_unavailable"] = serde_json::json!(native.unavailable);
    state["prompt_wake_eligible"] = serde_json::json!(native.prompt_wake_eligible);
    state["rate_limit_park"] = serde_json::json!(rate_limit_park);
    state["recovery"] = serde_json::json!(recovery);
    let channel_loss = native
        .unavailable
        .is_some_and(|unavailable| unavailable.source.is_channel_loss());
    if channel_loss || native.control_failure.is_some() {
        for field in ["turn_active", "cancelling", "compacting"] {
            state[field] = serde_json::Value::Bool(false);
        }
    }
    if channel_loss {
        for field in ["pending_approvals", "pending_elicitations"] {
            state[field] = serde_json::json!([]);
        }
        for field in ["in_flight_tool", "thinking", "compacting_tool_call_id"] {
            state[field] = serde_json::Value::Null;
        }
    }
    state["original_decision_nonces"] = serde_json::json!(original.nonces);
    for (field, requests) in [
        (
            "pending_approvals",
            serde_json::to_value(&original.approvals),
        ),
        (
            "pending_elicitations",
            serde_json::to_value(&original.elicitations),
        ),
    ] {
        let Ok(serde_json::Value::Array(requests)) = requests else {
            return false;
        };
        let Some(pending) = state[field].as_array_mut() else {
            return false;
        };
        for request in requests {
            if !pending
                .iter()
                .any(|existing| existing["nonce"] == request["nonce"])
            {
                pending.push(request);
            }
        }
    }
    cold.original_decision_nonces.clone_from(&original.nonces);
    cold.native = Some(native);
    cold.rate_limit_park = rate_limit_park;
    cold.recovery = Some(recovery);
    let seq = seq.max(cold.snapshot_seq).max(reduced.last_seq);
    cold.snapshot_seq = seq;
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
    send_acp_push(state, session_id, false, |url| AcpNotifyPayload {
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
    send_acp_push(state, session_id, true, |url| AcpClearPayload {
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
    send_acp_push(state, session_id, false, |url| AcpNotifyPayload {
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
    send_acp_push(state, session_id, true, |url| AcpClearPayload {
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
/// question) and their matching clear pushes. A clear shows nothing, so it skips
/// subscriptions whose browser revokes push after silent deliveries (#2491).
async fn send_acp_push<T, F>(state: &AppState, session_id: &str, silent: bool, make_payload: F)
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
    tracing::info!(
        target: "push",
        session = %session_id,
        silent,
        subs = subs.len(),
        "acp push fired"
    );
    let client = match super::push_send::build_client() {
        Ok(c) => c,
        Err(e) => {
            warn!(target: "acp.push", "build_client: {e}");
            return;
        }
    };
    for sub in subs {
        if silent && !super::push::accepts_silent_push(&sub) {
            continue;
        }
        let Some(url) = super::push::build_push_url(&sub, &path) else {
            continue;
        };
        super::push::deliver(push, &client, &sub, &make_payload(url), 60).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::state::Event;

    // Reconnect cursors restrict transcript replay, not pending control state.
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
            plan: None,
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        let row = |seq, event| crate::acp::event_store::StoredEvent {
            seq,
            event: Arc::new(event),
            metadata: crate::acp::event_sender::EventMetadata {
                scope: crate::acp::event_sender::EventScope::SessionCurrent,
                ..crate::acp::event_sender::EventMetadata::history()
            },
        };
        let history = vec![
            row(
                1,
                Event::AvailableCommandsUpdated {
                    commands: vec![crate::acp::state::AvailableCommand {
                        name: "review".into(),
                        description: "Review".into(),
                        accepts_input: false,
                    }],
                },
            ),
            row(
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
            row(3, Event::ApprovalRequested { approval }),
            row(
                4,
                Event::AgentMessageChunk {
                    text: "hello".into(),
                    message_id: None,
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
        let forwarded = fold_connect_history(history.clone(), 4, &mut folds).unwrap();

        assert!(forwarded.is_empty(), "nothing new to forward");
        assert_eq!(folds.last_applied_seq, 4);
        assert!(
            folds.transcript.rows().is_empty(),
            "transcript stays scoped to since; the client holds those rows"
        );
        // The control state is whole-session regardless of the cursor.
        let reduced = &folds.reduced;
        assert_eq!(reduced.available_commands[0].name, "review");
        assert_eq!(reduced.available_modes.len(), 1, "mode picker survives");
        assert_eq!(reduced.current_mode_id.as_deref(), Some("plan"));
        assert_eq!(reduced.pending_approvals[0].nonce.0, "n-1");

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
        let forwarded = fold_connect_history(history, 0, &mut cold_folds).unwrap();
        assert_eq!(forwarded.len(), 4);
        assert!(!cold_folds.transcript.rows().is_empty());
        assert_eq!(cold_folds.reduced.available_commands.len(), 1);
        assert_eq!(cold_folds.reduced.pending_approvals.len(), 1);
    }

    #[test]
    fn retained_notice_keeps_its_dismissal_identity_after_reconnect() {
        let directory = tempfile::tempdir().unwrap();
        let id = "notice-retention";
        let store =
            crate::acp::event_store::EventStore::open(&directory.path().join("events.db"), 2)
                .unwrap();
        store
            .record(
                id,
                1,
                &Event::AvailableCommandsUpdated {
                    commands: Vec::new(),
                },
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        for seq in 2..=4 {
            store
                .record(
                    id,
                    seq,
                    &Event::AgentMessageChunk {
                        text: "output".into(),
                        message_id: None,
                    },
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
        }
        store
            .record(
                id,
                5,
                &Event::SessionNotice {
                    key: None,
                    severity: "warning".into(),
                    title: "Model fallback".into(),
                    description: None,
                },
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        let retained = store.replay_from(id, 0).unwrap();
        assert_eq!(
            retained.iter().map(|stored| stored.seq).collect::<Vec<_>>(),
            [1, 4, 5]
        );
        let mut reduced = AcpState::new(AcpSessionId(id.into()), AgentName("claude".into()), None);
        let mut transcript = TranscriptModel::new();
        let mut cold = ColdFieldCache::default();
        let mut folds = ConnectionFolds {
            reduced: &mut reduced,
            transcript: &mut transcript,
            cold: &mut cold,
            last_applied_seq: 0,
        };
        fold_connect_history(retained, 5, &mut folds).unwrap();
        assert_eq!(folds.reduced.session_notices[0].id, "notice-5");
    }

    /// A reconnect between two agent messages still marks the new one, since
    /// the skipped history says which message the client already holds.
    #[test]
    fn reconnect_fold_marks_a_message_that_follows_the_cursor() {
        let chunk = |text: &str, id: &str| Event::AgentMessageChunk {
            text: text.into(),
            message_id: Some(id.into()),
        };
        let history = vec![
            crate::acp::event_store::StoredEvent {
                seq: 1,
                event: Arc::new(chunk("for #4656.", "m-a")),
                metadata: crate::acp::event_sender::EventMetadata::history(),
            },
            crate::acp::event_store::StoredEvent {
                seq: 2,
                event: Arc::new(chunk("", "m-b")),
                metadata: crate::acp::event_sender::EventMetadata::history(),
            },
        ];
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
        fold_connect_history(history, 1, &mut folds).unwrap();
        let rows = folds.transcript.rows();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].message_start);
    }

    /// Replay overlaps live broadcast; duplicates would leave unresolvable approvals.
    type TestSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    async fn connect_test_socket(
        state: Arc<AppState>,
        session_id: &str,
    ) -> (TestSocket, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = axum::Router::new()
            .route("/{id}", axum::routing::get(acp_ws))
            .with_state(state);
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let (socket, _) =
            tokio_tungstenite::connect_async(format!("ws://{addr}/{session_id}?frames=0"))
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
    #[test]
    fn quota_snapshot_never_claims_knowledge_at_a_different_frontier() {
        use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
        for park in [
            Known(None),
            Known(Some(false)),
            Known(Some(true)),
            Indeterminate,
        ] {
            assert_eq!(projected_rate_limit_park(21, 20, park), None);
            assert_eq!(projected_rate_limit_park(19, 20, park), None);
        }
        assert_eq!(projected_rate_limit_park(20, 20, Indeterminate), None);
    }

    #[tokio::test]
    async fn quota_snapshots_ignore_history_and_follow_real_host_boundaries() {
        use crate::acp::event_sender::EventMetadata;
        let _home = crate::session::test_support::isolate_app_dir();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let (mut socket, server) = connect_test_socket(state.clone(), "s-1").await;
        let initial = receive_kind(&mut socket, "reduced_state").await;
        assert_eq!(initial["state"]["rate_limit_park"], "none");
        receive_kind(&mut socket, "transcript_snapshot").await;
        for (seq, event, host, expected) in [
            (
                1,
                Event::Stopped {
                    reason: "rate_limit_exhausted_retries".into(),
                },
                false,
                "none",
            ),
            (
                2,
                Event::Stopped {
                    reason: "rate_limited".into(),
                },
                false,
                "none",
            ),
            (
                3,
                Event::Stopped {
                    reason: "rate_limit_exhausted_retries".into(),
                },
                true,
                "exhausted",
            ),
            (
                4,
                Event::Stopped {
                    reason: "rate_limited".into(),
                },
                false,
                "exhausted",
            ),
            (
                5,
                Event::UserPromptSent {
                    text: "new explicit input".into(),
                    prompt_id: None,
                    origin_plugin: None,
                    attachments: Vec::new(),
                    synthesized: false,
                },
                true,
                "none",
            ),
        ] {
            let metadata = if host {
                EventMetadata::host(&event)
            } else {
                EventMetadata::history()
            };
            state
                .acp_event_store
                .record("s-1", seq, &event, &metadata)
                .unwrap();
            publish(
                &state,
                AcpBroadcastFrame::from_qualified(
                    "s-1".into(),
                    seq,
                    Arc::new(event),
                    &metadata,
                    None,
                    None,
                    None,
                ),
            );
            let snapshot = receive_kind(&mut socket, "reduced_state").await;
            assert_eq!(snapshot["seq"], seq);
            assert_eq!(snapshot["state"]["rate_limit_park"], expected);
        }
        state.shutdown.cancel();
        drop(socket);
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn contended_native_observation_requalifies_at_the_same_frontier_without_holding_guards()
    {
        use crate::acp::supervisor::test_support::managed::with_managed;
        use futures_util::StreamExt;
        #[derive(Clone, Copy, Debug)]
        enum Change {
            None,
            Loss,
            Producer,
            Incarnation,
            Profile,
        }
        for (lifecycle_contention, registry_contention, change) in [
            (false, true, Change::None),
            (false, false, Change::None),
            (true, false, Change::None),
            (false, false, Change::Loss),
            (true, false, Change::Loss),
            (false, false, Change::Producer),
            (true, false, Change::Incarnation),
            (true, false, Change::Profile),
        ] {
            with_managed(|managed| {
                Box::pin(async move {
                    let live = crate::server::test_support::managed_control_session(
                        managed,
                        "contended-ws",
                    )
                    .await;
                    let state = live.state.clone();
                    let id = live.id().to_owned();
                    let setup_submission = tokio::time::timeout(
                        Duration::from_secs(10),
                        state.session_service.prompt_submission(&id),
                    )
                    .await
                    .expect("actual creation releases prompt admission before lock contention");
                    drop(setup_submission);
                    let row = state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone();
                    let storage =
                        Arc::new(crate::session::Storage::open_reference("test").unwrap());
                    let original = state
                        .acp_supervisor
                        .current_native_availability(&id)
                        .await
                        .unwrap();
                    assert!(
                        !original.ready && original.unavailable.is_none(),
                        "raw capture is not qualified availability"
                    );
                    let scope = original.scope.clone();
                    let drain_pause = live.client.pause_drain().await;
                    let frontier = state.acp_event_store.highest_seq(&id);
                    let native_requests = live.native.requests();
                    let (entered_tx, entered) = tokio::sync::oneshot::channel();
                    let (release, released) = std::sync::mpsc::channel();
                    let held_lifecycle = original.lifecycle.clone();
                    let held_scope = scope.clone();
                    let held_id = id.clone();
                    let blocker = tokio::task::spawn_blocking(move || {
                        if lifecycle_contention {
                            let _held = held_lifecycle.lock().unwrap();
                            entered_tx.send(()).unwrap();
                            released
                                .recv_timeout(std::time::Duration::from_secs(10))
                                .expect("qualification lock requires explicit release");
                        } else if registry_contention {
                            let _held =
                                crate::process::worker_registry::acquire_registry_lock(&held_id)
                                    .unwrap();
                            entered_tx.send(()).unwrap();
                            released
                                .recv_timeout(std::time::Duration::from_secs(10))
                                .expect("registry qualification requires explicit release");
                        } else {
                            let _held = held_scope.fence.lock_observation().unwrap();
                            entered_tx.send(()).unwrap();
                            released
                                .recv_timeout(std::time::Duration::from_secs(10))
                                .expect("qualification lock requires explicit release");
                        }
                    });
                    entered.await.unwrap();
                    let (mut socket, server) = connect_test_socket(state.clone(), &id).await;
                    let unknown = receive_kind(&mut socket, "reduced_state").await;
                    assert_eq!(unknown["seq"], frontier);
                    assert_eq!(
                        unknown["state"]["connection_ready"], false,
                        "{lifecycle_contention}/{change:?}"
                    );
                    assert!(unknown["state"]["connection_unavailable"].is_null());
                    assert_eq!(unknown["state"]["prompt_wake_eligible"], false);
                    assert!(
                        state.instances.try_write().is_ok(),
                        "instances must be free during qualification"
                    );
                    assert!(
                        state.publication.try_write().is_ok(),
                        "publication must be free during qualification"
                    );
                    assert!(state
                        .session_service
                        .try_prompt_submission(&id)
                        .await
                        .is_some());
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        state.acp_supervisor.current_native_availability(&id),
                    )
                    .await
                    .expect("worker map stays free")
                    .unwrap();
                    let cache = state.session_service.acp_control_cache.clone();
                    let cache_id = id.clone();
                    tokio::time::timeout(
                        Duration::from_secs(2),
                        tokio::task::spawn_blocking(move || cache.native_metadata(&cache_id)),
                    )
                    .await
                    .expect("cache slot stays free")
                    .unwrap();
                    if lifecycle_contention {
                        assert!(
                            scope
                                .fence
                                .try_lock_observation_matching(|_| true)
                                .unwrap()
                                .is_some(),
                            "lifecycle contention must release native fence"
                        );
                    } else {
                        assert!(
                            original.lifecycle.try_lock().is_ok(),
                            "native contention must release lifecycle"
                        );
                    }
                    let mut expected_loss = None;
                    match change {
                        Change::None => {}
                        Change::Loss => {
                            live.native.close_stdout().await;
                            let loss = tokio::time::timeout(Duration::from_secs(10), async {
                                loop {
                                    if let Some(loss) = live.client.connection_unavailable() {
                                        break loss;
                                    }
                                    tokio::task::yield_now().await;
                                }
                            })
                            .await
                            .expect("real Native EOF becomes observable during qualification");
                            assert_eq!(loss.source, crate::acp::state::AcpConnectionSource::Native);
                            assert_eq!(
                                loss.cause,
                                crate::acp::state::AcpConnectionCause::StdoutEof
                            );
                            expected_loss = Some(loss);
                        }
                        Change::Producer => {
                            state.acp_supervisor.test_respawn_worker(&id).await;
                        }
                        Change::Incarnation => {
                            let created = row.created_at + chrono::Duration::seconds(1);
                            let disk = storage.clone();
                            let disk_id = id.clone();
                            tokio::task::spawn_blocking(move || {
                                disk.update(|rows, _| {
                                    rows.iter_mut()
                                        .find(|row| row.id == disk_id)
                                        .unwrap()
                                        .created_at = created;
                                    Ok(())
                                })
                            })
                            .await
                            .unwrap()
                            .unwrap();
                            state
                                .instances
                                .write()
                                .await
                                .iter_mut()
                                .find(|row| row.id == id)
                                .unwrap()
                                .created_at = created;
                        }
                        Change::Profile => {
                            let mut moved = row.clone();
                            moved.source_profile = "superseded-profile".into();
                            let disk = storage.clone();
                            let disk_id = id.clone();
                            tokio::task::spawn_blocking(move || {
                                crate::session::Storage::new_unwatched("superseded-profile")
                                    .unwrap()
                                    .update(|rows, _| {
                                        rows.push(moved);
                                        Ok(())
                                    })
                                    .unwrap();
                                disk.update(|rows, _| {
                                    rows.retain(|row| row.id != disk_id);
                                    Ok(())
                                })
                                .unwrap();
                            })
                            .await
                            .unwrap();
                            state
                                .instances
                                .write()
                                .await
                                .iter_mut()
                                .find(|row| row.id == id)
                                .unwrap()
                                .source_profile = "superseded-profile".into();
                        }
                    }
                    // The physical side effect must happen inside the exclusion window,
                    // not after a timed-out blocker silently let qualification finish.
                    if lifecycle_contention {
                        assert!(
                            matches!(
                                original.lifecycle.try_lock(),
                                Err(std::sync::TryLockError::WouldBlock)
                            ),
                            "lifecycle exclusion must remain held until explicit release"
                        );
                    } else if registry_contention {
                        assert!(
                            crate::process::worker_registry::try_registry_lock(&id)
                                .unwrap()
                                .is_none(),
                            "registry exclusion must remain held until explicit release"
                        );
                    } else if matches!(change, Change::None | Change::Loss) {
                        assert!(
                            scope
                                .fence
                                .try_lock_observation_matching(|_| true)
                                .unwrap()
                                .is_none(),
                            "native exclusion must remain held until explicit release"
                        );
                    }
                    release.send(()).unwrap();
                    blocker.await.unwrap();
                    // Only the connect snapshot's settlement counts. A later runtime
                    // snapshot must not hide a failed qualification at this frontier.
                    let settled = tokio::time::timeout(Duration::from_secs(10), async {
                        let mut settled = None;
                        loop {
                            let message =
                                socket.next().await.expect("socket remains open").unwrap();
                            let tokio_tungstenite::tungstenite::Message::Text(text) = message
                            else {
                                continue;
                            };
                            let frame: serde_json::Value = serde_json::from_str(&text).unwrap();
                            match frame["kind"].as_str() {
                                Some("reduced_state") => {
                                    assert!(
                                        settled.is_none(),
                                        "connect must emit exactly one settled snapshot: {frame}"
                                    );
                                    settled = Some(frame);
                                }
                                Some("transcript_snapshot") => {
                                    break settled
                                        .expect("connect sends settlement before transcript");
                                }
                                _ => panic!("unexpected connect frame: {frame}"),
                            }
                        }
                    })
                    .await
                    .expect("contended connect completes after explicit release");
                    let current = state.acp_supervisor.current_native_availability(&id).await;
                    let same_actor = current
                        .as_ref()
                        .is_some_and(|current| original.same_actor(current));
                    assert_eq!(
                        live.native.requests(),
                        native_requests,
                        "same-frontier qualification must issue no Native RPC"
                    );
                    assert_eq!(settled["seq"], frontier, "qualification needs no new event");
                    assert_eq!(state.acp_event_store.highest_seq(&id), frontier);
                    assert_eq!(settled["state"]["prompt_wake_eligible"], false);
                    match change {
                        Change::None => assert_eq!(settled["state"]["connection_ready"], true),
                        Change::Loss => {
                            assert_eq!(settled["state"]["connection_ready"], false);
                            assert!(same_actor, "physical EOF must retain the original actor");
                            assert_eq!(
                                settled["state"]["connection_unavailable"],
                                serde_json::json!(expected_loss),
                                "qualification must report actual sticky Native loss; \
                                 lifecycle_contention={lifecycle_contention}, \
                                 same_actor={same_actor}, \
                                 current_loss={:?}, snapshot={settled}",
                                live.client.connection_unavailable()
                            );
                        }
                        Change::Producer | Change::Incarnation | Change::Profile => {
                            assert_eq!(settled["state"]["connection_ready"], false);
                            assert!(settled["state"]["connection_unavailable"].is_null());
                        }
                    }
                    drop(socket);
                    server.abort();
                    let _ = server.await;
                    if matches!(change, Change::Incarnation | Change::Profile) {
                        let disk = storage.clone();
                        let restore = row.clone();
                        let restore_id = id.clone();
                        tokio::task::spawn_blocking(move || {
                            if matches!(change, Change::Profile) {
                                crate::session::Storage::open_reference("superseded-profile")
                                    .unwrap()
                                    .update(|rows, _| {
                                        rows.retain(|row| row.id != restore_id);
                                        Ok(())
                                    })?;
                            }
                            disk.update(|rows, _| {
                                rows.retain(|row| row.id != restore_id);
                                rows.push(restore);
                                Ok(())
                            })
                        })
                        .await
                        .unwrap()
                        .unwrap();
                        *state
                            .instances
                            .write()
                            .await
                            .iter_mut()
                            .find(|row| row.id == id)
                            .unwrap() = row;
                    }
                    drop(drain_pause);
                })
            })
            .await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn physical_owner_replacement_during_native_wait_cannot_restore_old_ready() {
        use crate::acp::supervisor::test_support::{managed::with_managed, HeldAttachRunner};
        use crate::process::worker_registry;
        with_managed(|managed| {
            Box::pin(async move {
                let mut live =
                    crate::server::test_support::managed_control_session(managed, "physical-ws")
                        .await;
                let state = live.state.clone();
                let id = live.id().to_owned();
                crate::session::set_unread_enabled(true);
                let request = live.start_prompt("original actual turn").await;
                live.finish_prompt(&request, None).await;
                let current = state
                    .acp_supervisor
                    .current_native_availability(&id)
                    .await
                    .unwrap();
                let owner = current.owner.unwrap();
                let original_record = worker_registry::load_strict(&id).unwrap().unwrap();
                assert_eq!(Some(owner), live.client.connected_runner());
                assert!(owner.birth_fingerprint.is_some());
                assert!(
                    state
                        .session_service
                        .native_control_projection(&id, None, 0)
                        .await
                        .ready
                );
                let drain_pause = live.client.pause_drain().await;
                let frontier = state.acp_event_store.highest_seq(&id);
                let held_scope = current.scope.clone();
                let (entered_tx, entered) = tokio::sync::oneshot::channel();
                let (release, released) = std::sync::mpsc::channel();
                let blocker = tokio::task::spawn_blocking(move || {
                    let _held = held_scope.fence.lock_observation().unwrap();
                    entered_tx.send(()).unwrap();
                    let _ = released.recv_timeout(std::time::Duration::from_secs(10));
                });
                entered.await.unwrap();
                let (mut socket, server) = connect_test_socket(state.clone(), &id).await;
                let unknown = receive_kind(&mut socket, "reduced_state").await;
                assert_eq!(unknown["seq"], frontier);
                assert_eq!(unknown["state"]["connection_ready"], false);
                let project = std::path::PathBuf::from(
                    state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .project_path
                        .clone(),
                );
                // The replacement is a real physical sentinel, not a certified Native source.
                // No attach or positive availability is requested from it.
                let replacement =
                    HeldAttachRunner::new(&id, &project, owner.generation, Some("test")).await;
                assert_ne!(replacement.identity, owner);
                assert_eq!(replacement.identity.generation, owner.generation);
                assert!(worker_registry::is_record_live(&original_record));
                assert!(worker_registry::is_record_live(&replacement.record));
                release.send(()).unwrap();
                blocker.await.unwrap();
                let invalidated = receive_kind(&mut socket, "reduced_state").await;
                assert_eq!(invalidated["seq"], frontier);
                assert_eq!(invalidated["state"]["connection_ready"], false);
                assert_eq!(invalidated["state"]["prompt_wake_eligible"], false);
                assert_eq!(state.acp_event_store.highest_seq(&id), frontier);
                assert!(state.acp_supervisor.current_lease(&id) == Some(current.lease.clone()));
                assert!(
                    worker_registry::is_record_live(&original_record),
                    "projection never retires the original actor"
                );
                assert!(worker_registry::is_record_live(&replacement.record));
                state
                    .instances
                    .write()
                    .await
                    .iter_mut()
                    .find(|row| row.id == id)
                    .unwrap()
                    .status = crate::session::Status::Running;
                assert_eq!(
                    crate::server::acp_events::folded_sidebar_status(&state.session_service, &id)
                        .await,
                    None,
                    "an older source outcome cannot qualify through a different physical actor"
                );
                crate::server::acp_events::seed_acp_statuses(state.clone()).await;
                let marked = crate::server::acp_events::recover_structured_unread_after_lag(
                    &state.instances,
                    &state.session_service,
                    &state.instance_locks,
                    state.file_watch.clone(),
                    &state.status_tx,
                )
                .await;
                assert_eq!(marked, 0);
                let row = state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .clone();
                assert_eq!(row.status, crate::session::Status::Running);
                assert!(!row.unread);
                drop(socket);
                server.abort();
                let _ = server.await;
                drop(replacement);
                worker_registry::save(&original_record).unwrap();
                drop(drain_pause);
            })
        })
        .await;
    }

    async fn request_native_attention(
        live: &mut crate::server::test_support::ManagedControlSession,
        question: bool,
    ) {
        live.native
            .send(serde_json::json!({"jsonrpc":"2.0","id":"ws-permission",
            "method":"session/request_permission","params":{"sessionId":live.sid,
                "toolCall":{"toolCallId":"ws-tool","title":"Wait for user"},
                "options":[{"optionId":"allow","name":"Allow","kind":"allow_once"}]}}))
            .await;
        if question {
            live.native
                .send(serde_json::json!({"jsonrpc":"2.0","id":"ws-question",
                "method":"elicitation/create","params":{"sessionId":live.sid,"mode":"form",
                    "message":"Pick","requestedSchema":{"type":"object","properties":{}}}}))
                .await;
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut permission_seen = false;
            let mut question_seen = !question;
            while !permission_seen || !question_seen {
                match live.client.next_event().await.unwrap() {
                    Event::ApprovalRequested { .. } => permission_seen = true,
                    Event::ElicitationRequested { .. } => question_seen = true,
                    _ => {}
                }
            }
        })
        .await
        .expect("actual Native callbacks reach the durable event lane");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn current_loss_refreshes_the_same_frontier_and_retires_impossible_controls() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let mut live =
                    crate::server::test_support::managed_control_session(scope, "ws-loss").await;
                let state = live.state.clone();
                let id = live.id().to_owned();
                let request = live.start_prompt("original").await;
                request_native_attention(&mut live, true).await;
                live.finish_prompt(&request, Some("request_failed")).await;
                let frontier = state.acp_event_store.highest_seq(&id);
                let (mut socket, server) = connect_test_socket(state.clone(), &id).await;
                let initial = receive_kind(&mut socket, "reduced_state").await;
                assert_eq!(initial["seq"], frontier);
                assert_eq!(
                    initial["state"]["connection_ready"], true,
                    "actual Available survives an ordinary original request error"
                );
                assert_eq!(
                    initial["state"]["pending_approvals"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                assert_eq!(
                    initial["state"]["pending_elicitations"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                receive_kind(&mut socket, "transcript_snapshot").await;
                let drain_pause = live.client.pause_drain().await;
                live.native.close_stdout().await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    while live.client.connection_unavailable().is_none() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("real Native EOF invalidates the installed connection");
                state.runtime.publish(&state).await.unwrap();
                let lost = receive_kind(&mut socket, "reduced_state").await;
                assert_eq!(
                    lost["seq"], frontier,
                    "qualification never fabricates a durable event"
                );
                assert_eq!(lost["state"]["connection_ready"], false);
                assert_eq!(
                    lost["state"]["connection_unavailable"]["cause"],
                    "stdout_eof"
                );
                assert_eq!(lost["state"]["pending_approvals"], serde_json::json!([]));
                assert_eq!(lost["state"]["pending_elicitations"], serde_json::json!([]));
                assert_eq!(lost["state"]["turn_active"], false);
                assert_eq!(
                    state.acp_event_store.highest_seq(&id),
                    frontier,
                    "retiring controls synthesizes no human decisions"
                );
                drop(socket);
                server.abort();
                let _ = server.await;
                drop(drain_pause);
            })
        })
        .await;
    }

    #[tokio::test]
    async fn actual_native_ready_survives_fence_contention_without_another_event_or_rpc() {
        use crate::acp::establishment::{AcpEstablishment, EstablishmentBlockReason};
        use crate::acp::supervisor::test_support::managed::with_managed;
        use futures_util::FutureExt as _;
        with_managed(|managed| {
            Box::pin(async move {
                for uncertain_selection in [false, true] {
                    let live = crate::server::test_support::managed_control_session(
                        managed,
                        if uncertain_selection {
                            "websocket-uncertain-selection"
                        } else {
                            "websocket-final-ready"
                        },
                    )
                    .await;
                    let id = live.id().to_owned();
                    let state = live.state.clone();
                    let current = state
                        .acp_supervisor
                        .current_native_availability(&id)
                        .await
                        .unwrap();
                    assert_eq!(current.owner, live.client.lease.spawned_runner());
                    assert!(current.owner.unwrap().birth_fingerprint.is_some());
                    let applied = live
                        .client
                        .actual()
                        .establishment_projection()
                        .and_then(|projection| projection.applied())
                        .expect("actual Native result has a certified Applied postimage");
                    assert_eq!(current.scope.context, applied.context);
                    assert_eq!(current.scope.choice.as_deref(), Some(&applied.choice));
                    let disk = crate::session::Storage::open_reference("test").unwrap();
                    let preimage = disk
                        .load_instances_complete()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap();
                    assert!(applied.matches(&preimage, &disk).unwrap());
                    let preimage = serde_json::to_value(preimage).unwrap();
                    let frontier = state.acp_event_store.highest_seq(&id);
                    let requests = live.native.requests();
                    let mut events = state.acp_events_tx.subscribe();
                    let scope = current.scope.clone();
                    let held_id = id.clone();
                    let (entered_tx, entered) = tokio::sync::oneshot::channel();
                    let (release, released) = std::sync::mpsc::channel();
                    let mut release = Some(release);
                    let mut blocker = Some(tokio::task::spawn_blocking(move || {
                        let _held = scope.fence.lock_observation().unwrap();
                        entered_tx.send(()).unwrap();
                        released
                            .recv_timeout(std::time::Duration::from_secs(10))
                            .unwrap();
                        if uncertain_selection {
                            let disk = crate::session::Storage::open_reference("test").unwrap();
                            disk.update(|rows, _| {
                                let row = rows.iter_mut().find(|row| row.id == held_id).unwrap();
                                let selected = "different-uncertain-native".to_owned();
                                row.acp_session_id = Some(selected.clone());
                                row.resume_intent = crate::session::ResumeIntent::Use(selected);
                                let attempt = row.arm_acp_establishment(&disk, None)?;
                                row.acp_establishment = Some(AcpEstablishment::Blocked {
                                    attempt: Box::new(attempt),
                                    reason: EstablishmentBlockReason::OutcomeUnknown,
                                });
                                Ok(())
                            })
                            .unwrap();
                        }
                    }));
                    let mut servers = Vec::new();
                    let checked = std::panic::AssertUnwindSafe(async {
                        entered.await.unwrap();
                        let (mut socket, server) = connect_test_socket(state.clone(), &id).await;
                        servers.push(server);
                        let fenced = receive_kind(&mut socket, "reduced_state").await;
                        assert_eq!(fenced["seq"], frontier);
                        assert_eq!(fenced["state"]["connection_ready"], false);
                        assert!(fenced["state"]["connection_unavailable"].is_null());
                        assert_eq!(fenced["state"]["prompt_wake_eligible"], false);
                        let captured = state
                            .acp_supervisor
                            .current_native_availability(&id)
                            .await
                            .expect("physical contention must retain the actual actor");
                        assert!(current.same_actor(&captured));
                        assert!(state.instances.try_write().is_ok());
                        assert!(state.publication.try_write().is_ok());
                        assert!(current.lifecycle.try_lock().is_ok());
                        release.take().unwrap().send(()).unwrap();
                        blocker.take().unwrap().await.unwrap();
                        let settled = receive_kind(&mut socket, "reduced_state").await;
                        assert_eq!(settled["seq"], frontier);
                        assert_eq!(
                            settled["state"]["connection_ready"], !uncertain_selection,
                            "only the frozen actual selection may regain Ready"
                        );
                        assert!(settled["state"]["connection_unavailable"].is_null());
                        assert_eq!(state.acp_event_store.highest_seq(&id), frontier);
                        assert_eq!(live.native.requests(), requests);
                        assert!(matches!(
                            events.try_recv(),
                            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                        ));
                        if uncertain_selection {
                            let (mut fresh, fresh_server) =
                                connect_test_socket(state.clone(), &id).await;
                            servers.push(fresh_server);
                            let snapshot = receive_kind(&mut fresh, "reduced_state").await;
                            assert_eq!(snapshot["seq"], frontier);
                            assert_eq!(snapshot["state"]["connection_ready"], false);
                            assert!(live.client.actual().connection_ready());
                            assert_eq!(live.native.requests(), requests);
                            drop(fresh);
                        }
                        drop(socket);
                    })
                    .catch_unwind()
                    .await;
                    // Settle the fault producer before restoring its certified preimage,
                    // including when a contention assertion failed before releasing it.
                    if let Some(release) = release.take() {
                        let _ = release.send(());
                    }
                    if let Some(blocker) = blocker.take() {
                        let _ = blocker.await;
                    }
                    for server in servers {
                        server.abort();
                        let _ = server.await;
                    }
                    if uncertain_selection {
                        // The deliberately inconsistent authority cannot pass Storage's
                        // validated read. Repair only the three fields this case changed.
                        {
                            let _transition = disk.acquire_write_transition().unwrap();
                            let _storage = crate::session::acquire_storage_flock(
                                disk.sessions_path().parent().unwrap(),
                                crate::session::STORAGE_LOCK_FILENAME,
                            )
                            .unwrap();
                            let mut rows: serde_json::Value = serde_json::from_slice(
                                &std::fs::read(disk.sessions_path()).unwrap(),
                            )
                            .unwrap();
                            let row = rows
                                .as_array_mut()
                                .unwrap()
                                .iter_mut()
                                .find(|row| row["id"].as_str() == Some(id.as_str()))
                                .unwrap()
                                .as_object_mut()
                                .unwrap();
                            for field in ["acp_session_id", "resume_intent", "acp_establishment"] {
                                match preimage.get(field) {
                                    Some(value) => {
                                        row.insert(field.to_owned(), value.clone());
                                    }
                                    None => {
                                        row.remove(field);
                                    }
                                }
                            }
                            crate::session::atomic_write(
                                disk.sessions_path(),
                                &serde_json::to_vec_pretty(&rows).unwrap(),
                            )
                            .unwrap();
                        }
                        // Re-read repaired disk through the real canonical health commit.
                        crate::server::test_support::accept_runtime_read_cache_for_test(&state)
                            .await;
                    }
                    if let Err(panic) = checked {
                        std::panic::resume_unwind(panic);
                    }
                }
            })
        })
        .await;
    }

    #[tokio::test]
    async fn native_loss_refuses_input_without_erasing_retained_control_history() {
        use crate::acp::event_store::ControlKnowledge::Known;
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let mut live = crate::server::test_support::managed_control_session(
                    scope,
                    "websocket-native-loss",
                )
                .await;
                let id = live.id().to_owned();
                let _request = live.start_prompt("original retained work").await;
                let before = live.state.acp_event_store.highest_seq(&id);
                let budget = live
                    .state
                    .acp_event_store
                    .rate_limit_redelivery_streak(&id)
                    .unwrap();
                assert_eq!(
                    live.state.acp_event_store.has_in_flight_turn(&id).unwrap(),
                    Known(true)
                );
                let requests = live.native.requests();
                live.client.actual().test_observe_native_loss();
                // Admission uses the same real client as websocket projection; no fake Ready or row is installed.
                let response = crate::server::api::acp_prompt(
                    axum::extract::State(live.state.clone()),
                    axum::extract::Path(id.clone()),
                    Ok(axum::Json(
                        serde_json::from_value(serde_json::json!({
                            "text":"must not be delivered","prompt_id":"blocked",
                        }))
                        .unwrap(),
                    )),
                )
                .await
                .into_response();
                assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
                assert_eq!(live.state.acp_event_store.highest_seq(&id), before);
                assert_eq!(
                    live.state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap(),
                    budget
                );
                assert_eq!(
                    live.state.acp_event_store.has_in_flight_turn(&id).unwrap(),
                    Known(true)
                );
                assert!(live
                    .state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .queued_prompts
                    .is_empty());
                assert_eq!(live.native.requests(), requests);
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn control_fold_skips_events_the_drain_already_applied() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let mut live =
                    crate::server::test_support::managed_control_session(scope, "ws-overlap").await;
                let state = live.state.clone();
                let id = live.id().to_owned();
                let mut publications = live.client.subscribe_publications();
                live.start_prompt("ask permission").await;
                request_native_attention(&mut live, false).await;
                let frame = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let frame = publications.recv().await.unwrap();
                        if frame.session_id == id
                            && matches!(frame.event.as_ref(), Event::ApprovalRequested { .. })
                        {
                            return frame;
                        }
                    }
                })
                .await
                .expect("capture the genuine published callback before replay overlap");
                let frontier = state.acp_event_store.highest_seq(&id);
                let (mut socket, server) = connect_test_socket(state.clone(), &id).await;
                let initial = receive_kind(&mut socket, "reduced_state").await;
                assert_eq!(
                    initial["state"]["pending_approvals"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1
                );
                receive_kind(&mut socket, "transcript_snapshot").await;
                publish(&state, frame);
                let repeated = receive_kind(&mut socket, "reduced_state").await;
                assert_eq!(repeated["seq"], frontier);
                assert_eq!(
                    repeated["state"]["pending_approvals"]
                        .as_array()
                        .unwrap()
                        .len(),
                    1,
                    "drain/replay overlap cannot duplicate the actual Native request"
                );
                drop(socket);
                server.abort();
                let _ = server.await;
            })
        })
        .await;
    }

    #[tokio::test]
    async fn lagged_broadcast_reports_gap_and_rebuilds_control_state() {
        let _home = crate::session::test_support::isolate_app_dir();
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let plan = |version, status| Event::PlanUpdated {
            plan: crate::acp::state::Plan {
                plan_id: "lagged-plan".into(),
                version,
                steps: vec![crate::acp::state::PlanStep {
                    id: "step".into(),
                    title: "work".into(),
                    detail: None,
                    status,
                }],
            },
        };
        state
            .acp_event_store
            .record(
                "s-1",
                1,
                &plan(1, crate::acp::state::PlanStepStatus::Pending),
                &crate::acp::event_sender::EventMetadata::history(),
            )
            .unwrap();
        let (mut socket, server) = connect_test_socket(state.clone(), "s-1").await;
        let initial = receive_kind(&mut socket, "reduced_state").await;
        assert_eq!(
            initial["state"]["current_plan"]["steps"][0]["status"],
            "Pending"
        );
        receive_kind(&mut socket, "transcript_snapshot").await;
        // No await in this burst: the current-thread receiver cannot drain its eight slots.
        for seq in 2..=17 {
            let event = if seq == 2 {
                plan(2, crate::acp::state::PlanStepStatus::Done)
            } else {
                Event::ThinkingEnded
            };
            state
                .acp_event_store
                .record(
                    "s-1",
                    seq,
                    &event,
                    &crate::acp::event_sender::EventMetadata::history(),
                )
                .unwrap();
            publish(
                &state,
                AcpBroadcastFrame::from_qualified(
                    "s-1".into(),
                    seq,
                    Arc::new(event),
                    &crate::acp::event_sender::EventMetadata::history(),
                    None,
                    None,
                    None,
                ),
            );
        }
        let gap = receive_kind(&mut socket, "lagged").await;
        assert_eq!(gap["skipped"], 8);
        let rebuilt = receive_kind(&mut socket, "reduced_state").await;
        assert_eq!(rebuilt["seq"], 17);
        assert_eq!(
            rebuilt["state"]["current_plan"]["steps"][0]["status"], "Done",
            "the missed plan update must be recovered from durable history"
        );
        state.shutdown.cancel();
        drop(socket);
        server.abort();
        let _ = server.await;
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
}
