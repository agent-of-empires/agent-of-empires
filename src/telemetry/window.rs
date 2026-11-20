//! Per-loop bookkeeping that keeps a snapshot's counters inside one consent window and
//! one send at a time.

use std::future::Future;

/// The install id a snapshot loop last saw. Opting out deletes the id and opting in
/// creates a fresh one, as does `reset-id`, so a changed id means consent changed since
/// the counters started accumulating.
#[derive(Debug, Default)]
pub struct ConsentWindow {
    last: Option<Option<String>>,
}

impl ConsentWindow {
    /// Records this build's id (`None` while opted out) and returns whether the counters
    /// read for it must be discarded instead of reported. The first build of a process
    /// keeps them: it runs at startup, before anything is counted.
    pub fn discard(&mut self, install_id: Option<&str>) -> bool {
        let current = install_id.map(str::to_string);
        let changed = self.last.as_ref().is_some_and(|last| *last != current);
        let off = current.is_none();
        self.last = Some(current);
        off || changed
    }
}

/// A detached snapshot send that clears its counters on confirm. Holding one at a time
/// keeps two sends from reporting, and then clearing, the same counts.
#[derive(Debug, Default)]
pub struct SingleFlight {
    in_flight: Option<tokio::task::JoinHandle<()>>,
}

impl SingleFlight {
    pub fn is_busy(&self) -> bool {
        self.in_flight.as_ref().is_some_and(|h| !h.is_finished())
    }

    /// Spawns `send` unless one is still running; returns whether it spawned.
    pub fn spawn_if_idle(&mut self, send: impl Future<Output = ()> + Send + 'static) -> bool {
        if self.is_busy() {
            return false;
        }
        self.in_flight = Some(tokio::spawn(send));
        true
    }

    /// Waits out the in-flight send so the next build reads counters it already cleared.
    pub async fn settle(&mut self) {
        if let Some(handle) = self.in_flight.take() {
            let _ = handle.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn consent_window_discards_while_off_and_across_an_id_change() {
        let mut window = ConsentWindow::default();
        // (install id at this build, discard expected)
        let cases = [
            (Some("a"), false),
            (Some("a"), false),
            (None, true),
            (Some("b"), true),
            (Some("b"), false),
            (Some("c"), true),
        ];
        for (i, (id, expected)) in cases.into_iter().enumerate() {
            assert_eq!(window.discard(id), expected, "build {i} with {id:?}");
        }
        assert!(ConsentWindow::default().discard(None), "starting opted out");
    }

    #[tokio::test]
    async fn single_flight_skips_while_busy_and_settle_waits_for_the_clear() {
        let counter = Arc::new(AtomicU32::new(3));
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let mut slot = SingleFlight::default();
        let reported = counter.load(Ordering::SeqCst);
        let clearing = Arc::clone(&counter);
        assert!(slot.spawn_if_idle(async move {
            let _ = released.await;
            clearing.fetch_sub(reported, Ordering::SeqCst);
        }));
        assert!(
            !slot.spawn_if_idle(async {}),
            "a second send must wait for the first"
        );

        {
            let settle = slot.settle();
            tokio::pin!(settle);
            assert!(futures_util::poll!(&mut settle).is_pending());
            assert_eq!(
                counter.load(Ordering::SeqCst),
                3,
                "not cleared before confirm"
            );
            release.send(()).unwrap();
            settle.await;
        }
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "a build after settle reads the cleared counter"
        );
        assert!(!slot.is_busy());
        assert!(slot.spawn_if_idle(async {}));
    }
}
