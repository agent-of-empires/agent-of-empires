//! The status poll loop and the passive transitions it decides and writes.

use crate::session::Instance;
use crate::session::Status;
use std::sync::Arc;

use super::idle_reap::reap_idle_sessions;
use super::reload::{
    apply_tick_status_decisions, load_all_profiles, observed_transitions,
    reload_state_instances_from_disk, seed_tick_tracking, PriorTickTracking,
};
use super::session_identity::drain_session_id_updates_in_state;
use super::sleep_inhibit::update_sleep_inhibit;
use super::state::{AppState, StatusSource};
use super::structured_repair::live_structured_worker_records;
use crate::server::acp_reconciler;

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

/// Passive observations batched by profile. Lifecycle generations protect
/// patches and their unread marks from concurrent lifecycle commits.
#[derive(Default)]
pub(super) struct PassiveTransitionWrites {
    patches: std::collections::HashMap<String, crate::session::PassiveStatusPatch>,
    unread_ids: Vec<String>,
}

/// Apply passive writes and retain their exact rows/groups before publication.
pub(super) async fn flush_passive_transition_writes(
    file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    instances: &mut Vec<Instance>,
    metadata: &mut super::reload::CanonicalMetadata,
    bundles: std::collections::HashMap<String, PassiveTransitionWrites>,
    transition: &Arc<crate::session::StorageTransition>,
    _publication: &tokio::sync::RwLockWriteGuard<'_, ()>,
) -> Result<(), super::reload::ReloadFailure> {
    for (
        profile,
        PassiveTransitionWrites {
            patches,
            unread_ids,
        },
    ) in bundles
    {
        let patch_count = patches.len();
        let unread_count = unread_ids.len();
        let file_watch = file_watch.clone();
        let storage_profile = profile.clone();
        let transition = transition.clone();
        let persisted = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let storage = crate::session::Storage::open(&storage_profile, file_watch)?;
            transition.update_with_snapshot(&storage, |insts, _| {
                for inst in insts.iter_mut() {
                    if inst.is_structured() {
                        continue;
                    }
                    if let Some((id, patch)) = patches.get_key_value(&inst.id) {
                        inst.merge_passive_status_patch(id, patch);
                    }
                }
                for id in unread_ids {
                    let Some(inst) = insts.iter_mut().find(|inst| inst.id == id) else {
                        continue;
                    };
                    if inst.is_structured()
                        || patches.get(&id).is_some_and(|patch| {
                            patch.lifecycle_generation < inst.lifecycle_generation
                        })
                    {
                        continue;
                    }
                    inst.mark_unread();
                }
                Ok(patches)
            })
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result);
        tracing::debug!(
            target: "session.store",
            profile = %profile,
            patches = patch_count,
            unread = unread_count,
            ok = persisted.is_ok(),
            "persisted passive-status batch"
        );
        let (patches, rows, groups) = persisted.map_err(|error| super::reload::ReloadFailure {
            health: crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::ProfileData,
                profiles: vec![profile.clone()],
            },
            source: error,
        })?;
        super::reload::replace_committed_profiles(
            instances,
            metadata,
            [(&profile, rows, groups)],
            |id| {
                patches
                    .contains_key(id)
                    .then_some(super::reload::StatusCommit::Passive)
            },
        )?;
    }
    Ok(())
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

async fn sample_sandbox_health(
    state: &AppState,
) -> Arc<std::collections::HashMap<String, super::reload::SandboxHealth>> {
    let candidates: Vec<_> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|row| {
            row.is_sandboxed()
                && !row.is_trashed()
                && !matches!(
                    row.status,
                    Status::Starting | Status::Creating | Status::Deleting
                )
                && !row.has_fresh_lifecycle_reservation(chrono::Utc::now())
        })
        .filter_map(|row| {
            row.sandbox_info.as_ref().map(|sandbox| {
                (
                    row.id.clone(),
                    row.lifecycle_generation,
                    sandbox.container_name.clone(),
                )
            })
        })
        .collect();
    if candidates.is_empty() {
        return Arc::default();
    }
    match tokio::task::spawn_blocking(move || {
        let states = crate::containers::batch_container_health();
        candidates
            .into_iter()
            .filter_map(|(id, generation, container_name)| {
                states.get(&container_name).copied().map(|running| {
                    (
                        id,
                        super::reload::SandboxHealth {
                            generation,
                            container_name,
                            running,
                        },
                    )
                })
            })
            .collect()
    })
    .await
    {
        Ok(health) => Arc::new(health),
        Err(error) => {
            tracing::error!(target: "server.maintenance", %error, "sandbox health worker failed");
            Arc::default()
        }
    }
}

async fn refresh_sandbox_stores(state: &Arc<AppState>) {
    use crate::session::config::container_config::{self, CredentialFold};
    use crate::session::SessionStore;
    let candidates: Vec<_> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|row| row.is_sandboxed() && !row.is_trashed())
        .map(|row| row.id.clone())
        .collect();
    if candidates.is_empty() {
        return;
    }
    let auto_propagate = match tokio::task::spawn_blocking(|| {
        crate::session::Config::load().map(|config| config.skills.auto_propagate)
    })
    .await
    {
        Ok(Ok(enabled)) => enabled,
        result => {
            tracing::warn!(target: "server.maintenance", ?result, "sandbox refresh configuration unavailable");
            return;
        }
    };
    for id in candidates {
        if state.shutdown.is_cancelled() {
            return;
        }
        let namespace = state.profile_namespace.read().await;
        let worker_state = state.clone();
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _identity = crate::session::acquire_session_identity_lock()?;
            let profile = worker_state
                .instances
                .blocking_read()
                .iter()
                .find(|row| row.id == id)
                .map(|row| row.source_profile.clone());
            let Some(profile) = profile else {
                return Ok(());
            };
            let store = super::session_store::NativeSessionStore::open(
                worker_state.clone(),
                &profile,
                None,
            )?;
            let _lifecycle = store.storage().acquire_instance_lifecycle_lock(&id)?;
            store.check_available()?;
            let Some(row) = store.load()?.into_iter().find(|row| row.id == id) else {
                return Ok(());
            };
            if !row.is_sandboxed()
                || row.is_trashed()
                || matches!(
                    row.status,
                    Status::Starting | Status::Creating | Status::Deleting
                )
                || row.has_fresh_lifecycle_reservation(chrono::Utc::now())
                || row.sandbox_store_generation < container_config::CURRENT_SANDBOX_STORE_GENERATION
            {
                return Ok(());
            }
            let config = store.configuration(Some(store.storage().profile()))?;
            let container = crate::containers::DockerContainer::from_session_id(&id);
            let command = row.get_tool_command();
            if row.predates_shared_credential(&container, command, &config.session)? {
                return Ok(());
            }
            store.check_available()?;
            if worker_state.shutdown.is_cancelled() {
                return Ok(());
            }
            container_config::refresh_agent_configs_for_instance(
                &config,
                &id,
                &row.tool,
                Some(command),
                CredentialFold::SeedOnly,
                auto_propagate,
            );
            Ok(())
        })
        .await;
        drop(namespace);
        match result {
            Ok(Ok(())) => {}
            result => {
                tracing::warn!(target: "server.maintenance", ?result, "sandbox store refresh failed")
            }
        }
    }
}

/// Slow maintenance never delays the native terminal sampler.
pub(super) async fn maintenance_loop(
    state: Arc<AppState>,
    health: tokio::sync::watch::Sender<
        Arc<std::collections::HashMap<String, super::reload::SandboxHealth>>,
    >,
) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut attempted_acp_spawns: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut acp_reap_cadence = acp_reconciler::ReapCadence::default();
    let mut last_session_idle_reap: Option<std::time::Instant> = None;
    let mut sleep_inhibitor: Option<Box<dyn crate::process::SleepInhibit>> = None;
    let mut last_sleep_inhibit_reconcile: Option<std::time::Instant> = None;
    let mut acp_respawn_history: std::collections::HashMap<String, Vec<std::time::Instant>> =
        std::collections::HashMap::new();
    let mut acp_parked: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut acp_capacity_deferred: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut last_container_check: Option<std::time::Instant> = None;
    let mut last_credential_refresh = std::time::Instant::now();
    loop {
        tokio::select! {
            _ = state.shutdown.cancelled() => return,
            _ = interval.tick() => {}
        }
        {
            let rows = state.instances.read().await;
            let live_ids = rows.iter().map(|row| row.id.as_str()).collect();
            gc_reconciler_session_maps(
                &live_ids,
                &mut attempted_acp_spawns,
                &mut acp_respawn_history,
                &mut acp_parked,
                &mut acp_capacity_deferred,
            );
        }
        if last_container_check
            .is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(5))
        {
            health.send_replace(sample_sandbox_health(&state).await);
            last_container_check = Some(std::time::Instant::now());
        }
        if last_credential_refresh.elapsed() >= std::time::Duration::from_secs(1800) {
            refresh_sandbox_stores(&state).await;
            last_credential_refresh = std::time::Instant::now();
        }
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

pub(super) async fn status_poll_loop(
    state: Arc<AppState>,
    health: tokio::sync::watch::Receiver<
        Arc<std::collections::HashMap<String, super::reload::SandboxHealth>>,
    >,
) {
    let mut first = true;
    loop {
        if !first {
            let period = if state.runtime.has_subscribers() {
                std::time::Duration::from_millis(500)
            } else {
                std::time::Duration::from_secs(2)
            };
            tokio::select! {
                _ = state.shutdown.cancelled() => return,
                _ = state.runtime.wait_for_terminal_subscriber() => {},
                _ = tokio::time::sleep(period) => {},
            }
        }
        first = false;
        // Fence every input, including the prior runtime observations.
        let namespace = state.profile_namespace.read().await;
        let reload_lane = state.reload_lane.lock().await;
        let read_epoch = state
            .mutation_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        let prev: std::collections::HashMap<String, Status> = {
            let instances = state.instances.read().await;
            instances
                .iter()
                .map(|row| (row.id.clone(), row.status))
                .collect()
        };
        // Carry detection confirmations and the Unknown escalation clock across disk loads.
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
        let sandbox_health = health.borrow().clone();
        let updated = tokio::task::spawn_blocking(move || {
            let loaded = load_all_profiles(&file_watch_for_poll)?;
            let mut instances = loaded.instances;
            seed_tick_tracking(&mut instances, prev_tracking);
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
                &prev,
                &suppressed_ids,
                pane_metadata.as_ref().ok(),
                &sandbox_health,
            );
            for row in &mut instances {
                let tools = loaded
                    .metadata
                    .auxiliary_tools
                    .get(&row.source_profile)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                super::pane::sample_panes(row, tools, pane_metadata.as_ref().ok());
            }
            Ok::<_, super::reload::ReloadFailure>((
                instances,
                loaded.metadata,
                live_structured_worker_records(),
                prev,
            ))
        })
        .await;

        let updated = match updated {
            Ok(Ok(updated)) => updated,
            Ok(Err(error)) => {
                tracing::warn!(target: "server.status", %error, "retaining last complete session state after reload failure");
                state.mark_reload_failure(error.health).await;
                continue;
            }
            Err(error) => {
                tracing::error!(target: "server.status", %error, "session reload task failed");
                state
                    .mark_reload_failure(crate::daemon::RuntimeHealth::Degraded {
                        code: crate::daemon::ReloadFailureCode::Metadata,
                        profiles: Vec::new(),
                    })
                    .await;
                continue;
            }
        };
        {
            let (instances, metadata, live_worker_records, prev) = updated;
            let unread_enabled = crate::session::unread_enabled();
            let mut bundles: std::collections::HashMap<String, PassiveTransitionWrites> =
                std::collections::HashMap::new();
            for (idx, old) in observed_transitions(&instances, &prev) {
                let inst = &instances[idx];
                let decision = decide_passive_transition(inst, old, unread_enabled);
                if decision.patch.is_none() && !decision.mark_unread {
                    continue;
                }
                let bundle = bundles.entry(inst.source_profile.clone()).or_default();
                if let Some(patch) = decision.patch {
                    bundle.patches.insert(inst.id.clone(), patch);
                }
                if decision.mark_unread {
                    bundle.unread_ids.push(inst.id.clone());
                }
            }
            let changes = reload_state_instances_from_disk(
                &state,
                instances,
                live_worker_records,
                StatusSource::TmuxApplied,
                read_epoch,
                metadata,
                bundles,
            )
            .await;
            drop(reload_lane);
            drop(namespace);
            if let Err(error) = state.runtime.publish(&state).await {
                tracing::error!(target: "server.status", %error, "terminal snapshot publication failed");
                continue;
            }
            for change in changes {
                if change.old == Status::Running && change.new == Status::Idle {
                    let candidate = {
                        let instances = state.instances.read().await;
                        instances
                            .iter()
                            .find(|row| row.id == change.instance_id)
                            .filter(|row| {
                                crate::session::smart_rename::terminal_smart_rename_candidate(row)
                            })
                            .map(|row| (row.source_profile.clone(), row.id.clone()))
                    };
                    if let Some((profile, id)) = candidate {
                        state.runtime.work.spawn(
                            "server.terminal_smart_rename",
                            crate::session::smart_rename::try_terminal_smart_rename(
                                state.clone(),
                                profile,
                                id,
                                false,
                            ),
                        );
                    }
                }
                let _ = state.status_tx.send(change);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[serial_test::serial]
    async fn queued_tick_preserves_an_auxiliary_created_while_waiting_for_reload() {
        if !crate::tmux::is_tmux_available() {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let mut row = Instance::new("tick auxiliary", project.path().to_str().unwrap());
        row.source_profile = "work".into();
        row.status = Status::Stopped;
        let storage = crate::session::Storage::new_unwatched("work").unwrap();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        *state.canonical_metadata.write().await =
            load_all_profiles(&state.file_watch).unwrap().metadata;
        let (_health_tx, health_rx) =
            tokio::sync::watch::channel(Arc::new(std::collections::HashMap::new()));
        let lane = state.reload_lane.lock().await;
        let mut polling = Box::pin(status_poll_loop(state.clone(), health_rx));
        assert!(futures_util::poll!(polling.as_mut()).is_pending());
        let name = crate::server::pane::respawn_paired_if_dead(&state, &row.id, &row, 1)
            .await
            .unwrap();
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(name);
        drop(lane);
        let completed_tick = async {
            loop {
                let snapshot = state.runtime.publish(&state).await.unwrap();
                if snapshot
                    .value
                    .contents
                    .sessions
                    .iter()
                    .find(|item| item.id == row.id)
                    .is_some_and(|item| {
                        item.auxiliary.iter().any(|observation| {
                            observation.target == crate::session::AuxiliaryTarget::Host { index: 0 }
                        })
                    })
                {
                    break snapshot;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        };
        let snapshot = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::select! {
                _ = &mut polling => panic!("status loop ended before its tick"),
                snapshot = completed_tick => snapshot,
            }
        })
        .await
        .unwrap();
        state.shutdown.cancel();
        drop(polling);
        let published = snapshot
            .value
            .contents
            .sessions
            .iter()
            .find(|item| item.id == row.id)
            .unwrap();
        assert!(
            published
                .auxiliary
                .iter()
                .any(|observation| observation.target
                    == crate::session::AuxiliaryTarget::Host { index: 1 }
                    && observation.pane.state == crate::session::PanePresence::Alive),
            "a queued tick discarded the live extra terminal: {:?}",
            published.auxiliary
        );
    }

    async fn flush_test_rows(
        instances: &mut Vec<Instance>,
        bundles: std::collections::HashMap<String, PassiveTransitionWrites>,
    ) {
        let file_watch = crate::file_watch::FileWatchService::noop();
        let mut metadata = load_all_profiles(&file_watch).unwrap().metadata;
        let publication = tokio::sync::RwLock::new(());
        let transition = Arc::new(crate::session::StorageTransition::acquire().unwrap());
        let guard = publication.write().await;
        flush_passive_transition_writes(
            file_watch,
            instances,
            &mut metadata,
            bundles,
            &transition,
            &guard,
        )
        .await
        .unwrap();
    }

    /// #2758: the reconciler's persistent per-session maps must be swept
    /// against the live instance set every tick, so a deleted session's id
    /// does not linger and grow the daemon's footprint over its uptime.
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

    #[tokio::test]
    #[serial_test::serial]
    async fn flush_passive_transition_defers_unread_until_persist_ok() {
        let _app_dir = crate::session::test_support::isolate_app_dir();

        let profile = "flush-persist-failure";
        // Force the flock write to fail: making `sessions.json` a directory
        // makes the store's read-modify-write error out during `update`.
        // (`dir` is bound here but blocked below, after the metadata preload:
        // the preload reads this same profile, so it must run first.)
        let dir = crate::session::get_profile_dir(profile).expect("profile dir");

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
        let file_watch = crate::file_watch::FileWatchService::noop();
        let mut metadata = load_all_profiles(&file_watch).unwrap().metadata;
        let publication = tokio::sync::RwLock::new(());
        let transition = Arc::new(crate::session::StorageTransition::acquire().unwrap());
        // Block the data file only now: the preload above must read this
        // profile successfully so the failure surfaces at persist time,
        // where the flush defers the unread mark, not at reload.
        std::fs::create_dir_all(dir.join("sessions.json")).expect("sessions.json dir");
        let guard = publication.write().await;
        let persisted = flush_passive_transition_writes(
            file_watch,
            &mut instances,
            &mut metadata,
            bundles,
            &transition,
            &guard,
        )
        .await;
        assert!(
            persisted.is_err(),
            "blocking sessions.json must fail the passive-status persist"
        );
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

        flush_test_rows(&mut instances, bundles).await;

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
        flush_test_rows(&mut instances, bundles).await;

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
