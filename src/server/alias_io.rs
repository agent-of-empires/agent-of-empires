//! Bounded isolation for filesystem work that follows profile aliases.
//!
//! A hung network mount blocks its syscall with no way to cancel it. Each
//! operation therefore runs on its own thread and callers wait only briefly.
//! At most one operation per alias and kind is in flight; while it is stuck,
//! callers get the last completed result without waiting.

use std::any::Any;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How long a caller holding the reload gate or runtime flight waits for a freshly started
/// operation. Only the first call of a hang episode pays it.
pub(crate) const WAIT: Duration = Duration::from_millis(250);

struct Slot {
    target: Option<PathBuf>,
    /// Target of the running worker, so a repoint is not blocked by a hung old one.
    in_flight: Option<Option<PathBuf>>,
    last: Option<Arc<dyn Any + Send + Sync>>,
}

type Key = (String, &'static str);

static SLOTS: LazyLock<Mutex<HashMap<Key, Slot>>> = LazyLock::new(Mutex::default);

fn slots() -> MutexGuard<'static, HashMap<Key, Slot>> {
    SLOTS.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) struct Sample<T> {
    /// The completed result, absent when none has ever completed for this alias target.
    pub value: Option<Arc<T>>,
    /// Whether `value` came from the operation this call started.
    pub fresh: bool,
}

/// Clears the in-flight mark when the worker finishes or panics.
struct Finish(Key, Option<PathBuf>);

impl Drop for Finish {
    fn drop(&mut self) {
        if let Some(slot) = slots().get_mut(&self.0) {
            if slot.in_flight.as_ref() == Some(&self.1) {
                slot.in_flight = None;
            }
        }
    }
}

fn last<T: Send + Sync + 'static>(slot: &Slot) -> Option<Arc<T>> {
    slot.last.clone().and_then(|value| value.downcast().ok())
}

/// Run `work` for `alias` off the caller's thread, waiting at most [`WAIT`].
pub(crate) fn sample<T>(
    alias: &str,
    kind: &'static str,
    work: impl FnOnce() -> T + Send + 'static,
) -> Sample<T>
where
    T: Send + Sync + 'static,
{
    // The link text lives in the local profiles directory, so reading it cannot hang.
    let target = crate::session::get_profile_dir_path(alias)
        .ok()
        .and_then(|path| std::fs::read_link(path).ok());
    let key: Key = (alias.to_owned(), kind);
    let rx = {
        let mut guard = slots();
        let slot = guard.entry(key.clone()).or_insert(Slot {
            target: None,
            in_flight: None,
            last: None,
        });
        if slot.target != target {
            slot.target.clone_from(&target);
            slot.last = None;
        }
        if slot.in_flight.as_ref() == Some(&target) {
            return Sample {
                value: last(slot),
                fresh: false,
            };
        }
        slot.in_flight = Some(target.clone());
        let (tx, rx) = mpsc::channel();
        let worker_key = key.clone();
        let spawned = std::thread::Builder::new()
            .name("aoe-alias-io".into())
            .spawn(move || {
                let finish = Finish(worker_key.clone(), target.clone());
                #[cfg(test)]
                held::block(&worker_key);
                let value = Arc::new(work());
                if let Some(slot) = slots().get_mut(&worker_key) {
                    if slot.target == target {
                        slot.last = Some(value.clone());
                    }
                }
                // Idle before any caller can observe the result.
                drop(finish);
                let _ = tx.send(value);
            });
        if spawned.is_err() {
            slot.in_flight = None;
            return Sample {
                value: last(slot),
                fresh: false,
            };
        }
        rx
    };
    match rx.recv_timeout(WAIT) {
        Ok(value) => Sample {
            value: Some(value),
            fresh: true,
        },
        Err(_) => Sample {
            value: slots().get(&key).and_then(last),
            fresh: false,
        },
    }
}

#[cfg(test)]
pub(crate) fn in_flight_for_test(alias: &str) -> usize {
    slots()
        .iter()
        .filter(|((name, _), slot)| name == alias && slot.in_flight.is_some())
        .count()
}

/// Parks a worker at its entry, standing in for a hung mount.
#[cfg(test)]
pub(crate) mod held {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Condvar;

    type Gate = Arc<(Mutex<bool>, Condvar)>;

    /// Entry signal, release gate, and whether a worker already took the hold.
    type Held = (mpsc::Sender<()>, Gate, Arc<AtomicBool>);

    static HOLDS: LazyLock<Mutex<HashMap<Key, Held>>> = LazyLock::new(Mutex::default);

    pub(super) fn block(key: &Key) {
        let held = HOLDS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .cloned();
        let Some((entered, gate, taken)) = held else {
            return;
        };
        if taken.swap(true, Ordering::SeqCst) {
            return;
        }
        let _ = entered.send(());
        let (open, signal) = &*gate;
        let mut open = open.lock().unwrap_or_else(PoisonError::into_inner);
        while !*open {
            open = signal.wait(open).unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub(crate) struct Hold {
        key: Key,
        entered: mpsc::Receiver<()>,
        gate: Gate,
    }

    impl Hold {
        pub(crate) fn install(alias: &str, kind: &'static str) -> Self {
            let key = (alias.to_owned(), kind);
            let (tx, entered) = mpsc::channel();
            let gate = Gate::default();
            HOLDS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key.clone(), (tx, gate.clone(), Arc::default()));
            Self { key, entered, gate }
        }

        pub(crate) fn wait_entered(&self) {
            self.entered
                .recv_timeout(Duration::from_secs(10))
                .expect("a worker never reached the held operation");
        }

        pub(crate) fn release(&self) {
            let (open, signal) = &*self.gate;
            *open.lock().unwrap_or_else(PoisonError::into_inner) = true;
            signal.notify_all();
        }
    }

    impl Drop for Hold {
        fn drop(&mut self) {
            self.release();
            HOLDS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&self.key);
        }
    }
}

#[cfg(test)]
pub(crate) fn wait_idle_for_test(alias: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while in_flight_for_test(alias) != 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "alias worker never finished"
        );
        std::thread::yield_now();
    }
}

#[cfg(test)]
mod tests {
    use super::held::Hold;
    use super::*;
    use crate::server::reload::{load_all_instances, selectable_profiles, LoadedInstances};
    use crate::session::{Instance, Storage};

    fn with_alias() -> (crate::session::test_support::AppDirGuard, tempfile::TempDir) {
        let home = crate::session::test_support::isolate_app_dir();
        Storage::new_unwatched("main")
            .unwrap()
            .update(|rows, _| {
                rows.push(Instance::new("local", "/tmp/local"));
                Ok(())
            })
            .unwrap();
        let external = tempfile::tempdir().unwrap();
        let root = crate::session::get_profile_dir_path("main")
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        std::os::unix::fs::symlink(external.path(), root.join("hung")).unwrap();
        Storage::open_unwatched("hung")
            .unwrap()
            .update(|rows, _| {
                rows.push(Instance::new("remote", "/tmp/remote"));
                Ok(())
            })
            .unwrap();
        (home, external)
    }

    fn load() -> LoadedInstances {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            tx.send(load_all_instances(
                &crate::file_watch::FileWatchService::noop(),
            ))
        });
        rx.recv_timeout(Duration::from_secs(10))
            .expect("a blocked alias must not block the load")
    }

    #[test]
    #[serial_test::serial]
    fn a_blocked_alias_read_stalls_neither_loads_nor_healthy_profiles() {
        let (_home, _external) = with_alias();
        let healthy = load();
        assert!(healthy.cache.health.unreadable_profiles.is_empty());
        assert_eq!(healthy.cache.alias_only_instances.len(), 1);

        let hold = Hold::install("hung", "sessions");
        let first = load();
        hold.wait_entered();
        let second = load();
        for blocked in [first, second] {
            assert_eq!(blocked.instances.len(), 1, "the local profile still loads");
            assert!(blocked.cache.health.unreadable_profiles.contains("hung"));
            assert_eq!(
                blocked.cache.alias_only_instances.len(),
                1,
                "the last completed alias rows are kept"
            );
        }
        assert_eq!(in_flight_for_test("hung"), 1, "no second worker starts");

        hold.release();
        wait_idle_for_test("hung");
        let recovered = load();
        assert!(recovered.cache.health.unreadable_profiles.is_empty());
        assert_eq!(recovered.cache.alias_only_instances.len(), 1);
    }

    #[test]
    #[serial_test::serial]
    fn repointing_an_alias_is_not_blocked_by_a_hung_worker_for_the_old_target() {
        let (_home, _external) = with_alias();
        let hold = Hold::install("hung", "sessions");
        load();
        hold.wait_entered();

        let healthy = tempfile::tempdir().unwrap();
        let link = crate::session::get_profile_dir_path("hung").unwrap();
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(healthy.path(), &link).unwrap();
        Storage::open_unwatched("hung")
            .unwrap()
            .update(|rows, _| {
                rows.push(Instance::new("repointed", "/tmp/repointed"));
                Ok(())
            })
            .unwrap();
        let repointed = load();
        assert!(repointed.cache.health.unreadable_profiles.is_empty());
        assert_eq!(repointed.cache.alias_only_instances.len(), 1);
        assert_eq!(repointed.cache.alias_only_instances[0].title, "repointed");

        hold.release();
        wait_idle_for_test("hung");
    }

    #[test]
    #[serial_test::serial]
    fn a_blocked_identity_stat_keeps_the_accepted_identity() {
        let (_home, _external) = with_alias();
        let accepted = selectable_profiles().unwrap();
        assert!(accepted.iter().any(|profile| profile.name == "hung"));

        let hold = Hold::install("hung", "identity");
        let blocked = selectable_profiles().unwrap();
        hold.wait_entered();
        assert_eq!(
            blocked
                .iter()
                .map(|p| (&p.name, p.identity))
                .collect::<Vec<_>>(),
            accepted
                .iter()
                .map(|p| (&p.name, p.identity))
                .collect::<Vec<_>>()
        );
        hold.release();
        wait_idle_for_test("hung");
    }
}
