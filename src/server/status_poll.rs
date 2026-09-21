//! The status poll loop and the passive transitions it decides and writes.

use crate::server::push::StatusChange;
use crate::session::Instance;
use crate::session::Status;
use std::sync::Arc;

use super::idle_reap::reap_idle_sessions;
use super::reload::{
    apply_tick_status_decisions, load_all_instances, observed_transitions,
    reload_state_instances_from_disk, seed_tick_tracking, PriorTickTracking,
};
use super::session_identity::drain_session_id_updates_in_state;
use super::sleep_inhibit::update_sleep_inhibit;
use super::state::{AppState, StatusSource};
use super::structured_repair::live_structured_worker_records;
use crate::server::{acp_reconciler, api};

/// What to do with one instance's status_poll_loop diff, once a genuine
/// `old != inst.status` transition (against the tick's `prev` snapshot) has
/// already been established by the caller.
pub(super) struct PassiveTransitionDecision {
    /// `None` for structured (ACP) sessions.
    patch: Option<crate::session::PassiveStatusPatch>,
    /// Always `false` for structured / ACP sessions.
    mark_unread: bool,
}

/// Compute the passive-status write decision for one instance whose `status` differs from
/// the tick's `prev` snapshot.
pub(super) fn decide_passive_transition(
    inst: &Instance,
    old_status: Status,
    unread_enabled: bool,
) -> PassiveTransitionDecision {
    let patch =
        (!inst.is_structured()).then(|| crate::session::PassiveStatusPatch::from_instance(inst));
    // Structured rows are excluded for the same reason as the patch.
    let mark_unread = unread_enabled
        && !inst.is_structured()
        && old_status == Status::Running
        && inst.status == Status::Idle
        && !inst.unread;
    PassiveTransitionDecision { patch, mark_unread }
}

/// Per-profile bundle of passive-status writes accumulated in one `status_poll_loop` tick.
#[derive(Default)]
pub(super) struct PassiveTransitionWrites {
    /// Keyed by instance id for O(1) lookup inside the persist closure.
    patches: std::collections::HashMap<String, crate::session::PassiveStatusPatch>,
    unread_ids: Vec<String>,
}

/// Flush one tick's per-profile passive-status writes.
pub(super) async fn flush_passive_transition_writes(
    file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    instances: &mut [Instance],
    bundles: std::collections::HashMap<String, PassiveTransitionWrites>,
) {
    for (
        profile,
        PassiveTransitionWrites {
            patches,
            unread_ids,
        },
    ) in bundles
    {
        // The closure moves `unread_ids`; keep a copy to mirror into the live
        // vec once the write is durable.
        let unread_ids_for_local = unread_ids.clone();
        let patch_count = patches.len();
        let unread_count = unread_ids.len();
        let persisted = api::persist_session_update(
            profile.clone(),
            "passive-status",
            file_watch.clone(),
            move |insts| {
                for inst in insts.iter_mut() {
                    if let Some((id, patch)) = patches.get_key_value(&inst.id) {
                        inst.merge_passive_status_patch(id, patch);
                    }
                    if unread_ids.contains(&inst.id) {
                        inst.mark_unread();
                    }
                }
            },
        )
        .await;
        // Per-tick roll-up of the passive-status batch this flush persisted.
        tracing::debug!(
            target: "session.store",
            profile = %profile,
            patches = patch_count,
            unread = unread_count,
            ok = persisted.is_ok(),
            "persisted passive-status batch"
        );
        if persisted.is_ok() {
            for inst in instances.iter_mut() {
                if unread_ids_for_local.contains(&inst.id) {
                    inst.mark_unread();
                }
            }
        }
    }
}

/// Drop entries whose session id is no longer live from the persistent per-session
/// reconciler maps the status loop owns.
pub(super) fn gc_reconciler_session_maps(
    live_ids: &std::collections::HashSet<&str>,
    attempted: &mut std::collections::HashSet<String>,
    respawn_history: &mut std::collections::HashMap<String, Vec<std::time::Instant>>,
    parked: &mut std::collections::HashSet<String>,
    capacity_deferred: &mut std::collections::HashSet<String>,
) {
    attempted.retain(|id| live_ids.contains(id.as_str()));
    respawn_history.retain(|id, _| live_ids.contains(id.as_str()));
    parked.retain(|id| live_ids.contains(id.as_str()));
    capacity_deferred.retain(|id| live_ids.contains(id.as_str()));
}

/// Background task that periodically refreshes session statuses.
pub(super) async fn status_poll_loop(state: Arc<AppState>) {
    // `Delay` re-arms the next tick `period` after the current one returns,
    // so a stall (suspend, scheduler stall, flock contention) does not drain
    // queued ticks and collapse the 2s cooldown the per-tick work expects.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut attempted_acp_spawns: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut acp_reap_cadence = acp_reconciler::ReapCadence::default();
    let mut last_session_idle_reap: Option<std::time::Instant> = None;
    // Loop-local, single-owner sleep-inhibit assertion (single global toggle, so one slot
    // for the whole daemon).
    let mut sleep_inhibitor: Option<Box<dyn crate::process::SleepInhibit>> = None;
    let mut last_sleep_inhibit_reconcile: Option<std::time::Instant> = None;
    // Per-session reconciler respawn budget + crash-loop park set.
    let mut acp_respawn_history: std::collections::HashMap<String, Vec<std::time::Instant>> =
        std::collections::HashMap::new();
    let mut acp_parked: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Per-session capacity-deferred marker.
    let mut acp_capacity_deferred: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    loop {
        interval.tick().await;

        let prev: std::collections::HashMap<String, crate::session::Status> = {
            let instances = state.instances.read().await;
            instances.iter().map(|i| (i.id.clone(), i.status)).collect()
        };

        // GC the reconciler's persistent per-session maps against the live instance set
        // (keyed by `prev`, the full snapshot above) so a long-uptime daemon's footprint
        // stays bounded by live-session count, not by lifetime-observed sessions.
        let live_ids: std::collections::HashSet<&str> = prev.keys().map(String::as_str).collect();
        gc_reconciler_session_maps(
            &live_ids,
            &mut attempted_acp_spawns,
            &mut acp_respawn_history,
            &mut acp_parked,
            &mut acp_capacity_deferred,
        );
        // Snapshot of the prior tick's status bookkeeping, taken from the same in-memory
        // `state.instances` this tick's `load_all_instances()` call is about to reset to
        // defaults.
        let prev_tracking: std::collections::HashMap<String, PriorTickTracking> = {
            let instances = state.instances.read().await;
            instances
                .iter()
                .map(|i| (i.id.clone(), PriorTickTracking::of(i)))
                .collect()
        };

        // Snapshot suppression BEFORE `batch_pane_metadata()` so a worker that unmarks
        // between the scrape and the per-instance decision cannot combine "pane missing"
        // metadata with a cleared mark and re-emit the phantom Error transition the
        // suppression exists to prevent.
        let suppressed_ids =
            crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);
        let file_watch_for_poll = state.file_watch.clone();
        // Seed each freshly-disk-loaded instance's live status baseline from `prev` (the
        // true previous-tick live status) rather than letting `update_status_with_metadata`
        // fall back to comparing against its own possibly-stale disk-loaded `status`.
        let prev_for_poll = prev.clone();
        // Invariant 8.
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let updated = tokio::task::spawn_blocking(move || {
            let mut instances = load_all_instances(&file_watch_for_poll).unwrap_or_default();
            seed_tick_tracking(&mut instances, &prev_tracking);
            crate::tmux::refresh_session_cache();
            let pane_metadata = crate::tmux::batch_pane_metadata();
            if let Err(error) = &pane_metadata {
                tracing::warn!(
                    target: "server.status",
                    %error,
                    "holding tmux-backed statuses because pane metadata is unavailable",
                );
            }
            apply_tick_status_decisions(
                &mut instances,
                &prev_for_poll,
                &suppressed_ids,
                pane_metadata.as_ref().ok(),
            );
            (instances, live_structured_worker_records())
        })
        .await;

        if let Ok((mut instances, live_worker_records)) = updated {
            // Diff BEFORE `reload_state_instances_from_disk`.
            let now = chrono::Utc::now();
            let unread_enabled = crate::session::unread_enabled();
            // Passive status transitions observed this tick, batched per profile so one
            // `Storage::update` flock covers every transitioned session on that profile
            // (plus its unread mark when applicable).
            let mut bundles: std::collections::HashMap<String, PassiveTransitionWrites> =
                std::collections::HashMap::new();
            for (idx, old) in observed_transitions(&instances, &prev) {
                let inst = &instances[idx];
                // First turn's `Running -> Idle` edge.
                if old == Status::Running && inst.status == Status::Idle {
                    crate::session::smart_rename::maybe_spawn_terminal_smart_rename(inst);
                }
                let _ = state.status_tx.send(StatusChange {
                    instance_id: inst.id.clone(),
                    instance_title: inst.title.clone(),
                    old,
                    new: inst.status,
                    at: now,
                });
                let decision = decide_passive_transition(inst, old, unread_enabled);
                if decision.patch.is_none() && !decision.mark_unread {
                    continue;
                }
                let bundle = bundles.entry(inst.source_profile.clone()).or_default();
                if let Some(patch) = decision.patch {
                    bundle.patches.insert(inst.id.clone(), patch);
                }
                if decision.mark_unread {
                    // Record the id only; the in-memory mark on `instances` is deferred to
                    // `flush_passive_transition_writes` so it fires only after the durable
                    // write returns Ok.
                    bundle.unread_ids.push(inst.id.clone());
                }
            }
            flush_passive_transition_writes(state.file_watch.clone(), &mut instances, bundles)
                .await;

            reload_state_instances_from_disk(
                &state,
                instances,
                live_worker_records,
                StatusSource::TmuxApplied,
                read_epoch,
            )
            .await;

            drain_session_id_updates_in_state(&state).await;

            acp_reconciler::reconcile_acp_workers(
                &state,
                &mut attempted_acp_spawns,
                &mut acp_reap_cadence,
                &mut acp_respawn_history,
                &mut acp_parked,
                &mut acp_capacity_deferred,
            )
            .await;

            reap_idle_sessions(&state, &mut last_session_idle_reap).await;

            update_sleep_inhibit(
                &state,
                &mut sleep_inhibitor,
                &mut last_sleep_inhibit_reconcile,
            )
            .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #2758.
    #[test]
    fn gc_reconciler_session_maps_drops_deleted_session_ids() {
        use std::collections::{HashMap, HashSet};
        use std::time::Instant;

        let mut attempted: HashSet<String> = HashSet::new();
        let mut respawn_history: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut parked: HashSet<String> = HashSet::new();
        let mut capacity_deferred: HashSet<String> = HashSet::new();

        // A session that has been spawn-attempted, parked (crash-loop), has
        // respawn history, and is capacity-deferred.
        let doomed = "sess-deleted".to_string();
        let kept = "sess-live".to_string();
        for id in [&doomed, &kept] {
            attempted.insert(id.clone());
            respawn_history.insert(id.clone(), vec![Instant::now()]);
            parked.insert(id.clone());
            capacity_deferred.insert(id.clone());
        }

        // Tick with both sessions live: nothing is swept.
        let mut live: HashSet<&str> = HashSet::new();
        live.insert(doomed.as_str());
        live.insert(kept.as_str());
        gc_reconciler_session_maps(
            &live,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        );
        assert!(attempted.contains(&doomed) && attempted.contains(&kept));
        assert!(parked.contains(&doomed) && parked.contains(&kept));

        // Delete the session (drops out of the live set), then tick.
        live.remove(doomed.as_str());
        gc_reconciler_session_maps(
            &live,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        );

        assert!(
            !attempted.contains(&doomed),
            "attempted must forget the deleted session id"
        );
        assert!(
            !respawn_history.contains_key(&doomed),
            "respawn_history must forget the deleted session id"
        );
        assert!(
            !parked.contains(&doomed),
            "parked must forget the deleted session id"
        );
        assert!(
            !capacity_deferred.contains(&doomed),
            "capacity_deferred must forget the deleted session id"
        );

        // The still-live session is untouched.
        assert!(attempted.contains(&kept));
        assert!(respawn_history.contains_key(&kept));
        assert!(parked.contains(&kept));
        assert!(capacity_deferred.contains(&kept));
    }

    /// The tmux poller owns the passive transition for terminal rows only: a structured
    /// row gets neither a patch (#2697) nor an unread mark (#3181, the acp event listener
    /// owns that). A terminal row's patch carries the row's own timestamps and must not
    /// fabricate a `last_accessed_at` a brand-new session never had, since idle-reap and
    /// the freshness sort both read its absence.
    #[test]
    fn decide_passive_transition_patches_only_terminal_rows() {
        let mut structured = Instance::new("acp-session", "/tmp/test");
        structured.view = crate::session::View::Structured;
        structured.status = Status::Idle;
        let decision = decide_passive_transition(&structured, Status::Starting, false);
        assert!(decision.patch.is_none());
        let decision = decide_passive_transition(&structured, Status::Running, true);
        assert!(!decision.mark_unread);

        let mut inst = Instance::new("tmux-session", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(chrono::Utc::now());
        inst.last_accessed_at = Some(chrono::Utc::now());
        let patch = decide_passive_transition(&inst, Status::Running, false)
            .patch
            .expect("a terminal row gets a patch");
        assert_eq!(patch.status, Status::Idle);
        assert_eq!(patch.idle_entered_at, inst.idle_entered_at);
        assert_eq!(patch.last_accessed_at, inst.last_accessed_at);

        inst.last_accessed_at = None;
        let patch = decide_passive_transition(&inst, Status::Running, false)
            .patch
            .expect("a terminal row gets a patch");
        assert_eq!(patch.last_accessed_at, None, "no gesture stamp is invented");

        // Unread is marked once, on the Running -> Idle turn end.
        assert!(decide_passive_transition(&inst, Status::Running, true).mark_unread);
        assert!(!decide_passive_transition(&inst, Status::Waiting, true).mark_unread);
        inst.unread = true;
        assert!(!decide_passive_transition(&inst, Status::Running, true).mark_unread);
    }

    // #2755 (follow-up to #2729).
    #[tokio::test]
    #[serial_test::serial]
    async fn flush_passive_transition_defers_unread_until_persist_ok() {
        let _app_dir = crate::session::test_support::isolate_app_dir();

        let profile = "flush-persist-failure";
        // Force the flock write to fail.
        let dir = crate::session::get_profile_dir(profile).expect("profile dir");
        std::fs::create_dir_all(dir.join("sessions.json")).expect("sessions.json dir");

        let mut inst = Instance::new("idle-session", "/tmp/idle");
        inst.source_profile = profile.to_string();
        let id = inst.id.clone();
        let mut instances = vec![inst];

        let mut bundles: std::collections::HashMap<String, PassiveTransitionWrites> =
            std::collections::HashMap::new();
        bundles
            .entry(profile.to_string())
            .or_default()
            .unread_ids
            .push(id.clone());

        flush_passive_transition_writes(
            crate::file_watch::FileWatchService::noop(),
            &mut instances,
            bundles,
        )
        .await;

        assert!(
            !instances[0].unread,
            "a failed persist must not leave a phantom in-memory unread mark (see #2755)"
        );
    }

    // The success path.
    #[tokio::test]
    #[serial_test::serial]
    async fn flush_passive_transition_applies_unread_after_persist_ok() {
        let _app_dir = crate::session::test_support::isolate_app_dir();

        let profile = "flush-persist-success";
        let mut inst = Instance::new("idle-session", "/tmp/idle");
        inst.source_profile = profile.to_string();
        let id = inst.id.clone();

        // Seed the row on disk so the persist closure has a matching id to mark.
        let seed = inst.clone();
        crate::session::Storage::new_unwatched(profile)
            .expect("storage")
            .update(move |instances, _groups| {
                *instances = vec![seed];
                Ok(())
            })
            .expect("seed write");

        let mut instances = vec![inst];
        let mut bundles: std::collections::HashMap<String, PassiveTransitionWrites> =
            std::collections::HashMap::new();
        bundles
            .entry(profile.to_string())
            .or_default()
            .unread_ids
            .push(id.clone());

        flush_passive_transition_writes(
            crate::file_watch::FileWatchService::noop(),
            &mut instances,
            bundles,
        )
        .await;

        assert!(
            instances[0].unread,
            "a durable persist must mirror the unread mark into the live vec"
        );
        let disk = crate::session::Storage::new_unwatched(profile)
            .expect("storage")
            .load()
            .expect("load");
        assert!(
            disk.iter().find(|i| i.id == id).expect("seeded row").unread,
            "the unread mark must be durable on disk"
        );
    }

    /// Each profile receives only its own durable status and timestamp patch.
    #[tokio::test]
    #[serial_test::serial]
    async fn flush_passive_transition_routes_patches_per_profile() {
        let _app_dir = crate::session::test_support::isolate_app_dir();

        let old = chrono::Utc::now() - chrono::Duration::minutes(1);
        let new_ts = chrono::Utc::now();

        let mut a1 = Instance::new("session-a", "/tmp/a");
        a1.source_profile = "flush-a".to_string();
        a1.status = Status::Running;
        a1.last_accessed_at = Some(old);
        let a1_id = a1.id.clone();

        let mut b1 = Instance::new("session-b", "/tmp/b");
        b1.source_profile = "flush-b".to_string();
        b1.status = Status::Idle;
        b1.last_accessed_at = Some(old);
        let b1_id = b1.id.clone();

        let seed_a = a1.clone();
        crate::session::Storage::new_unwatched("flush-a")
            .expect("storage")
            .update(move |instances, _groups| {
                *instances = vec![seed_a];
                Ok(())
            })
            .expect("seed write");
        let seed_b = b1.clone();
        crate::session::Storage::new_unwatched("flush-b")
            .expect("storage")
            .update(move |instances, _groups| {
                *instances = vec![seed_b];
                Ok(())
            })
            .expect("seed write");

        let mut bundles: std::collections::HashMap<String, PassiveTransitionWrites> =
            std::collections::HashMap::new();
        bundles
            .entry("flush-a".to_string())
            .or_default()
            .patches
            .insert(
                a1_id.clone(),
                crate::session::PassiveStatusPatch {
                    lifecycle_generation: 0,
                    status: Status::Idle,
                    idle_entered_at: None,
                    last_accessed_at: Some(new_ts),
                },
            );
        bundles
            .entry("flush-b".to_string())
            .or_default()
            .patches
            .insert(
                b1_id.clone(),
                crate::session::PassiveStatusPatch {
                    lifecycle_generation: 0,
                    status: Status::Running,
                    idle_entered_at: None,
                    last_accessed_at: Some(new_ts),
                },
            );

        let mut instances = vec![a1, b1];
        flush_passive_transition_writes(
            crate::file_watch::FileWatchService::noop(),
            &mut instances,
            bundles,
        )
        .await;

        let disk_a = crate::session::Storage::new_unwatched("flush-a")
            .expect("storage")
            .load()
            .expect("load");
        let row_a = disk_a
            .iter()
            .find(|i| i.id == a1_id)
            .expect("a1 on flush-a disk");
        assert_eq!(
            row_a.status,
            Status::Idle,
            "profile A's patch must merge its status onto profile A's storage"
        );
        assert_eq!(
            row_a.last_accessed_at,
            Some(new_ts),
            "profile A's patch must merge its last_accessed_at onto profile A's storage"
        );

        let disk_b = crate::session::Storage::new_unwatched("flush-b")
            .expect("storage")
            .load()
            .expect("load");
        let row_b = disk_b
            .iter()
            .find(|i| i.id == b1_id)
            .expect("b1 on flush-b disk");
        assert_eq!(
            row_b.status,
            Status::Running,
            "profile B's patch must merge its status onto profile B's storage"
        );
        assert_eq!(
            row_b.last_accessed_at,
            Some(new_ts),
            "profile B's patch must merge its last_accessed_at onto profile B's storage"
        );
    }
}
