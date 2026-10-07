//! Background sampler for the diagnostics strip.
//!
//! Memory and process sampling forks `ps` or walks `/proc`, so it stays off
//! the render loop. Like [`StatusPoller`](super::status_poller::StatusPoller),
//! a [`Worker`] samples on request; the main loop drains results each frame.

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
