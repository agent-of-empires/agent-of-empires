//! Background stop handler for TUI responsiveness.
//!
//! `docker stop` blocks for the container's grace period (~10s), which froze
//! the UI event loop (issue #1496), so stops run on a worker thread and the
//! main loop drains results each frame.

use std::sync::mpsc::TryRecvError;

use crate::session::stop::perform_stop;
pub use crate::session::stop::{StopRequest, StopResult};
use crate::tui::worker::{SessionScoped, TrackedWorker};

impl SessionScoped for StopResult {
    fn session_id(&self) -> &str {
        &self.session_id
    }
}

pub struct StopPoller {
    worker: TrackedWorker<StopRequest, StopResult>,
}

impl StopPoller {
    pub fn new() -> Self {
        Self {
            worker: TrackedWorker::spawn("aoe-stop-poller", |request| perform_stop(&request)),
        }
    }

    pub fn request_stop(&mut self, request: StopRequest) {
        self.worker.request(request.session_id.clone(), request);
    }

    pub fn try_recv_result(&mut self) -> Result<StopResult, TryRecvError> {
        self.worker.try_recv()
    }

    /// Sessions whose stop never landed, for recovering rows left optimistically
    /// `Stopped` once the worker is known dead.
    pub fn take_pending(&mut self) -> Vec<String> {
        self.worker.take_pending()
    }
}

impl Default for StopPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Instance;
    use std::time::Duration;

    struct TestPoller(Option<StopPoller>);

    impl Drop for TestPoller {
        fn drop(&mut self) {
            if let Some(poller) = self.0.take() {
                let _ = poller.worker.finish_for_test();
            }
        }
    }

    /// A stored, isolated session plus a live poller to stop it with.
    fn fixture(profile: &str) -> (crate::session::Storage, TestPoller, Instance) {
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        let mut instance = Instance::new("Test Session", "/tmp/test-project");
        instance.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        (storage, TestPoller(Some(StopPoller::new())), instance)
    }

    fn await_result(poller: &mut StopPoller) -> StopResult {
        for _ in 0..50 {
            match poller.try_recv_result() {
                Ok(result) => return result,
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => panic!("stop worker disconnected: {error}"),
            }
        }
        panic!("timed out waiting for stop result");
    }

    #[test]
    fn try_recv_reports_empty_while_idle() {
        let mut poller = StopPoller::new();
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
    }

    /// A request is in flight until its result lands, and stopping writes the
    /// durable `Stopped` status.
    #[test]
    #[serial_test::serial]
    fn stop_tracks_its_request_and_persists_the_status() {
        if !crate::tui::isolated_test_process(
            "tui::stop_poller::tests::stop_tracks_its_request_and_persists_the_status",
            Duration::from_secs(5),
        ) {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        let (storage, mut held, instance) = fixture("default");
        let poller = held.0.as_mut().unwrap();
        let session_id = instance.id.clone();

        poller.request_stop(StopRequest {
            session_id: session_id.clone(),
            instance,
        });
        assert_eq!(poller.take_pending(), vec![session_id.clone()]);
        assert!(poller.take_pending().is_empty(), "take_pending drains");

        let result = await_result(poller);
        assert_eq!(result.session_id, session_id);
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            storage.load().unwrap()[0].status,
            crate::session::Status::Stopped
        );
    }
}
