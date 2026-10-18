//! `GET /api/sessions/status/ws`: streams live status moves to the dashboard, plus a `sound_due`
//! cue once a Waiting, Idle or Error status has held for its dwell. The server times the dwell
//! because a hidden tab's timers are throttled but its WebSocket messages are not.
//!
//! A `status` frame goes out only after a snapshot publication that began after the move, and
//! carries that snapshot's cursor, so a `GET /api/sessions` after the frame is never staler than it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tokio::sync::broadcast::{self, error::RecvError};
use tokio::time::Instant;

use crate::session::Status;

use crate::daemon::RuntimeCursor;

use super::push::{NotificationEvent, StatusChange};
use super::runtime::PublishedSnapshot;
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
    fn track(&mut self, id: &str, status: Status, now: Instant) {
        match NotificationEvent::for_status(status) {
            Some(event) => {
                let due = now + Duration::from_millis(event.dwell_ms());
                self.0.insert(id.to_string(), (due, event));
            }
            None => {
                self.0.remove(id);
            }
        }
    }

    fn on_change(&mut self, change: &StatusChange, now: Instant) {
        self.track(&change.instance_id, change.new, now);
    }

    /// A dwell for every session already in a cueing status, so a new connection still cues a
    /// status that started before it; the client drops a cue it already sounded.
    async fn seeded(state: &AppState) -> Self {
        let now = Instant::now();
        let mut dwells = Self::default();
        for inst in state.instances.read().await.iter() {
            dwells.track(&inst.id, inst.status, now);
        }
        dwells
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

fn status_frame(id: &str, status: &str, cursor: &RuntimeCursor) -> String {
    json!({
        "kind": "status",
        "id": id,
        "status": status,
        "epoch": cursor.epoch,
        "revision": cursor.revision.to_string(),
    })
    .to_string()
}

/// The frame states the row as the snapshot it cites has it, so queued moves that share a revision
/// agree and the last one sent is the current status. A row absent from the snapshot gets no frame.
fn frame_for(change: &StatusChange, published: &PublishedSnapshot) -> Option<String> {
    let row = published
        .value
        .contents
        .sessions
        .iter()
        .find(|row| row.id == change.instance_id)?;
    Some(status_frame(&row.id, &row.status, &published.value.cursor))
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

#[derive(Debug, PartialEq)]
enum Next {
    Frames(Vec<String>),
    /// A move to publish before it is sent; awaited outside this cancellable future so it is not lost.
    Change(StatusChange),
    /// Moves were dropped; the caller reseeds and resyncs outside this cancellable future.
    Lagged,
}

/// The next step, or `None` once the status channel closes. Queued moves win over an expired
/// dwell, so a cue can never beat the move that cancels it.
async fn next_frames(
    rx: &mut broadcast::Receiver<StatusChange>,
    dwells: &mut Dwells,
) -> Option<Next> {
    let sleep = match dwells.next_due() {
        Some(due) => tokio::time::sleep_until(due),
        None => tokio::time::sleep(Duration::from_secs(3600)),
    };
    tokio::select! {
        biased;
        recv = rx.recv() => match recv {
            Ok(change) => {
                dwells.on_change(&change, Instant::now());
                Some(Next::Change(change))
            }
            Err(RecvError::Lagged(skipped)) => {
                tracing::warn!(target: "http.middleware", skipped, "session status ws lagged");
                Some(Next::Lagged)
            }
            Err(RecvError::Closed) => None,
        },
        _ = sleep => Some(Next::Frames(
            dwells
                .take_due(Instant::now())
                .into_iter()
                .map(|(id, event)| sound_due_frame(&id, event))
                .collect(),
        )),
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

    let mut dwells = Dwells::seeded(&state).await;
    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ping.tick().await;
    let mut last_pong_at = Instant::now();

    loop {
        let next = tokio::select! {
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
            next = next_frames(&mut rx, &mut dwells) => match next {
                Some(next) => next,
                None => break,
            },
        };
        let frames = match next {
            Next::Frames(frames) => frames,
            Next::Change(change) => match state.runtime.publish(&state).await {
                Ok(published) => frame_for(&change, &published).into_iter().collect(),
                Err(error) => {
                    tracing::error!(target: "server.runtime", %error, "status move not published");
                    vec![json!({ "kind": "resync" }).to_string()]
                }
            },
            Next::Lagged => {
                dwells = Dwells::seeded(&state).await;
                vec![json!({ "kind": "resync" }).to_string()]
            }
        };
        for text in frames {
            if !send(&mut socket, Message::Text(text.into())).await {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn cursor(revision: u64) -> RuntimeCursor {
        RuntimeCursor {
            epoch: "boot".to_string(),
            revision,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_queued_move_cancels_the_dwell_before_its_expired_cue() {
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let tx = state.status_tx.clone();
        let mut rx = tx.subscribe();
        let mut dwells = Dwells::default();
        dwells.on_change(&change("a", Status::Waiting), Instant::now());
        let running = change("a", Status::Running);
        tx.send(running.clone()).unwrap();
        tokio::time::advance(Duration::from_secs(6)).await;

        let frames = next_frames(&mut rx, &mut dwells).await;
        assert_eq!(frames, Some(Next::Change(running)));
        let later =
            tokio::time::timeout(Duration::from_secs(30), next_frames(&mut rx, &mut dwells)).await;
        assert!(later.is_err(), "no cue for a session that moved on");
    }

    #[tokio::test(start_paused = true)]
    async fn a_new_connection_cues_a_status_that_started_before_it() {
        let mut waiting = crate::session::Instance::new("w", "/tmp/aoe-ws-w");
        waiting.status = Status::Waiting;
        let mut running = crate::session::Instance::new("r", "/tmp/aoe-ws-r");
        running.status = Status::Running;
        let waiting_id = waiting.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![waiting, running]);
        let mut rx = state.status_tx.subscribe();
        let mut dwells = Dwells::seeded(&state).await;

        let frames = next_frames(&mut rx, &mut dwells).await;
        assert_eq!(
            frames,
            Some(Next::Frames(vec![sound_due_frame(
                &waiting_id,
                NotificationEvent::Waiting
            )]))
        );
    }

    #[tokio::test]
    async fn a_move_is_published_before_its_frame_so_a_later_read_is_never_staler() {
        let mut row = crate::session::Instance::new("m", "/tmp/aoe-ws-m");
        row.status = Status::Running;
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        let before = state.runtime.snapshot(&state).await.unwrap();
        state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|inst| inst.id == id)
            .unwrap()
            .status = Status::Waiting;

        let published = state.runtime.publish(&state).await.unwrap();
        assert!(published.value.cursor.revision > before.value.cursor.revision);
        let read = state.runtime.snapshot(&state).await.unwrap();
        assert_eq!(read.value.cursor, published.value.cursor);
        assert_eq!(
            read.value
                .contents
                .sessions
                .iter()
                .find(|s| s.id == id)
                .unwrap()
                .status,
            "Waiting"
        );
    }

    /// The session list carries paths under the app dir, so both publishes must read the same
    /// one: a test that isolates the app dir meanwhile would otherwise change the second.
    #[tokio::test]
    #[serial_test::serial]
    async fn queued_moves_sharing_a_snapshot_all_report_the_status_that_snapshot_has() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let mut row = crate::session::Instance::new("q", "/tmp/aoe-ws-q");
        row.status = Status::Running;
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        // Waiting then Idle both commit before either broadcast is drained.
        state
            .instances
            .write()
            .await
            .iter_mut()
            .find(|inst| inst.id == id)
            .unwrap()
            .status = Status::Idle;
        let waiting = change(&id, Status::Waiting);
        let idle = change(&id, Status::Idle);

        let first = state.runtime.publish(&state).await.unwrap();
        let second = state.runtime.publish(&state).await.unwrap();
        assert_eq!(first.value.cursor, second.value.cursor);
        let frames: Vec<serde_json::Value> = [(&waiting, &first), (&idle, &second)]
            .into_iter()
            .map(|(c, p)| serde_json::from_str(&frame_for(c, p).unwrap()).unwrap())
            .collect();
        assert_eq!(frames[0], frames[1]);
        assert_eq!(frames[1]["status"], "Idle");
    }

    #[tokio::test]
    async fn a_move_for_a_row_missing_from_the_snapshot_sends_no_frame() {
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        let published = state.runtime.publish(&state).await.unwrap();
        assert_eq!(
            frame_for(&change("gone", Status::Waiting), &published),
            None
        );
    }

    #[tokio::test]
    async fn a_lagged_receiver_reports_the_lag_for_the_caller_to_resync() {
        let (tx, mut rx) = broadcast::channel(2);
        for _ in 0..5 {
            tx.send(change("a", Status::Waiting)).unwrap();
        }
        let mut dwells = Dwells::default();
        assert_eq!(next_frames(&mut rx, &mut dwells).await, Some(Next::Lagged));
    }

    #[test]
    fn frames_use_the_wire_spelling_the_session_list_uses() {
        let status: serde_json::Value =
            serde_json::from_str(&status_frame("a", "Waiting", &cursor(7))).unwrap();
        assert_eq!(
            status,
            json!({
                "kind": "status", "id": "a", "status": "Waiting", "epoch": "boot", "revision": "7"
            })
        );
        let cue: serde_json::Value =
            serde_json::from_str(&sound_due_frame("a", NotificationEvent::Error)).unwrap();
        assert_eq!(
            cue,
            json!({ "kind": "sound_due", "id": "a", "event": "error" })
        );
    }
}
