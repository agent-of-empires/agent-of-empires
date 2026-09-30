//! Background loader for the selected session's usage summary, so SQLite
//! reads stay off the render loop.

use crate::tui::worker::Worker;
use crate::usage::UsageSummary;

pub struct UsagePoller {
    worker: Worker<String, (String, Option<UsageSummary>)>,
}

impl UsagePoller {
    pub fn new() -> Self {
        Self {
            worker: Worker::spawn("aoe-usage-poller", |instance_id: String| {
                let summary = crate::usage::UsageStore::open_default()
                    .and_then(|store| store.summary_for(&instance_id))
                    .ok();
                (instance_id, summary)
            }),
        }
    }

    pub fn request_refresh(&self, instance_id: String) {
        self.worker.request(instance_id);
    }

    pub fn try_recv_updates(
        &self,
    ) -> Result<(String, Option<UsageSummary>), std::sync::mpsc::TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for UsagePoller {
    fn default() -> Self {
        Self::new()
    }
}
