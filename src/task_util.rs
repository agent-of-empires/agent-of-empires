//! Panic-aware task spawning: a dropped `JoinHandle` would otherwise lose the panic.

use std::future::Future;
use std::panic::AssertUnwindSafe;

use futures_util::FutureExt;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum PanicPolicy {
    Log,
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
