//! What the daemon repairs on boot: rows left behind by a crash or an
//! unclean shutdown.

use std::sync::Arc;

use super::state::AppState;

/// Startup auto-recovery for AI agent sessions whose tmux pane is missing
/// after a daemon restart or system reboot.
///
/// Acquires the cross-process recovery lock; if another process holds it
/// (TUI in standalone mode, or a peer daemon), this returns without doing
/// anything. The lock is held for the entire pass so a late-starting peer
/// cannot duplicate cascades.
///
/// For each candidate:
/// 1. Acquire the per-instance `instance_lock` (serialises against any
///    `ensure_session` REST call that arrives concurrently).
/// 2. Mark `recently_restarted` BEFORE the cascade so the
///    `status_poll_loop` suppression window covers the entire ~7s
///    worst-case latency.
/// 3. Run `restart_with_size_opts(None, false)` via `spawn_blocking`.
/// 4. Update `state.instances` in place with the post-cascade `Instance`.
///
/// Concurrency is capped at `recovery::STARTUP_RECOVERY_CONCURRENCY` to
/// bound cold-start latency without thundering-herd-ing tmux at server
/// warm-up.
/// Phase A: acquire the cross-process lock, warm tmux, snapshot the
/// candidate set, and pre-mark every candidate in `recently_restarted`.
///
/// Returning the marked candidates synchronously (before
/// `status_poll_loop` is spawned) closes the first-tick race where the
/// poller's immediate first iteration could observe missing tmux state
/// and broadcast a phantom Idle->Error transition before any worker
/// has had a chance to mark.
///
/// Uses `batch_pane_metadata()` instead of per-instance probes to keep
/// the listener-bind path under ~20ms regardless of session count.
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
    // On probe failure we cannot distinguish "all panes dead" from "tmux
    // unreachable", and treating the latter as the former would trigger
    // spurious recovery cascades that kill possibly-alive panes. Skip
    // the entire pass on Err; the next daemon launch will retry.
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

    // #2994 (deterministic): drop any session already attempted this boot. The
    // boot-scoped ledger makes startup recovery idempotent per boot for every
    // agent, so a prior pass that resumed (then whose owner exited) cannot be
    // duplicated here regardless of whether the orphan is still identifiable.
    let attempted = crate::session::recovery::recovery_attempted_this_boot();
    candidates.retain(|i| !attempted.contains(&i.id));

    // #2994 (defense-in-depth): also skip sessions whose agent is positively
    // still alive on a tmux server this daemon can no longer see (orphaned
    // socket). The batched process-table scan runs in `spawn_blocking` and only
    // after the `instances` read lock is dropped, so its heavy synchronous I/O
    // cannot stall the executor or block REST writers.
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
    // Seed the pending set so the refresher (spawned between Phase A and
    // Phase B) keeps these marks fresh while candidates wait on a
    // STARTUP_RECOVERY_CONCURRENCY permit. Each worker drains its own id on
    // completion.
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

            // Re-check both `is_recovery_candidate` AND tmux liveness after
            // acquiring the lock: between the snapshot and this point a
            // REST handler (e.g. ensure_session) could have toggled
            // `structured view` OR brought the tmux pane back. Without the
            // tmux re-check, recovery would `kill_clean` a freshly-started
            // pane the user just attached to. The lock + this re-check
            // serialise against any other AoE writer.
            //
            // Use the fallible `batch_pane_metadata()` here so a transient
            // tmux probe failure does NOT collapse to "pane dead" and
            // wrongly proceed with the cascade: skip + unmark instead.
            // Mirrors Phase A's pattern at the mark site.
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
            // #2994: re-check the orphan guard *outside* the lock and inside
            // `spawn_blocking` so an agent still alive on an invisible tmux
            // server is not duplicated by the cascade, without the process-table
            // scan stalling the executor or blocking REST writers on the
            // `instances` lock. The boot ledger is intentionally not re-checked
            // here: Phase A already recorded this id, so re-reading it would
            // self-skip every candidate.
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
                // Phase A pre-marked this id and seeded recovery_pending;
                // without draining, the refresher would keep re-stamping the
                // mark and status_poll_loop would suppress the real status
                // even though we are not running a cascade.
                crate::session::recovery::drain_recovery_pending(
                    &inst_state.recovery_pending,
                    &inst_state.recently_restarted,
                    &id,
                );
                return;
            }

            // Phase A already marked this id, but re-mark now to refresh
            // the timestamp so the suppression window covers the full
            // cascade latency starting from this point rather than from
            // the (possibly older) Phase A snapshot.
            crate::session::recovery::mark_recently_restarted(&inst_state.recently_restarted, &id);

            // Refresh the working snapshot from latest in-memory state.
            // Between Phase A's snapshot and acquiring instance_lock, a
            // serialised REST writer (ensure_session, set-session-id, etc.)
            // could have mutated this instance. Without the refresh, the
            // final `*slot = updated` would silently revert that writer's
            // changes (e.g. a freshly-set agent_session_id).
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
                    let mut instances = inst_state.instances.write().await;
                    if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
                        *slot = updated;
                    }
                    drop(instances);
                    // Release the suppression now that the cascade has
                    // succeeded and the pane is alive. Without this, the
                    // next `status_poll_loop` tick (within 2s) would force
                    // `Status::Starting` for the rest of the TTL window,
                    // broadcasting a phantom `Idle -> Starting` transition
                    // followed by `Starting -> Idle/Running` at TTL expiry.
                    // The suppression's purpose is to cover the in-cascade
                    // window where `last_start_time` is lost on the disk
                    // reload; once the cascade has finished the on-disk
                    // status is current and the poll path resolves to the
                    // correct status without help.
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
                    let mut instances = inst_state.instances.write().await;
                    if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
                        *slot = updated;
                    }
                    drop(instances);
                    // Release the suppression so the next poll respects the
                    // Error state instead of forcing Status::Starting for
                    // the rest of the TTL window.
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
                    let mut instances = inst_state.instances.write().await;
                    if let Some(slot) = instances.iter_mut().find(|i| i.id == id) {
                        slot.status = crate::session::Status::Error;
                        slot.last_error = Some(format!("recovery worker panicked: {}", join_err));
                        // Same stickiness arming as the cascade-Err arm above.
                        slot.last_error_check = Some(std::time::Instant::now());
                    }
                    drop(instances);
                    // Same suppression release as above: without unmarking,
                    // the next poll forces Status::Starting and wipes the
                    // panic-specific last_error written above.
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

    // Completion sweep: every worker drains its own id on each exit arm
    // (including the spawn_blocking panic arm), but a panic in a worker's
    // async body *outside* that match would skip its drain and leave the id
    // pending, so the refresher would re-stamp it until daemon shutdown. By
    // the time the JoinSet is fully drained every worker has terminated, so
    // sweeping all ids guarantees `recovery_pending` is empty and the
    // refresher exits on its next tick. Idempotent for ids already drained.
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

    /// #2994 wiring test for `daemon_startup_recovery_mark` (Phase A). Proves
    /// the two guards are consulted in the real daemon recovery path, not merely
    /// as standalone predicates:
    ///
    /// - **ledger (deterministic):** an id already attempted this boot is
    ///   excluded, so a second pass cannot duplicate it (the #2994 crash-then-
    ///   re-run scenario). Covers every agent, needle or not.
    /// - **process scan (defense-in-depth):** an id whose agent is positively
    ///   still alive (a live `sleep` carrying `AOE_INSTANCE_ID=<id>`, the marker
    ///   aoe injects into a resumed agent) is excluded.
    ///
    /// Deterministic without reproducing the `/tmp`-wipe. The ledger is isolated
    /// to a tempdir via `AOE_RECOVERY_ATTEMPT_DIR`, so it never touches real
    /// user state. The TUI path gates on the same two calls.
    #[tokio::test]
    #[serial_test::serial]
    async fn daemon_recovery_ledger_and_scan_exclude_candidates() {
        if !crate::tmux::is_tmux_available() {
            eprintln!("skipping daemon_recovery_ledger_and_scan_exclude_candidates: no tmux");
            return;
        }

        let ledger_dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var(
            crate::session::recovery::RECOVERY_ATTEMPT_DIR_ENV,
            ledger_dir.path(),
        );

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

        // Ledger case: with id_a now recorded this boot, a second pass must
        // exclude it deterministically (only assert when the ledger is active
        // on this host, i.e. a boot id was resolvable).
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

        // Scan case: a distinct id_b (never attempted) whose agent is positively
        // alive must be excluded by the process scan. Uses a non-hook agent
        // (opencode) whose sid the decoy carries in argv, so detection goes
        // through the cross-platform cmdline needle rather than `ps -E` env
        // visibility, which a hardened macOS can hide (#3006 review).
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
        std::env::remove_var(crate::session::recovery::RECOVERY_ATTEMPT_DIR_ENV);

        assert!(
            !candidates.iter().any(|c| c.id == id_b),
            "a live orphan process must exclude the session from recovery candidates",
        );
    }
}
