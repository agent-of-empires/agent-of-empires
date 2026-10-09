//! What the daemon repairs on boot.

use std::sync::Arc;

use super::push::publish_status_change;
use super::state::AppState;

/// Startup auto-recovery for AI agent sessions whose tmux pane is missing after a daemon
/// restart or system reboot.
pub(super) async fn daemon_startup_recovery_mark(
    state: Arc<AppState>,
) -> Option<(
    crate::session::recovery::RecoveryLock,
    Vec<crate::session::Instance>,
)> {
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
                !has_live_tmux && crate::session::recovery::is_recovery_candidate(i)
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
        .unwrap_or_else(|_| vec![false; candidates.len()]);
        let mut idx = 0;
        candidates.retain(|i| {
            let alive = orphan_flags.get(idx).copied().unwrap_or(false);
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

/// What a finished recovery worker hands back for the live row.
enum RecoveryApplied {
    /// The worker's row, whether its cascade succeeded or failed.
    Updated(Box<crate::session::Instance>),
    /// The worker panicked; the row is stamped `Error`.
    Panicked(String),
}

/// Fold a finished recovery into the live row and publish the status move it made, so an online
/// plugin hears of it.
async fn apply_recovery_outcome(state: &AppState, id: &str, applied: RecoveryApplied) {
    let mut instances = state.instances.write().await;
    let Some(slot) = instances.iter_mut().find(|i| i.id == id) else {
        return;
    };
    let old_status = slot.status;
    match applied {
        RecoveryApplied::Updated(updated) => *slot = *updated,
        RecoveryApplied::Panicked(error) => {
            slot.status = crate::session::Status::Error;
            slot.last_error = Some(format!("recovery worker panicked: {error}"));
            // Same stickiness arming as a failed cascade.
            slot.last_error_check = Some(std::time::Instant::now());
        }
    }
    publish_status_change(&state.status_tx, slot, old_status);
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
                        !has_live_tmux && crate::session::recovery::is_recovery_candidate(i)
                    })
                    .cloned()
            };
            // #2994.
            let still_candidate = match recheck_inst {
                Some(inst) => {
                    let alive = tokio::task::spawn_blocking(move || {
                        crate::session::recovery::orphaned_agent_process_alive(&inst)
                    })
                    .await
                    .unwrap_or(false);
                    !alive
                }
                None => false,
            };
            if !still_candidate {
                // Phase A pre-marked this id and seeded recovery_pending; without draining,
                // the refresher would keep re-stamping the mark and status_poll_loop would
                // suppress the real status even though we are not running a cascade.
                crate::session::recovery::drain_recovery_pending(
                    &inst_state.recovery_pending,
                    &inst_state.recently_restarted,
                    &id,
                );
                return;
            }

            // Phase A already marked this id, but re-mark now to refresh the timestamp so
            // the suppression window covers the full cascade latency starting from this
            // point rather than from the (possibly older) Phase A snapshot.
            crate::session::recovery::mark_recently_restarted(&inst_state.recently_restarted, &id);

            // Refresh the working snapshot from latest in-memory state.
            let mut working = {
                let instances = inst_state.instances.read().await;
                instances
                    .iter()
                    .find(|i| i.id == id)
                    .cloned()
                    .unwrap_or(inst)
            };
            let title = working.title.clone();
            let result = tokio::task::spawn_blocking(move || {
                let res = crate::session::recovery::run_recovery_for_instance(&mut working);
                (working, res)
            })
            .await;

            match result {
                Ok((updated, Ok(outcome))) => {
                    tracing::info!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        title = %title,
                        ?outcome,
                        "recovery completed",
                    );
                    let applied = RecoveryApplied::Updated(Box::new(updated));
                    apply_recovery_outcome(&inst_state, &id, applied).await;
                    // Release the suppression now that the cascade has succeeded and the
                    // pane is alive.
                    crate::session::recovery::drain_recovery_pending(
                        &inst_state.recovery_pending,
                        &inst_state.recently_restarted,
                        &id,
                    );
                }
                Ok((updated, Err(e))) => {
                    tracing::warn!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        title = %title,
                        error = %e,
                        "recovery cascade failed",
                    );
                    let applied = RecoveryApplied::Updated(Box::new(updated));
                    apply_recovery_outcome(&inst_state, &id, applied).await;
                    // Release the suppression so the next poll respects the Error state
                    // instead of forcing Status::Starting for the rest of the TTL window.
                    crate::session::recovery::drain_recovery_pending(
                        &inst_state.recovery_pending,
                        &inst_state.recently_restarted,
                        &id,
                    );
                }
                Err(join_err) => {
                    tracing::error!(
                        target: "session.startup_recovery",
                        instance_id = %id,
                        title = %title,
                        error = %join_err,
                        "recovery worker panicked",
                    );
                    apply_recovery_outcome(
                        &inst_state,
                        &id,
                        RecoveryApplied::Panicked(join_err.to_string()),
                    )
                    .await;
                    // Same suppression release as above.
                    crate::session::recovery::drain_recovery_pending(
                        &inst_state.recovery_pending,
                        &inst_state.recently_restarted,
                        &id,
                    );
                }
            }
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
mod tests {
    use super::*;
    use crate::server::test_support;
    use crate::session::Status;

    fn recovering(status: Status) -> crate::session::Instance {
        let mut inst = crate::session::Instance::new("recovering", "/tmp/aoe-recovery-publish");
        inst.source_profile = "default".to_string();
        inst.status = status;
        inst
    }

    /// The recovery cascade writes the live row itself, so it must publish the status move: if a
    /// poll tick reported Starting and recovery lands Error between ticks, the next tick snapshots
    /// Error and an online plugin would never hear of it.
    #[tokio::test]
    async fn a_finished_recovery_publishes_the_status_move_it_makes() {
        // (live status, status the worker returned, expected move)
        let cases = [
            (
                Status::Starting,
                Status::Error,
                Some((Status::Starting, Status::Error)),
            ),
            (
                Status::Running,
                Status::Starting,
                Some((Status::Running, Status::Starting)),
            ),
            (Status::Error, Status::Error, None),
        ];
        for (live, returned, want) in cases {
            let inst = recovering(live);
            let state = test_support::build_test_app_state(vec![inst.clone()]);
            let mut rx = state.status_tx.subscribe();
            let mut updated = inst.clone();
            updated.status = returned;

            let applied = RecoveryApplied::Updated(Box::new(updated));
            apply_recovery_outcome(&state, &inst.id, applied).await;

            let got = rx.try_recv().ok().map(|c| (c.old, c.new));
            assert_eq!(got, want, "live={live:?} returned={returned:?}");
            assert_eq!(state.instances.read().await[0].status, returned);
        }
    }

    #[tokio::test]
    async fn a_panicked_recovery_publishes_the_error_it_stamps() {
        let inst = recovering(Status::Starting);
        let state = test_support::build_test_app_state(vec![inst.clone()]);
        let mut rx = state.status_tx.subscribe();

        apply_recovery_outcome(&state, &inst.id, RecoveryApplied::Panicked("boom".into())).await;

        let change = rx.try_recv().expect("the Error stamp is published");
        assert_eq!((change.old, change.new), (Status::Starting, Status::Error));
        let row = state.instances.read().await[0].clone();
        assert_eq!(
            row.last_error.as_deref(),
            Some("recovery worker panicked: boom")
        );
        assert!(row.last_error_check.is_some(), "the stickiness is armed");
    }

    #[tokio::test]
    async fn a_recovery_for_a_vanished_row_publishes_nothing() {
        let state = test_support::build_test_app_state(Vec::new());
        let mut rx = state.status_tx.subscribe();

        apply_recovery_outcome(&state, "gone", RecoveryApplied::Panicked("boom".into())).await;

        assert!(rx.try_recv().is_err());
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
            let picked = daemon_startup_recovery_mark(state).await;
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
            let picked = daemon_startup_recovery_mark(state).await;
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
        let picked = daemon_startup_recovery_mark(state).await;
        let candidates = picked.map(|(_lock, c)| c).unwrap_or_default();

        let _ = decoy.kill();
        let _ = decoy.wait();

        assert!(
            !candidates.iter().any(|c| c.id == id_b),
            "a live orphan process must exclude the session from recovery candidates",
        );
    }
}
