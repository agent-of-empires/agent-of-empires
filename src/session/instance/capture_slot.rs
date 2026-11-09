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

/// What a handoff does with an offered poller that does not serve the receiving row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unserved {
    /// The offering row is giving it up: stop its thread.
    Stop,
    /// The offering row stays live and keeps it.
    Leave,
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

    #[cfg(any(test, debug_assertions))]
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

    /// Return an offered live handle only while it serves the receiving execution.
    /// Stopped handles remain useful to their owner's final drain, not as replacement watchers.
    fn running_handle(&self, held: Held<'_>) -> Option<&Arc<Mutex<SessionPoller>>> {
        self.handle.as_ref().filter(|handle| {
            let poller = handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            poller.serves(held.tool, held.execution) && poller.is_running()
        })
    }

    /// Take a compatible live offer. A missing or stopped offer leaves the current handle alone.
    fn take_handle(&mut self, prior: &Self, held: Held<'_>) -> bool {
        if let Some(handle) = prior.running_handle(held) {
            self.handle = Some(Arc::clone(handle));
            true
        } else {
            false
        }
    }

    /// Take pacing for the same execution from a live offer or an actual pending repair.
    /// An empty transient snapshot cannot clear the receiver's own repair window.
    fn take_pacing(
        &mut self,
        prior: &Self,
        prior_held: Held<'_>,
        held: Held<'_>,
        took_handle: bool,
    ) {
        if held == prior_held
            && (took_handle
                || prior.retry_after.is_some()
                || prior.repair != PollerRepairBackoff::default())
        {
            self.repair = prior.repair.clone();
            self.retry_after = prior.retry_after;
        }
    }

    /// Take a prior row's handle and pacing for the row that now runs `held`. The slot settles
    /// its own handle against `held` first, so the caller need not order anything.
    pub(crate) fn adopt_reload(
        &mut self,
        prior: &Self,
        prior_held: Held<'_>,
        held: Held<'_>,
        unserved: Unserved,
    ) {
        self.settle(held);
        let took_handle = self.take_handle(prior, held);
        if !took_handle && unserved == Unserved::Stop {
            prior.stop();
        }
        self.take_pacing(prior, prior_held, held, took_handle);
    }

    /// Take what a relaunch offers for the row that now runs `held`: the handle (stopping one
    /// that does not serve it), and its timing when `stamped` says the launch re-evaluated the
    /// row's capture and the row still runs what it launched.
    pub(crate) fn adopt_relaunch(
        &mut self,
        launched: &Self,
        launched_held: Held<'_>,
        stamped: bool,
        held: Held<'_>,
    ) {
        self.settle(held);
        if !self.take_handle(launched, held) {
            launched.stop();
        }
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

#[cfg(test)]
impl CapturePollerSlot {
    pub(crate) fn set_retry_after(&mut self, retry_after: Option<Instant>) {
        self.retry_after = retry_after;
    }

    pub(crate) fn repair_mut(&mut self) -> &mut PollerRepairBackoff {
        &mut self.repair
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::ExecutionBinding;

    fn execution(launch: &str) -> ActiveExecution {
        ActiveExecution {
            launch_id: launch.into(),
            binding: ExecutionBinding {
                agent: "claude".into(),
                stores: Vec::new(),
                configuration: Vec::new(),
                cwd: std::path::PathBuf::from("/tmp"),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            },
            capture: None,
            container: None,
        }
    }

    fn slot_for(tool: &str, execution: Option<ActiveExecution>) -> CapturePollerSlot {
        let mut slot = CapturePollerSlot::default();
        slot.set_handle(Some(Arc::new(Mutex::new(SessionPoller::new(
            "tmux".into(),
            tool.into(),
            execution,
        )))));
        slot
    }

    fn paced(mut slot: CapturePollerSlot) -> CapturePollerSlot {
        slot.repair_mut().reprobe(Instant::now());
        slot.set_retry_after(Some(Instant::now()));
        slot
    }

    fn has_pacing(slot: &CapturePollerSlot) -> bool {
        slot.retry_after().is_some() || *slot.repair() != PollerRepairBackoff::default()
    }

    #[test]
    fn settle_keeps_a_handle_only_while_it_serves_the_held_execution() {
        let (one, two) = (execution("one"), execution("two"));
        for (held_tool, held_exec, keeps) in [
            ("claude", Some(&one), true),
            ("claude", Some(&two), false),
            ("claude", None, false),
            ("codex", Some(&one), false),
        ] {
            let mut slot = slot_for("claude", Some(one.clone()));
            slot.settle(Held::new(held_tool, held_exec));
            assert_eq!(slot.handle().is_some(), keeps, "{held_tool} {held_exec:?}");
        }
        let mut empty = CapturePollerSlot::default();
        empty.settle(Held::new("claude", None));
        assert!(empty.handle().is_none());
    }

    fn running_slot(tool: &str, execution: Option<ActiveExecution>) -> CapturePollerSlot {
        let mut poller = SessionPoller::new("tmux".into(), tool.into(), execution);
        assert_eq!(
            poller.start("id".into(), Box::new(|| None), Box::new(|_| {}), None),
            crate::session::poller::PollerSpawn::Spawned
        );
        let mut slot = CapturePollerSlot::default();
        slot.set_handle(Some(Arc::new(Mutex::new(poller))));
        slot
    }

    #[test]
    fn reload_adoption_settles_the_row_then_takes_what_applies() {
        let (one, two) = (execution("one"), execution("two"));
        // (incoming serves, unserved policy, row takes incoming handle, incoming still running)
        for (incoming_exec, unserved, takes, incoming_runs) in [
            (&two, Unserved::Stop, true, true),
            (&one, Unserved::Stop, false, false),
            (&one, Unserved::Leave, false, true),
        ] {
            let mut row = running_slot("claude", Some(one.clone()));
            let old = row.handle().unwrap().clone();
            let prior = paced(running_slot("claude", Some(incoming_exec.clone())));
            let prior_held = Held::new("claude", Some(incoming_exec));
            row.adopt_reload(
                &prior,
                prior_held,
                Held::new("claude", Some(&two)),
                unserved,
            );

            assert!(
                !row.is_running() || takes,
                "the row's poller for the swapped-out execution is gone"
            );
            assert_eq!(
                row.handle()
                    .is_some_and(|h| Arc::ptr_eq(h, prior.handle().unwrap())),
                takes
            );
            assert_eq!(prior.is_running(), incoming_runs);
            assert!(!old.lock().unwrap().is_running());
            // Pacing follows the run: kept only when the prior row ran the target.
            assert_eq!(has_pacing(&row), takes);
            prior.stop();
        }
    }

    #[test]
    fn relaunch_timing_needs_a_stamped_launch_for_the_row_s_own_run() {
        let (one, two) = (execution("one"), execution("two"));
        let launched = paced(slot_for("claude", Some(one.clone())));
        let launched_held = Held::new("claude", Some(&one));
        // (stamped, receiving execution, takes the launch's deadline)
        for (stamped, exec, takes) in [
            (true, &one, true),
            (false, &one, false),
            (true, &two, false),
        ] {
            let mut row = paced(CapturePollerSlot::default());
            row.set_retry_after(None);
            row.adopt_relaunch(
                &launched,
                launched_held,
                stamped,
                Held::new("claude", Some(exec)),
            );
            assert_eq!(
                row.retry_after().is_some(),
                takes,
                "stamped={stamped} {exec:?}"
            );
        }
    }

    #[test]
    fn a_repair_walked_on_a_clone_brings_its_deadline_and_only_a_running_poller() {
        let mut walked = CapturePollerSlot::default();
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        walked.set_retry_after(Some(deadline));
        let mut live = CapturePollerSlot::default();
        live.adopt_repaired(&walked);
        assert_eq!(live.retry_after(), Some(deadline));
        assert!(
            live.handle().is_none(),
            "a walk that started nothing hands over no handle"
        );
    }
}
