//! What the daemon repairs on boot.

use std::sync::Arc;

use super::state::AppState;

pub(super) fn repair_startup_profile_ownership(
    file_watch: &Arc<crate::file_watch::FileWatchService>,
) -> anyhow::Result<()> {
    let profiles = crate::session::list_profiles()?;
    let mut storages = Vec::with_capacity(profiles.len());
    let mut loads = Vec::with_capacity(profiles.len());
    for profile in profiles {
        let storage = crate::session::Storage::open(&profile, file_watch.clone())?;
        let (mut instances, _) = storage.load_complete_with_groups()?;
        for instance in &mut instances {
            instance.source_profile = profile.clone();
        }
        loads.push((profile.clone(), instances));
        storages.push((profile, storage));
    }
    let loads_view: Vec<(&str, &[crate::session::Instance])> = loads
        .iter()
        .map(|(profile, rows)| (profile.as_str(), rows.as_slice()))
        .collect();
    let storages_view: Vec<(&str, &crate::session::Storage)> = storages
        .iter()
        .map(|(profile, storage)| (profile.as_str(), storage))
        .collect();
    let outcome = crate::session::reconcile_profile_duplicates(&loads_view, &storages_view);
    for report in &outcome.reports {
        tracing::warn!(target: "session.startup_recovery", ?report, "ambiguous profile identity retained for operator review");
    }
    let _identity = crate::session::acquire_session_identity_lock()?;
    for (_, storage) in &storages {
        let instances = storage.load_complete_with_groups()?.0;
        let now = chrono::Utc::now();
        // Purge owners must survive until recover_pending_purges has proved teardown.
        let mut expired: Vec<String> = instances
            .iter()
            .filter(|row| {
                row.lifecycle_reservation
                    .as_ref()
                    .is_some_and(|reservation| {
                        reservation.op != crate::session::LifecycleOperation::Purge
                            && row.terminal_handoff.is_none()
                    })
                    && !row.has_fresh_lifecycle_reservation(now)
            })
            .map(|row| row.id.clone())
            .collect();
        expired.sort();
        let mut locks = Vec::with_capacity(expired.len());
        for id in &expired {
            locks.push(storage.acquire_instance_lifecycle_lock(id)?);
        }
        if expired.is_empty() {
            continue;
        }
        storage.update(|rows, _groups| {
            for id in &expired {
                if let Some(row) = rows.iter_mut().find(|row| &row.id == id) {
                    if row
                        .lifecycle_reservation
                        .as_ref()
                        .is_some_and(|reservation| {
                            reservation.op != crate::session::LifecycleOperation::Purge
                                && row.terminal_handoff.is_none()
                        })
                    {
                        row.clear_expired_lifecycle_reservation(
                            crate::session::Instance::LIFECYCLE_RESERVATION_TTL,
                            now,
                        );
                    }
                }
            }
            Ok(())
        })?;
    }
    Ok(())
}

pub(super) async fn publish_startup_identity(state: &Arc<AppState>) {
    let instances: Vec<_> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|row| !state.cityhall_mode || row.is_structured())
        .cloned()
        .collect();
    match tokio::task::spawn_blocking(move || {
        // Batch-sync instance IDs and captured session IDs to tmux hidden env
        // so that build_exclusion_set() on other AoE instances can see them.
        // One observation for both per-instance walks below. They visit every
        // instance in the view, so a per-item `list-sessions` fork scales with
        // the whole store, measured as the dominant tmux cost of this pass on
        // a store of a few hundred sessions.
        let live = crate::tmux::LiveSessionSnapshot::new();
        {
            let mut set_batch: Vec<(String, String, String)> = Vec::new();
            let mut unset_batch: Vec<(String, String)> = Vec::new();
            for inst in &instances {
                // This publication is one-shot: no reload re-runs it and a
                // poller does not re-emit an unchanged sid, so a row dropped
                // here stays unpublished until an unrelated sid change or a
                // relaunch. A snapshot that could not reach the server is
                // therefore probed per row rather than read as "no live pane".
                let Some(tmux_name) = inst.tmux_env_session_name_in_or_probe(&live) else {
                    continue;
                };

                set_batch.push((
                    tmux_name.clone(),
                    crate::tmux::env::AOE_INSTANCE_ID_KEY.to_string(),
                    inst.id.clone(),
                ));
                if let Some(ref sid) = inst.agent_session_id {
                    set_batch.push((
                        tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                        sid.clone(),
                    ));
                } else {
                    unset_batch.push((
                        tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                    ));
                }
            }
            if !set_batch.is_empty() {
                let batch_refs: Vec<(&str, &str, &str)> = set_batch
                    .iter()
                    .map(|(s, k, v)| (s.as_str(), k.as_str(), v.as_str()))
                    .collect();
                if let Err(e) = crate::tmux::env::set_hidden_env_batch(&batch_refs) {
                    tracing::warn!(target: "session.startup_recovery", "Batch env sync failed: {}", e);
                }
            }
            if !unset_batch.is_empty() {
                let batch_refs: Vec<(&str, &str)> = unset_batch
                    .iter()
                    .map(|(s, k)| (s.as_str(), k.as_str()))
                    .collect();
                if let Err(e) = crate::tmux::env::remove_hidden_env_batch(&batch_refs) {
                    tracing::warn!(target: "tui.home", "Batch env unset failed: {}", e);
                }
            }
        }
    }).await {
        Ok(()) => {},
        Err(error) => tracing::warn!(target: "session.startup_recovery", %error, "initial tmux identity publication failed"),
    }
}

/// Startup auto-recovery for AI agent sessions whose tmux pane is missing after a daemon
/// restart or system reboot.
pub(super) async fn daemon_startup_recovery_mark(
    state: Arc<AppState>,
    failed_repairs: &std::collections::HashSet<String>,
) -> Option<(
    crate::session::recovery::RecoveryLock,
    Vec<crate::session::Instance>,
)> {
    if state.read_only
        || *state.canonical_health.read().await != crate::daemon::RuntimeHealth::Healthy
    {
        return None;
    }
    let lock = match crate::session::recovery::try_acquire_recovery_lock() {
        Ok(Some(l)) => l,
        Ok(None) => {
            tracing::info!(
                target: "session.startup_recovery",
                "another process holds the recovery lock; skipping daemon startup recovery",
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                target: "session.startup_recovery",
                error = %e,
                "failed to acquire recovery lock; skipping daemon startup recovery",
            );
            return None;
        }
    };

    crate::session::recovery::warm_tmux_server();
    crate::tmux::refresh_session_cache();
    // On probe failure we cannot distinguish "all panes dead" from "tmux unreachable", and
    // treating the latter as the former would trigger spurious recovery cascades that kill
    // possibly-alive panes.
    let pane_meta = match crate::tmux::batch_pane_metadata() {
        Ok(map) => map,
        Err(e) => {
            tracing::warn!(
                target: "session.startup_recovery",
                error = %e,
                "tmux probe failed at daemon startup; skipping recovery this launch",
            );
            return None;
        }
    };

    let mut candidates: Vec<crate::session::Instance> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                let session_name = crate::tmux::resolve_agent_session_name_in(
                    &pane_meta,
                    &i.id,
                    &crate::tmux::Session::generate_name(&i.id, &i.title),
                );
                let has_live_tmux = pane_meta
                    .get(&session_name)
                    .map(|m| !m.pane_dead)
                    .unwrap_or(false);
                !failed_repairs.contains(&i.id)
                    && (!state.cityhall_mode || i.is_structured())
                    && !has_live_tmux
                    && crate::session::recovery::is_recovery_candidate(i)
            })
            .cloned()
            .collect()
    };

    // #2994 (deterministic).
    let attempted = crate::session::recovery::recovery_attempted_this_boot();
    candidates.retain(|i| !attempted.contains(&i.id));

    // #2994 (defense-in-depth).
    if !candidates.is_empty() {
        let scan_input = candidates.clone();
        let orphan_flags = tokio::task::spawn_blocking(move || {
            crate::session::recovery::orphaned_agents_alive(&scan_input)
        })
        .await
        .unwrap_or_else(|_| vec![true; candidates.len()]);
        let mut idx = 0;
        candidates.retain(|i| {
            let alive = orphan_flags.get(idx).copied().unwrap_or(true);
            idx += 1;
            if alive {
                tracing::info!(
                    target: "session.startup_recovery",
                    id = %i.id,
                    "skipping recovery: agent already alive on an orphaned tmux server",
                );
            }
            !alive
        });
    }

    if candidates.is_empty() {
        return None;
    }

    // Record the attempt *before* any worker runs `tmux new-session`, so a
    // mid-pass crash fails toward "already attempted" for the next pass.
    crate::session::recovery::mark_recovery_attempted(
        &candidates.iter().map(|i| i.id.clone()).collect::<Vec<_>>(),
    );

    for inst in &candidates {
        crate::session::recovery::mark_recently_restarted(&state.recently_restarted, &inst.id);
    }
    // Seed the pending set so the refresher (spawned between Phase A and Phase B) keeps
    // these marks fresh while candidates wait on a STARTUP_RECOVERY_CONCURRENCY permit.
    crate::session::recovery::seed_recovery_pending(
        &state.recovery_pending,
        candidates.iter().map(|i| i.id.clone()),
    );

    tracing::info!(
        target: "session.startup_recovery",
        count = candidates.len(),
        "starting daemon recovery for missing tmux sessions",
    );

    Some((lock, candidates))
}

/// Phase B: drive the cascade workers for the pre-marked candidates.
pub(super) async fn daemon_startup_recovery_cascade(
    state: Arc<AppState>,
    lock: crate::session::recovery::RecoveryLock,
    candidates: Vec<crate::session::Instance>,
) {
    let semaphore = Arc::new(tokio::sync::Semaphore::new(
        crate::session::recovery::STARTUP_RECOVERY_CONCURRENCY,
    ));
    // Captured up front for the completion sweep below; the worker loop
    // consumes `candidates`.
    let all_ids: Vec<String> = candidates.iter().map(|i| i.id.clone()).collect();
    let mut tasks: tokio::task::JoinSet<()> = tokio::task::JoinSet::new();

    for inst in candidates {
        let permit_sem = semaphore.clone();
        let inst_state = state.clone();
        let id = inst.id.clone();
        let lock_handle = inst_state.instance_lock(&id).await;
        tasks.spawn(async move {
            let _permit = permit_sem
                .acquire_owned()
                .await
                .expect("recovery semaphore not closed");
            let _guard = lock_handle.lock().await;

            // Re-check both `is_recovery_candidate` AND tmux liveness after acquiring the
            // lock.
            let pane_meta = match crate::tmux::batch_pane_metadata() {
                Ok(map) => map,
                Err(e) => {
                    tracing::warn!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        error = %e,
                        "tmux probe failed during recovery re-check; skipping cascade",
                    );
                    crate::session::recovery::drain_recovery_pending(
                        &inst_state.recovery_pending,
                        &inst_state.recently_restarted,
                        &id,
                    );
                    return;
                }
            };
            let recheck_inst: Option<crate::session::Instance> = {
                let instances = inst_state.instances.read().await;
                instances
                    .iter()
                    .find(|i| i.id == id)
                    .filter(|i| {
                        let session_name = crate::tmux::resolve_agent_session_name_in(
                            &pane_meta,
                            &i.id,
                            &crate::tmux::Session::generate_name(&i.id, &i.title),
                        );
                        let has_live_tmux = pane_meta
                            .get(&session_name)
                            .map(|m| !m.pane_dead)
                            .unwrap_or(false);
                        i.lifecycle_generation == inst.lifecycle_generation
                            && i.source_profile == inst.source_profile
                            && !has_live_tmux
                            && crate::session::recovery::is_recovery_candidate(i)
                    })
                    .cloned()
            };
            // #2994.
            let generation = match recheck_inst {
                Some(inst) => {
                    let generation = inst.lifecycle_generation;
                    let alive = tokio::task::spawn_blocking(move || {
                        crate::session::recovery::orphaned_agent_process_alive(&inst)
                    })
                    .await
                    .unwrap_or(true);
                    (!alive).then_some(generation)
                }
                None => None,
            };
            let Some(generation) = generation else {
                crate::session::recovery::drain_recovery_pending(
                    &inst_state.recovery_pending,
                    &inst_state.recently_restarted,
                    &id,
                );
                return;
            };

            // Phase A already marked this id, but re-mark now to refresh the timestamp so
            // the suppression window covers the full cascade latency starting from this
            // point rather than from the (possibly older) Phase A snapshot.
            crate::session::recovery::mark_recently_restarted(&inst_state.recently_restarted, &id);

            drop(_guard);
            let response = crate::server::api::sessions::recovery_restart(
                inst_state.clone(),
                id.clone(),
                generation,
            )
            .await;
            if response.status().is_success() {
                tracing::info!(
                    target: "session.startup_recovery",
                    instance_id = %id, title = %inst.title, "recovery completed",
                );
            } else {
                tracing::warn!(
                    target: "session.startup_recovery",
                    instance_id = %id, title = %inst.title, status = %response.status(),
                    "canonical recovery restart refused or failed",
                );
            }
            crate::session::recovery::drain_recovery_pending(
                &inst_state.recovery_pending,
                &inst_state.recently_restarted,
                &id,
            );
        });
    }

    while tasks.join_next().await.is_some() {}

    // Completion sweep.
    for id in &all_ids {
        crate::session::recovery::drain_recovery_pending(
            &state.recovery_pending,
            &state.recently_restarted,
            id,
        );
    }
    drop(lock);
}

#[cfg(test)]
mod migrated_tui_ownership_tests {
    use super::repair_startup_profile_ownership;
    use crate::session::test_support::{isolate_app_dir_at, AppDirGuard};
    use crate::session::{
        Group, Instance, LifecycleOperation, LifecycleReservation, Status, Storage,
    };
    use serial_test::serial;
    use tempfile::TempDir;
    fn setup_test_home(temp: &TempDir) -> AppDirGuard {
        isolate_app_dir_at(temp.path())
    }
    fn boot_ambiguous_state(with_journal: bool) -> (TempDir, AppDirGuard, String) {
        let temp = TempDir::new().unwrap();
        let guard = setup_test_home(&temp);
        let alpha = Storage::new_unwatched("alpha").unwrap();
        let mut inst = Instance::new("moved", "/repo/moved");
        inst.group_path = "work".to_string();
        let id = inst.id.clone();
        alpha
            .update(|i, g| {
                i.push(inst.clone());
                g.push(Group::new("work", "work"));
                Ok(())
            })
            .unwrap();
        let beta = Storage::new_unwatched("beta").unwrap();
        beta.update(|i, _| {
            let mut copy = inst.clone();
            copy.source_profile = "beta".to_string();
            i.push(copy);
            Ok(())
        })
        .unwrap();
        if with_journal {
            crate::session::record_move_journal(
                &crate::session::MoveJournalEntry {
                    version: crate::session::MOVE_JOURNAL_VERSION,
                    ids: vec![id.clone()],
                    source_profile: "alpha".to_string(),
                    target_profile: "beta".to_string(),
                    source_sessions_path: alpha.sessions_path().to_path_buf(),
                    target_sessions_path: beta.sessions_path().to_path_buf(),
                    group_move_source_path: "work".to_string(),
                    group_move_target_path: "moved".to_string(),
                    group_move_subtree: false,
                    created_at_epoch_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or_default(),
                },
                alpha.sessions_path(),
            )
            .unwrap();
        }
        (temp, guard, id)
    }

    #[tokio::test]
    #[serial]
    async fn interrupted_move_with_journal_repairs_before_canonical_publish() {
        let (_temp, _guard, id) = boot_ambiguous_state(true);
        repair_startup_profile_ownership(&crate::file_watch::FileWatchService::noop()).unwrap();
        let loaded =
            crate::server::reload::load_all_profiles(&crate::file_watch::FileWatchService::noop())
                .unwrap();
        assert_eq!(
            loaded.instances.iter().filter(|row| row.id == id).count(),
            1
        );
        assert_eq!(
            loaded
                .instances
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .source_profile,
            "beta"
        );
        assert!(Storage::new_unwatched("alpha")
            .unwrap()
            .load()
            .unwrap()
            .is_empty());
        let beta = Storage::new_unwatched("beta").unwrap().load().unwrap();
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].id, id);
        let state = crate::server::test_support::build_test_app_state(loaded.instances);
        *state.canonical_metadata.write().await = loaded.metadata;
        let published = state.runtime.publish(&state).await.unwrap();
        assert_eq!(
            published
                .value
                .contents
                .sessions
                .iter()
                .filter(|row| row.id == id)
                .count(),
            1
        );
        assert_eq!(
            published
                .value
                .contents
                .sessions
                .iter()
                .find(|row| row.id == id)
                .unwrap()
                .profile,
            "beta"
        );
    }

    #[tokio::test]
    #[serial]
    async fn startup_retains_fresh_launch_owner_but_releases_expired_owner_before_profile_move() {
        use axum::{
            body::Body,
            http::{Request, StatusCode},
        };
        use tower::ServiceExt;
        for expired in [false, true] {
            let temp = TempDir::new().unwrap();
            let _guard = setup_test_home(&temp);
            let source = Storage::new_unwatched("source").unwrap();
            let target = Storage::new_unwatched("target").unwrap();
            target.update(|_, _| Ok(())).unwrap();
            let mut row = Instance::new("reserved", "/tmp/reserved");
            row.source_profile = "source".into();
            row.status = Status::Starting;
            row.lifecycle_generation = 1;
            let at = if expired {
                chrono::Utc::now()
                    - Instance::LIFECYCLE_RESERVATION_TTL
                    - chrono::Duration::seconds(1)
            } else {
                chrono::Utc::now()
            };
            row.lifecycle_reservation = Some(LifecycleReservation {
                op: LifecycleOperation::Launch,
                generation: 1,
                at,
            });
            source
                .update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })
                .unwrap();
            repair_startup_profile_ownership(&crate::file_watch::FileWatchService::noop()).unwrap();
            let persisted = source.load().unwrap();
            assert_eq!(persisted[0].lifecycle_reservation.is_none(), expired);
            let loaded = crate::server::reload::load_all_profiles(
                &crate::file_watch::FileWatchService::noop(),
            )
            .unwrap();
            let state = crate::server::test_support::build_test_app_state_with_policy(
                loaded.instances,
                vec!["localhost".into()],
                Vec::new(),
                None,
            );
            *state.canonical_metadata.write().await = loaded.metadata;
            let response = crate::server::test_support::build_router_for_test(state.clone())
                .oneshot(
                    Request::builder()
                        .method("PATCH")
                        .uri(format!("/api/sessions/{}", row.id))
                        .header("host", "localhost")
                        .header("content-type", "application/json")
                        .body(Body::from(r#"{"profile":"target"}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if expired {
                    StatusCode::OK
                } else {
                    StatusCode::CONFLICT
                }
            );
            assert_eq!(
                source.load().unwrap().iter().any(|r| r.id == row.id),
                !expired
            );
            assert_eq!(
                target.load().unwrap().iter().any(|r| r.id == row.id),
                expired
            );
            if expired {
                let published = state.runtime.publish(&state).await.unwrap();
                let moved = published
                    .value
                    .contents
                    .sessions
                    .iter()
                    .find(|r| r.id == row.id)
                    .unwrap();
                assert_eq!(moved.profile, "target");
                assert!(moved.lifecycle_reservation.is_none());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::test_support;

    #[tokio::test]
    #[serial_test::serial]
    async fn startup_recovery_requires_healthy_authority_without_consuming_ledger() {
        let _home = crate::session::test_support::isolate_app_dir();
        let ledger = tempfile::tempdir().unwrap();
        let _env = crate::session::test_support::EnvGuard::set(&[(
            crate::session::recovery::RECOVERY_ATTEMPT_DIR_ENV,
            ledger.path(),
        )]);
        let mut row = crate::session::Instance::new("needs-repair", "/tmp/stale-recovery-path");
        row.agent_session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(crate::session::recovery::is_recovery_candidate(&row));
        let id = row.id.clone();
        let state = test_support::build_test_app_state(vec![row]);
        for code in [
            crate::daemon::ReloadFailureCode::Metadata,
            crate::daemon::ReloadFailureCode::ProfileData,
        ] {
            *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
                code,
                profiles: vec!["default".into()],
            };
            assert!(
                daemon_startup_recovery_mark(state.clone(), &std::collections::HashSet::new())
                    .await
                    .is_none()
            );
            assert!(!crate::session::recovery::recovery_attempted_this_boot().contains(&id));
            assert!(!state.recovery_pending.read().unwrap().contains(&id));
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_startup_repair_is_not_consumed_as_a_recovery_attempt() {
        if !crate::tmux::is_tmux_available() {
            return;
        }
        let _home = crate::session::test_support::isolate_app_dir();
        let ledger = tempfile::tempdir().unwrap();
        let _env = crate::session::test_support::EnvGuard::set(&[(
            crate::session::recovery::RECOVERY_ATTEMPT_DIR_ENV,
            ledger.path(),
        )]);
        let repaired = tempfile::tempdir().unwrap();
        let mut row =
            crate::session::Instance::new("repaired-path", repaired.path().to_str().unwrap());
        row.agent_session_id = Some(uuid::Uuid::new_v4().to_string());
        let id = row.id.clone();
        // ENV_LOCK already isolates the app and ledger. An unrelated, non-aoe
        // pane keeps the shared server alive and is not swept by stop-all tests.
        let probe = crate::tmux::test_helpers::TmuxTestSession::new("recovery_probe");
        let started = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", probe.name(), "exec cat"])
            .output()
            .expect("create unrelated recovery probe pane");
        assert!(
            started.status.success(),
            "{}",
            String::from_utf8_lossy(&started.stderr)
        );
        let panes = crate::tmux::batch_pane_metadata().expect("owned probe authority is healthy");
        let present = crate::tmux::tmux_command()
            .args(["has-session", "-t", probe.name()])
            .output()
            .expect("probe session presence");
        assert!(
            present.status.success(),
            "{}",
            String::from_utf8_lossy(&present.stderr)
        );
        let candidate_session = crate::tmux::resolve_agent_session_name_in(
            &panes,
            &row.id,
            &crate::tmux::Session::generate_name(&row.id, &row.title),
        );
        assert!(
            !panes.contains_key(&candidate_session),
            "candidate still has no tmux pane"
        );
        assert!(crate::session::recovery::is_recovery_candidate(&row));
        let state = test_support::build_test_app_state(vec![row]);
        assert!(daemon_startup_recovery_mark(
            state.clone(),
            &std::collections::HashSet::from([id.clone()])
        )
        .await
        .is_none());
        assert!(!crate::session::recovery::recovery_attempted_this_boot().contains(&id));
        assert!(!state.recovery_pending.read().unwrap().contains(&id));
        let (lock, candidates) =
            daemon_startup_recovery_mark(state.clone(), &std::collections::HashSet::new())
                .await
                .expect("repaired row becomes eligible");
        let selected = candidates.iter().find(|row| row.id == id).unwrap();
        assert_eq!(
            std::path::Path::new(&selected.project_path),
            repaired.path()
        );
        drop(lock);
        assert!(crate::session::recovery::recovery_attempted_this_boot().contains(&id));
        assert!(
            daemon_startup_recovery_mark(state, &std::collections::HashSet::new())
                .await
                .is_none()
        );
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn recovery_refuses_a_superseded_candidate_without_publishing() {
        let _home = crate::session::test_support::isolate_app_dir();
        let mut row = crate::session::Instance::new("superseded recovery", "/tmp/recovery");
        row.source_profile = "test".into();
        row.lifecycle_generation = 1;
        row.status = crate::session::Status::Stopped;
        test_support::seed_instances_on_disk_for_test("test", vec![row.clone()]);
        let state = test_support::build_test_app_state(vec![row.clone()]);
        test_support::refresh_canonical_metadata_for_test(&state).await;
        let mut rx = state.status_tx.subscribe();
        let response =
            crate::server::api::sessions::recovery_restart(state.clone(), row.id.clone(), 0).await;
        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
        assert!(rx.try_recv().is_err());
        let current = state.instances.read().await;
        assert_eq!(current[0].lifecycle_generation, 1);
        assert_eq!(current[0].status, crate::session::Status::Stopped);
        assert!(current[0].lifecycle_reservation.is_none());
        let disk = test_support::load_instances_from_disk_for_test("test");
        assert_eq!(disk[0].lifecycle_generation, 1);
        assert!(disk[0].lifecycle_reservation.is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn recovery_refuses_structured_rows_without_mutation_or_launch() {
        use crate::session::{Instance, Status, View};
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        for status in [
            Status::Running,
            Status::Idle,
            Status::Waiting,
            Status::Stopped,
        ] {
            let mut row = Instance::new("structured recovery", project.path().to_str().unwrap());
            row.source_profile = "test".into();
            row.view = View::Structured;
            row.status = status;
            row.agent_session_id = Some(uuid::Uuid::new_v4().to_string());
            test_support::seed_instances_on_disk_for_test("test", vec![row.clone()]);
            let disk_before =
                serde_json::to_value(test_support::load_instances_from_disk_for_test("test"))
                    .unwrap();
            let (launcher, launches) = test_support::counting_failing_launcher();
            let state =
                test_support::build_test_app_state_with_launcher(vec![row.clone()], launcher);
            test_support::refresh_canonical_metadata_for_test(&state).await;
            let before = serde_json::to_value(&*state.instances.read().await).unwrap();
            let mut updates = state.status_tx.subscribe();
            let response = crate::server::api::sessions::recovery_restart(
                state.clone(),
                row.id.clone(),
                row.lifecycle_generation,
            )
            .await;
            assert_eq!(
                response.status(),
                axum::http::StatusCode::CONFLICT,
                "{status:?}"
            );
            assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert!(updates.try_recv().is_err());
            assert_eq!(
                serde_json::to_value(&*state.instances.read().await).unwrap(),
                before
            );
            assert_eq!(
                serde_json::to_value(test_support::load_instances_from_disk_for_test("test"))
                    .unwrap(),
                disk_before
            );
            if status == Status::Running {
                let _response = crate::server::api::sessions::restart_session(
                    axum::extract::State(state),
                    axum::extract::Path(row.id),
                    Ok(Some(axum::Json(
                        crate::daemon::RestartSessionBody::default(),
                    ))),
                )
                .await;
                assert_eq!(
                    launches.load(std::sync::atomic::Ordering::SeqCst),
                    1,
                    "explicit restart still reaches the launcher"
                );
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn recovery_refuses_shelved_rows_after_the_candidate_snapshot() {
        use crate::session::{Instance, Status, View};
        let _home = crate::session::test_support::isolate_app_dir();
        for structured in [false, true] {
            for (shelve, code) in [
                (Instance::archive as fn(&mut Instance), "session_archived"),
                (Instance::trash as fn(&mut Instance), "session_trashed"),
            ] {
                for published in [false, true] {
                    let mut candidate = Instance::new("shelved recovery", "/tmp/recovery");
                    candidate.source_profile = "test".into();
                    candidate.view = if structured {
                        View::Structured
                    } else {
                        View::Terminal
                    };
                    candidate.status = Status::Running;
                    candidate.last_error = Some("previous diagnostic".into());
                    let mut shelved = candidate.clone();
                    shelve(&mut shelved);
                    test_support::seed_instances_on_disk_for_test("test", vec![shelved.clone()]);
                    let visible = if published {
                        shelved
                    } else {
                        candidate.clone()
                    };
                    let state = test_support::build_test_app_state(vec![visible.clone()]);
                    test_support::refresh_canonical_metadata_for_test(&state).await;
                    let disk_before = serde_json::to_value(
                        test_support::load_instances_from_disk_for_test("test"),
                    )
                    .unwrap();
                    let mut rx = state.status_tx.subscribe();
                    let response = crate::server::api::sessions::recovery_restart(
                        state.clone(),
                        candidate.id,
                        candidate.lifecycle_generation,
                    )
                    .await;
                    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
                    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        body["error"], code,
                        "structured={structured} published={published}"
                    );
                    assert!(
                        rx.try_recv().is_err(),
                        "refused recovery publishes no status edge"
                    );
                    let current = state.instances.read().await;
                    assert_eq!(
                        serde_json::to_value(&current[0]).unwrap(),
                        serde_json::to_value(visible).unwrap()
                    );
                    let disk_after = serde_json::to_value(
                        test_support::load_instances_from_disk_for_test("test"),
                    )
                    .unwrap();
                    assert_eq!(
                        disk_after, disk_before,
                        "recovery must not mutate shelved metadata"
                    );
                }
            }
        }
    }

    /// #2994 wiring test for `daemon_startup_recovery_mark` (Phase A).
    #[tokio::test]
    #[serial_test::serial]
    async fn daemon_recovery_ledger_and_scan_exclude_candidates() {
        if !crate::tmux::is_tmux_available() {
            eprintln!("skipping daemon_recovery_ledger_and_scan_exclude_candidates: no tmux");
            return;
        }

        // The recovery lock lives in the app dir; the shared one can be held by
        // another process running this test or a daemon.
        let _home = crate::session::test_support::isolate_app_dir();
        let ledger_dir = tempfile::tempdir().expect("tempdir");
        let _env = crate::session::test_support::EnvGuard::set(&[(
            crate::session::recovery::RECOVERY_ATTEMPT_DIR_ENV,
            ledger_dir.path(),
        )]);

        let unique = format!("{:012}", std::process::id());
        let mut inst_a = crate::session::Instance::new("orphan-wire-a", "/tmp/aoe-test-2994");
        inst_a.id = format!("wireA{unique}");
        inst_a.agent_session_id = Some(format!("55555555-5555-4555-8555-{unique}"));
        let id_a = inst_a.id.clone();
        assert!(
            crate::session::recovery::is_recovery_candidate(&inst_a),
            "precondition: the fixture must be a recovery candidate",
        );

        // Pass 1: no orphan, id_a unattempted -> included (and now marked).
        {
            let state = test_support::build_test_app_state(vec![inst_a.clone()]);
            let picked =
                daemon_startup_recovery_mark(state, &std::collections::HashSet::new()).await;
            let candidates = picked.map(|(_lock, c)| c).unwrap_or_default();
            assert!(
                candidates.iter().any(|c| c.id == id_a),
                "an unattempted, non-orphaned missing session must be a candidate",
            );
        }

        // Ledger case.
        let ledger_active =
            crate::session::recovery::recovery_attempted_this_boot().contains(&id_a);
        if ledger_active {
            let state = test_support::build_test_app_state(vec![inst_a.clone()]);
            let picked =
                daemon_startup_recovery_mark(state, &std::collections::HashSet::new()).await;
            let candidates = picked.map(|(_lock, c)| c).unwrap_or_default();
            assert!(
                !candidates.iter().any(|c| c.id == id_a),
                "an id attempted earlier this boot must be excluded (idempotent recovery)",
            );
        }

        // Scan case.
        let sid_b = format!("66666666-6666-4666-8666-{unique}");
        let mut inst_b = crate::session::Instance::new("orphan-wire-b", "/tmp/aoe-test-2994");
        inst_b.id = format!("wireB{unique}");
        inst_b.tool = "opencode".to_string();
        inst_b.agent_session_id = Some(sid_b.clone());
        let id_b = inst_b.id.clone();
        assert!(
            crate::session::recovery::is_recovery_candidate(&inst_b),
            "precondition: inst_b must be a recovery candidate",
        );

        // The sid rides as `$0` of a compound-list `sh` so it stays alive with
        // the sid in argv (visible via plain `ps`, no `-E` needed).
        let mut decoy = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 60; true")
            .arg(&sid_b)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn orphan decoy");

        // Wait until the decoy's argv is observable before running recovery.
        for _ in 0..100 {
            let flags = crate::process::processes_matching(
                &[String::new()],
                &[Some(sid_b.clone())],
                &[None],
            );
            if flags.first().copied().unwrap_or(false) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }

        let state = test_support::build_test_app_state(vec![inst_b.clone()]);
        let picked = daemon_startup_recovery_mark(state, &std::collections::HashSet::new()).await;
        let candidates = picked.map(|(_lock, c)| c).unwrap_or_default();

        let _ = decoy.kill();
        let _ = decoy.wait();

        assert!(
            !candidates.iter().any(|c| c.id == id_b),
            "a live orphan process must exclude the session from recovery candidates",
        );
    }
}
