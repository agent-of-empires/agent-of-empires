//! Binds every snapshot counter to the consent state it was recorded under. Opting out
//! deletes the install id and opting in or `reset-id` writes a fresh one, so a changed id
//! is a consent change. Each record, capture and clear rereads the id under one lock, and
//! a change zeroes every counter before anything is counted or read under the new state.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

type Reset = Box<dyn Fn() -> bool + Send>;

struct Gate {
    /// `None` until first read; then the id, `None` while off.
    seen: Option<Option<String>>,
    epoch: u64,
    resets: Vec<Reset>,
}

static GATE: Mutex<Gate> = Mutex::new(Gate {
    seen: None,
    epoch: 0,
    resets: Vec::new(),
});

fn lock() -> MutexGuard<'static, Gate> {
    GATE.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Gate {
    fn refresh(&mut self) {
        let id = if super::do_not_track() {
            None
        } else {
            super::install_id()
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
    pub fn bind(&self, snapshot: &mut super::UsageSnapshot) {
        if self.install_id.as_deref() != Some(snapshot.install_id.as_str()) {
            snapshot.clear_deltas();
        }
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

    /// Counts made while off never reach an opted-in capture; counts made after opt-in but
    /// before the first capture do, and a send confirmed across a change clears nothing new.
    #[test]
    #[serial_test::serial]
    fn counts_stay_on_the_side_of_the_transition_they_were_recorded_on() {
        let _env = crate::session::test_support::EnvGuard::unset(&["DO_NOT_TRACK"]);
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let counter = Arc::new(AtomicU32::new(0));
        let owned = Arc::downgrade(&counter);
        register(move || {
            owned
                .upgrade()
                .map(|c| c.store(0, Ordering::Relaxed))
                .is_some()
        });
        let read = || counter.load(Ordering::Relaxed);

        bump(&counter);
        let (off, _) = capture(read);
        assert_eq!(off.install_id, None);
        bump(&counter);

        crate::telemetry::apply_opt_in_change(true);
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
        crate::telemetry::apply_opt_in_change(false);
        on.clear_reported(|| {
            counter.fetch_sub(reported, Ordering::Relaxed);
        });
        assert_eq!(
            read(),
            0,
            "opting out zeroes, and the stale clear is skipped"
        );

        crate::telemetry::apply_opt_in_change(true);
        let (again, reported) = capture(read);
        assert_eq!(reported, 0, "counts from the opted-out gap never return");
        bump(&counter);
        again.clear_reported(|| {
            counter.fetch_sub(reported, Ordering::Relaxed);
        });
        assert_eq!(read(), 1, "a clear inside the window keeps later counts");

        crate::telemetry::apply_opt_in_change(false);
    }
}
