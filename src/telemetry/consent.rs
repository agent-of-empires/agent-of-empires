//! Binds every snapshot counter to the consent state it was recorded under. The Daemon is
//! the only writer of consent: [`set_consent`] saves the opt-in and the install id, then
//! moves the gate. Each move zeroes every counter. Recording touches only memory; captures
//! and binds reread both files, which also catches a hand edit.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

type Reset = Box<dyn Fn() -> bool + Send>;

struct Gate {
    /// `None` until first read; then the id, `None` while opted out.
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

/// Serializes consent writers from their read of the saved opt-in to their last file
/// write, so an opt-out never deletes an id a later opt-in installed. Separate from
/// [`GATE`] so counting never waits on file work.
static WRITER: Mutex<()> = Mutex::new(());

fn writer() -> MutexGuard<'static, ()> {
    WRITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
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

/// Saves an opt-in answer. An opt-in replaces any id before it saves `enabled`, minting a
/// fresh one or, under `DO_NOT_TRACK`, deleting it, so an id an earlier opt-out failed to
/// delete is never reused; if that fails, consent stays off. File work runs outside the
/// gate, so counting never waits on the state lock.
pub fn set_consent(enabled: bool) -> anyhow::Result<()> {
    let _writer = writer();
    let saved = crate::session::get_telemetry_settings().enabled;
    if enabled && !saved {
        if super::do_not_track() {
            super::state::delete_install_id()?;
        } else {
            super::state::rotate_install_id()?;
        }
    }
    crate::session::update_config(|config| config.telemetry.enabled = enabled)?;
    if !enabled {
        if let Err(e) = super::state::delete_install_id() {
            tracing::debug!(target: "telemetry", "failed to delete install id on opt-out: {e}");
        }
    }
    lock().refresh();
    Ok(())
}

/// Replaces the install id; counts recorded under the old one are zeroed. A no-op while
/// opted out or under `DO_NOT_TRACK`, checked under the writer guard so a reset queued
/// behind an opt-out never recreates an id.
pub fn reset_install_id() -> anyhow::Result<()> {
    let _writer = writer();
    if !super::is_opted_in() {
        return Ok(());
    }
    super::state::rotate_install_id()?;
    lock().refresh();
    Ok(())
}

/// Adds counters to zero on every consent change; `reset` returns false once its owner
/// is gone.
pub fn register(reset: impl Fn() -> bool + Send + 'static) {
    lock().resets.push(Box::new(reset));
}

/// Runs one counter increment under the current consent state.
pub fn record<R>(count: impl FnOnce() -> R) -> R {
    let mut gate = lock();
    if gate.seen.is_none() {
        gate.refresh();
    }
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

    /// Strips the counts from a snapshot unless consent is still the one they were
    /// captured under and the snapshot carries that id. Returns whether it is.
    pub fn bind(&self, snapshot: &mut super::UsageSnapshot) -> bool {
        let mut gate = lock();
        gate.refresh();
        let same = gate.epoch == self.epoch
            && self.install_id.as_deref() == Some(snapshot.install_id.as_str());
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

    /// A counter zeroed on every consent change.
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

        set_consent(true).unwrap();
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
        set_consent(false).unwrap();
        on.clear_reported(|| {
            counter.fetch_sub(reported, Ordering::Relaxed);
        });
        assert_eq!(
            read(),
            0,
            "opting out zeroes, and the stale clear is skipped"
        );

        set_consent(true).unwrap();
        let (again, reported) = capture(read);
        assert_eq!(reported, 0, "counts from the opted-out gap never return");
        bump(&counter);
        again.clear_reported(|| {
            counter.fetch_sub(reported, Ordering::Relaxed);
        });
        assert_eq!(read(), 1, "a clear inside the window keeps later counts");

        set_consent(false).unwrap();
    }

    /// An id an opt-out failed to delete is never handed out while disabled and is
    /// replaced by the next opt-in, which also discards the counts made in between.
    #[test]
    #[serial_test::serial]
    fn a_retained_id_does_not_carry_counts_or_identity_across_an_opt_out() {
        let _env = crate::session::test_support::EnvGuard::unset(&["DO_NOT_TRACK"]);
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let counter = gated_counter();
        let read = || counter.load(Ordering::Relaxed);
        set_consent(true).unwrap();
        let id = crate::telemetry::install_id().expect("opted in");

        // The opt-out saved, but its delete failed and left the id behind.
        crate::session::update_config(|c| c.telemetry.enabled = false).unwrap();
        bump(&counter);
        assert_eq!(capture(read).0.install_id, None);
        assert_eq!(
            crate::telemetry::ensure_install_id(),
            None,
            "a snapshot never reuses the retained id while disabled"
        );
        bump(&counter);

        set_consent(true).unwrap();
        bump(&counter);
        let (on, reported) = capture(read);
        let rotated = on.install_id.expect("opted in");
        assert_ne!(rotated, id, "the next opt-in replaces a retained id");
        assert_eq!(
            reported, 1,
            "only the count made after re-enable is captured"
        );

        set_consent(false).unwrap();
    }

    /// A consent change between capture and send strips the counts, even when the id the
    /// snapshot carries still matches.
    #[test]
    #[serial_test::serial]
    fn a_bind_after_a_consent_change_strips_the_counts() {
        let _env = crate::session::test_support::EnvGuard::unset(&["DO_NOT_TRACK"]);
        let _app_dir = crate::session::test_support::isolate_app_dir();
        set_consent(true).unwrap();
        let (bound, ()) = capture(|| ());
        let mut snapshot = crate::telemetry::build_usage_snapshot(
            crate::telemetry::Surface::Serve,
            &[],
            Default::default(),
            3,
            None,
            None,
            &Default::default(),
        )
        .expect("opted in");
        assert!(bound.clone().bind(&mut snapshot.clone()));

        // Hand edits off and on again: the retained id matches, the epoch does not.
        for enabled in [false, true] {
            crate::session::update_config(|c| c.telemetry.enabled = enabled).unwrap();
            lock().refresh();
        }
        assert_eq!(
            bound.install_id.as_deref(),
            Some(snapshot.install_id.as_str())
        );
        assert!(!bound.bind(&mut snapshot));
        assert_eq!(snapshot.session_creates_since_last_snapshot, 0);

        set_consent(false).unwrap();
    }

    /// CLI command counts are recorded under the id they would be sent with, so a new id,
    /// from `reset-id` or from an opt-in over an id an opt-out failed to delete, starts
    /// with none.
    #[test]
    #[serial_test::serial]
    fn a_new_id_never_carries_cli_counts_from_the_old_one() {
        let _env = crate::session::test_support::EnvGuard::unset(&["DO_NOT_TRACK"]);
        let _app_dir = crate::session::test_support::isolate_app_dir();
        set_consent(true).unwrap();
        crate::telemetry::record_cli_command("add");
        assert!(crate::telemetry::build_cli_usage().is_some());
        reset_install_id().unwrap();
        assert!(
            crate::telemetry::build_cli_usage().is_none(),
            "reset-id carried counts to the new id"
        );

        crate::telemetry::record_cli_command("add");
        crate::session::update_config(|c| c.telemetry.enabled = false).unwrap();
        set_consent(true).unwrap();
        assert!(
            crate::telemetry::build_cli_usage().is_none(),
            "an opt-in over a retained id carried its counts"
        );

        set_consent(false).unwrap();
    }

    /// A reset that runs after an opt-out leaves no id behind.
    #[test]
    #[serial_test::serial]
    fn a_reset_after_an_opt_out_does_not_recreate_the_id() {
        let _env = crate::session::test_support::EnvGuard::unset(&["DO_NOT_TRACK"]);
        let _app_dir = crate::session::test_support::isolate_app_dir();
        set_consent(true).unwrap();
        set_consent(false).unwrap();
        reset_install_id().unwrap();
        assert_eq!(crate::telemetry::state::install_id(), None);
    }

    /// Concurrent opt-ins and opt-outs leave the saved opt-in and the id in agreement:
    /// an opt-out never deletes the id a later opt-in installed.
    #[test]
    #[serial_test::serial]
    fn concurrent_consent_writers_keep_the_opt_in_and_id_in_agreement() {
        let _env = crate::session::test_support::EnvGuard::unset(&["DO_NOT_TRACK"]);
        let _app_dir = crate::session::test_support::isolate_app_dir();
        for _ in 0..20 {
            let start = Arc::new(std::sync::Barrier::new(4));
            let writers: Vec<_> = [true, false, true, false]
                .into_iter()
                .map(|enabled| {
                    let start = start.clone();
                    std::thread::spawn(move || {
                        start.wait();
                        set_consent(enabled).unwrap();
                    })
                })
                .collect();
            for writer in writers {
                writer.join().unwrap();
            }
            assert_eq!(
                crate::session::get_telemetry_settings().enabled,
                crate::telemetry::state::install_id().is_some(),
            );
        }
        set_consent(false).unwrap();
    }

    /// Under `DO_NOT_TRACK` an opt-in deletes a retained id instead of minting one, so
    /// lifting it later never revives the old id.
    #[test]
    #[serial_test::serial]
    fn an_opt_in_under_do_not_track_drops_a_retained_id() {
        use crate::session::test_support::EnvGuard;
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let id = {
            let _env = EnvGuard::unset(&["DO_NOT_TRACK"]);
            set_consent(true).unwrap();
            crate::telemetry::install_id().expect("opted in")
        };
        // The opt-out saved, but its delete failed and left the id behind.
        crate::session::update_config(|c| c.telemetry.enabled = false).unwrap();
        {
            let _env = EnvGuard::set(&[("DO_NOT_TRACK", "1")]);
            set_consent(true).unwrap();
        }
        assert_eq!(crate::telemetry::install_id(), None);

        let _env = EnvGuard::unset(&["DO_NOT_TRACK"]);
        let fresh = crate::telemetry::ensure_install_id().expect("opted in");
        assert_ne!(fresh, id, "lifting DO_NOT_TRACK revived the retained id");
        set_consent(false).unwrap();
    }
}
