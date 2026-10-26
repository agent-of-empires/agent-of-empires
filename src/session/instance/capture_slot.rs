//! The session-id capture poller a row holds, with the pacing that decides when to replace it.
//!
//! A poller serves the tool and execution it was installed for, so every handoff between rows
//! decides here whether the handle and the pacing still apply to the row taking them.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::ActiveExecution;
use crate::session::poller::{PollerRepairBackoff, SessionPoller, MANAGED_CAPTURE_RETRY_BACKOFF};

/// The tool and execution a row runs: what a poller or a repair window is about.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held<'a> {
    pub(crate) tool: &'a str,
    pub(crate) execution: Option<&'a ActiveExecution>,
}

impl<'a> Held<'a> {
    /// Built from the two fields, not the row, so a caller can hold it across a `&mut` borrow of
    /// the slot.
    pub(crate) fn new(tool: &'a str, execution: Option<&'a ActiveExecution>) -> Self {
        Self { tool, execution }
    }
}

/// Runtime-only. A clone shares the handle.
#[derive(Debug, Clone, Default)]
pub(crate) struct CapturePollerSlot {
    handle: Option<Arc<Mutex<SessionPoller>>>,
    /// Re-probe ladder and thread-budget backoff for replacing a missing poller.
    repair: PollerRepairBackoff,
    /// Managed-store ownership or lease contention backoff.
    retry_after: Option<Instant>,
}

impl CapturePollerSlot {
    pub(crate) fn handle(&self) -> Option<&Arc<Mutex<SessionPoller>>> {
        self.handle.as_ref()
    }

    pub(crate) fn is_running(&self) -> bool {
        self.handle.as_ref().is_some_and(|poller| {
            poller
                .lock()
                .map(|guard| guard.is_running())
                .unwrap_or_else(|poisoned| poisoned.into_inner().is_running())
        })
    }

    /// A row with no poller has none to watch, whatever it used to hold.
    pub(crate) fn serves(&self, held: Held<'_>) -> bool {
        self.handle.as_ref().is_some_and(|poller| {
            poller
                .lock()
                .map(|guard| guard.serves(held.tool, held.execution))
                .unwrap_or_else(|poisoned| poisoned.into_inner().serves(held.tool, held.execution))
        })
    }

    /// Stops the thread and keeps the handle, which the final drain still reads.
    pub(crate) fn stop(&self) {
        if let Some(poller) = &self.handle {
            match poller.lock() {
                Ok(mut poller) => poller.stop(),
                Err(poisoned) => poisoned.into_inner().stop(),
            }
        }
    }

    /// Forgets the handle without stopping it: a clone may still own the thread.
    pub(crate) fn drop_handle(&mut self) {
        self.handle = None;
    }

    pub(crate) fn set_handle(&mut self, handle: Option<Arc<Mutex<SessionPoller>>>) {
        self.handle = handle;
    }

    pub(crate) fn clear(&mut self) {
        self.stop();
        self.handle = None;
    }

    /// Store a freshly spawned poller. The one place a start succeeds, so it also clears the
    /// repair schedule: a stale backoff must not make the next repair wait.
    pub(crate) fn install(&mut self, poller: SessionPoller) {
        self.handle = Some(Arc::new(Mutex::new(poller)));
        self.repair.reset();
    }

    /// Keep the handle only while it serves `held`; a poller for another execution reads files
    /// the row no longer owns.
    pub(crate) fn settle(&mut self, held: Held<'_>) {
        if !self.serves(held) {
            self.clear();
        }
    }

    /// Take the handle `prior` offers if it serves `held`; report whether it did.
    pub(crate) fn adopt_handle_if_serving(&mut self, prior: &Self, held: Held<'_>) -> bool {
        let serves = prior.serves(held);
        if serves {
            self.handle = prior.handle.clone();
        }
        serves
    }

    /// As [`Self::adopt_handle_if_serving`], stopping the offered poller when it does not serve.
    /// Settle the receiving row's own execution before asking: against the one it still holds,
    /// a handoff's poller would be refused and stopped.
    pub(crate) fn adopt_handle(&mut self, prior: &Self, held: Held<'_>) {
        if !self.adopt_handle_if_serving(prior, held) {
            prior.stop();
        }
    }

    /// Take the repair pacing of a prior row about the same execution; a row whose execution
    /// another process replaced does not inherit a window armed for the old one.
    pub(crate) fn adopt_pacing(&mut self, prior: &Self, prior_held: Held<'_>, held: Held<'_>) {
        if held == prior_held {
            self.repair = prior.repair.clone();
            self.retry_after = prior.retry_after;
        }
    }

    /// Take what a relaunch offers: the handle, and its timing when `stamped` says the launch
    /// re-evaluated the row's capture and the row still runs what it launched.
    pub(crate) fn adopt_relaunch(
        &mut self,
        launched: &Self,
        launched_held: Held<'_>,
        stamped: bool,
        held: Held<'_>,
    ) {
        self.adopt_handle(launched, held);
        if stamped && held == launched_held {
            self.repair.reset();
            self.retry_after = launched.retry_after;
        }
    }

    /// Take the outcome of a repair walked on a clone, unless the row started its own poller.
    pub(crate) fn adopt_repaired(&mut self, repaired: &Self) {
        self.retry_after = repaired.retry_after;
        if repaired.is_running() {
            self.handle = repaired.handle.clone();
        }
    }

    pub(crate) fn repair(&self) -> &PollerRepairBackoff {
        &self.repair
    }

    pub(crate) fn set_repair(&mut self, repair: PollerRepairBackoff) {
        self.repair = repair;
    }

    pub(crate) fn retry_after(&self) -> Option<Instant> {
        self.retry_after
    }

    pub(crate) fn clear_retry_after(&mut self) {
        self.retry_after = None;
    }

    pub(crate) fn defer_managed_start(&mut self) {
        self.retry_after = Some(Instant::now() + MANAGED_CAPTURE_RETRY_BACKOFF);
    }

    pub(crate) fn repair_due(&self, now: Instant) -> bool {
        self.repair.due(now)
    }

    pub(crate) fn reset_repair(&mut self) {
        self.repair.reset();
    }

    pub(crate) fn reprobe(&mut self, now: Instant) {
        self.repair.reprobe(now);
    }

    pub(crate) fn defer_repair(&mut self, now: Instant) -> Option<std::time::Duration> {
        self.repair.defer(now)
    }

    pub(crate) fn deferrals(&self) -> u32 {
        self.repair.deferrals()
    }
}

/// What a row may keep of a prior row's slot when it takes the whole slot over: a poller serves
/// one tool on one execution, and the repair window it paces was armed for the same, so each is
/// kept only while the receiving row still runs what the prior row ran.
///
/// Decided against the prior slot before the slot moves, since the move consumes it.
pub(crate) struct Handoff {
    keeps_handle: bool,
    keeps_pacing: bool,
}

impl Handoff {
    /// Stops the prior poller when the receiving row cannot use it: its thread keeps reading the
    /// launch the row is giving up.
    pub(crate) fn decide(prior: &CapturePollerSlot, prior_held: Held<'_>, held: Held<'_>) -> Self {
        let keeps_handle = prior.serves(held);
        if !keeps_handle {
            prior.stop();
        }
        Self {
            keeps_handle,
            keeps_pacing: held == prior_held,
        }
    }

    /// Drop what the move brought in that the receiving row cannot use.
    pub(crate) fn settle(self, slot: &mut CapturePollerSlot) {
        if !self.keeps_handle {
            slot.handle = None;
        }
        if !self.keeps_pacing {
            slot.repair = PollerRepairBackoff::default();
            slot.retry_after = None;
        }
    }
}

#[cfg(test)]
impl CapturePollerSlot {
    pub(crate) fn set_retry_after(&mut self, retry_after: Option<Instant>) {
        self.retry_after = retry_after;
    }

    pub(crate) fn repair_mut(&mut self) -> &mut PollerRepairBackoff {
        &mut self.repair
    }
}
