use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::Utc;

use super::{LifecycleOperation, Storage};

pub(crate) struct ReservationHeartbeat {
    stop_tx: Sender<()>,
    join: Option<JoinHandle<()>>,
}

impl ReservationHeartbeat {
    pub(crate) fn start(
        storage: &Storage,
        session_id: &str,
        operation: LifecycleOperation,
        generation: u64,
        interval: Duration,
    ) -> Result<Self> {
        let storage = storage.clone();
        let session_id = session_id.to_string();
        let (stop_tx, stop_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("aoe-lifecycle-heartbeat".to_string())
            .spawn(move || {
                reservation_heartbeat_loop(
                    storage, session_id, operation, generation, interval, stop_rx,
                )
            })
            .context("failed to start lifecycle reservation heartbeat")?;
        Ok(Self {
            stop_tx,
            join: Some(join),
        })
    }

    pub(crate) fn stop(mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ReservationHeartbeat {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(());
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn reservation_heartbeat_loop(
    storage: Storage,
    session_id: String,
    operation: LifecycleOperation,
    generation: u64,
    interval: Duration,
    stop_rx: Receiver<()>,
) {
    loop {
        match stop_rx.recv_timeout(interval) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        let Ok(Some(_lifecycle_lock)) = storage.try_acquire_instance_lifecycle_lock(&session_id)
        else {
            continue;
        };
        match storage.update(|instances, _groups| {
            let owned = instances
                .iter_mut()
                .find(|instance| instance.id == session_id)
                .is_some_and(|instance| {
                    instance.renew_lifecycle_reservation_if_owned(operation, generation, Utc::now())
                });
            Ok(owned)
        }) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                tracing::warn!(
                    target: "session.lifecycle",
                    session_id = %session_id,
                    operation = ?operation,
                    generation,
                    %error,
                    "lifecycle reservation heartbeat could not be persisted; retrying"
                );
            }
        }
    }
}
