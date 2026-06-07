//! Tokio task helpers: panic aware spawning plus tracing span
//! propagation for long lived background work.
//!
//! `tokio::spawn` swallows panics into a `JoinError` returned from
//! `JoinHandle::await`. For long lived tasks whose `JoinHandle` is
//! dropped (every fire and forget `tokio::spawn(...)` in this crate),
//! the panic message is lost. `spawn_supervised` wraps the future in
//! `catch_unwind` so a panic surfaces through `tracing::error!` with
//! a static task name attached, which makes `hmp logs` answer
//! "why did the cleanup task stop running" instead of dropping the
//! signal on the floor.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use tokio::task::JoinHandle;

/// What to do when the wrapped future panics. Production code logs;
/// tests surface so failures are loud.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PanicPolicy {
    /// Log the panic via `tracing::error!(target = "task.panic", ...)`
    /// and let the runtime continue. The default for daemon tasks.
    Log,
    /// Re-raise the panic after logging it. Use in tests so a
    /// panicking task fails the test instead of disappearing.
    Surface,
}

/// Spawn a future on the tokio runtime with panic logging. The
/// returned `JoinHandle<()>` is equivalent to `tokio::spawn` for
/// futures with `Output = ()`; callers that drop it still get the
/// diagnostic via `tracing::error!` on panic, which is the whole
/// point.
///
/// Pair with `tracing::Instrument` at the call site to propagate
/// a span across the spawn boundary:
///
/// ```ignore
/// use tracing::Instrument;
/// let span = tracing::info_span!("my.task", session_id = %id);
/// spawn_supervised("my.task", PanicPolicy::Log, work.instrument(span));
/// ```
pub fn spawn_supervised<F>(name: &'static str, policy: PanicPolicy, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        match AssertUnwindSafe(fut).catch_unwind().await {
            Ok(()) => {}
            Err(payload) => {
                let msg = panic_payload_string(&*payload);
                tracing::error!(
                    target: "task.panic",
                    task = name,
                    message = %msg,
                    "background task panicked",
                );
                if matches!(policy, PanicPolicy::Surface) {
                    std::panic::resume_unwind(payload);
                }
            }
        }
    })
}

fn panic_payload_string(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non string panic payload>".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn normal_completion_runs_to_end() {
        let touched = Arc::new(AtomicBool::new(false));
        let t = touched.clone();
        let handle = spawn_supervised("test.normal", PanicPolicy::Log, async move {
            t.store(true, Ordering::SeqCst);
        });
        handle.await.expect("task should join cleanly");
        assert!(touched.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn panic_in_log_policy_does_not_propagate() {
        // The wrapper catches the panic; the caller's join completes
        // Ok(()) and the runtime survives. We can't assert on the
        // tracing output without a custom subscriber, but absence
        // of a JoinError is the contract we depend on.
        let handle = spawn_supervised("test.panic.log", PanicPolicy::Log, async {
            panic!("intentional panic for test");
        });
        let result = handle.await;
        assert!(
            result.is_ok(),
            "Log policy must convert panic into a clean join: {result:?}",
        );
    }

    #[tokio::test]
    async fn panic_in_surface_policy_propagates_join_error() {
        let handle = spawn_supervised("test.panic.surface", PanicPolicy::Surface, async {
            panic!("intentional panic that must surface");
        });
        let result = handle.await;
        assert!(
            result.is_err(),
            "Surface policy must propagate panic as JoinError: {result:?}",
        );
        assert!(
            result.unwrap_err().is_panic(),
            "JoinError must report is_panic() = true",
        );
    }
}
