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
    /// The running worker was launched from another manifest than the one the request was
    /// chosen from, so it is not delivered.
    Superseded,
    Rejected,
}

/// The worker a request is for: only one launched from this manifest receives it, so a
/// request chosen under one manifest and grant never reaches a worker started under another.
#[derive(Debug, Clone, Copy)]
pub struct WorkerRef<'a> {
    pub plugin_id: &'a str,
    pub manifest_hash: &'a str,
}

impl std::fmt::Display for WorkerRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoWorker => f.write_str("no running worker"),
            Self::Timeout => f.write_str("timed out"),
            Self::WorkerGone => f.write_str("worker exited"),
            Self::Superseded => f.write_str("worker was launched from a different manifest"),
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
struct State {
    entries: HashMap<String, Entry>,
    /// The generation each plugin's output ended at: no reply can arrive for it any more, so a
    /// request registered after the sweep fails at once instead of waiting out its timeout.
    closed: HashMap<String, u64>,
}

#[derive(Default)]
pub struct PendingRequests {
    next: AtomicU64,
    /// One lock for both maps, so a registration is either swept by an exit or refused by it.
    state: Mutex<State>,
}

impl PendingRequests {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `generation` is the supervisor id, which an in-place respawn keeps, so an exit fails
    /// every request of that supervisor.
    pub fn register(
        &self,
        plugin_id: &str,
        generation: u64,
    ) -> (String, oneshot::Receiver<Result<Value, String>>) {
        let id = format!("h-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let (tx, rx) = oneshot::channel();
        let mut state = self.state();
        if state.closed.get(plugin_id) == Some(&generation) {
            return (id, rx);
        }
        state.entries.insert(
            id.clone(),
            Entry {
                plugin_id: plugin_id.to_string(),
                generation,
                tx,
            },
        );
        (id, rx)
    }

    /// Runs `send` only while `id` is still registered. Retirement takes the same lock, so a
    /// frame is either queued before the worker is retired or not at all.
    pub fn send_while_registered(&self, id: &str, send: impl FnOnce() -> bool) -> bool {
        let state = self.state();
        state.entries.contains_key(id) && send()
    }

    /// Only the worker instance the request was queued to can answer it: a superseded worker
    /// whose stdout is still read must not answer a request registered for its replacement.
    pub fn resolve(&self, plugin_id: &str, generation: u64, reply: WorkerReply) -> bool {
        let mut state = self.state();
        let entries = &mut state.entries;
        if entries
            .get(&reply.id)
            .is_none_or(|e| e.plugin_id != plugin_id || e.generation != generation)
        {
            return false;
        }
        let entry = entries.remove(&reply.id).expect("checked above");
        entry.tx.send(reply.outcome).is_ok()
    }

    pub fn cancel(&self, id: &str) {
        self.state().entries.remove(id);
    }

    /// Cancels `id` when the guard drops; a no-op once the request was resolved.
    pub fn forget_on_drop<'a>(&'a self, id: &'a str) -> ForgetOnDrop<'a> {
        ForgetOnDrop { pending: self, id }
    }

    /// Drops the senders so every waiter on this worker instance wakes with a closed channel.
    /// Later registrations for it fail at once until [`Self::reopen`].
    pub fn fail_worker(&self, plugin_id: &str, generation: u64) {
        let mut state = self.state();
        // Supervisor ids only grow, so a late teardown of an older worker keeps a newer marker.
        let ended = state
            .closed
            .entry(plugin_id.to_string())
            .or_insert(generation);
        *ended = (*ended).max(generation);
        state
            .entries
            .retain(|_, e| e.plugin_id != plugin_id || e.generation != generation);
    }

    /// A respawn under the same supervisor accepts requests again.
    pub fn reopen(&self, plugin_id: &str, generation: u64) {
        let mut state = self.state();
        if state.closed.get(plugin_id) == Some(&generation) {
            state.closed.remove(plugin_id);
        }
    }

    #[cfg(test)]
    pub fn waiting(&self, plugin_id: &str) -> usize {
        self.state()
            .entries
            .values()
            .filter(|e| e.plugin_id == plugin_id)
            .count()
    }
}

pub struct ForgetOnDrop<'a> {
    pending: &'a PendingRequests,
    id: &'a str,
}

impl Drop for ForgetOnDrop<'_> {
    fn drop(&mut self) {
        self.pending.cancel(self.id);
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

    /// Still waiting: `try_recv` also errors on a closed channel, which is not the same.
    fn is_open(rx: &mut oneshot::Receiver<Result<Value, String>>) -> bool {
        matches!(rx.try_recv(), Err(oneshot::error::TryRecvError::Empty))
    }

    #[tokio::test]
    async fn resolve_delivers_to_the_registered_receiver() {
        let pending = PendingRequests::new();
        let (id, rx) = pending.register("acme.a", 1);
        assert!(pending.resolve("acme.a", 1, reply(&id)));
        assert_eq!(rx.await.unwrap().unwrap(), json!({"ok": true}));
    }

    /// Request ids are sequential, so a worker could guess the next one: only the instance the
    /// request was queued to may answer it.
    #[tokio::test]
    async fn a_reply_from_another_worker_instance_is_dropped() {
        let pending = PendingRequests::new();
        let (id, mut rx) = pending.register("acme.a", 2);
        assert!(
            !pending.resolve("acme.a", 1, reply(&id)),
            "a superseded instance must not answer"
        );
        assert!(is_open(&mut rx), "the request is still waiting");
        assert!(pending.resolve("acme.a", 2, reply(&id)));
        assert_eq!(rx.await.unwrap().unwrap(), json!({"ok": true}));
    }

    #[tokio::test]
    async fn another_plugin_cannot_answer_a_request_it_does_not_own() {
        let pending = PendingRequests::new();
        let (id, mut rx) = pending.register("acme.a", 1);
        assert!(
            !pending.resolve("acme.evil", 1, reply(&id)),
            "forged reply must be ignored"
        );
        assert!(is_open(&mut rx), "receiver must still be waiting");
        assert!(pending.resolve("acme.a", 1, reply(&id)));
    }

    #[tokio::test]
    async fn fail_worker_wakes_only_that_worker_instances_waiters() {
        let pending = PendingRequests::new();
        let (_a, rx_a) = pending.register("acme.a", 1);
        let (b, mut rx_b) = pending.register("acme.b", 1);
        let (_a2, mut rx_a2) = pending.register("acme.a", 2);
        pending.fail_worker("acme.a", 1);
        assert!(rx_a.await.is_err(), "sender dropped means worker gone");
        assert!(is_open(&mut rx_a2), "a newer instance is untouched");
        assert!(is_open(&mut rx_b));
        assert!(pending.resolve("acme.b", 1, reply(&b)));
    }

    #[tokio::test]
    async fn a_request_registered_after_the_worker_ended_fails_at_once() {
        let pending = PendingRequests::new();
        pending.fail_worker("acme.a", 1);
        let (_id, rx) = pending.register("acme.a", 1);
        let ended = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("a request registered after the end must not wait");
        assert!(ended.is_err(), "no reply can arrive for an ended worker");
        let (_other, mut rx_other) = pending.register("acme.a", 2);
        assert!(is_open(&mut rx_other), "a newer instance is untouched");
        pending.reopen("acme.a", 1);
        let (id, mut rx_again) = pending.register("acme.a", 1);
        assert!(is_open(&mut rx_again), "a respawn accepts requests again");
        assert!(pending.resolve("acme.a", 1, reply(&id)));
    }

    #[tokio::test]
    async fn a_late_teardown_of_an_older_worker_keeps_the_newer_closure() {
        let pending = PendingRequests::new();
        pending.fail_worker("acme.a", 2);
        pending.fail_worker("acme.a", 1);
        let (_id, rx) = pending.register("acme.a", 2);
        assert!(rx.await.is_err(), "the newer worker stays closed");
    }

    /// Whatever the interleaving, a registration is either swept by the exit or refused by it.
    #[test]
    fn a_registration_racing_the_exit_is_never_left_waiting() {
        for _ in 0..2000 {
            let pending = PendingRequests::new();
            let mut rx = std::thread::scope(|s| {
                let ender = s.spawn(|| pending.fail_worker("acme.a", 1));
                let (_id, rx) = pending.register("acme.a", 1);
                ender.join().unwrap();
                rx
            });
            assert!(!is_open(&mut rx), "a waiter outlived the exit sweep");
        }
    }

    #[tokio::test]
    async fn cancel_forgets_the_request() {
        let pending = PendingRequests::new();
        let (id, _rx) = pending.register("acme.a", 1);
        pending.cancel(&id);
        assert!(!pending.resolve("acme.a", 1, reply(&id)));
    }
}
