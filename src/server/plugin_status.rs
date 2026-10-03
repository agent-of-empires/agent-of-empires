//! Forwards daemon status transitions to plugin workers as `session.status.changed`.

use std::sync::Arc;

use tokio::sync::broadcast::{self, error::RecvError};
use tokio_util::sync::CancellationToken;

use super::push::StatusChange;
use super::AppState;
use crate::plugin::host::PluginHost;

/// Spawn the forwarder. The caller guarantees the plugin host exists.
pub fn spawn_forwarder(state: Arc<AppState>, host: Arc<PluginHost>) {
    let rx = state.status_tx.subscribe();
    tokio::spawn(run_forwarder(rx, host, state.shutdown.clone()));
}

pub(crate) async fn run_forwarder(
    mut rx: broadcast::Receiver<StatusChange>,
    host: Arc<PluginHost>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(change) => {
                    host.emit_session_status_changed(&change, &change.effective_profile).await;
                }
                Err(RecvError::Lagged(n)) => {
                    tracing::warn!(target: "plugin.host", lagged = n, "plugin status forwarder lagged, skipped events");
                }
                Err(RecvError::Closed) => return,
            },
            _ = shutdown.cancelled() => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Status;
    use std::time::Duration;
    use tokio::sync::broadcast;

    fn change(n: usize) -> StatusChange {
        StatusChange {
            instance_id: format!("sess-{n}"),
            instance_title: "t".to_string(),
            effective_profile: "default".to_string(),
            old: Status::Running,
            new: Status::Idle,
            at: chrono::Utc::now(),
        }
    }

    async fn host_with_worker() -> (
        std::sync::Arc<PluginHost>,
        tokio::sync::mpsc::Receiver<String>,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let host = PluginHost::new(tmp.path(), "default", None).unwrap();
        let rx = host.register_test_worker("acme.watcher", Some(16)).await;
        (host, rx, tmp)
    }

    async fn next_session_id(rx: &mut tokio::sync::mpsc::Receiver<String>) -> String {
        let line = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for a notification")
            .expect("worker channel closed");
        let msg: serde_json::Value = serde_json::from_str(line.trim_end()).unwrap();
        msg["params"]["session_id"].as_str().unwrap().to_string()
    }

    #[tokio::test]
    async fn forwards_each_transition_in_order() {
        let (host, mut rx, _tmp) = host_with_worker().await;
        let (tx, brx) = broadcast::channel(16);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_forwarder(brx, host, shutdown.clone()));

        tx.send(change(1)).unwrap();
        tx.send(change(2)).unwrap();
        assert_eq!(next_session_id(&mut rx).await, "sess-1");
        assert_eq!(next_session_id(&mut rx).await, "sess-2");

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn skips_sessions_of_other_profiles() {
        let (host, mut rx, _tmp) = host_with_worker().await;
        let (tx, brx) = broadcast::channel(16);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_forwarder(brx, host, shutdown.clone()));

        // Events are handled in order, so seeing sess-2 means sess-1 was already dropped.
        tx.send(StatusChange {
            effective_profile: "other".to_string(),
            ..change(1)
        })
        .unwrap();
        tx.send(change(2)).unwrap();
        assert_eq!(next_session_id(&mut rx).await, "sess-2");
        assert!(rx.try_recv().is_err(), "nothing else was forwarded");

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn survives_lag_and_keeps_forwarding() {
        let (host, mut rx, _tmp) = host_with_worker().await;
        let (tx, brx) = broadcast::channel(2);
        // Overflow the receiver before the forwarder first polls it.
        for n in 1..=5 {
            tx.send(change(n)).unwrap();
        }
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_forwarder(brx, host, shutdown.clone()));

        // The two newest survive the lag; the forwarder must still be alive.
        assert_eq!(next_session_id(&mut rx).await, "sess-4");
        assert_eq!(next_session_id(&mut rx).await, "sess-5");
        tx.send(change(6)).unwrap();
        assert_eq!(next_session_id(&mut rx).await, "sess-6");

        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn exits_on_shutdown_and_on_closed_channel() {
        let (host, _rx, _tmp) = host_with_worker().await;

        let (_tx, brx) = broadcast::channel::<StatusChange>(2);
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(run_forwarder(brx, host.clone(), shutdown.clone()));
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("forwarder ignored shutdown")
            .unwrap();

        let (tx, brx) = broadcast::channel::<StatusChange>(2);
        let task = tokio::spawn(run_forwarder(brx, host, CancellationToken::new()));
        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("forwarder ignored closed channel")
            .unwrap();
    }
}
