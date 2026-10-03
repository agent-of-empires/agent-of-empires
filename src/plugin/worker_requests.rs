//! Correlates host-initiated JSON-RPC requests with the worker replies that answer them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::oneshot;

use crate::plugin::protocol::WorkerReply;

#[derive(Debug)]
pub enum WorkerRequestError {
    NoWorker,
    Timeout,
    WorkerGone,
    Rejected,
}

impl std::fmt::Display for WorkerRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoWorker => f.write_str("no running worker"),
            Self::Timeout => f.write_str("timed out"),
            Self::WorkerGone => f.write_str("worker exited"),
            Self::Rejected => f.write_str("worker rejected the request"),
        }
    }
}

struct Entry {
    plugin_id: String,
    generation: u64,
    tx: oneshot::Sender<Result<Value, String>>,
}

#[derive(Default)]
pub struct PendingRequests {
    next: AtomicU64,
    entries: Mutex<HashMap<String, Entry>>,
}

impl PendingRequests {
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `generation` identifies the worker instance the request is sent to.
    pub fn register(
        &self,
        plugin_id: &str,
        generation: u64,
    ) -> (String, oneshot::Receiver<Result<Value, String>>) {
        let id = format!("h-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let (tx, rx) = oneshot::channel();
        self.entries().insert(
            id.clone(),
            Entry {
                plugin_id: plugin_id.to_string(),
                generation,
                tx,
            },
        );
        (id, rx)
    }

    /// Only the plugin that owns the request can answer it.
    pub fn resolve(&self, plugin_id: &str, reply: WorkerReply) -> bool {
        let mut entries = self.entries();
        if entries
            .get(&reply.id)
            .is_none_or(|e| e.plugin_id != plugin_id)
        {
            return false;
        }
        let entry = entries.remove(&reply.id).expect("checked above");
        entry.tx.send(reply.outcome).is_ok()
    }

    pub fn cancel(&self, id: &str) {
        self.entries().remove(id);
    }

    /// Drops the senders so every waiter on this worker instance wakes with a closed channel.
    pub fn fail_worker(&self, plugin_id: &str, generation: u64) {
        self.entries()
            .retain(|_, e| e.plugin_id != plugin_id || e.generation != generation);
    }

    #[cfg(test)]
    pub fn waiting(&self, plugin_id: &str) -> usize {
        self.entries()
            .values()
            .filter(|e| e.plugin_id == plugin_id)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::protocol::WorkerReply;
    use serde_json::json;

    fn reply(id: &str) -> WorkerReply {
        WorkerReply {
            id: id.to_string(),
            outcome: Ok(json!({"ok": true})),
        }
    }

    #[tokio::test]
    async fn resolve_delivers_to_the_registered_receiver() {
        let pending = PendingRequests::new();
        let (id, rx) = pending.register("acme.a", 1);
        assert!(pending.resolve("acme.a", reply(&id)));
        assert_eq!(rx.await.unwrap().unwrap(), json!({"ok": true}));
    }

    #[tokio::test]
    async fn another_plugin_cannot_answer_a_request_it_does_not_own() {
        let pending = PendingRequests::new();
        let (id, mut rx) = pending.register("acme.a", 1);
        assert!(
            !pending.resolve("acme.evil", reply(&id)),
            "forged reply must be ignored"
        );
        assert!(rx.try_recv().is_err(), "receiver must still be waiting");
        assert!(pending.resolve("acme.a", reply(&id)));
    }

    #[tokio::test]
    async fn fail_worker_wakes_only_that_worker_instances_waiters() {
        let pending = PendingRequests::new();
        let (_a, rx_a) = pending.register("acme.a", 1);
        let (b, mut rx_b) = pending.register("acme.b", 1);
        let (_a2, mut rx_a2) = pending.register("acme.a", 2);
        pending.fail_worker("acme.a", 1);
        assert!(rx_a.await.is_err(), "sender dropped means worker gone");
        assert!(rx_a2.try_recv().is_err(), "a newer instance is untouched");
        assert!(rx_b.try_recv().is_err());
        assert!(pending.resolve("acme.b", reply(&b)));
    }

    #[tokio::test]
    async fn cancel_forgets_the_request() {
        let pending = PendingRequests::new();
        let (id, _rx) = pending.register("acme.a", 1);
        pending.cancel(&id);
        assert!(!pending.resolve("acme.a", reply(&id)));
    }
}
