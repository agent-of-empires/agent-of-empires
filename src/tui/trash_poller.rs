//! Background trash handler for TUI responsiveness.
//!
//! Trashing stops the session's container (~10s grace period) and relocates its
//! worktree into the holding area, so it runs on a worker thread and the main
//! loop drains results each frame.

use std::sync::mpsc::TryRecvError;

use crate::session::trash::perform_trash;
pub use crate::session::trash::{TrashRequest, TrashResult};
use crate::tui::worker::{SessionScoped, TrackedWorker};

impl SessionScoped for TrashResult {
    fn session_id(&self) -> &str {
        &self.session_id
    }
}

pub struct TrashPoller {
    worker: TrackedWorker<TrashRequest, TrashResult>,
}

impl TrashPoller {
    pub fn new() -> Self {
        Self {
            worker: TrackedWorker::spawn("aoe-trash-poller", |request| perform_trash(&request)),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_handler_for_test(
        handler: impl FnMut(TrashRequest) -> TrashResult + Send + 'static,
    ) -> Self {
        Self {
            worker: TrackedWorker::spawn("aoe-trash-poller-test", handler),
        }
    }

    pub fn request_trash(&mut self, request: TrashRequest) {
        self.worker.request(request.session_id.clone(), request);
    }

    pub fn try_recv_result(&mut self) -> Result<TrashResult, TryRecvError> {
        self.worker.try_recv()
    }

    /// Relocations that never landed, for logging as deferred once the worker is
    /// known dead. The rows stay trashed; a later reconcile pass moves them.
    pub fn take_pending(&mut self) -> Vec<String> {
        self.worker.take_pending()
    }

    #[cfg(test)]
    pub(crate) fn is_pending(&self, id: &str) -> bool {
        self.worker.is_pending(id)
    }
}

impl Default for TrashPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Instance;
    use std::time::Duration;

    struct TestPoller(Option<TrashPoller>);

    impl Drop for TestPoller {
        fn drop(&mut self) {
            if let Some(poller) = self.0.take() {
                let _ = poller.worker.finish_for_test();
            }
        }
    }

    fn create_test_instance() -> (crate::session::test_support::AppDirGuard, Instance, u64) {
        let guard = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut instance = Instance::new("Test Session", "/tmp/test-project");
        instance.source_profile = "default".to_string();
        instance.trash();
        let generation = instance
            .try_acquire_lifecycle_reservation(
                crate::session::LifecycleOperation::Trash,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )
            .unwrap();
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        (guard, instance, generation)
    }

    #[test]
    fn try_recv_reports_empty_while_idle() {
        let mut poller = TrashPoller::new();
        assert!(matches!(poller.try_recv_result(), Err(TryRecvError::Empty)));
    }

    /// A request is in flight until its result lands. A plain (non-worktree,
    /// non-sandbox) session has nothing to relocate.
    #[test]
    #[serial_test::serial]
    fn trash_tracks_its_request_until_the_result_lands() {
        if !crate::tui::isolated_test_process(
            "tui::trash_poller::tests::trash_tracks_its_request_until_the_result_lands",
            Duration::from_secs(5),
        ) {
            return;
        }
        let (_guard, instance, generation) = create_test_instance();
        let mut held = TestPoller(Some(TrashPoller::new()));
        let poller = held.0.as_mut().unwrap();
        let session_id = instance.id.clone();

        poller.request_trash(TrashRequest {
            session_id: session_id.clone(),
            instance,
            generation,
        });
        assert_eq!(poller.take_pending(), vec![session_id.clone()]);
        assert!(poller.take_pending().is_empty(), "take_pending drains");

        let result = loop {
            match poller.try_recv_result() {
                Ok(result) => break result,
                Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
                Err(error) => panic!("trash worker disconnected: {error}"),
            }
        };
        assert_eq!(result.session_id, session_id);
        assert!(result.relocation.is_none());
        assert!(
            result.relocate_warning.is_none(),
            "{:?}",
            result.relocate_warning
        );
    }
}
