//! Background sandbox-store move for TUI responsiveness.
//!
//! A session still on the shared sandbox store copies it before its first
//! container launch. That copy can take minutes, and on the event loop it
//! froze the TUI with nothing on screen. This mirrors `RestartPoller`: the
//! move runs on a worker thread, and its progress events and result come
//! back over channels the main loop drains each frame.

use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;

use crate::migrations::progress::{tracing_reporter, Event, Reporter};
use crate::session::Instance;
use crate::tui::app::Action;
use crate::tui::worker::Worker;

pub struct StoreMoveRequest {
    pub instance: Instance,
    /// What to do once the store has moved: the attach that needed it.
    pub resume: Option<Action>,
}

pub struct StoreMoveResult {
    pub session_id: String,
    /// `Ok(false)` when the container was up, so nothing had to move for the
    /// launch to proceed; see `Instance::move_sandbox_store`.
    pub outcome: Result<bool, String>,
    pub resume: Option<Action>,
}

type Mover = dyn Fn(&Instance, Reporter) -> anyhow::Result<bool> + Send;

pub struct StoreMovePoller {
    worker: Worker<StoreMoveRequest, StoreMoveResult>,
    progress_rx: mpsc::Receiver<Event>,
}

impl StoreMovePoller {
    pub fn new() -> Self {
        Self::spawn(Box::new(|instance, reporter| {
            instance.move_sandbox_store(Some(reporter))
        }))
    }

    fn spawn(mover: Box<Mover>) -> Self {
        let (progress_tx, progress_rx) = mpsc::channel::<Event>();
        let worker = Worker::spawn("aoe-store-move-poller", move |request: StoreMoveRequest| {
            // The status line is transient; the log keeps the trail.
            let log = tracing_reporter();
            let progress_tx = progress_tx.clone();
            let reporter: Reporter = Arc::new(move |event: Event| {
                log(event.clone());
                let _ = progress_tx.send(event);
            });
            let outcome = mover(&request.instance, reporter).map_err(|error| format!("{error:#}"));
            StoreMoveResult {
                session_id: request.instance.id.clone(),
                outcome,
                resume: request.resume,
            }
        });
        Self {
            worker,
            progress_rx,
        }
    }

    pub fn request_move(&self, request: StoreMoveRequest) {
        self.worker.request(request);
    }

    /// The next progress event of the move in flight, if any.
    pub fn try_recv_progress(&self) -> Option<Event> {
        self.progress_rx.try_recv().ok()
    }

    /// Non-blocking poll for a finished move. Surfaces `Disconnected` rather
    /// than collapsing it into `None`, so a panic in the mover clears the
    /// caller's in-flight state instead of leaving the status line stuck.
    pub fn try_recv_result(&self) -> Result<StoreMoveResult, TryRecvError> {
        self.worker.try_recv()
    }

    #[cfg(test)]
    pub(crate) fn with_result_for_test(result: StoreMoveResult) -> Self {
        let (_progress_tx, progress_rx) = mpsc::channel();
        Self {
            worker: Worker::seeded_for_test("aoe-store-move-poller-test", result),
            progress_rx,
        }
    }
}

impl Default for StoreMovePoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// The real launch-time move, with its copy held behind a gate: its
    /// progress must be on the channel before the gate opens, and only once
    /// it does may the result follow.
    #[test]
    #[serial_test::serial]
    fn progress_reaches_the_channel_before_the_copy_completes() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        std::fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        let mut row = serde_json::to_value(Instance::new("legacy", "/tmp")).unwrap();
        row["tool"] = "gemini".into();
        row["sandbox_store_generation"] = 0.into();
        row["sandbox_info"] = serde_json::json!({
            "enabled": true,
            "image": "img",
            "container_name": "aoe-sandbox-legacy",
        });
        std::fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&vec![row.clone()]).unwrap(),
        )
        .unwrap();
        let instance: Instance = serde_json::from_value(row).unwrap();
        let id = instance.id.clone();

        let (paused_tx, paused_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(Some((paused_tx, release_rx)));
        let poller = StoreMovePoller::spawn(Box::new(move |instance, reporter| {
            if let Some((paused_tx, release_rx)) = gate.lock().unwrap().take() {
                crate::migrations::v027_isolate_sandbox_stores::COPY_GATE.with(|hook| {
                    *hook.borrow_mut() = Some(Box::new(move |_: &Path| {
                        paused_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    }));
                });
            }
            crate::migrations::migrate_sandbox_store_for_test(
                &instance.id,
                Some(reporter),
                &|_| Ok(false),
                &|_| Ok(true),
            )
            .map(|()| true)
        }));
        poller.request_move(StoreMoveRequest {
            instance,
            resume: Some(Action::Quit),
        });
        paused_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the copy started");

        let mut events = Vec::new();
        while let Some(event) = poller.try_recv_progress() {
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Started { version: 27, .. })),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Step(step) if step.starts_with("copying agent store 1/1"))),
            "{events:?}"
        );
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = loop {
            match poller.try_recv_result() {
                Ok(result) => break result,
                Err(TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("no result after the copy was released: {error:?}"),
            }
        };
        assert_eq!(result.session_id, id);
        assert_eq!(result.outcome, Ok(true));
        assert_eq!(result.resume, Some(Action::Quit));
        while let Some(event) = poller.try_recv_progress() {
            events.push(event);
        }
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::Finished { version: 27, .. })),
            "{events:?}"
        );
        let rows: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["sandbox_store_generation"], 2);
        assert_eq!(
            std::fs::read(
                home.join(".gemini/sandbox-v2")
                    .join(&id)
                    .join("history/id.json")
            )
            .unwrap(),
            b"legacy"
        );
    }
}
