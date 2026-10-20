//! `GET /api/sessions/status/ws`: streams live status moves to the dashboard, plus a `sound_due`
//! cue once a Waiting, Idle or Error status has held for its dwell. The server times the dwell
//! because a hidden tab's timers are throttled but its WebSocket messages are not.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use super::push::{NotificationEvent, StatusChange};
use super::AppState;

const PING_INTERVAL: Duration = Duration::from_secs(25);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const PONG_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

pub async fn session_status_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> Response {
    if let Some(response) = super::api::cityhall_block(&state) {
        return response;
    }
    ws.protocols(["aoe-auth"])
        .on_upgrade(move |socket| serve(socket, state))
        .into_response()
}

/// Pending dwell per session; a later move replaces the entry, so a flap never cues.
#[derive(Default)]
struct Dwells(HashMap<String, (Instant, NotificationEvent)>);

impl Dwells {
    fn on_change(&mut self, change: &StatusChange, now: Instant) {
        match NotificationEvent::for_status(change.new) {
            Some(event) => {
                let due = now + Duration::from_millis(event.dwell_ms());
                self.0.insert(change.instance_id.clone(), (due, event));
            }
            None => {
                self.0.remove(&change.instance_id);
            }
        }
    }

    fn next_due(&self) -> Option<Instant> {
        self.0.values().map(|(due, _)| *due).min()
    }

    fn take_due(&mut self, now: Instant) -> Vec<(String, NotificationEvent)> {
        let ids: Vec<String> = self
            .0
            .iter()
            .filter(|(_, (due, _))| *due <= now)
            .map(|(id, _)| id.clone())
            .collect();
        ids.into_iter()
            .filter_map(|id| self.0.remove(&id).map(|(_, event)| (id, event)))
            .collect()
    }
}

fn status_frame(change: &StatusChange) -> String {
    json!({ "kind": "status", "id": change.instance_id, "status": change.new.wire_str() })
        .to_string()
}

fn sound_due_frame(id: &str, event: NotificationEvent) -> String {
    json!({ "kind": "sound_due", "id": id, "event": event.as_str() }).to_string()
}

async fn send(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        tokio::time::timeout(WRITE_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

/// The next frames to send, or `None` once the status channel closes. Queued moves win over an
/// expired dwell, so a cue can never beat the move that cancels it.
async fn next_frames(
    rx: &mut broadcast::Receiver<StatusChange>,
    dwells: &mut Dwells,
) -> Option<Vec<String>> {
    let sleep = match dwells.next_due() {
        Some(due) => tokio::time::sleep_until(due),
        None => tokio::time::sleep(Duration::from_secs(3600)),
    };
    tokio::select! {
        biased;
        recv = rx.recv() => match recv {
            Ok(change) => {
                dwells.on_change(&change, Instant::now());
                Some(vec![status_frame(&change)])
            }
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(target: "http.middleware", skipped, "session status ws lagged");
                *dwells = Dwells::default();
                Some(vec![json!({ "kind": "resync" }).to_string()])
            }
            Err(RecvError::Closed) => None,
        },
        _ = sleep => Some(
            dwells
                .take_due(Instant::now())
                .into_iter()
                .map(|(id, event)| sound_due_frame(&id, event))
                .collect(),
        ),
    }
}

async fn serve(mut socket: WebSocket, state: Arc<AppState>) {
    // Subscribe before `ready` so the client's resync fetch cannot miss a move.
    let mut rx = state.status_tx.subscribe();
    if !send(
        &mut socket,
        Message::Text(json!({ "kind": "ready" }).to_string().into()),
    )
    .await
    {
        return;
    }

    let mut dwells = Dwells::default();
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.tick().await;
    let mut last_pong_at = Instant::now();

    loop {
        let frame = tokio::select! {
            _ = state.shutdown.cancelled() => break,
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(Message::Pong(_))) => {
                    last_pong_at = Instant::now();
                    continue;
                }
                Some(Ok(_)) => continue,
            },
            _ = ping.tick() => {
                if last_pong_at.elapsed() > PONG_IDLE_TIMEOUT {
                    tracing::debug!(target: "http.middleware", "session status ws idle reaper fired");
                    break;
                }
                if !send(&mut socket, Message::Ping(Vec::new().into())).await {
                    break;
                }
                continue;
            }
            frames = next_frames(&mut rx, &mut dwells) => match frames {
                Some(frames) => frames,
                None => break,
            },
        };
        for text in frame {
            if !send(&mut socket, Message::Text(text.into())).await {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Status;
    use chrono::Utc;

    fn change(id: &str, new: Status) -> StatusChange {
        StatusChange {
            instance_id: id.to_string(),
            instance_title: id.to_string(),
            effective_profile: "default".to_string(),
            old: Status::Running,
            new,
            at: Utc::now(),
        }
    }

    #[test]
    fn dwell_cues_after_the_status_holds_and_a_later_move_cancels_it() {
        let t0 = Instant::now();
        let mut dwells = Dwells::default();
        dwells.on_change(&change("a", Status::Waiting), t0);
        dwells.on_change(&change("b", Status::Idle), t0);
        dwells.on_change(&change("c", Status::Running), t0);
        assert_eq!(dwells.next_due(), Some(t0 + Duration::from_millis(2_000)));

        let mut due = dwells.take_due(t0 + Duration::from_millis(2_000));
        due.sort_by(|x, y| x.0.cmp(&y.0));
        assert_eq!(due, vec![("b".to_string(), NotificationEvent::Idle)]);

        dwells.on_change(
            &change("a", Status::Running),
            t0 + Duration::from_millis(3_000),
        );
        assert!(dwells.take_due(t0 + Duration::from_secs(60)).is_empty());
        assert_eq!(dwells.next_due(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_move_cancels_the_dwell_before_its_expired_cue() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut dwells = Dwells::default();
        dwells.on_change(&change("a", Status::Waiting), Instant::now());
        let running = change("a", Status::Running);
        tx.send(running.clone()).unwrap();
        tokio::time::advance(Duration::from_secs(6)).await;

        let frames = next_frames(&mut rx, &mut dwells).await;
        assert_eq!(frames, Some(vec![status_frame(&running)]));
        let later =
            tokio::time::timeout(Duration::from_secs(30), next_frames(&mut rx, &mut dwells)).await;
        assert!(later.is_err(), "no cue for a session that moved on");
    }

    #[test]
    fn frames_use_the_wire_spelling_the_session_list_uses() {
        let status: serde_json::Value =
            serde_json::from_str(&status_frame(&change("a", Status::Waiting))).unwrap();
        assert_eq!(
            status,
            json!({ "kind": "status", "id": "a", "status": "Waiting" })
        );
        let cue: serde_json::Value =
            serde_json::from_str(&sound_due_frame("a", NotificationEvent::Error)).unwrap();
        assert_eq!(
            cue,
            json!({ "kind": "sound_due", "id": "a", "event": "error" })
        );
    }
}
