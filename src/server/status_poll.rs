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
    /// `None` for structured (ACP) sessions: their `status` isn't
    /// poller-authoritative (see the `is_structured()` guard in
    /// `update_status_with_metadata_inner`, and `apply_acp_overlay_inplace`,
    /// which is the sole authority for their status/timestamps). Persisting
    /// a patch here would write a bogus tmux-derived status to disk for a
    /// session the poller never actually controls. Locked by
    /// `decide_passive_transition_skips_patch_for_structured_session`
    /// (a `#[cfg(test)]` item; kept as a code-span rather than an
    /// intra-doc link that would degrade to literal text under
    /// `cargo doc`).
    patch: Option<crate::session::PassiveStatusPatch>,
    /// Always `false` for structured / ACP sessions: `should_mark_acp_unread`,
    /// driven off the live ACP turn-end event, is the sole producer of their
    /// automatic mark. See the gate in `decide_passive_transition`.
    mark_unread: bool,
}

/// Compute the passive-status write decision for one instance whose
/// `status` differs from the tick's `prev` snapshot. The full
/// contract lives on the return type at [`PassiveTransitionDecision`]:
/// `patch: None` for structured / ACP sessions (the ACP overlay is the
/// sole authority), and `mark_unread: true` only on a genuine
/// Running -> Idle for a *terminal* session when unread is enabled and the
/// row is not already unread.
pub(super) fn decide_passive_transition(
    inst: &Instance,
    old_status: Status,
    unread_enabled: bool,
) -> PassiveTransitionDecision {
    let patch =
        (!inst.is_structured()).then(|| crate::session::PassiveStatusPatch::from_instance(inst));
    // Structured rows are excluded for the same reason as the patch: the poll
    // loop has no authority over a paneless row, and since #3162 one never
    // reaches here anyway (it compares equal to `prev`, so `observed_transitions`
    // does not report it). `should_mark_acp_unread`, driven off the live ACP
    // `Stopped` event, is the sole producer for them; the gate is what stops a
    // later change to this loop from quietly re-marking from two daemon paths.
    let mark_unread = unread_enabled
        && !inst.is_structured()
        && old_status == Status::Running
        && inst.status == Status::Idle
        && !inst.unread;
    PassiveTransitionDecision { patch, mark_unread }
}

/// Per-profile bundle of passive-status writes accumulated in one
/// `status_poll_loop` tick. `patches` is keyed by instance id so the
/// persistence closure resolves each row in O(1); `unread_ids` stays a
/// small `Vec` because per-tick cardinality is low and `Vec::contains`
/// beats `HashSet` at that N.
///
/// ## Persistence divergence between daemon and TUI (#2690 follow-up)
///
/// The daemon batches transitions here (one `Storage::update` per profile
/// per tick, via `persist_session_update`). The TUI's
/// [`crate::tui::home::HomeView::persist_passive_status_transition`]
/// writes one transition at a time. Both funnel through
/// [`crate::session::Instance::merge_passive_status_patch`], whose field
/// semantics are: `last_accessed_at` is monotone non-decreasing (guarded
/// by `>=`, so an older-or-equal incoming value is dropped);
/// `status` and `idle_entered_at` are unconditional writes
/// (last-writer-wins). The two paths are safe to interleave today because
/// the poller is the sole authority on those two fields and both writers
/// read the same live source, so they converge within one poll interval
/// of the slower cadence (daemon at 2s, TUI at ~500ms) even when their
/// observations disagree mid-cadence.
///
/// A future field added to [`crate::session::PassiveStatusPatch`] that is
/// neither monotone (like `last_accessed_at`) nor single-authority (like
/// the current `status`/`idle_entered_at`) would diverge silently
/// between the daemon's batched replay and the TUI's per-transition
/// writes. Any such addition must either unify the two paths first, or
/// explicitly document why the two-writer shape stays safe.
#[derive(Default)]
pub(super) struct PassiveTransitionWrites {
    /// Keyed by instance id for O(1) lookup inside the persist closure.
    /// The patch value carries no id of its own; the flush site reads the
    /// map key (via `get_key_value`) and threads it into
    /// [`crate::session::Instance::merge_passive_status_patch`].
    patches: std::collections::HashMap<String, crate::session::PassiveStatusPatch>,
    unread_ids: Vec<String>,
}

/// Flush one tick's per-profile passive-status writes: persist each bundle,
/// then mirror its unread marks into the live `instances` slice ONLY for the
/// bundles whose durable write returned `Ok`.
///
/// The ordering is load-bearing. `instances` is the vec that
/// `reload_state_instances_from_disk` folds straight into `state.instances`,
/// so a mark applied here is what makes the unread indicator visible this
/// tick. Marking before the flock write landed stranded that mark on a failed
/// persist: disk stayed unmarked, the next tick reloaded the unmarked row,
/// and the `prev == inst.status` short-circuit blocked any re-mark, so a
/// Running -> Idle transition whose write failed silently lost its unread
/// indicator with no user-visible recovery path. Deferring the in-memory mark
/// to a persisted `Ok` keeps memory and disk in lockstep: on failure neither
/// is marked. See #2755 (follow-up to #2729).
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
        // `merge_passive_status_patch` only logs when it drops a stale
        // `last_accessed_at`, so without this there is no per-tick anchor for
        // "why did N rows change on this tick". `ok` reports the durable
        // write's outcome; on a failure the counts are what was attempted, not
        // what landed, and the unread mirror below is skipped. See #2760.
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

/// Drop entries whose session id is no longer live from the persistent
/// per-session reconciler maps the status loop owns. Without this sweep a
/// long-uptime daemon accumulates one entry per ever-observed instance id in
/// each map, so the footprint grows with lifetime-observed sessions rather than
/// with the live-session count (#2758).
///
/// The reconciler also retains these maps, but against its resume-eligible
/// subset (structured, not archived / snoozed / trashed / idle-dormant) and
/// only when the tmux scrape succeeds and the reconciler runs. This sweep runs
/// at the top of every tick against the full live-instance set, so deletion GC
/// is guaranteed even on a tick whose scrape fails, and entries for a session
/// that is merely paused (archived / snoozed / idle-dormant) are not needed to
/// be re-derived here.
#[cfg(feature = "serve")]
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

/// Background task that periodically refreshes session statuses. On each
/// tick, diffs pre- and post-refresh statuses and emits a `StatusChange`
/// on `state.status_tx` for every transition. Keeping the diff here,
/// rather than pushing it into `Instance::update_status_with_metadata`,
/// leaves the session module free of any broadcast-channel dependency
/// and keeps TUI/CLI callers unchanged.
pub(super) async fn status_poll_loop(state: Arc<AppState>) {
    // `Delay` re-arms the next tick `period` after the current one returns,
    // so a stall (suspend, scheduler stall, flock contention) does not drain
    // queued ticks and collapse the 2s cooldown the per-tick work expects.
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    #[cfg(feature = "serve")]
    let mut attempted_acp_spawns: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    #[cfg(feature = "serve")]
    let mut acp_reap_cadence = acp_reconciler::ReapCadence::default();
    #[cfg(feature = "serve")]
    let mut last_session_idle_reap: Option<std::time::Instant> = None;
    // Loop-local, single-owner sleep-inhibit assertion (single global toggle,
    // so one slot for the whole daemon). Kept off `AppState`, which is for
    // cross-task shared state; this is owned solely by the poll loop, like
    // `last_session_idle_reap`.
    #[cfg(feature = "serve")]
    let mut sleep_inhibitor: Option<Box<dyn crate::process::SleepInhibit>> = None;
    #[cfg(feature = "serve")]
    let mut last_sleep_inhibit_reconcile: Option<std::time::Instant> = None;
    // Per-session reconciler respawn budget + crash-loop park set (#1945).
    // Owned by the loop so they persist across ticks, swept against live
    // sessions inside the reconciler.
    #[cfg(feature = "serve")]
    let mut acp_respawn_history: std::collections::HashMap<String, Vec<std::time::Instant>> =
        std::collections::HashMap::new();
    #[cfg(feature = "serve")]
    let mut acp_parked: std::collections::HashSet<String> = std::collections::HashSet::new();
    // Per-session capacity-deferred marker (#1027). A structured session
    // refused by `CapacityFull` is re-armed for retry every tick; this set
    // gates the capacity banner to publish once per transition and is cleared
    // once the session's worker comes online or leaves the live set.
    #[cfg(feature = "serve")]
    let mut acp_capacity_deferred: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    loop {
        interval.tick().await;

        let prev: std::collections::HashMap<String, crate::session::Status> = {
            let instances = state.instances.read().await;
            instances.iter().map(|i| (i.id.clone(), i.status)).collect()
        };

        // GC the reconciler's persistent per-session maps against the live
        // instance set (keyed by `prev`, the full snapshot above) so a
        // long-uptime daemon's footprint stays bounded by live-session count,
        // not by lifetime-observed sessions (#2758). Above the scrape guard so
        // the sweep still runs on a tick whose tmux scrape fails.
        #[cfg(feature = "serve")]
        {
            let live_ids: std::collections::HashSet<&str> =
                prev.keys().map(String::as_str).collect();
            gc_reconciler_session_maps(
                &live_ids,
                &mut attempted_acp_spawns,
                &mut acp_respawn_history,
                &mut acp_parked,
                &mut acp_capacity_deferred,
            );
        }
        // Snapshot of the prior tick's status bookkeeping, taken from the same
        // in-memory `state.instances` this tick's `load_all_instances()` call
        // is about to reset to defaults. Fed to `seed_tick_tracking` below,
        // before `update_status_with_metadata` runs, so the Unknown->Error
        // escalation window can accumulate elapsed time across ticks (#2865)
        // and a detection awaiting confirmation survives to meet the poll that
        // confirms it (#3642), instead of both restarting every 2s.
        let prev_tracking: std::collections::HashMap<String, PriorTickTracking> = {
            let instances = state.instances.read().await;
            instances
                .iter()
                .map(|i| (i.id.clone(), PriorTickTracking::of(i)))
                .collect()
        };

        // Snapshot suppression BEFORE `batch_pane_metadata()` so a worker
        // that unmarks between the scrape and the per-instance decision
        // cannot combine "pane missing" metadata with a cleared mark and
        // re-emit the phantom Error transition the suppression exists to
        // prevent.
        let suppressed_ids =
            crate::session::recovery::snapshot_recently_restarted(&state.recently_restarted);
        let file_watch_for_poll = state.file_watch.clone();
        // Seed each freshly-disk-loaded instance's live status baseline from
        // `prev` (the true previous-tick live status) rather than letting
        // `update_status_with_metadata` fall back to comparing against its
        // own possibly-stale disk-loaded `status`. Without this, every tick
        // that finds disk out of sync with live reality (the common case,
        // since nothing persists a passive transition until the patch below
        // lands) misreads that mismatch as a brand new transition and
        // restamps idle_entered_at. See #2690.
        let prev_for_poll = prev.clone();
        // Invariant 8: read before `load_all_instances()` below. The tmux
        // scrape that follows it can block for seconds when the tmux server is
        // unreachable, which is exactly when a concurrent delete has time to
        // land and this tick's snapshot goes stale.
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
            // Diff BEFORE `reload_state_instances_from_disk`: for a tmux-backed
            // row, status_tx must observe the raw post-suppression,
            // post-tmux-scrape value, never the acp overlay that helper
            // re-applies. A structured row is the deliberate exception:
            // `skip_tmux_decision_for_structured` above already put the live acp
            // status on it, which is what makes it compare equal to `prev` here
            // instead of reporting a phantom transition every tick.
            let now = chrono::Utc::now();
            let unread_enabled = crate::session::unread_enabled();
            // Passive status transitions observed this tick, batched per
            // profile so one `Storage::update` flock covers every
            // transitioned session on that profile (plus its unread mark
            // when applicable). Persisting promptly is what keeps the next
            // reload (this loop's next tick, or a TUI relaunch) from
            // comparing against a stale snapshot and restamping again. See
            // #2690.
            let mut bundles: std::collections::HashMap<String, PassiveTransitionWrites> =
                std::collections::HashMap::new();
            for (idx, old) in observed_transitions(&instances, &prev) {
                let inst = &instances[idx];
                // First turn's `Running -> Idle` edge: best-effort auto-name a
                // still-default-named terminal session. Detached and
                // self-gating, so ineligible sessions cost only the cheap gate.
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
                    // Record the id only; the in-memory mark on `instances`
                    // is deferred to `flush_passive_transition_writes` so it
                    // fires only after the durable write returns Ok. See
                    // #2755.
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

            #[cfg(feature = "serve")]
            acp_reconciler::reconcile_acp_workers(
                &state,
                &mut attempted_acp_spawns,
                &mut acp_reap_cadence,
                &mut acp_respawn_history,
                &mut acp_parked,
                &mut acp_capacity_deferred,
            )
            .await;

            #[cfg(feature = "serve")]
            reap_idle_sessions(&state, &mut last_session_idle_reap).await;

            #[cfg(feature = "serve")]
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
    use std::time::Duration;

    /// #2758: the reconciler's persistent per-session maps must be swept
    /// against the live instance set every tick, so a deleted session's id
    /// does not linger and grow the daemon's footprint over its uptime.
    #[cfg(feature = "serve")]
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

        // Delete the session (drops out of the live set), then tick: every
        // map must forget it while the surviving session's entries remain.
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

    #[test]
    fn decide_passive_transition_skips_patch_for_structured_session() {
        // Locks the CI regression from #2697: structured/ACP sessions
        // have no tmux pane for the poller to probe; their `status` is not
        // poller-authoritative (the ACP overlay is), so a disk/detected
        // mismatch must not be persisted as a passive status patch.
        let mut inst = Instance::new("acp-session", "/tmp/test");
        inst.view = crate::session::View::Structured;
        inst.status = Status::Idle;

        let decision = decide_passive_transition(&inst, Status::Starting, false);

        assert!(
            decision.patch.is_none(),
            "structured sessions must never get a passive status patch"
        );
    }

    #[test]
    fn decide_passive_transition_patches_plain_tmux_session() {
        let mut inst = Instance::new("tmux-session", "/tmp/test");
        inst.status = Status::Idle;
        inst.idle_entered_at = Some(chrono::Utc::now());
        inst.last_accessed_at = Some(chrono::Utc::now());

        let decision = decide_passive_transition(&inst, Status::Running, false);

        let patch = decision.patch.expect("plain tmux session must get a patch");
        assert_eq!(patch.status, Status::Idle);
        assert_eq!(patch.idle_entered_at, inst.idle_entered_at);
        assert_eq!(patch.last_accessed_at, inst.last_accessed_at);
    }

    #[test]
    fn decide_passive_transition_never_fabricates_last_accessed_at() {
        // A session that transitions status before any user touch has
        // last_accessed_at == None on disk; the patch must preserve that,
        // not fabricate a stamp, or a brand-new session gains a spurious
        // "touched" signal that idle-reap and the freshness sort rely on
        // being absent.
        let mut inst = Instance::new("tmux-session", "/tmp/test");
        inst.status = Status::Idle;
        inst.last_accessed_at = None;

        let decision = decide_passive_transition(&inst, Status::Running, false);

        let patch = decision.patch.expect("plain tmux session must get a patch");
        assert_eq!(patch.last_accessed_at, None);
    }

    #[test]
    fn decide_passive_transition_marks_unread_only_on_running_to_idle() {
        let mut inst = Instance::new("tmux-session", "/tmp/test");
        inst.status = Status::Idle;

        let decision = decide_passive_transition(&inst, Status::Running, true);
        assert!(decision.mark_unread);

        let decision = decide_passive_transition(&inst, Status::Waiting, true);
        assert!(
            !decision.mark_unread,
            "only a Running -> Idle transition marks unread"
        );

        inst.unread = true;
        let decision = decide_passive_transition(&inst, Status::Running, true);
        assert!(
            !decision.mark_unread,
            "already-unread sessions must not re-mark"
        );

        // #3181: a structured row's turn-end mark belongs to the live ACP
        // listener (`should_mark_acp_unread`), so the poll loop must not also
        // produce it. Paired with
        // `tick_reports_no_transition_for_a_structured_phantom` above, which
        // covers the other half: the tick never even reports such a row, so a
        // read structured session cannot be re-marked seconds after the user
        // read it (the #3162 defect).
        let mut structured = Instance::new("acp-session", "/tmp/test");
        structured.view = crate::session::View::Structured;
        structured.status = Status::Idle;
        let decision = decide_passive_transition(&structured, Status::Running, true);
        assert!(
            !decision.mark_unread,
            "structured turn-end unread is owned by the acp event listener"
        );
    }

    // #2755 (follow-up to #2729): the poller must not strand an in-memory
    // unread mark on a persist that never landed. `flush_passive_transition_writes`
    // applies the mark to the live vec only after `persist_session_update`
    // returns Ok; on failure the row stays unmarked so memory and disk agree,
    // rather than showing a phantom unread that the next reload silently drops.
    #[tokio::test]
    #[serial_test::serial]
    async fn flush_passive_transition_defers_unread_until_persist_ok() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

        let profile = "flush-persist-failure";
        // Force the flock write to fail: making `sessions.json` a directory
        // makes the store's read-modify-write error out during `update`.
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

    // The success path: once the write is durable, the mark lands on both the
    // live vec (which feeds `state.instances`) and disk.
    #[tokio::test]
    #[serial_test::serial]
    async fn flush_passive_transition_applies_unread_after_persist_ok() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

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

    /// Closes I1's patches-routing half from #2756: each profile's `patches`
    /// bundle must merge onto that profile's own storage and nowhere else. The
    /// two adjacent tests above already cover the `unread_ids` mirror, so this
    /// test asserts only the `patches` write (status / last_accessed_at) and
    /// its per-profile routing, never unread. Each profile is seeded with the
    /// opposite status it is patched to, so a mis-routed bundle leaves a row at
    /// its seeded status and fails the status assertion: that is the routing
    /// discriminator. The instance-to-bundle assignment in `status_poll_loop`
    /// (bundles.entry(inst.source_profile)) stays out of unit-test reach: it
    /// needs an `AppState`, which has no test constructor.
    #[tokio::test]
    #[serial_test::serial]
    async fn flush_passive_transition_routes_patches_per_profile() {
        let temp = tempfile::tempdir().expect("tempdir");
        // SAFETY: serialized test; no other test mutates HOME concurrently.
        unsafe { std::env::set_var("HOME", temp.path()) };
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", temp.path().join(".config"));
        }

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

    // Pins the `MissedTickBehavior::Delay` contract on `tokio::time::interval`;
    // the prod callsite (`status_poll_loop`) is not exercised by this test.
    #[tokio::test]
    async fn status_poll_loop_interval_delays_after_stall() {
        let period = Duration::from_millis(100);
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        interval.tick().await;
        tokio::time::sleep(period * 4).await;
        interval.tick().await;

        let before = std::time::Instant::now();
        interval.tick().await;
        let gap = before.elapsed();

        assert!(
            gap >= Duration::from_millis(80),
            "second post-stall tick must wait ~period (Delay), got {gap:?}"
        );
    }
}
