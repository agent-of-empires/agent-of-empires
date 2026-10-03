//! The one process-wide lock behind every test that mutates a process-global
//! environment variable.
//!
//! It lives here, and not in either guard's own `test_support` module, because
//! the two guards disagree on visibility: `session::test_support` is
//! `#[cfg(test)]`, while `server::test_support` is
//! `#[cfg(any(test, debug_assertions))]` and the integration test binaries
//! compile the library with debug assertions but without `cfg(test)`. A lock
//! that only one of them can reach is two locks in all but name, and a second
//! mutex for the same key defeats the structural guarantee documented on
//! [`crate::session::test_support::restore_or_remove`]: the process environment
//! is one slot per key, whatever the module boundaries say.
//!
//! Compiled under `any(test, debug_assertions)` because that is exactly the
//! set of builds in which either guard exists. Outside it, nothing here is
//! compiled and no test binary pays for it.
//!
//! The nesting rule is inherited from the original guard: a thread that
//! already holds the lock through an outer guard does not re-lock the
//! non-reentrant `Mutex` (that would deadlock the thread against itself), it
//! inherits the outer guard's exclusion and acquires nothing.
//! Same-thread nesting is race-free by construction, so skipping the re-lock
//! loses no safety.

use std::cell::Cell;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// The single lock for every environment key a test writes. There is no
/// per-key refinement on purpose: `EnvGuard` shims a caller-chosen key, so no
/// fixed list can bound which readers might race, and one lock is the only
/// thing that keeps the open set safe.
static ENV_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// True while *this* thread already owns [`ENV_LOCK`] through an outer
    /// guard.
    static ENV_LOCK_HELD: Cell<bool> = const { Cell::new(false) };
}

/// Acquire [`ENV_LOCK`] unless this thread already holds it, calling
/// `contended` when another guard had to be waited out. `Some` means this
/// thread took the lock and must pass `true` to [`release_env_lock`] on drop;
/// `None` means it is nested inside an outer guard and holds nothing.
pub(crate) fn acquire_env_lock(contended: impl FnOnce()) -> Option<MutexGuard<'static, ()>> {
    if ENV_LOCK_HELD.with(Cell::get) {
        return None;
    }
    let guard = match ENV_LOCK.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            contended();
            ENV_LOCK.lock().unwrap_or_else(PoisonError::into_inner)
        }
    };
    ENV_LOCK_HELD.with(|held| held.set(true));
    Some(guard)
}

/// Release the thread-local nesting mark for a guard that took the lock.
/// Pair with [`acquire_env_lock`], whose `Option` says which case this is.
pub(crate) fn release_env_lock(acquired: bool) {
    if acquired {
        ENV_LOCK_HELD.with(|held| held.set(false));
    }
}

/// Whether some other thread currently holds the lock. Used by the tests that
/// assert the exclusion is real rather than asserted.
#[cfg(test)]
pub(crate) fn env_lock_is_held_elsewhere() -> bool {
    matches!(
        ENV_LOCK.try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// One lock, two guard types, proven rather than asserted.
    ///
    /// The claim is narrow and it is checked with a rendezvous, not a sleep:
    /// `acquire_env_lock`'s contention callback fires *only* when the mutex
    /// this thread tried is held by someone else, so receiving it on the
    /// reader's thread while the holder is alive is direct evidence that the
    /// two guard types share one mutex. The reader also cannot have finished
    /// acquiring at that point, and it does finish once the holder drops.
    ///
    /// The first direction is the one that was broken: a `RuntimeEnvGuard`
    /// holding `XDG_CONFIG_HOME`, against a reader taking the lock the way
    /// every `session` test guard does. The guard is what makes this true, not
    /// a `#[serial_test::serial]` group, which is exactly what an unannotated
    /// `#[tokio::test]` relies on. `#[serial]` on the test itself
    /// only keeps the two directions in this module from writing the same
    /// environment key at each other.
    #[test]
    #[serial_test::serial]
    fn a_session_lock_taker_is_excluded_by_a_server_env_guard() {
        let base = tempfile::TempDir::new().expect("base");
        let (contended_tx, contended_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();

        let held = crate::server::test_support::RuntimeEnvGuard::set(base.path());
        let reader = std::thread::spawn(move || {
            let _guard = acquire_env_lock(move || {
                let _ = contended_tx.send(());
            });
            let _ = acquired_tx.send(());
        });

        contended_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the reader must find the lock held by the server guard");
        assert!(
            acquired_rx.try_recv().is_err(),
            "the reader cannot have acquired while the guard lives"
        );
        assert_eq!(
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            Some(base.path().as_os_str()),
            "the holder's value stands while the reader waits"
        );
        drop(held);
        acquired_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the reader proceeds once the guard is dropped");
        reader.join().expect("reader joined");
    }

    /// The symmetric direction: a `session` guard excludes a `server` one.
    #[test]
    #[serial_test::serial]
    fn a_server_lock_taker_is_excluded_by_a_session_env_guard() {
        let (contended_tx, contended_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();

        let held = crate::session::test_support::EnvGuard::read_lock();
        let reader = std::thread::spawn(move || {
            let _guard = crate::test_env_lock::acquire_env_lock(move || {
                let _ = contended_tx.send(());
            });
            let _ = acquired_tx.send(());
        });

        contended_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the reader must find the lock held by the session guard");
        assert!(
            acquired_rx.try_recv().is_err(),
            "the reader cannot have acquired while the guard lives"
        );
        drop(held);
        acquired_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the reader proceeds once the guard is dropped");
        reader.join().expect("reader joined");
    }
}
