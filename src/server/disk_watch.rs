//! Per-profile disk watches.

use crate::file_watch::{FileMatcher, WatchSpec};
use std::sync::Arc;

use super::reload::{load_all_instances, reload_state_instances_from_disk};
use super::state::{AppState, DiskWatchEntry, StatusSource};
use super::structured_repair::live_structured_worker_records;

/// Build a per-profile disk-watch entry.
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
    // Test-only barrier.
    #[cfg(any(test, debug_assertions))]
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

/// Test-only barrier installed inside `build_disk_watch_entry` to deterministically pin a
/// building task at a known point so a concurrent same-profile remove can run against it.
#[cfg(any(test, debug_assertions))]
pub(crate) struct DiskWatchBuildBarrier {
    pub(crate) entered: tokio::sync::Notify,
    pub(crate) release: tokio::sync::Notify,
    #[cfg(test)]
    pub(crate) armed: tokio::sync::Notify,
}

#[cfg(any(test, debug_assertions))]
pub(crate) fn disk_watch_build_barrier(
) -> &'static std::sync::Mutex<Option<Arc<DiskWatchBuildBarrier>>> {
    static BARRIER: std::sync::OnceLock<std::sync::Mutex<Option<Arc<DiskWatchBuildBarrier>>>> =
        std::sync::OnceLock::new();
    BARRIER.get_or_init(|| std::sync::Mutex::new(None))
}

/// RAII guard for the test barrier slot.
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

/// Drop the subscription handle FIRST so the dispatcher stops queuing events on this id,
/// then abort the forwarder; aborting first would race a buffered `try_send`.
pub(super) fn drop_disk_watch_entry(entry: DiskWatchEntry) {
    let DiskWatchEntry { handle, forwarder } = entry;
    drop(handle);
    forwarder.abort();
}

/// Install a disk-watch subscription for `profile` under one critical section.
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

/// Swap the disk-watch subscription from `old` to `new` under one critical section.
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

/// Background task.
pub(super) async fn disk_watcher_consumer(state: Arc<AppState>) {
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => break,
            _ = state.disk_changed.notified() => {}
        }
        let started = std::time::Instant::now();
        // Invariant 8.
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

    /// These tests assert on live subscription counts, so the state needs a real
    /// `FileWatchService` rather than the test harness's noop.
    fn state_with_live_watch() -> Arc<AppState> {
        let state = test_support::build_test_app_state(Vec::new());
        let mut state = Arc::try_unwrap(state)
            .map_err(|_| ())
            .expect("unique state");
        state.file_watch = FileWatchService::new().expect("live svc");
        Arc::new(state)
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn init_disk_watch_subscriptions_bootstraps_one_reload_after_wiring() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());

        let storage = crate::session::Storage::new_unwatched("startup-gap").expect("storage");
        storage
            .update(|instances, _groups| {
                *instances = vec![Instance::new("seed", "/tmp/seed")];
                Ok(())
            })
            .expect("seed write");

        let state = state_with_live_watch();

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

    // Concurrent same-profile rewires must converge to a single consistent map entry and
    // matching live subscription count.
    #[tokio::test]
    #[serial_test::serial]
    async fn add_remove_profile_disk_watch_serializes_concurrent_add_and_remove() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _ = crate::session::get_profile_dir("rewire-race").expect("profile dir");

        let state = state_with_live_watch();

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

    // Concurrent same-profile add and remove must converge to the last-completed call's
    // intent.
    #[tokio::test]
    #[serial_test::serial]
    async fn add_profile_disk_watch_resists_resurrection_under_concurrent_remove() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _ = crate::session::get_profile_dir("race-fix").expect("profile dir");

        let state = state_with_live_watch();

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

        // Wait deterministically until A is parked inside the barrier.
        barrier.entered.notified().await;

        let s_b = state.clone();
        let barrier_b = barrier.clone();
        let task_b = tokio::spawn(async move {
            // Signal "B is about to call remove" so the test can proceed to release A
            // without a fixed-time sleep.
            barrier_b.armed.notify_one();
            remove_profile_disk_watch(&s_b, "race-fix").await;
        });

        // Deterministic happens-before for B's lock attempt.
        barrier.armed.notified().await;
        tokio::task::yield_now().await;

        // Release A; it finishes building, installs the entry, and releases the lock.
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

    // Writes that land during init's per-profile iteration, before their profile has been
    // subscribed, must still be reconciled once init returns.
    #[tokio::test]
    #[serial_test::serial]
    async fn init_disk_watch_subscriptions_reconciles_writes_landing_during_iteration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(temp.path());

        let storage_p1 = crate::session::Storage::new_unwatched("init-gap-p1").expect("p1");
        storage_p1
            .update(|i, _| {
                *i = vec![Instance::new("p1-pre-init", "/tmp/p1-pre")];
                Ok(())
            })
            .expect("seed p1");
        let _ = crate::session::get_profile_dir("init-gap-p2").expect("p2 dir");

        let state = state_with_live_watch();

        init_disk_watch_subscriptions_with_hook(state.clone(), |profile| {
            if profile == "init-gap-p1" {
                // Write to P2 at the precise moment when P1 has just been subscribed but P2
                // has not.
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

        // Invariant 8.
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

    // A reloader reads `sessions.json`, then does slow work (the poll loop's tmux scrape,
    // which blocks for seconds when the tmux server is unreachable) before folding the
    // snapshot into `state.instances`. A delete committing inside that window used to come
    // straight back, because the merge rebuilds `state.instances` wholesale from the stale
    // snapshot.
    #[tokio::test]
    async fn a_reload_predating_a_delete_does_not_resurrect_the_removed_row() {
        let doomed = Instance::new("doomed", "/tmp/doomed");
        let survivor = Instance::new("survivor", "/tmp/survivor");
        // What a reloader read from disk before the delete landed.
        let stale_snapshot = vec![doomed.clone(), survivor.clone()];
        let read_epoch = 0;

        let state = test_support::build_test_app_state(vec![survivor.clone()]);
        // The delete already committed.
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

    // The mirror image of the delete case, and the reason `mutation_epoch` is not named
    // `delete_epoch`.
    #[tokio::test]
    async fn a_reload_predating_a_create_does_not_drop_the_new_row() {
        let existing = Instance::new("existing", "/tmp/existing");
        let created = Instance::new("created", "/tmp/created");
        // What a reloader read from disk before the create persisted.
        let stale_snapshot = vec![existing.clone()];

        // The create already committed.
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

        // And the converse, which is what the create path's bump buys. The guard
        // must not work by dropping ids missing from the prior in-memory map: a
        // row this daemon never saw is still adopted at the current epoch.
        let unbumped = test_support::build_test_app_state(vec![existing.clone(), created.clone()]);
        let mut current_snapshot = stale_snapshot;
        current_snapshot.push(Instance::new("created-elsewhere", "/tmp/elsewhere"));
        reload_state_instances_from_disk(
            &unbumped,
            current_snapshot,
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
            vec!["existing".to_string(), "created-elsewhere".to_string()],
            "without the epoch bump the reload drops the created row"
        );
    }

    // The epoch comparison has to be atomic against the delete, not merely ordered by
    // `SeqCst`.
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
        let reload = async move {
            reload_state_instances_from_disk(
                &reload_state,
                stale_snapshot,
                Vec::new(),
                StatusSource::DiskOnly,
                read_epoch,
            )
            .await;
        };
        tokio::pin!(reload);
        assert!(futures_util::poll!(&mut reload).is_pending());

        // The delete commits while the reload is parked.
        state
            .mutation_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(guard);

        reload.await;

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
}
