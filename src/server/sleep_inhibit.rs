//! Holding the system awake while sessions are working.

use std::sync::Arc;

use super::state::AppState;

/// Cadence at which the daemon reconciles the OS sleep-inhibit assertion.
/// Mirrors [`super::idle_reap::SESSION_IDLE_REAP_INTERVAL`]: a 2s status tick
/// must not drive a
/// config-file read plus a subprocess reconcile every iteration. Recovery
/// latency is irrelevant here: the backing child only dies on external kill
/// (caffeinate `-w <pid>` and the systemd-inhibit `cat` otherwise outlive every
/// tick), and both acquire and release lag are dominated by the minutes-long
/// grace window and the OS idle-sleep timer.
pub(super) const SLEEP_INHIBIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// `AppState::sleep_inhibit_snapshot` bit: `prevent_sleep_when_active` was on
/// at the last reconcile.
pub(crate) const SLEEP_INHIBIT_SNAPSHOT_ENABLED: u8 = 0b01;

/// `AppState::sleep_inhibit_snapshot` bit: an inhibitor slot is retained. This
/// is slot presence, not held: the endpoint gates it on `backend_available`.
pub(crate) const SLEEP_INHIBIT_SNAPSHOT_SLOT_PRESENT: u8 = 0b10;

/// Acquire or release the OS sleep-inhibit assertion. Throttled to
/// [`SLEEP_INHIBIT_INTERVAL`] like the idle reaper, so the per-tick disk read
/// is avoided. Reads the global config (the toggle is daemon-global, not
/// per-profile) off the async runtime because `Config::load_or_warn` touches disk,
/// then reconciles: hold the assertion while the toggle is on and any session
/// has recent activity, release once every session has been idle past the
/// grace window.
pub(super) async fn update_sleep_inhibit(
    state: &Arc<AppState>,
    slot: &mut Option<Box<dyn crate::process::SleepInhibit>>,
    last_reconcile: &mut Option<std::time::Instant>,
) {
    if last_reconcile.is_some_and(|t| t.elapsed() < SLEEP_INHIBIT_INTERVAL) {
        return;
    }
    *last_reconcile = Some(std::time::Instant::now());
    let Ok(config) = tokio::task::spawn_blocking(crate::session::Config::load_or_warn).await else {
        return;
    };
    let window = std::time::Duration::from_secs(
        u64::from(config.session.prevent_sleep_idle_grace_minutes) * 60,
    );
    let desired = config.session.prevent_sleep_when_active && {
        let instances = state.instances.read().await;
        instances.iter().any(|i| i.has_recent_activity(window))
    };
    reconcile_sleep_inhibit(desired, slot, crate::process::sleep_inhibitor);

    let mut snapshot = 0u8;
    if config.session.prevent_sleep_when_active {
        snapshot |= SLEEP_INHIBIT_SNAPSHOT_ENABLED;
    }
    if slot.is_some() {
        snapshot |= SLEEP_INHIBIT_SNAPSHOT_SLOT_PRESENT;
    }
    state
        .sleep_inhibit_snapshot
        .store(snapshot, std::sync::atomic::Ordering::Relaxed);
}

/// Level-triggered reconciler for the sleep-inhibit slot. Acquiring on a slot
/// whose child has died respawns it, so a caffeinate / systemd-inhibit child
/// that exits unexpectedly is replaced within one reconcile interval while
/// sessions are still active. The grace window is the hysteresis (`idle_age`
/// is monotonic within an idle period and crosses the window exactly once), so
/// no extra debounce is needed. `make` is injected so tests can supply a mock.
///
/// Runs inline on the async task rather than `spawn_blocking`: every operation
/// here is a subprocess spawn / `try_wait` / kill that returns in milliseconds,
/// and the call is throttled to once per interval; only the config read in
/// `update_sleep_inhibit`, which touches disk, is offloaded.
pub(super) fn reconcile_sleep_inhibit(
    desired: bool,
    slot: &mut Option<Box<dyn crate::process::SleepInhibit>>,
    make: impl FnOnce() -> Box<dyn crate::process::SleepInhibit>,
) {
    match (desired, slot.as_mut().map(|i| i.is_held_alive())) {
        (true, Some(true)) => {}
        (true, _) => {
            // In the normal path any prior dead child was already reaped by the
            // `try_wait` inside `is_held_alive` above, so overwriting the slot
            // below leaks no zombie. The only unreaped case is a `try_wait`
            // error, which is ECHILD-class (the child is already gone), so no
            // defensive reap is warranted.
            let mut inhibitor = make();
            match inhibitor.acquire() {
                Ok(()) => *slot = Some(inhibitor),
                Err(e) => {
                    tracing::warn!(
                        target: "server.sleep_inhibit",
                        error = %e,
                        "failed to acquire OS sleep-inhibit assertion",
                    );
                    *slot = None;
                }
            }
        }
        (false, Some(_)) => {
            if let Some(mut inhibitor) = slot.take() {
                inhibitor.release();
            }
        }
        (false, None) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct MockInhibitState {
        acquires: u32,
        releases: u32,
        alive: bool,
    }

    struct MockInhibitor {
        state: std::sync::Arc<std::sync::Mutex<MockInhibitState>>,
    }

    impl crate::process::SleepInhibit for MockInhibitor {
        fn acquire(&mut self) -> anyhow::Result<()> {
            let mut s = self.state.lock().unwrap();
            s.acquires += 1;
            s.alive = true;
            Ok(())
        }

        fn release(&mut self) {
            let mut s = self.state.lock().unwrap();
            s.releases += 1;
            s.alive = false;
        }

        fn is_held_alive(&mut self) -> bool {
            self.state.lock().unwrap().alive
        }
    }

    fn mock_factory(
        state: &std::sync::Arc<std::sync::Mutex<MockInhibitState>>,
    ) -> impl FnOnce() -> Box<dyn crate::process::SleepInhibit> {
        let state = state.clone();
        move || Box::new(MockInhibitor { state }) as Box<dyn crate::process::SleepInhibit>
    }

    fn never_built() -> Box<dyn crate::process::SleepInhibit> {
        panic!("reconcile_sleep_inhibit must not build an inhibitor on this transition")
    }

    #[test]
    fn reconcile_sleep_inhibit_acquires_when_desired_and_not_held() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(MockInhibitState::default()));
        let mut slot: Option<Box<dyn crate::process::SleepInhibit>> = None;
        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        let s = state.lock().unwrap();
        assert_eq!(s.acquires, 1);
        assert_eq!(s.releases, 0);
        assert!(slot.is_some());
    }

    #[test]
    fn reconcile_sleep_inhibit_noop_when_desired_and_held_alive() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(MockInhibitState::default()));
        let mut slot: Option<Box<dyn crate::process::SleepInhibit>> = None;
        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        reconcile_sleep_inhibit(true, &mut slot, never_built);
        assert_eq!(state.lock().unwrap().acquires, 1);
        assert!(slot.is_some());
    }

    #[test]
    fn reconcile_sleep_inhibit_respawns_when_desired_and_child_dead() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(MockInhibitState::default()));
        let mut slot: Option<Box<dyn crate::process::SleepInhibit>> = None;
        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        state.lock().unwrap().alive = false;
        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        let s = state.lock().unwrap();
        assert_eq!(s.acquires, 2);
        assert!(slot.is_some());
    }

    #[test]
    fn reconcile_sleep_inhibit_releases_when_not_desired_and_held() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(MockInhibitState::default()));
        let mut slot: Option<Box<dyn crate::process::SleepInhibit>> = None;
        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        reconcile_sleep_inhibit(false, &mut slot, never_built);
        let s = state.lock().unwrap();
        assert_eq!(s.releases, 1);
        assert!(slot.is_none());
    }

    #[test]
    fn reconcile_sleep_inhibit_noop_when_not_desired_and_not_held() {
        let mut slot: Option<Box<dyn crate::process::SleepInhibit>> = None;
        reconcile_sleep_inhibit(false, &mut slot, never_built);
        assert!(slot.is_none());
    }

    #[test]
    fn reconcile_sleep_inhibit_sequence_hold_release_hold() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(MockInhibitState::default()));
        let mut slot: Option<Box<dyn crate::process::SleepInhibit>> = None;
        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        reconcile_sleep_inhibit(false, &mut slot, never_built);
        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        let s = state.lock().unwrap();
        assert_eq!(s.acquires, 2);
        assert_eq!(s.releases, 1);
        assert!(slot.is_some());
    }
}
