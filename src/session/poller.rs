//! Adaptive polling interval and command channel for session monitoring

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::mpsc::RecvTimeoutError;
use std::thread::JoinHandle;
use std::time::Duration;

/// Global count of active session-id poller threads for budget enforcement
static ACTIVE_POLLER_COUNT: AtomicU32 = AtomicU32::new(0);

/// Default ceiling on concurrent session-id poller threads.
///
/// Each session that loses its poller stops refreshing the agent session id
/// shown in its TUI row, so this cap doubles as a "how many concurrent
/// sessions can keep their identity live" budget.
pub const DEFAULT_SESSION_ID_POLLER_MAX_THREADS: u32 = 50;

/// Resolved cap, settable at runtime from the TUI Settings panel.
///
/// Read with `Ordering::Relaxed` inside the CAS loop in `try_acquire` so each
/// retry observes the latest cap. The counter's own CAS uses `SeqCst` and is
/// the authoritative gate against imbalance. There is no ordering dependency
/// between this cap and any other memory location, so `Release`/`Acquire`
/// would add a fence with zero semantic benefit.
static SESSION_ID_POLLER_MAX_THREADS: AtomicU32 =
    AtomicU32::new(DEFAULT_SESSION_ID_POLLER_MAX_THREADS);

/// Push a new cap into the atomic. Clamped to ≥1 because zero would
/// silently disable polling for every future session.
pub fn set_session_id_poller_max_threads(n: u32) {
    SESSION_ID_POLLER_MAX_THREADS.store(n.max(1), Ordering::Relaxed);
}

/// Read the currently effective cap. Used by the budget-exhausted warn
/// message and by tests.
pub fn session_id_poller_max_threads() -> u32 {
    SESSION_ID_POLLER_MAX_THREADS.load(Ordering::Relaxed)
}

/// RAII guard that decrements `ACTIVE_POLLER_COUNT` on drop.
///
/// Ensures the counter is always decremented even if the poller thread panics,
/// preventing permanent budget exhaustion.
struct PollerCountGuard;

impl PollerCountGuard {
    /// Atomically check the budget and increment. Returns `None` if at capacity.
    fn try_acquire() -> Option<Self> {
        let mut current = ACTIVE_POLLER_COUNT.load(Ordering::SeqCst);
        loop {
            let cap = SESSION_ID_POLLER_MAX_THREADS.load(Ordering::Relaxed);
            if current >= cap {
                return None;
            }
            match ACTIVE_POLLER_COUNT.compare_exchange_weak(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(Self),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for PollerCountGuard {
    fn drop(&mut self) {
        ACTIVE_POLLER_COUNT.fetch_sub(1, Ordering::SeqCst);
    }
}

const POLL_INITIAL_INTERVAL: Duration = Duration::from_secs(2);
const POLL_MAX_INTERVAL: Duration = Duration::from_secs(60);
const POLL_BACKOFF_FACTOR: f64 = 1.5;
const POLL_STABLE_THRESHOLD: u32 = 3;

/// Manages adaptive polling intervals that back off when no changes are detected
#[derive(Debug)]
struct AdaptiveInterval {
    initial: Duration,
    current: Duration,
    max: Duration,
    backoff_factor: f64,
    stable_threshold: u32,
    stable_count: u32,
}

impl AdaptiveInterval {
    /// Create a new adaptive interval with custom parameters
    fn new(initial: Duration, max: Duration, backoff_factor: f64, stable_threshold: u32) -> Self {
        Self {
            initial,
            current: initial,
            max,
            backoff_factor,
            stable_threshold,
            stable_count: 0,
        }
    }

    fn current(&self) -> Duration {
        self.current
    }

    /// Record that no changes were detected; increases backoff if threshold is reached.
    ///
    /// Uses `Duration::from_secs_f64` for sub-second precision in the backoff calculation
    /// (e.g., 2.0s * 1.5 = 3.0s, 3.0s * 1.5 = 4.5s).
    fn record_no_change(&mut self) {
        self.stable_count += 1;
        if self.stable_count >= self.stable_threshold {
            let next_secs = self.current.as_secs_f64() * self.backoff_factor;
            let next_duration = Duration::from_secs_f64(next_secs);
            self.current = next_duration.min(self.max);
            self.stable_count = 0;
        }
    }

    /// Record that a change was detected; reset to initial interval
    fn record_change(&mut self) {
        self.current = self.initial;
        self.stable_count = 0;
    }
}

/// Command sent to the session poller thread
#[derive(Debug, Clone, Copy)]
enum PollCommand {
    /// Stop the poller thread
    Stop,
}

/// Manages polling thread lifecycle and inter-thread communication via mpsc channels.
///
/// # Cleanup
///
/// Cleanup is performed explicitly via `stop()` rather than `Drop` because
/// `Drop` alone cannot guarantee prompt shutdown. The poller thread holds
/// the `cmd_rx` receiver; when `SessionPoller` drops, the corresponding
/// `cmd_tx` sender is dropped too and `recv_timeout` returns `Disconnected`
/// immediately -- so in the common case the thread exits promptly.
///
/// `stop()` sends an explicit `PollCommand::Stop` and joins the thread,
/// providing a deterministic shutdown path for callers like `Instance::kill`
/// and `Instance::restart_with_size`.
pub struct SessionPoller {
    session_name: String,
    cmd_tx: mpsc::Sender<PollCommand>,
    cmd_rx: Option<mpsc::Receiver<PollCommand>>,
    result_tx: mpsc::Sender<(String, String)>,
    result_rx: Option<mpsc::Receiver<(String, String)>>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for SessionPoller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPoller")
            .field("session_name", &self.session_name)
            .field("running", &self.handle.is_some())
            .finish()
    }
}

impl SessionPoller {
    /// Create a new poller (does not start the thread)
    pub fn new(session_name: String) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        Self {
            session_name,
            cmd_tx,
            cmd_rx: Some(cmd_rx),
            result_tx,
            result_rx: Some(result_rx),
            handle: None,
        }
    }

    /// Start the polling thread with the given callbacks.
    ///
    /// Returns `true` if the thread was successfully spawned, `false` if the
    /// poller was already started, the thread budget was exhausted, or spawning failed.
    pub fn start(
        &mut self,
        instance_id: String,
        poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static>,
        on_change: Box<dyn Fn(&str) + Send + 'static>,
        initial_known: Option<String>,
    ) -> bool {
        let cmd_rx = match self.cmd_rx.take() {
            Some(rx) => rx,
            None => {
                tracing::warn!(target: "session.create",
                    "Poller for {} already started, ignoring duplicate start",
                    instance_id
                );
                return false;
            }
        };

        let _guard = match PollerCountGuard::try_acquire() {
            Some(g) => g,
            None => {
                tracing::warn!(target: "session.create",
                    "Session-id poller budget exhausted ({}/{}), skipping poller for {}; \
                     raise the cap from the TUI Settings panel \
                     (Session > Max Session-ID Poller Threads) before creating new sessions",
                    ACTIVE_POLLER_COUNT.load(Ordering::SeqCst),
                    SESSION_ID_POLLER_MAX_THREADS.load(Ordering::Relaxed),
                    instance_id,
                );
                self.cmd_rx = Some(cmd_rx);
                return false;
            }
        };

        let session_name = self.session_name.clone();
        let thread_label = format!("aoe-poller/{}", instance_id);
        let result_tx = self.result_tx.clone();

        let handle = std::thread::Builder::new()
            .name(thread_label.clone())
            .stack_size(128 * 1024)
            .spawn(move || {
                // Rebind so the closure captures `_guard` and the counter only
                // decrements when the thread exits (including via panic). Without
                // this, `move` would not capture an unreferenced binding and the
                // counter would decrement as soon as `start()` returned.
                let _guard = _guard;

                let mut last_known = initial_known;
                let mut interval = AdaptiveInterval::new(
                    POLL_INITIAL_INTERVAL,
                    POLL_MAX_INTERVAL,
                    POLL_BACKOFF_FACTOR,
                    POLL_STABLE_THRESHOLD,
                );

                let report = |new_id_opt: Option<String>,
                              last: &mut Option<String>,
                              interval: &mut AdaptiveInterval| {
                    match new_id_opt {
                        Some(new_id) if last.as_deref() != Some(&new_id) => {
                            on_change(&new_id);
                            let _ = result_tx.send((instance_id.clone(), new_id.clone()));
                            *last = Some(new_id);
                            interval.record_change();
                        }
                        _ => interval.record_no_change(),
                    }
                };

                // Immediate first poll (e.g. pre-existing sessions loaded from disk).
                report(poll_fn(), &mut last_known, &mut interval);

                loop {
                    match cmd_rx.recv_timeout(interval.current()) {
                        Ok(PollCommand::Stop) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => break,
                    }

                    if crate::tmux::utils::is_pane_dead(&session_name) {
                        tracing::info!(target: "session.create", "Pane dead for {}, stopping poller", session_name);
                        break;
                    }

                    report(poll_fn(), &mut last_known, &mut interval);
                }
            });

        match handle {
            Ok(h) => {
                self.handle = Some(h);
                true
            }
            Err(e) => {
                tracing::warn!(target: "session.create", "Failed to spawn poller thread {}: {}", thread_label, e);
                // Restore channels to allow retrying spawn
                let (cmd_tx, cmd_rx) = mpsc::channel();
                self.cmd_tx = cmd_tx;
                self.cmd_rx = Some(cmd_rx);
                let (result_tx, result_rx) = mpsc::channel();
                self.result_tx = result_tx;
                self.result_rx = Some(result_rx);
                false
            }
        }
    }

    /// Drain a pending session ID update, if any. Returns `(instance_id, session_id)`.
    pub fn try_recv_session_update(&self) -> Option<(String, String)> {
        self.result_rx.as_ref()?.try_recv().ok()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn inject_test_update(&self, instance_id: &str, session_id: &str) {
        self.result_tx
            .send((instance_id.to_string(), session_id.to_string()))
            .expect("inject_test_update: result channel disconnected");
    }

    /// Stop the poller thread and wait for it to finish
    pub fn stop(&mut self) {
        let _ = self.cmd_tx.send(PollCommand::Stop);
        if let Some(handle) = self.handle.take() {
            if let Err(e) = handle.join() {
                tracing::warn!(target: "session.create", "Poller thread panicked: {:?}", e);
            }
        }
    }

    /// Check if the poller thread is running
    pub fn is_running(&self) -> bool {
        match &self.handle {
            Some(handle) => !handle.is_finished(),
            None => false,
        }
    }
}

impl Default for SessionPoller {
    fn default() -> Self {
        Self::new("default".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::{Arc, Mutex};

    /// Restores `SESSION_ID_POLLER_MAX_THREADS` to its original value on drop,
    /// even if the test panics. Mirrors the `VibeHomeGuard` / `TmuxCleanup`
    /// pattern used elsewhere in the test suite.
    struct CapRestorer(u32);
    impl Drop for CapRestorer {
        fn drop(&mut self) {
            set_session_id_poller_max_threads(self.0);
        }
    }

    /// Restores `ACTIVE_POLLER_COUNT` to its original value on drop.
    struct CountRestorer(u32);
    impl Drop for CountRestorer {
        fn drop(&mut self) {
            ACTIVE_POLLER_COUNT.store(self.0, Ordering::SeqCst);
        }
    }

    #[test]
    fn test_adaptive_interval_initial() {
        let interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        assert_eq!(interval.current(), Duration::from_secs(2));
    }

    #[test]
    fn test_adaptive_interval_record_no_change_increments_count() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        assert_eq!(interval.stable_count, 0);
        interval.record_no_change();
        assert_eq!(interval.stable_count, 1);
        interval.record_no_change();
        assert_eq!(interval.stable_count, 2);
    }

    #[test]
    fn test_adaptive_interval_backoff_at_threshold() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        interval.record_no_change();
        interval.record_no_change();
        interval.record_no_change();
        // After 3 calls: 2 * 1.5 = 3 seconds
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_adaptive_interval_multiple_backoffs() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        // First backoff: 2 -> 3
        for _ in 0..3 {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));

        // Second backoff: 3 -> 4.5 (with sub-second precision)
        for _ in 0..3 {
            interval.record_no_change();
        }
        let expected_secs = 3.0 * 1.5;
        assert_eq!(interval.current(), Duration::from_secs_f64(expected_secs));
    }

    #[test]
    fn test_adaptive_interval_respects_max() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            1, // threshold of 1 for faster test
        );
        interval.record_no_change(); // 2 * 1.5 = 3.0
        interval.record_no_change(); // 3.0 * 1.5 = 4.5
        interval.record_no_change(); // 4.5 * 1.5 = 6.75
        interval.record_no_change(); // 6.75 * 1.5 = 10.125
        interval.record_no_change(); // 10.125 * 1.5 = 15.1875
        interval.record_no_change(); // 15.1875 * 1.5 = 22.78125
        interval.record_no_change(); // 22.78125 * 1.5 = 34.171875
        interval.record_no_change(); // 34.171875 * 1.5 = 51.2578125
        interval.record_no_change(); // 51.2578125 * 1.5 = 76.88671875 > 60, capped at 60
        assert!(interval.current() <= Duration::from_secs(60));
    }

    #[test]
    fn test_adaptive_interval_record_change_resets() {
        let mut interval =
            AdaptiveInterval::new(Duration::from_secs(2), Duration::from_secs(60), 1.5, 3);
        for _ in 0..3 {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));

        interval.record_change();
        assert_eq!(interval.current(), Duration::from_secs(2));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_session_poller_new() {
        let poller = SessionPoller::new("test-session".to_string());
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_stop_when_no_thread() {
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.stop(); // Should not panic
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_double_stop_safe() {
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.stop();
        poller.stop(); // Should not panic
        assert!(!poller.is_running());
    }

    #[test]
    fn test_session_poller_drop_is_clean() {
        let poller = SessionPoller::new("test-session".to_string());
        drop(poller); // Should not panic
    }

    #[test]
    fn test_adaptive_interval_with_constants() {
        let mut interval = AdaptiveInterval::new(
            POLL_INITIAL_INTERVAL,
            POLL_MAX_INTERVAL,
            POLL_BACKOFF_FACTOR,
            POLL_STABLE_THRESHOLD,
        );
        assert_eq!(interval.current(), Duration::from_secs(2));
        for _ in 0..POLL_STABLE_THRESHOLD {
            interval.record_no_change();
        }
        assert_eq!(interval.current(), Duration::from_secs(3));
    }

    #[test]
    fn test_poller_detects_change() {
        let call_count = Arc::new(Mutex::new(0u32));
        let call_count_clone = call_count.clone();

        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = Box::new(move || {
            let mut count = call_count_clone.lock().unwrap();
            *count += 1;
            if *count <= 1 {
                Some("id-1".to_string())
            } else {
                Some("id-2".to_string())
            }
        });

        let changed_ids: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let changed_ids_clone = changed_ids.clone();

        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |id: &str| {
            changed_ids_clone.lock().unwrap().push(id.to_string());
        });

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-change".to_string(),
            poll_fn,
            on_change,
            Some("id-1".to_string()),
        );

        // Wait for the adaptive interval (2s initial) to fire at least once
        std::thread::sleep(Duration::from_millis(2500));
        poller.stop();

        let ids = changed_ids.lock().unwrap();
        assert!(
            ids.contains(&"id-2".to_string()),
            "on_change should have been called with id-2, got: {:?}",
            *ids
        );
        assert!(
            !ids.contains(&"id-1".to_string()),
            "on_change should NOT have been called with id-1 (initial known)"
        );
    }

    #[test]
    #[serial]
    fn test_set_session_id_poller_max_threads_clamps_zero() {
        let _restore = CapRestorer(session_id_poller_max_threads());
        set_session_id_poller_max_threads(0);
        assert_eq!(
            session_id_poller_max_threads(),
            1,
            "zero must be clamped to 1 to avoid silently disabling polling"
        );
    }

    #[test]
    #[serial]
    fn test_set_session_id_poller_max_threads_roundtrip() {
        let _restore = CapRestorer(session_id_poller_max_threads());
        set_session_id_poller_max_threads(42);
        assert_eq!(session_id_poller_max_threads(), 42);
    }

    #[test]
    #[serial]
    fn test_thread_budget_cap() {
        let _restore_count = CountRestorer(ACTIVE_POLLER_COUNT.load(Ordering::SeqCst));
        let _restore_cap = CapRestorer(session_id_poller_max_threads());
        ACTIVE_POLLER_COUNT.store(session_id_poller_max_threads(), Ordering::SeqCst);

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-budget".to_string(),
            Box::new(|| Some("id".to_string())),
            Box::new(|_| {}),
            None,
        );

        assert!(
            !poller.is_running(),
            "poller should not have spawned when budget exhausted"
        );
        assert!(
            poller.cmd_rx.is_some(),
            "cmd_rx should be returned when budget exhausted"
        );
    }

    #[test]
    #[serial]
    fn test_poller_is_running_after_start() {
        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-running".to_string(),
            Box::new(|| {
                std::thread::sleep(Duration::from_millis(10));
                Some("id".to_string())
            }),
            Box::new(|_| {}),
            None,
        );

        assert!(poller.is_running(), "poller should be running after start");
        poller.stop();
    }

    #[test]
    #[serial]
    fn test_poller_cleanup_decrements_counter() {
        let entered = Arc::new(Mutex::new(false));
        let entered_clone = entered.clone();

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start(
            "test-cleanup".to_string(),
            Box::new(move || {
                *entered_clone.lock().unwrap() = true;
                Some("id".to_string())
            }),
            Box::new(|_| {}),
            None,
        );

        // Wait for the immediate first poll to run
        std::thread::sleep(Duration::from_millis(100));

        let count_before_stop = ACTIVE_POLLER_COUNT.load(Ordering::SeqCst);
        poller.stop();
        let count_after_stop = ACTIVE_POLLER_COUNT.load(Ordering::SeqCst);

        assert!(
            count_after_stop < count_before_stop,
            "counter should decrement after stop (before_stop={}, after_stop={})",
            count_before_stop,
            count_after_stop
        );
        assert!(*entered.lock().unwrap(), "poll_fn should have been called");
    }

    #[test]
    fn test_interval_exact_at_threshold() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            POLL_STABLE_THRESHOLD,
        );

        for _ in 0..POLL_STABLE_THRESHOLD {
            interval.record_no_change();
        }
        // 2 * 1.5 = 3
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 0);

        interval.record_no_change();
        assert_eq!(interval.current(), Duration::from_secs(3));
        assert_eq!(interval.stable_count, 1);
    }

    #[test]
    fn test_interval_max_clamping_precision() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            POLL_MAX_INTERVAL,
            POLL_BACKOFF_FACTOR,
            POLL_STABLE_THRESHOLD,
        );

        for _ in 0..1000 {
            interval.record_no_change();
            assert!(
                interval.current() <= POLL_MAX_INTERVAL,
                "interval {} exceeded max {}",
                interval.current().as_secs(),
                POLL_MAX_INTERVAL.as_secs()
            );
        }
        assert_eq!(interval.current(), POLL_MAX_INTERVAL);
    }

    #[test]
    fn test_interval_change_mid_backoff() {
        let mut interval = AdaptiveInterval::new(
            Duration::from_secs(2),
            Duration::from_secs(60),
            1.5,
            POLL_STABLE_THRESHOLD,
        );

        interval.record_no_change();
        interval.record_no_change();
        assert_eq!(interval.stable_count, 2);
        assert_eq!(interval.current(), Duration::from_secs(2));

        interval.record_change();
        assert_eq!(interval.current(), Duration::from_secs(2));
        assert_eq!(interval.stable_count, 0);
    }

    #[test]
    fn test_poller_starts_polling_immediately() {
        let poll_count = Arc::new(Mutex::new(0u32));
        let poll_count_clone = poll_count.clone();

        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = Box::new(move || {
            let mut count = poll_count_clone.lock().unwrap();
            *count += 1;
            Some("ses_polled".to_string())
        });

        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(|_| {});

        let mut poller = SessionPoller::new("test-session".to_string());
        poller.start("test-immediate".to_string(), poll_fn, on_change, None);

        std::thread::sleep(Duration::from_millis(100));

        let count = *poll_count.lock().unwrap();
        assert!(
            count > 0,
            "poller should have started polling immediately (count={})",
            count
        );

        poller.stop();
    }
}
