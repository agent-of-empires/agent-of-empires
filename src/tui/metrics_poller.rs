//! Background sampler for the diagnostics strip.
//!
//! Reading memory and counting agent process trees forks `ps` / walks `/proc`,
//! so it must not run on the render loop. This mirrors [`StatusPoller`]: a
//! [`Worker`] on a named thread samples on request and the main loop drains the
//! result each frame.

use crate::process::metrics::{MetricsSampler, MetricsSnapshot};
use crate::session::Instance;
use crate::tui::worker::Worker;

/// Background thread that samples system memory + agent counts without blocking
/// the UI.
pub struct MetricsPoller {
    worker: Worker<Vec<Instance>, MetricsSnapshot>,
}

impl MetricsPoller {
    pub fn new() -> Self {
        let mut sampler = MetricsSampler::default();
        Self {
            worker: Worker::spawn("aoe-metrics-poller", move |instances: Vec<Instance>| {
                sampler.sample(&instances)
            }),
        }
    }

    /// Request a sample for the given instances (non-blocking).
    pub fn request_refresh(&self, instances: Vec<Instance>) {
        self.worker.request(instances);
    }

    /// Try to receive a completed sample without blocking. Surfaces
    /// `Disconnected` (see [`Worker::try_recv`]) so the caller can clear its
    /// in-flight guard and respawn rather than freeze the strip forever.
    pub fn try_recv_updates(&self) -> Result<MetricsSnapshot, std::sync::mpsc::TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for MetricsPoller {
    fn default() -> Self {
        Self::new()
    }
}
