//! Typed runtime completion polling; durable lifecycle work belongs to the daemon.

use super::*;

impl HomeView {
    pub(super) const CANONICAL_RELOAD_RETRY_INTERVAL: std::time::Duration =
        std::time::Duration::from_secs(5);

    pub fn apply_restart_results(&mut self) -> bool {
        let before = self.restart_in_flight.len();
        let drained = self.session_feed.drain_command_errors();
        let presented = self.present_command_errors(drained);
        self.restart_in_flight
            .retain(|id| self.session_feed.has_pending(id));
        before != self.restart_in_flight.len() || presented
    }
}
