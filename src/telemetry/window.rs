//! Keeps a detached snapshot's counters inside one send at a time.

use std::future::Future;

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
