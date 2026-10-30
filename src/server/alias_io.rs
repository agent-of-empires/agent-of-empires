//! Bounded isolation for filesystem work that follows profile aliases.
//!
//! A hung network mount blocks its syscall with no way to cancel it. Each
//! operation therefore runs on its own thread and callers wait only briefly.
//! At most one operation per alias and kind is in flight; while it is stuck,
//! callers get the last completed result without waiting.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{mpsc, Arc, LazyLock, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How long a caller holding the reload gate or runtime flight waits for a freshly started
/// operation. Only the first call of a hang episode pays it.
pub(crate) const WAIT: Duration = Duration::from_millis(250);

struct Slot {
    target: Option<PathBuf>,
    /// Targets with a worker still running; a hung one stays registered across repoints.
    in_flight: HashSet<Option<PathBuf>>,
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
            slot.in_flight.remove(&self.1);
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
            in_flight: HashSet::new(),
            last: None,
        });
        if slot.target != target {
            slot.target.clone_from(&target);
            slot.last = None;
        }
        if slot.in_flight.contains(&target) {
            return Sample {
                value: last(slot),
                fresh: false,
            };
        }
        slot.in_flight.insert(target.clone());
        #[cfg(test)]
        held::count_start(&key);
        let worker_target = target.clone();
        let (tx, rx) = mpsc::channel();
        let worker_key = key.clone();
        let spawned = std::thread::Builder::new()
            .name("aoe-alias-io".into())
            .spawn(move || {
                let finish = Finish(worker_key.clone(), worker_target.clone());
                #[cfg(test)]
                held::block(&worker_key);
                let value = Arc::new(work());
                if let Some(slot) = slots().get_mut(&worker_key) {
                    if slot.target == worker_target {
                        slot.last = Some(value.clone());
                    }
                }
                // Idle before any caller can observe the result.
                drop(finish);
                let _ = tx.send(value);
            });
        if spawned.is_err() {
            slot.in_flight.remove(&target);
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
        .filter(|((name, _), _)| name == alias)
        .map(|(_, slot)| slot.in_flight.len())
        .sum()
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
    static STARTED: LazyLock<Mutex<HashMap<Key, usize>>> = LazyLock::new(Mutex::default);

    pub(super) fn count_start(key: &Key) {
        *STARTED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(key.clone())
            .or_default() += 1;
    }

    /// Workers started for this alias and kind in this process.
    pub(crate) fn started_for_test(alias: &str, kind: &'static str) -> usize {
        STARTED
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&(alias.to_owned(), kind))
            .copied()
            .unwrap_or(0)
    }

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
    use super::held::{started_for_test, Hold};
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
        std::fs::write(
            external.path().join("sessions.json"),
            serde_json::to_vec(&vec![Instance::new("remote", "/tmp/remote")]).unwrap(),
        )
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
    fn same_path_replacement_never_relabels_cached_rows_as_the_new_store() {
        let _home = crate::session::test_support::isolate_app_dir();
        let external = tempfile::tempdir().unwrap();
        let target = external.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let old = Instance::new("old store", "/tmp/old");
        std::fs::write(
            target.join("sessions.json"),
            serde_json::to_vec(&vec![old]).unwrap(),
        )
        .unwrap();
        let link = crate::session::get_profile_dir_path("same-path").unwrap();
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let primed = load();
        assert_eq!(primed.cache.alias_only_instances[0].title, "old store");

        std::fs::rename(&target, external.path().join("retired")).unwrap();
        std::fs::create_dir(&target).unwrap();
        let replacement = Instance::new("new store", "/tmp/new");
        std::fs::write(
            target.join("sessions.json"),
            serde_json::to_vec(&vec![replacement]).unwrap(),
        )
        .unwrap();
        let hold = Hold::install("same-path", "sessions");
        let pending = load();
        hold.wait_entered();
        let identity = |loaded: &LoadedInstances| {
            loaded
                .cache
                .inventory
                .iter()
                .find(|profile| profile.name == "same-path")
                .unwrap()
                .identity
        };
        assert_ne!(identity(&pending), identity(&primed));
        assert!(pending
            .cache
            .health
            .unreadable_profiles
            .contains("same-path"));
        assert!(
            pending.cache.alias_only_instances.is_empty(),
            "old rows must not acquire a replacement store's identity"
        );

        hold.release();
        wait_idle_for_test("same-path");
        let accepted = load();
        assert!(accepted.cache.health.unreadable_profiles.is_empty());
        assert_eq!(
            accepted
                .cache
                .alias_only_instances
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            vec!["new store"]
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_blocked_alias_does_not_hold_up_a_reload_gated_mutation() {
        let (_home, _external) = with_alias();
        let row = Storage::new_unwatched("main")
            .unwrap()
            .load()
            .unwrap()
            .remove(0);
        let id = row.id.clone();
        let state = crate::server::test_support::build_test_app_state(vec![row]);
        let service = &state.session_service;
        load();

        let hold = Hold::install("hung", "sessions");
        let gate = service.disk_reload_guard().await;
        let watch = state.file_watch.clone();
        let gated_load = tokio::task::spawn_blocking(move || load_all_instances(&watch));
        let gated = tokio::time::timeout(Duration::from_secs(10), gated_load)
            .await
            .expect("the gated load must not wait on the blocked alias")
            .unwrap();
        hold.wait_entered();
        drop(gate);
        assert!(gated.cache.health.unreadable_profiles.contains("hung"));

        let _submission = service.prompt_submission(&id).await;
        tokio::time::timeout(
            Duration::from_secs(10),
            service.enqueue_prompt(&id, "A".into(), "first".into(), vec![], None, None),
        )
        .await
        .expect("a persisted mutation must not wait on the blocked alias")
        .unwrap();
        assert_eq!(
            in_flight_for_test("hung"),
            1,
            "the alias worker is still parked"
        );

        hold.release();
        wait_idle_for_test("hung");
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
        std::fs::write(
            healthy.path().join("sessions.json"),
            serde_json::to_vec(&vec![Instance::new("repointed", "/tmp/repointed")]).unwrap(),
        )
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
    fn returning_to_a_blocked_target_reuses_its_worker() {
        let (_home, external) = with_alias();
        load();
        let link = crate::session::get_profile_dir_path("hung").unwrap();
        let repoint = |to: &std::path::Path| {
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(to, &link).unwrap();
        };
        let before = started_for_test("hung", "sessions");

        let hold = Hold::install("hung", "sessions");
        load();
        hold.wait_entered();
        let other = tempfile::tempdir().unwrap();
        repoint(other.path());
        let fresh = load();
        assert!(fresh.cache.health.unreadable_profiles.is_empty());
        assert_eq!(started_for_test("hung", "sessions") - before, 2);

        repoint(external.path());
        let returned = load();
        assert!(returned.cache.health.unreadable_profiles.contains("hung"));
        assert_eq!(
            started_for_test("hung", "sessions") - before,
            2,
            "the blocked worker for the original target is reused"
        );
        assert_eq!(in_flight_for_test("hung"), 1);

        hold.release();
        wait_idle_for_test("hung");
        assert!(load().cache.health.unreadable_profiles.is_empty());
        assert_eq!(started_for_test("hung", "sessions") - before, 3);
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
