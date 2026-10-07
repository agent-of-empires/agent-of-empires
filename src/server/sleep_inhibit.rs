//! Holding the system awake while sessions are working.

use std::sync::Arc;

use super::state::AppState;

/// Cadence at which the daemon reconciles the OS sleep-inhibit assertion.
pub(super) const SLEEP_INHIBIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);

/// `AppState::sleep_inhibit_snapshot` bit.
pub(crate) const SLEEP_INHIBIT_SNAPSHOT_ENABLED: u8 = 0b01;

/// `AppState::sleep_inhibit_snapshot` bit.
pub(crate) const SLEEP_INHIBIT_SNAPSHOT_SLOT_PRESENT: u8 = 0b10;

/// Acquire or release the OS sleep-inhibit assertion.
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

/// Level-triggered reconciler for the sleep-inhibit slot.
pub(super) fn reconcile_sleep_inhibit(
    desired: bool,
    slot: &mut Option<Box<dyn crate::process::SleepInhibit>>,
    make: impl FnOnce() -> Box<dyn crate::process::SleepInhibit>,
) {
    match (desired, slot.as_mut().map(|i| i.is_held_alive())) {
        (true, Some(true)) => {}
        (true, _) => {
            // In the normal path any prior dead child was already reaped by the `try_wait`
            // inside `is_held_alive` above, so overwriting the slot below leaks no zombie.
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

    /// One inhibitor is held for as long as it is wanted: it is acquired once, kept
    /// across repeat ticks while the child is alive, respawned if the child died,
    /// released when it stops being wanted, and never built when it is not.
    #[test]
    fn reconcile_sleep_inhibit_holds_one_live_inhibitor_while_desired() {
        let state = std::sync::Arc::new(std::sync::Mutex::new(MockInhibitState::default()));
        let mut slot: Option<Box<dyn crate::process::SleepInhibit>> = None;
        let counts = |slot: &Option<Box<dyn crate::process::SleepInhibit>>| {
            let s = state.lock().unwrap();
            (s.acquires, s.releases, slot.is_some())
        };

        reconcile_sleep_inhibit(false, &mut slot, never_built);
        assert_eq!(counts(&slot), (0, 0, false), "not desired, not held");

        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        assert_eq!(counts(&slot), (1, 0, true), "desired, not held");

        reconcile_sleep_inhibit(true, &mut slot, never_built);
        assert_eq!(counts(&slot), (1, 0, true), "desired, held alive");

        state.lock().unwrap().alive = false;
        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        assert_eq!(counts(&slot), (2, 0, true), "desired, child died");

        reconcile_sleep_inhibit(false, &mut slot, never_built);
        assert_eq!(counts(&slot), (2, 1, false), "no longer desired");

        reconcile_sleep_inhibit(true, &mut slot, mock_factory(&state));
        assert_eq!(counts(&slot), (3, 1, true), "wanted again");
    }
}
