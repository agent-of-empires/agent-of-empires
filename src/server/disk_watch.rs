//! Per-profile disk watches: wiring them up, tearing them down, and the
//! reload the watcher consumer drives.

use crate::file_watch::{FileMatcher, WatchSpec};
use std::sync::Arc;

use super::reload::{load_all_instances, reload_state_instances_from_disk};
use super::state::{AppState, DiskWatchEntry, StatusSource};
use super::structured_repair::live_structured_worker_records;

/// Build a per-profile disk-watch entry: register a `subscribe_channel`
/// against `<profile_dir>/{sessions,groups}.json` and spawn a forwarder
/// task that drains the receiver into `state.disk_changed`. Returns
/// `None` when the profile dir cannot be resolved or `subscribe_channel`
/// fails; both cases are logged. Polling stays canonical, so a `None`
/// here degrades propagation to the 2s tick rather than failing closed.
pub(super) async fn build_disk_watch_entry(
    state: &Arc<AppState>,
    profile: &str,
) -> Option<DiskWatchEntry> {
    let profile_dir = match crate::session::get_profile_dir_path(profile) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "server.file_watch",
                profile = %profile,
                error = %e,
                "could not resolve profile dir; live propagation disabled"
            );
            return None;
        }
    };
    let sessions_path = profile_dir.join("sessions.json");
    let groups_path = profile_dir.join("groups.json");
    let spec = WatchSpec {
        dir: profile_dir,
        matcher: FileMatcher::AnyOf(vec![sessions_path, groups_path]),
        debounce: Some(std::time::Duration::from_millis(75)),
    };
    let (mut rx, handle) = match state.file_watch.subscribe_channel(spec, 16) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(
                target: "server.file_watch",
                profile = %profile,
                error = %e,
                "subscribe_channel failed; live propagation disabled for this profile"
            );
            return None;
        }
    };
    let signal = state.disk_changed.clone();
    let profile = profile.to_owned();
    let shutdown = state.shutdown.clone();
    let join = crate::task_util::spawn_supervised(
        "server.disk_watch.forwarder",
        crate::task_util::PanicPolicy::Log,
        async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    ev = rx.recv() => match ev {
                        Some(_) => signal.notify_one(),
                        None => break,
                    }
                }
            }
            tracing::debug!(
                target: "server.file_watch",
                profile = %profile,
                "disk-watch forwarder exit"
            );
        },
    );
    // Test-only barrier: when armed, signals `entered` after the
    // subscription is built and parks on `release`. The enclosing
    // `add_profile_disk_watch` / `rename_profile_disk_watch` hold `disk_watch_handles` through the
    // build, so a task parked here also holds that lock; this lets a
    // controlled-ordering test drive a concurrent same-profile remove
    // against a known mid-build state.
    #[cfg(any(test, feature = "test-support"))]
    {
        let armed = disk_watch_build_barrier().lock().unwrap().clone();
        if let Some(barrier) = armed {
            barrier.entered.notify_one();
            barrier.release.notified().await;
        }
    }
    Some(DiskWatchEntry {
        handle,
        forwarder: join.abort_handle(),
    })
}

/// Test-only barrier installed inside `build_disk_watch_entry` to
/// deterministically pin a building task at a known point so a
/// concurrent same-profile remove can run against it. Not compiled
/// into production builds.
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct DiskWatchBuildBarrier {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
    #[cfg(test)]
    pub(crate) armed: tokio::sync::Notify,
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) fn disk_watch_build_barrier(
) -> &'static std::sync::Mutex<Option<Arc<DiskWatchBuildBarrier>>> {
    static BARRIER: std::sync::OnceLock<std::sync::Mutex<Option<Arc<DiskWatchBuildBarrier>>>> =
        std::sync::OnceLock::new();
    BARRIER.get_or_init(|| std::sync::Mutex::new(None))
}

/// RAII guard for the test barrier slot: installs on construction and
/// clears unconditionally on drop, so a panicking test cannot leave
/// the slot armed for subsequent tests in the same process.
#[cfg(test)]
pub(crate) struct DiskWatchBuildBarrierGuard;

#[cfg(test)]
impl DiskWatchBuildBarrierGuard {
    pub(crate) fn install(barrier: Arc<DiskWatchBuildBarrier>) -> Self {
        *disk_watch_build_barrier().lock().unwrap() = Some(barrier);
        Self
    }
}

#[cfg(test)]
impl Drop for DiskWatchBuildBarrierGuard {
    fn drop(&mut self) {
        *disk_watch_build_barrier().lock().unwrap() = None;
    }
}

/// Drop the subscription handle FIRST so the dispatcher stops queuing
/// events on this id, then abort the forwarder; aborting first would
/// race a buffered `try_send`. Centralized so every teardown path keeps
/// the same canonical order.
pub(super) fn drop_disk_watch_entry(entry: DiskWatchEntry) {
    let DiskWatchEntry { handle, forwarder } = entry;
    drop(handle);
    forwarder.abort();
}

/// Install a disk-watch subscription for `profile` under one critical
/// section. If a prior entry exists for the same name, it is replaced
/// (drop handle, abort forwarder, then install the new entry).
///
/// Holding `disk_watch_handles` across `build_disk_watch_entry` is the
/// linearisation point: a concurrent `remove_profile_disk_watch` for
/// the same name cannot interleave between "subscription created" and
/// "entry installed" and silently leave a stale watcher behind for a
/// profile that was just removed.
pub(crate) async fn add_profile_disk_watch(state: &Arc<AppState>, profile: &str) {
    let mut handles = state.disk_watch_handles.lock().await;
    let Some(entry) = build_disk_watch_entry(state, profile).await else {
        return;
    };
    if let Some(prior) = handles.remove(profile) {
        drop_disk_watch_entry(prior);
    }
    handles.insert(profile.to_owned(), entry);
    tracing::debug!(
        target: "server.file_watch",
        profile = %profile,
        op = "add",
        "disk-watch subscription registered"
    );
}

/// Remove the disk-watch subscription for `profile` (no-op if absent).
pub(crate) async fn remove_profile_disk_watch(state: &Arc<AppState>, profile: &str) {
    let mut handles = state.disk_watch_handles.lock().await;
    if let Some(entry) = handles.remove(profile) {
        drop_disk_watch_entry(entry);
        tracing::debug!(
            target: "server.file_watch",
            profile = %profile,
            op = "remove",
            "disk-watch subscription removed"
        );
    }
}

/// Swap the disk-watch subscription from `old` to `new` under one
/// critical section. Concurrent same-name add/remove cannot interleave
/// between the two halves; this is concurrent-atomic, not
/// failure-atomic. On `build_disk_watch_entry` failure the `old` entry
/// is still removed because the production caller (rename_profile) has
/// already moved the on-disk directory and the old kernel watch points
/// at a path that no longer exists.
pub(crate) async fn rename_profile_disk_watch(state: &Arc<AppState>, old: &str, new: &str) {
    if old == new {
        return;
    }
    let mut handles = state.disk_watch_handles.lock().await;
    if let Some(entry) = handles.remove(old) {
        drop_disk_watch_entry(entry);
    }
    let Some(entry) = build_disk_watch_entry(state, new).await else {
        return;
    };
    if let Some(prior) = handles.remove(new) {
        drop_disk_watch_entry(prior);
    }
    handles.insert(new.to_owned(), entry);
    tracing::debug!(
        target: "server.file_watch",
        old = %old,
        new = %new,
        op = "rename",
        "disk-watch subscription renamed"
    );
}

/// Wire up disk-watch subscriptions for every currently-active profile.
/// Called during startup before request serving begins so the initial
/// watcher set is in place before any handler mutates storage. Per-profile
/// `subscribe_channel` errors are logged and skipped; polling stays
/// canonical so propagation degrades to the 2s tick rather than failing
/// closed. Emits one bootstrap wake at the end so any write that landed
/// while we were walking the profile list is reconciled immediately once
/// the consumer begins awaiting `disk_changed`.
pub(crate) async fn init_disk_watch_subscriptions(state: Arc<AppState>) {
    init_disk_watch_subscriptions_inner(state, |_: &str| {}, false).await;
}

/// Test-only variant that runs `hook` after each profile's subscription
/// is installed, so a test can drive disk writes between iterations to
/// exercise the bootstrap reconciliation path.
#[cfg(test)]
pub(super) async fn init_disk_watch_subscriptions_with_hook<F>(state: Arc<AppState>, hook: F)
where
    F: FnMut(&str) + Send,
{
    init_disk_watch_subscriptions_inner(state, hook, true).await;
}

pub(super) async fn init_disk_watch_subscriptions_inner<F>(
    state: Arc<AppState>,
    mut hook: F,
    with_hook: bool,
) where
    F: FnMut(&str) + Send,
{
    let profiles = crate::session::list_profiles().unwrap_or_default();
    let count = profiles.len();
    for profile in &profiles {
        add_profile_disk_watch(&state, profile).await;
        hook(profile);
    }
    state.disk_changed.notify_one();
    let suffix = if with_hook { " (with hook)" } else { "" };
    tracing::info!(
        target: "server.file_watch",
        profiles_count = count,
        "disk-watch subscriptions initialized{suffix}",
    );
}

/// Background task: reload `state.instances` from disk on every wake of
/// `state.disk_changed`. Mirrors `status_poll_loop`'s lock-acquisition
/// pattern but does NOT touch tmux or `state.status_tx`. Polling stays
/// canonical; this task is pure latency reduction.
pub(super) async fn disk_watcher_consumer(state: Arc<AppState>) {
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = state.disk_changed.notified() => {}
        }
        let started = std::time::Instant::now();
        // Invariant 8: read before the disk read, so a delete committing
        // during it invalidates this snapshot.
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let file_watch_for_load = state.file_watch.clone();
        let loaded = match tokio::task::spawn_blocking(move || {
            load_all_instances(&file_watch_for_load)
                .map(|fresh| (fresh, live_structured_worker_records()))
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                tracing::warn!(
                    target: "server.file_watch",
                    error = %e,
                    "disk reload failed"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    target: "server.file_watch",
                    error = %e,
                    "spawn_blocking joined with error"
                );
                continue;
            }
        };
        let (fresh, live_worker_records) = loaded;
        let count = fresh.len();
        reload_state_instances_from_disk(
            &state,
            fresh,
            live_worker_records,
            StatusSource::DiskOnly,
            read_epoch,
        )
        .await;
        tracing::trace!(
            target: "server.file_watch",
            latency_us = started.elapsed().as_micros() as u64,
            instance_count = count,
            "disk reload completed"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::file_watch::FileWatchService;
    use crate::server::test_support;
    use crate::session::Instance;

    #[tokio::test]
    #[serial_test::serial]
    async fn init_disk_watch_subscriptions_bootstraps_one_reload_after_wiring() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let storage = crate::session::Storage::new_unwatched("startup-gap").expect("storage");
        storage
            .update(|instances, _groups| {
                *instances = vec![Instance::new("seed", "/tmp/seed")];
                Ok(())
            })
            .expect("seed write");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        let wake = {
            let signal = state.disk_changed.clone();
            tokio::spawn(async move {
                tokio::time::timeout(std::time::Duration::from_secs(2), signal.notified()).await
            })
        };

        init_disk_watch_subscriptions(state.clone()).await;

        let woke = wake.await.expect("join");
        assert!(
            woke.is_ok(),
            "startup wiring must bootstrap one disk_changed wake after subscriptions are installed"
        );
        assert_eq!(
            state.file_watch.subscriber_count(),
            1,
            "startup wiring must leave exactly one live subscription for the single profile"
        );
    }

    // Concurrent same-profile rewires must converge to a single
    // consistent map entry and matching live subscription count. The
    // unified helper holds `disk_watch_handles` through the full
    // teardown-then-install transition, so the lock-acquisition order
    // alone decides which call wins; an interleaved unsubscribe and
    // subscribe across two callers cannot leave a half-state where the
    // map and the dispatcher disagree.
    #[tokio::test]
    #[serial_test::serial]
    async fn add_remove_profile_disk_watch_serializes_concurrent_add_and_remove() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }
        let _ = crate::session::get_profile_dir("rewire-race").expect("profile dir");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        let mut joins = Vec::new();
        for i in 0..50 {
            let s = state.clone();
            joins.push(tokio::spawn(async move {
                if i % 2 == 0 {
                    add_profile_disk_watch(&s, "rewire-race").await;
                } else {
                    remove_profile_disk_watch(&s, "rewire-race").await;
                }
            }));
        }
        for j in joins {
            j.await.expect("join");
        }

        let count = test_support::disk_watch_handle_count(&state).await;
        assert!(
            count <= 1,
            "concurrent rewires must not leak duplicate entries (got {count})"
        );
        let live_subs = state.file_watch.subscriber_count();
        assert_eq!(
            live_subs, count,
            "live subscriptions must equal map entries; mismatch indicates a leaked or orphaned entry"
        );

        add_profile_disk_watch(&state, "rewire-race").await;
        assert_eq!(
            test_support::disk_watch_handle_count(&state).await,
            1,
            "deterministic add must produce exactly one entry"
        );
        assert_eq!(state.file_watch.subscriber_count(), 1);

        remove_profile_disk_watch(&state, "rewire-race").await;
        assert_eq!(test_support::disk_watch_handle_count(&state).await, 0);
        assert_eq!(state.file_watch.subscriber_count(), 0);
    }

    // Concurrent same-profile add and remove must converge to the
    // last-completed call's intent. The barrier inside
    // `build_disk_watch_entry` lets the test pin task A mid-build
    // while A still holds `disk_watch_handles`, so B's remove blocks
    // until A finishes installing. Once A releases the lock, B's
    // remove wins because it ran strictly after A's install: the
    // final map is empty. If `disk_watch_handles` were not held
    // across the build, B could acquire the lock during A's parked
    // window, observe an empty map, and let A install a stale entry
    // on resume.
    #[tokio::test]
    #[serial_test::serial]
    async fn add_profile_disk_watch_resists_resurrection_under_concurrent_remove() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }
        let _ = crate::session::get_profile_dir("race-fix").expect("profile dir");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        let barrier = Arc::new(DiskWatchBuildBarrier {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
            armed: tokio::sync::Notify::new(),
        });
        let _barrier_guard = DiskWatchBuildBarrierGuard::install(barrier.clone());

        let s_a = state.clone();
        let task_a = tokio::spawn(async move {
            add_profile_disk_watch(&s_a, "race-fix").await;
        });

        // Wait deterministically until A is parked inside the
        // barrier. No fixed sleep: the `entered` notification is
        // sent strictly after `subscribe_channel` returns and the
        // forwarder is spawned, which is the build-vs-install
        // boundary the test wants to exercise.
        barrier.entered.notified().await;

        let s_b = state.clone();
        let barrier_b = barrier.clone();
        let task_b = tokio::spawn(async move {
            // Signal "B is about to call remove" so the test can
            // proceed to release A without a fixed-time sleep. The
            // notify lands one executor tick before B's `lock().await`
            // registers as a waiter; A still holds the lock so B
            // parks behind A regardless of relative scheduling.
            barrier_b.armed.notify_one();
            remove_profile_disk_watch(&s_b, "race-fix").await;
        });

        // Deterministic happens-before for B's lock attempt: replaces
        // the prior bounded sleep that flaked on heavily-loaded CI.
        barrier.armed.notified().await;
        tokio::task::yield_now().await;

        // Release A; it finishes building, installs the entry, and
        // releases the lock. B then acquires and removes.
        barrier.release.notify_one();

        task_a.await.expect("join A");
        task_b.await.expect("join B");

        let count = test_support::disk_watch_handle_count(&state).await;
        let live_subs = state.file_watch.subscriber_count();
        assert_eq!(
            count, 0,
            "B's remove must observe A's installed entry and tear it down. \
             A non-zero count here means a removed profile was resurrected by \
             an interleaved subscribe."
        );
        assert_eq!(
            live_subs, 0,
            "live subscription count must match the empty handle map; mismatch \
             indicates a leaked subscriber from a resurrected entry."
        );
    }

    // Writes that land during init's per-profile iteration, before
    // their profile has been subscribed, must still be reconciled
    // once init returns. The hook fires after each install; the
    // test uses it to seed a write to a profile not yet reached by
    // the loop. The bootstrap notify at init's end wakes the
    // consumer, which then loads from disk and surfaces both the
    // pre-init seed and the mid-iteration seed.
    #[tokio::test]
    #[serial_test::serial]
    async fn init_disk_watch_subscriptions_reconciles_writes_landing_during_iteration() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let storage_p1 = crate::session::Storage::new_unwatched("init-gap-p1").expect("p1");
        storage_p1
            .update(|i, _| {
                *i = vec![Instance::new("p1-pre-init", "/tmp/p1-pre")];
                Ok(())
            })
            .expect("seed p1");
        let _ = crate::session::get_profile_dir("init-gap-p2").expect("p2 dir");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        init_disk_watch_subscriptions_with_hook(state.clone(), |profile| {
            if profile == "init-gap-p1" {
                // Write to P2 at the precise moment when P1 has just
                // been subscribed but P2 has not. The watcher path
                // cannot deliver this event for P2 (no subscription
                // exists yet); only the bootstrap wake plus a reload
                // can reconcile it.
                let storage = crate::session::Storage::new_unwatched("init-gap-p2").expect("p2");
                storage
                    .update(|i, _| {
                        *i = vec![Instance::new("p2-mid-init", "/tmp/p2-mid")];
                        Ok(())
                    })
                    .expect("seed p2");
            }
        })
        .await;

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.disk_changed.notified(),
        )
        .await
        .expect("bootstrap wake must fire after init returns");

        // Invariant 8: capture the epoch BEFORE the disk read, the order every
        // production caller uses.
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let file_watch = state.file_watch.clone();
        let fresh = tokio::task::spawn_blocking(move || load_all_instances(&file_watch))
            .await
            .expect("join")
            .expect("load");
        reload_state_instances_from_disk(
            &state,
            fresh,
            Vec::new(),
            StatusSource::DiskOnly,
            read_epoch,
        )
        .await;

        let instances = state.instances.read().await;
        let titles: Vec<&str> = instances.iter().map(|i| i.title.as_str()).collect();
        assert!(
            titles.contains(&"p1-pre-init"),
            "writes BEFORE init started must be reconciled; titles: {:?}",
            titles
        );
        assert!(
            titles.contains(&"p2-mid-init"),
            "writes DURING init's iteration (the gap window) must be reconciled by the bootstrap wake; titles: {:?}",
            titles
        );
    }

    // A reloader reads `sessions.json`, then does slow work (the poll loop's
    // tmux scrape, which blocks for seconds when the tmux server is
    // unreachable) before folding the snapshot into `state.instances`. A
    // delete committing inside that window used to come straight back, because
    // the merge rebuilds `state.instances` wholesale from the stale snapshot.
    // Observed as a live Playwright failure: DELETE returned 200, the sidebar
    // row went away, and the very next `GET /api/sessions` listed the session
    // again with its pre-delete status. See invariant 8.
    #[tokio::test]
    async fn a_reload_predating_a_delete_does_not_resurrect_the_removed_row() {
        let doomed = Instance::new("doomed", "/tmp/doomed");
        let survivor = Instance::new("survivor", "/tmp/survivor");
        // What a reloader read from disk before the delete landed.
        let stale_snapshot = vec![doomed.clone(), survivor.clone()];
        let read_epoch = 0;

        let state = test_support::build_test_app_state(vec![survivor.clone()]);
        // The delete already committed: `doomed` is gone from memory, and the
        // epoch moved to say so.
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        reload_state_instances_from_disk(
            &state,
            stale_snapshot,
            Vec::new(),
            StatusSource::DiskOnly,
            read_epoch,
        )
        .await;

        let titles: Vec<String> = state
            .instances
            .read()
            .await
            .iter()
            .map(|i| i.title.clone())
            .collect();
        assert!(
            !titles.contains(&"doomed".to_string()),
            "a deleted session must not come back from a pre-delete snapshot: {titles:?}"
        );
        assert_eq!(titles, vec!["survivor".to_string()]);
    }

    // The mirror image of the delete case, and the reason `mutation_epoch` is
    // not named `delete_epoch`. `create_session` persists the new row to
    // `sessions.json` and only then upserts it into `state.instances`
    // (`upsert_instance`). A poll tick whose disk read STARTED before that
    // persist carries a `fresh` without the new row, and since the merge
    // rebuilds `state.instances` exclusively from `fresh`, the wholesale
    // replace drops the session the create just inserted. `GET /api/sessions`
    // then loses it until the next tick re-reads disk 2s later. Observed as a
    // live Playwright failure: `wizard-scratch-launch` polled until the
    // session appeared, and the very next `GET /api/sessions` returned `[]`.
    #[tokio::test]
    async fn a_reload_predating_a_create_does_not_drop_the_new_row() {
        let existing = Instance::new("existing", "/tmp/existing");
        let created = Instance::new("created", "/tmp/created");
        // What a reloader read from disk before the create persisted.
        let stale_snapshot = vec![existing.clone()];

        // The create already committed: `created` is in memory (and on disk),
        // and the epoch moved to say so.
        let state = test_support::build_test_app_state(vec![existing.clone(), created.clone()]);
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        reload_state_instances_from_disk(
            &state,
            stale_snapshot.clone(),
            Vec::new(),
            StatusSource::DiskOnly,
            0,
        )
        .await;

        let titles: Vec<String> = state
            .instances
            .read()
            .await
            .iter()
            .map(|i| i.title.clone())
            .collect();
        assert!(
            titles.contains(&"created".to_string()),
            "a created session must survive a pre-create snapshot: {titles:?}"
        );

        // And the converse, which is what the create path's bump buys: hand
        // the same stale snapshot a matching epoch, as if the create had never
        // bumped, and the row is gone. This is the pre-fix behaviour, pinned so
        // the bump cannot be dropped as redundant.
        let unbumped = test_support::build_test_app_state(vec![existing.clone(), created.clone()]);
        reload_state_instances_from_disk(
            &unbumped,
            stale_snapshot,
            Vec::new(),
            StatusSource::DiskOnly,
            0,
        )
        .await;
        let titles: Vec<String> = unbumped
            .instances
            .read()
            .await
            .iter()
            .map(|i| i.title.clone())
            .collect();
        assert_eq!(
            titles,
            vec!["existing".to_string()],
            "without the epoch bump the reload drops the created row"
        );
    }

    // The epoch comparison has to be atomic against the delete, not merely
    // ordered by `SeqCst`. Comparing before taking the `instances` write lock
    // leaves a check-then-act race: a reload passes the check, parks on the
    // lock, a delete takes the lock and removes the row, and the reload then
    // wakes and writes its stale snapshot over the removal.
    //
    // This drives that exact interleaving. The test holds the write lock so
    // the spawned reload is guaranteed to be parked on it, bumps the epoch
    // while it waits (standing in for the delete), then releases. On a
    // current-thread runtime the ordering is deterministic, not timing
    // dependent.
    #[tokio::test]
    async fn a_reload_parked_on_the_instances_lock_still_sees_a_delete_that_won_the_race() {
        let doomed = Instance::new("doomed", "/tmp/doomed");
        let survivor = Instance::new("survivor", "/tmp/survivor");
        let stale_snapshot = vec![doomed.clone(), survivor.clone()];

        let state = test_support::build_test_app_state(vec![survivor.clone()]);
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);

        // Hold the lock the reload needs, so it cannot get past it.
        let guard = state.instances.write().await;

        let reload_state = Arc::clone(&state);
        let reload = tokio::spawn(async move {
            reload_state_instances_from_disk(
                &reload_state,
                stale_snapshot,
                Vec::new(),
                StatusSource::DiskOnly,
                read_epoch,
            )
            .await;
        });

        // Let the spawned task run until it parks on the write lock.
        tokio::task::yield_now().await;

        // The delete commits while the reload is parked: row out, epoch up.
        // Both happen before the lock is released, mirroring the real purge.
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(guard);

        reload.await.expect("reload task");

        let titles: Vec<String> = state
            .instances
            .read()
            .await
            .iter()
            .map(|i| i.title.clone())
            .collect();
        assert!(
            !titles.contains(&"doomed".to_string()),
            "a reload that was already waiting on the lock when the delete landed must still drop: {titles:?}"
        );
    }

    // The guard must not be implemented by dropping ids missing from the prior
    // in-memory map: that is also how a session created by another process
    // (the CLI, a peer daemon) legitimately arrives. Only a moved epoch means
    // "this snapshot predates a delete".
    #[tokio::test]
    async fn a_reload_at_the_current_epoch_still_adopts_externally_created_rows() {
        let known = Instance::new("known", "/tmp/known");
        let created_elsewhere = Instance::new("created-elsewhere", "/tmp/elsewhere");
        let state = test_support::build_test_app_state(vec![known.clone()]);
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);

        reload_state_instances_from_disk(
            &state,
            vec![known, created_elsewhere],
            Vec::new(),
            StatusSource::DiskOnly,
            read_epoch,
        )
        .await;

        let titles: Vec<String> = state
            .instances
            .read()
            .await
            .iter()
            .map(|i| i.title.clone())
            .collect();
        assert!(
            titles.contains(&"created-elsewhere".to_string()),
            "a row this daemon has never seen must still be adopted: {titles:?}"
        );
    }

    // Bootstrap correctness here has two requirements: subscriptions must be
    // installed before the first wake, and writes that land before init
    // returns must still be visible after the consumer reloads disk state.
    #[tokio::test]
    #[serial_test::serial]
    async fn bootstrap_wake_makes_pre_init_writes_reachable_via_reload() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let storage = crate::session::Storage::new_unwatched("startup-reload").expect("storage");
        storage
            .update(|instances, _groups| {
                *instances = vec![Instance::new("pre-init", "/tmp/pre-init")];
                Ok(())
            })
            .expect("seed write");

        let state = test_support::build_test_app_state(Vec::new());
        let live = FileWatchService::new().expect("live svc");
        let mut state_mut = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state_mut.file_watch = live;
        let state = Arc::new(state_mut);

        init_disk_watch_subscriptions(state.clone()).await;

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            state.disk_changed.notified(),
        )
        .await
        .expect("bootstrap wake must fire after init returns");

        // Invariant 8: capture the epoch BEFORE the disk read, the order every
        // production caller uses.
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let file_watch = state.file_watch.clone();
        let fresh = tokio::task::spawn_blocking(move || load_all_instances(&file_watch))
            .await
            .expect("join")
            .expect("load");
        reload_state_instances_from_disk(
            &state,
            fresh,
            Vec::new(),
            StatusSource::DiskOnly,
            read_epoch,
        )
        .await;

        let instances = state.instances.read().await;
        assert!(
            instances.iter().any(|i| i.title == "pre-init"),
            "bootstrap wake plus reload must surface writes that landed before init returned"
        );
    }
}
