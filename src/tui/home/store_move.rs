//! Presentation of daemon-owned sandbox preparation.

use super::*;

impl HomeView {
    pub(crate) fn poll_store_move(&mut self) -> bool {
        self.session_feed.poll_sandbox_progress()
    }

    pub(crate) fn store_move_status_line(&self) -> Option<String> {
        self.session_feed.sandbox_status_line()
    }
}
