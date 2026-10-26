//! The session-id capture poller a row holds, with the pacing that decides when to replace it.

use std::sync::{Arc, Mutex};

use crate::session::poller::{PollerRepairBackoff, SessionPoller};

/// Runtime-only: none of it is persisted, and a clone shares the handle.
#[derive(Debug, Clone, Default)]
pub(crate) struct CapturePollerSlot {
    pub(crate) handle: Option<Arc<Mutex<SessionPoller>>>,
    /// Re-probe ladder and thread-budget backoff for replacing a missing poller.
    pub(crate) repair: PollerRepairBackoff,
    /// Managed-store ownership or lease contention backoff.
    pub(crate) retry_after: Option<std::time::Instant>,
}
