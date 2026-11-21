//! Binds every snapshot counter to the consent state it was recorded under: the persisted
//! opt-in together with the install id, which opting out deletes and opting in or
//! `reset-id` regenerates. Each record, capture and clear rereads both under one lock, and
//! a change zeroes every counter before anything is counted or read under the new state.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

type Reset = Box<dyn Fn() -> bool + Send>;

struct Gate {
    /// `None` until first read; then the id, `None` while opted out.
    seen: Option<Option<String>>,
    epoch: u64,
    resets: Vec<Reset>,
    /// An id an opt-out here failed to delete; never reused by a later opt-in.
    retained: Option<String>,
}

static GATE: Mutex<Gate> = Mutex::new(Gate {
    seen: None,
    epoch: 0,
    resets: Vec::new(),
    retained: None,
});

fn lock() -> MutexGuard<'static, Gate> {
    GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Gate {
    fn refresh(&mut self) {
        let id = if super::is_opted_in() {
            super::install_id()
        } else {
            None
        };
        if self.seen.as_ref() == Some(&id) {
            return;
        }
        self.seen = Some(id);
        self.epoch += 1;
        super::feature_use::zero();
        self.resets.retain(|reset| reset());
    }
}

/// Reconciles the install id with a saved opt-in choice under the gate, so no count or
/// capture interleaves. An id an opt-out failed to delete is regenerated, not reused.
pub(super) fn apply_opt_in_change(enabled: bool) {
    let mut gate = lock();
    if !enabled {
        if let Err(e) = super::state::delete_install_id() {
            tracing::debug!(target: "telemetry", "failed to delete install id on opt-out: {e}");
        }
        gate.retained = super::install_id();
    } else if !super::do_not_track() {
        let retained = gate.retained.take();
        if retained.is_some() && super::install_id() == retained {
            if let Err(e) = super::state::reset_install_id() {
                tracing::debug!(target: "telemetry", "failed to rotate a retained install id: {e}");
            }
        } else {
            let _ = super::state::ensure_install_id();
        }
    }
    gate.refresh();
}

/// Adds counters to zero on every consent change; `reset` returns false once its owner
/// is gone.
pub fn register(reset: impl Fn() -> bool + Send + 'static) {
    lock().resets.push(Box::new(reset));
}

/// Runs one counter increment under the current consent state.
pub fn record<R>(count: impl FnOnce() -> R) -> R {
    let mut gate = lock();
    gate.refresh();
    count()
}

pub fn bump(counter: &AtomicU32) {
    record(|| counter.fetch_add(1, Ordering::Relaxed));
}

/// The consent state a set of counters was read under.
#[derive(Debug, Clone)]
pub struct Bound {
    epoch: u64,
    install_id: Option<String>,
}

/// Reads counters together with the consent state they were recorded under.
pub fn capture<R>(read: impl FnOnce() -> R) -> (Bound, R) {
    let mut gate = lock();
    gate.refresh();
    let bound = Bound {
        epoch: gate.epoch,
        install_id: gate.seen.clone().flatten(),
    };
    (bound, read())
}

impl Bound {
    pub fn same_window(&self, other: &Bound) -> bool {
        self.epoch == other.epoch
    }

    /// Strips the counts from a snapshot sent under an id other than the one they were
    /// captured under; the consent change that caused it zeroes them on the next refresh.
    /// Returns whether the snapshot stays in the captured window.
    pub fn bind(&self, snapshot: &mut super::UsageSnapshot) -> bool {
        let same = self.install_id.as_deref() == Some(snapshot.install_id.as_str());
        if !same {
            snapshot.clear_deltas();
        }
        same
    }

    /// Subtracts the reported counts, unless a consent change already zeroed them.
    pub fn clear_reported(&self, clear: impl FnOnce()) {
        let mut gate = lock();
        gate.refresh();
        if gate.epoch == self.epoch {
            clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn set_opt_in(enabled: bool) {
        crate::session::update_config(|c| c.telemetry.enabled = enabled).unwrap();
    }

    fn opt_in(enabled: bool) {
        set_opt_in(enabled);
        apply_opt_in_change(enabled);
    }

    /// A counter zeroed on every consent change, and a reader for it.
    fn gated_counter() -> Arc<AtomicU32> {
        let counter = Arc::new(AtomicU32::new(0));
        let owned = Arc::downgrade(&counter);
        register(move || {
            owned
                .upgrade()
                .map(|c| c.store(0, Ordering::Relaxed))
                .is_some()
        });
        counter
    }

    /// Counts made while off never reach an opted-in capture; counts made after opt-in but
    /// before the first capture do, and a send confirmed across a change clears nothing new.
    #[test]
    #[serial_test::serial]
    fn counts_stay_on_the_side_of_the_transition_they_were_recorded_on() {
        let _env = crate::session::test_support::EnvGuard::unset(&["DO_NOT_TRACK"]);
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let counter = gated_counter();
        let read = || counter.load(Ordering::Relaxed);

        bump(&counter);
        let (off, _) = capture(read);
        assert_eq!(off.install_id, None);
        bump(&counter);

        opt_in(true);
        let after_opt_in = 2;
        for _ in 0..after_opt_in {
            bump(&counter);
        }
        let (on, reported) = capture(read);
        assert!(on.install_id.is_some());
        assert!(!on.same_window(&off));
        assert_eq!(
            reported, after_opt_in,
            "only post opt-in counts are captured"
        );

        bump(&counter);
        opt_in(false);
        on.clear_reported(|| {
            counter.fetch_sub(reported, Ordering::Relaxed);
        });
        assert_eq!(
            read(),
            0,
            "opting out zeroes, and the stale clear is skipped"
        );

        opt_in(true);
        let (again, reported) = capture(read);
        assert_eq!(reported, 0, "counts from the opted-out gap never return");
        bump(&counter);
        again.clear_reported(|| {
            counter.fetch_sub(reported, Ordering::Relaxed);
        });
        assert_eq!(read(), 1, "a clear inside the window keeps later counts");

        opt_in(false);
    }

    /// The persisted opt-in gates counts even when the id outlives an opt-out, as when its
    /// delete failed: counts made while disabled are dropped on re-enable, the stale id is
    /// never handed out while disabled, and an opt-in here regenerates it.
    #[test]
    #[serial_test::serial]
    fn a_retained_id_does_not_carry_counts_or_identity_across_an_opt_out() {
        let _env = crate::session::test_support::EnvGuard::unset(&["DO_NOT_TRACK"]);
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let counter = gated_counter();
        let read = || counter.load(Ordering::Relaxed);
        opt_in(true);
        let id = crate::telemetry::install_id().expect("opted in");

        // Another process saved the opt-out; its delete has not happened or failed.
        set_opt_in(false);
        bump(&counter);
        assert_eq!(capture(read).0.install_id, None);
        assert_eq!(
            crate::telemetry::ensure_install_id(),
            None,
            "a snapshot never reuses the retained id while disabled"
        );
        bump(&counter);
        assert_eq!(crate::telemetry::install_id().as_deref(), Some(id.as_str()));

        opt_in(true);
        bump(&counter);
        let (on, reported) = capture(read);
        assert_eq!(on.install_id.as_deref(), Some(id.as_str()));
        assert_eq!(
            reported, 1,
            "only the count made after re-enable is captured"
        );

        // This process's own opt-out failed to delete the id.
        set_opt_in(false);
        lock().retained = crate::telemetry::install_id();
        opt_in(true);
        let rotated = crate::telemetry::install_id().expect("opted in");
        assert_ne!(rotated, id, "a retained id is regenerated on opt-in");

        opt_in(false);
    }
}
