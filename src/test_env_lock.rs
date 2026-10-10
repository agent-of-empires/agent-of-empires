//! Shared process-global environment exclusion for server and session fixtures.
//!
//! Unit tests and debug integration tests use this same mutex.
//! Same-thread nested guards retain the outer guard's exclusion.

use std::cell::Cell;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Guard callers choose arbitrary keys, so exclusion covers the whole environment.
static ENV_LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// True while *this* thread already owns [`ENV_LOCK`] through an outer
    /// guard.
    static ENV_LOCK_HELD: Cell<bool> = const { Cell::new(false) };
}
#[cfg(test)]
thread_local! {
    static LOCK_WAITING: std::cell::RefCell<Option<std::sync::mpsc::Sender<()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn observe_env_lock_contention(waiting: std::sync::mpsc::Sender<()>) {
    LOCK_WAITING.with_borrow_mut(|slot| *slot = Some(waiting));
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
            #[cfg(test)]
            LOCK_WAITING.with_borrow_mut(|waiting| {
                if let Some(waiting) = waiting.take() {
                    let _ = waiting.send(());
                }
            });
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

    /// The contention rendezvous proves both guard types use the same mutex.
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
