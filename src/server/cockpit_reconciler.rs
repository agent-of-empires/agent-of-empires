//! Cockpit worker reconciler. Runs every 2s tick (and on cold start,
//! the first tick fires immediately) to reconcile on-disk session
//! state against the supervisor's live worker pool.
//!
//! Responsibilities:
//!
//! 1. Honor the master switch (`cockpit.enabled`) and the
//!    `aoe cockpit stop|kill|restart` side-channel.
//! 2. Sweep orphan registry entries whose session is gone.
//! 3. For every cockpit-mode session without a live worker, run a
//!    resume task: reattach to an existing runner if one is alive,
//!    otherwise fresh-spawn the agent.
//!
//! The resume tasks run in parallel under a `tokio::sync::Semaphore`
//! cap derived from `cockpit.max_concurrent_resumes` (default 4,
//! clamped to `max_concurrent_workers`). The supervisor's per-agent
//! install gate (see `Supervisor::spawn`) serialises only the first
//! spawn of each agent per daemon lifetime so the claude-agent-acp
//! lazy-install race never bites; every subsequent spawn for that
//! agent runs in parallel. See #1088.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;

use super::AppState;

/// Per-target resume outcome. Drives whether the reconciler should
/// retry on the next tick or leave `attempted` set so the same target
/// isn't poked every 2s.
#[derive(Debug, Clone)]
enum ResumeOutcome {
    /// Reattach succeeded; nothing else to do for this id.
    Attached,
    /// Reattach timed out; the orphan registry entry was swept and the
    /// reconciler should drop the id from `attempted` so the next tick
    /// can try a fresh spawn cleanly.
    RetryAfterAttachTimeout,
    /// Fresh spawn finished, with or without error. `attempted` stays
    /// populated; a permanently-failing spawn (e.g. missing
    /// claude-agent-acp) does not loop forever.
    SpawnFinished,
}

/// A single cockpit session that needs a worker. Snapshotted from the
/// instance list under the outer read lock so the parallel resume
/// tasks don't have to re-take it.
#[derive(Clone)]
struct ResumeTarget {
    id: String,
    tool: String,
    agent_override: Option<String>,
    model: Option<String>,
    project_path: String,
    stored_acp_session_id: Option<String>,
    source_profile: String,
    in_flight_turn: bool,
    yolo_mode: bool,
}

/// Tuple shape used by the instance-list snapshot. Aliased to dodge
/// clippy::type_complexity since the columns are fixed by the
/// upstream `Instance` schema.
type RawTargetTuple = (
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    String,
    bool,
);

pub async fn reconcile_cockpit_workers(
    state: &Arc<AppState>,
    attempted: &mut HashSet<String>,
    last_idle_reap: &mut Option<std::time::Instant>,
) {
    // Honor `cockpit.enabled = false` from config.toml — the persistent
    // master switch. Mirrored as an atomic; `PATCH /api/cockpit/master`
    // flips it live without restarting `aoe serve`.
    if !state
        .cockpit_master_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }

    // Detect `aoe cockpit stop|kill|restart` (a separate process that
    // deletes the registry entry + SIGTERMs the runner) and surface it
    // as a typed Stopped event. The daemon's protocol-layer connection
    // task blocks on `cmd_rx.recv()` while idle, so socket EOF doesn't
    // propagate to the drain task on its own — without this poll, the
    // UI stays stuck on "thinking" and the supervisor keeps a phantom
    // worker. For the `restart` case, the reaper returns the ids it
    // marked as `restart_pending`; clear them from `attempted` so the
    // spawn pass below treats them as fresh and the next 2s tick
    // reattaches with the cached `acp_session_id`.
    let restart_pending = state.cockpit_supervisor.reap_user_stopped().await;
    for id in &restart_pending {
        attempted.remove(id);
    }

    // Idle auto-stop (#1689). Cadence-gated to IDLE_REAP_INTERVAL so the
    // batched activity query does not run on every 2s tick. Runs BEFORE
    // the resume snapshot below: a worker marked dormant here is excluded
    // from this same tick's respawn pass by the `!i.is_idle_dormant()`
    // filter. The idle threshold is resolved per session profile inside
    // `reap_idle_workers`; `auto_stop_idle_secs == 0` (the default)
    // disables the feature for sessions on that profile.
    if last_idle_reap.is_none_or(|t| t.elapsed() >= IDLE_REAP_INTERVAL) {
        reap_idle_workers(state).await;
        *last_idle_reap = Some(std::time::Instant::now());
    }

    // Snapshot per-target resume inputs under the instances read lock.
    // We then drop the lock so the parallel resume tasks (each ~3s for
    // a fresh spawn) don't pin it.
    //
    // Triaged sessions (archived or currently-snoozed) are excluded from
    // the resume targets so the reconciler does not race the web
    // archive/snooze handler's worker teardown. Without this skip, the
    // 2s tick would respawn an archived cockpit worker immediately after
    // the API handler shuts it down, defeating the archive semantics.
    // Expired snoozes naturally rejoin via `is_snoozed()` returning
    // false past the deadline. See #1581.
    let raw_targets: Vec<RawTargetTuple> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.cockpit_mode && !i.is_archived() && !i.is_snoozed() && !i.is_idle_dormant()
            })
            .map(|i| {
                (
                    i.id.clone(),
                    i.tool.clone(),
                    i.cockpit_agent.clone(),
                    i.cockpit_model.clone(),
                    i.project_path.clone(),
                    i.cockpit_acp_session_id.clone(),
                    i.source_profile.clone(),
                    i.yolo_mode,
                )
            })
            .collect()
    };

    let live: HashSet<&String> = raw_targets.iter().map(|t| &t.0).collect();
    attempted.retain(|id| live.contains(id));

    // ORDERING INVARIANT: this orphan sweep MUST run before the
    // resume scheduling pass below. The capacity check counts both
    // in-memory workers AND on-disk registry entries (so a fresh
    // daemon can't race the reconciler and over-spawn). If the sweep
    // ran after, dead-PID entries from a previous unclean shutdown
    // would still count toward `max_concurrent_workers` and could
    // block legitimate spawns until the next tick. Do not reorder.
    sweep_orphan_workers(state, &live).await;

    // Build the work list. Skip ids already in `attempted` (a
    // permanently-failing spawn shouldn't loop every tick) and ids the
    // supervisor already knows about (REST-triggered spawn or
    // already-attached). For the rest, decide attach vs fresh-spawn at
    // task time so concurrent tasks see consistent registry state.
    let mut tasks: Vec<ResumeTarget> = Vec::new();
    for (
        id,
        tool,
        agent_override,
        model,
        project_path,
        stored_acp_session_id,
        source_profile,
        yolo_mode,
    ) in raw_targets
    {
        if attempted.contains(&id) {
            continue;
        }
        if state.cockpit_supervisor.is_running(&id).await {
            // A REST-triggered spawn (POST /api/sessions or
            // /api/cockpit/sessions/:id/enable) already owns the worker;
            // record the id so we don't poll is_running every tick.
            attempted.insert(id);
            continue;
        }
        // Rate-limit park: if the most recent lifecycle event for this
        // session is `Stopped { reason: "rate_limited" }`, the previous
        // worker exited because the adapter hit a quota. Auto-resuming
        // would `session/load` and immediately fail the next prompt the
        // same way; on daemon restart that would undo the entire #1281
        // fix. Hold the session parked until the user explicitly retries
        // via `/cockpit/spawn` or hands off via `/cockpit/switch-agent`.
        // SQLite call wrapped in spawn_blocking to match the
        // has_in_flight_turn pattern below; the reconciler runs on the
        // tokio runtime and these queries can stall under load.
        let store = Arc::clone(&state.cockpit_event_store);
        let id_for_status = id.clone();
        let latest_status =
            tokio::task::spawn_blocking(move || store.latest_status_event(&id_for_status))
                .await
                .unwrap_or(None);
        if let Some(crate::cockpit::Event::Stopped { reason }) = latest_status {
            if reason == "rate_limited" {
                tracing::debug!(
                    target: "cockpit.supervisor",
                    session = %id,
                    "skipping auto-resume: latest lifecycle event is Stopped{{rate_limited}}"
                );
                attempted.insert(id);
                continue;
            }
        }
        let store = Arc::clone(&state.cockpit_event_store);
        let id_owned = id.clone();
        let in_flight_turn =
            match tokio::task::spawn_blocking(move || store.has_in_flight_turn(&id_owned)).await {
                Ok(v) => v,
                Err(e) => {
                    // `attempted.insert` below runs unconditionally, so a swallowed
                    // panic does not produce a retry storm; the only consequence is
                    // the synthetic Stopped fanout is skipped this tick and the UI
                    // may stay "thinking" until the next live event.
                    tracing::warn!(
                        target: "cockpit.supervisor",
                        session_id = %id,
                        error = %e,
                        "in-flight turn probe blocking task failed; assuming no in-flight turn"
                    );
                    false
                }
            };
        // Mark before spawning so the next 2s tick doesn't double-poke
        // while the parallel resume task is still in flight. A task
        // that returns RetryAfterAttachTimeout will clear itself below.
        attempted.insert(id.clone());
        tasks.push(ResumeTarget {
            id,
            tool,
            agent_override,
            model,
            project_path,
            stored_acp_session_id,
            source_profile,
            in_flight_turn,
            yolo_mode,
        });
    }

    if tasks.is_empty() {
        return;
    }

    // Resume concurrency cap. Bounded by total worker capacity so this
    // setting can never exceed `max_concurrent_workers`. Floor at 1
    // so a misconfigured zero doesn't deadlock the reconciler.
    let cfg = crate::session::profile_config::resolve_config_or_warn(&state.profile);
    let resume_limit = cfg
        .cockpit
        .max_concurrent_resumes
        .min(cfg.cockpit.max_concurrent_workers)
        .max(1);
    let semaphore = Arc::new(Semaphore::new(resume_limit as usize));

    let mut set: JoinSet<(String, ResumeOutcome)> = JoinSet::new();
    for target in tasks {
        let state = Arc::clone(state);
        let sem = Arc::clone(&semaphore);
        set.spawn(async move {
            // Permit acquire is the only thing keeping us under the
            // cap; on shutdown the semaphore is dropped and acquire
            // returns Err, which we treat as "nothing to do".
            let _permit = match sem.acquire().await {
                Ok(p) => p,
                Err(_) => return (target.id, ResumeOutcome::SpawnFinished),
            };
            let id = target.id.clone();
            let outcome = resume_one(state, target).await;
            (id, outcome)
        });
    }

    while let Some(result) = set.join_next().await {
        match result {
            Ok((id, ResumeOutcome::RetryAfterAttachTimeout)) => {
                attempted.remove(&id);
            }
            Ok((_, ResumeOutcome::Attached)) | Ok((_, ResumeOutcome::SpawnFinished)) => {}
            Err(e) => {
                // Task panicked or was cancelled. Don't keep retrying
                // the same id every tick if the task panics on every
                // run; the `attempted` insert above already protects
                // us. Log so operators see it.
                tracing::error!(
                    target: "cockpit.supervisor",
                    "resume task panicked: {e}"
                );
            }
        }
    }
}

/// How often the idle-reap pass actually runs. The reconciler ticks
/// every 2s, but the idle threshold is measured in hours, so reaping on
/// every tick would hammer SQLite for no benefit; this gates the batched
/// activity query to a coarse cadence. See #1689.
const IDLE_REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Pure idle-reap decision. A cockpit worker is auto-stopped only when the
/// feature is enabled (`threshold_secs > 0`), it is not mid-turn, and its
/// last recorded event is at least `threshold_secs` old. A session with no
/// events (`last_event_ms == None`) is never reaped, so a freshly-spawned
/// worker without history survives. Extracted from `reap_idle_workers` so
/// the policy is unit-testable without a live supervisor or DB. See #1689.
fn should_auto_stop(
    now_ms: i64,
    last_event_ms: Option<i64>,
    threshold_secs: u32,
    in_flight: bool,
) -> bool {
    if threshold_secs == 0 || in_flight {
        return false;
    }
    match last_event_ms {
        Some(ms) => now_ms.saturating_sub(ms) >= i64::from(threshold_secs) * 1000,
        None => false,
    }
}

/// Idle auto-stop pass (#1689). Shuts down cockpit workers that have seen
/// no activity for `idle_secs` and are not mid-turn, marking their
/// session dormant so the resume pass does not respawn them. The next
/// user prompt clears dormancy (via `Instance::touch_last_accessed`) and
/// the following reconciler tick spawns a fresh worker.
///
/// Ordering and races: dormancy is persisted BEFORE the worker is shut
/// down, so a persist failure leaves the worker alive instead of orphaning
/// a still-running worker the next tick would respawn. `has_in_flight_turn`
/// is re-checked immediately before shutdown to avoid killing a worker a
/// prompt started in the gap since the candidate snapshot.
async fn reap_idle_workers(state: &Arc<AppState>) {
    // Candidates: cockpit sessions not already sunk/dormant. Snapshot
    // (id, profile) under the read lock so we don't hold it across awaits.
    let candidates: Vec<(String, String)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.cockpit_mode && !i.is_archived() && !i.is_snoozed() && !i.is_idle_dormant()
            })
            .map(|i| (i.id.clone(), i.source_profile.clone()))
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    // Resolve auto_stop_idle_secs per distinct profile (config touches
    // disk, so resolve off-thread, once per profile). Each session is
    // reaped against its OWN profile's threshold, not the daemon's.
    let distinct_profiles: Vec<String> = {
        let mut seen = HashSet::new();
        candidates
            .iter()
            .map(|(_, p)| p.clone())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    };
    let idle_by_profile: std::collections::HashMap<String, u32> =
        tokio::task::spawn_blocking(move || {
            distinct_profiles
                .into_iter()
                .map(|p| {
                    let secs = crate::session::profile_config::resolve_config_or_warn(&p)
                        .cockpit
                        .auto_stop_idle_secs;
                    (p, secs)
                })
                .collect()
        })
        .await
        .unwrap_or_default();
    // Keep only sessions whose profile enables idle auto-stop and that
    // have a live worker; nothing to reap otherwise.
    let mut live: Vec<(String, String, u32)> = Vec::new();
    for (id, profile) in candidates {
        let idle_secs = idle_by_profile.get(&profile).copied().unwrap_or(0);
        if idle_secs == 0 {
            continue;
        }
        if state.cockpit_supervisor.is_running(&id).await {
            live.push((id, profile, idle_secs));
        }
    }
    if live.is_empty() {
        return;
    }
    // One batched query for the latest event timestamp per candidate.
    let ids: Vec<String> = live.iter().map(|(id, _, _)| id.clone()).collect();
    let store = Arc::clone(&state.cockpit_event_store);
    let latest = match tokio::task::spawn_blocking(move || store.last_event_at_for_sessions(&ids))
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(target: "cockpit.supervisor", error = %e, "idle-reap activity query failed");
            return;
        }
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    for (id, profile, idle_secs) in live {
        // Cheap pre-check (no in-flight probe yet): skips sessions with no
        // history or still within the idle window. Sessions with no events
        // are never reaped, so a freshly-spawned worker is safe.
        let last_ms = latest.get(&id).copied();
        if !should_auto_stop(now_ms, last_ms, idle_secs, false) {
            continue;
        }
        // Re-check mid-turn right before stopping: a turn may have started
        // since the snapshot. spawn_blocking matches the SQLite-on-tokio
        // pattern used by the resume pass above.
        let store = Arc::clone(&state.cockpit_event_store);
        let id_probe = id.clone();
        let in_flight = tokio::task::spawn_blocking(move || store.has_in_flight_turn(&id_probe))
            .await
            .unwrap_or(false);
        if !should_auto_stop(now_ms, last_ms, idle_secs, in_flight) {
            continue;
        }
        // Mark dormant in-memory so this tick's resume snapshot skips it.
        {
            let mut instances = state.instances.write().await;
            match instances.iter_mut().find(|i| i.id == id) {
                Some(inst) => inst.mark_idle_dormant(),
                None => continue,
            }
        }
        // Persist BEFORE shutdown: a daemon restart must keep the worker
        // stopped, and if persistence fails we must not orphan a killed
        // worker that the next tick would respawn.
        let persisted = if let Ok(storage) = crate::session::Storage::new(&profile) {
            let id_persist = id.clone();
            tokio::task::spawn_blocking(move || {
                storage.update(|instances, _groups| {
                    if let Some(inst) = instances.iter_mut().find(|i| i.id == id_persist) {
                        inst.mark_idle_dormant();
                    }
                    Ok(())
                })
            })
            .await
            .map(|r| r.is_ok())
            .unwrap_or(false)
        } else {
            false
        };
        if !persisted {
            // Roll back the in-memory mark and leave the worker alive; retry
            // on the next interval.
            let mut instances = state.instances.write().await;
            if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                inst.idle_dormant_since = None;
            }
            tracing::warn!(
                target: "cockpit.supervisor",
                session = %id,
                "idle-reap persist failed; leaving worker alive"
            );
            continue;
        }
        match state.cockpit_supervisor.shutdown_idle(&id).await {
            Ok(()) | Err(crate::cockpit::supervisor::SupervisorError::UnknownSession(_)) => {
                tracing::info!(
                    target: "cockpit.supervisor",
                    session = %id,
                    idle_secs,
                    "auto-stopped idle cockpit worker"
                );
            }
            Err(e) => {
                // Shutdown failed and the worker may still be running. Clear
                // the dormant marker (in-memory + on disk) so future reap and
                // respawn passes are not permanently blocked for this session
                // by the resume snapshot's `!is_idle_dormant()` filter. Only
                // UnknownSession (handled above) means the worker is truly gone.
                {
                    let mut instances = state.instances.write().await;
                    if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                        inst.idle_dormant_since = None;
                    }
                }
                if let Ok(storage) = crate::session::Storage::new(&profile) {
                    let id_clear = id.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        storage.update(|instances, _groups| {
                            if let Some(inst) = instances.iter_mut().find(|i| i.id == id_clear) {
                                inst.idle_dormant_since = None;
                            }
                            Ok(())
                        })
                    })
                    .await;
                }
                tracing::warn!(
                    target: "cockpit.supervisor",
                    session = %id,
                    "idle-reap shutdown failed; cleared dormant marker: {e}"
                );
            }
        }
    }
}

async fn resume_one(state: Arc<AppState>, target: ResumeTarget) -> ResumeOutcome {
    let ResumeTarget {
        id,
        tool,
        agent_override,
        model,
        project_path,
        stored_acp_session_id,
        source_profile,
        in_flight_turn,
        yolo_mode,
    } = target;

    // Reattach path: if a previous daemon detached a runner for this
    // session and the runner is still alive, dial its socket instead
    // of spawning a fresh agent. Bounded by the registry probe — no
    // network IO unless we have a live PID + socket on disk.
    if let Ok(Some(record)) = crate::cockpit::worker_registry::load(&id) {
        if crate::cockpit::worker_registry::is_record_live(&record) {
            let supervisor = Arc::clone(&state.cockpit_supervisor);
            let cwd = PathBuf::from(&project_path);
            // Reconstruct sandbox context from the live instance state
            // so the reattached session's fs/terminal handlers can
            // still route across the container boundary.
            let sandbox_for_attach = {
                let instances = state.instances.read().await;
                instances
                    .iter()
                    .find(|i| i.id == id)
                    .and_then(|i| i.sandbox_info.clone())
            };
            let attach_res = timeout(
                Duration::from_secs(3),
                supervisor.attach(id.clone(), cwd, vec![], in_flight_turn, sandbox_for_attach),
            )
            .await;
            match attach_res {
                Ok(Ok(())) => {
                    tracing::info!(
                        target: "cockpit.supervisor",
                        session = %id,
                        pid = record.pid,
                        in_flight_turn,
                        "reattached to existing cockpit runner"
                    );
                    // The startup pass in `seed_cockpit_statuses`
                    // covers the cold-start case. Anything attached
                    // later (e.g. a session created after the daemon
                    // started) also needs its status seeded; the
                    // attach path's only sidebar-moving signal is the
                    // next live event, which can be many seconds
                    // away. Re-derive from history here too so the
                    // dot turns green immediately. See #1103 (A).
                    if in_flight_turn {
                        if let Some(event) = state.cockpit_event_store.latest_status_event(&id) {
                            if let Some(intent) = crate::server::derive_cockpit_status(&event) {
                                let mut instances = state.instances.write().await;
                                if let Some(inst) = instances.iter_mut().find(|i| i.id == id) {
                                    crate::server::apply_status_intent(
                                        inst,
                                        Some(intent),
                                        &state.status_tx,
                                    );
                                }
                            }
                        }
                    }
                    return ResumeOutcome::Attached;
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        target: "cockpit.supervisor",
                        session = %id,
                        "attach failed; falling back to fresh spawn: {e}"
                    );
                    crate::cockpit::worker_registry::delete(&id).ok();
                }
                Err(_) => {
                    tracing::warn!(
                        target: "cockpit.supervisor",
                        session = %id,
                        "attach timed out after 3s; falling back to fresh spawn"
                    );
                    crate::cockpit::worker_registry::delete(&id).ok();
                    return ResumeOutcome::RetryAfterAttachTimeout;
                }
            }
        } else {
            // Dead PID or missing socket: sweep the orphan registry
            // entry so the next attempt is a clean fresh spawn.
            crate::cockpit::worker_registry::delete(&id).ok();
        }
    }

    // Fresh-spawn fallback: we are about to spin up a brand new agent
    // process. The previous one (if any) was killed before it could
    // complete the in-flight prompt, so its turn is forever orphaned.
    // Publish a synthetic Stopped now so the UI doesn't keep
    // "thinking" after restart.
    if in_flight_turn {
        state
            .cockpit_supervisor
            .synthesize_stopped_for_orphan(&id, "orphaned_at_restart");
    }

    let supervisor = Arc::clone(&state.cockpit_supervisor);
    let agent = supervisor
        .pick_agent_for_tool(
            &tool,
            agent_override.as_deref(),
            &source_profile,
            std::path::Path::new(&project_path),
        )
        .await;
    let cwd = PathBuf::from(project_path);

    let inst_lock = state.instance_lock(&id).await;
    let sandbox_info = match crate::cockpit::sandbox::ensure_container_for_session(
        &state.instances,
        &inst_lock,
        &id,
        false,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            let message = format!("sandbox container ensure failed: {e}");
            tracing::warn!(
                target: "cockpit.supervisor",
                session = %id,
                "reconciler container ensure failed: {message}"
            );
            supervisor.publish_startup_error(&id, message);
            return ResumeOutcome::SpawnFinished;
        }
    };

    // Thread the session profile through regardless of sandboxing: the
    // spawn path resolves agent_cockpit_cmd and worker env from it, so a
    // non-sandbox session on a non-default profile must not fall back to
    // the default profile.
    let source_profile_for_spawn = Some(source_profile.clone());
    let spawn_result = supervisor
        .spawn(crate::cockpit::supervisor::SpawnRequest {
            session_id: id.clone(),
            agent: agent.clone(),
            cwd,
            additional_dirs: vec![],
            provider_env: vec![],
            model,
            stored_acp_session_id,
            sandbox_info,
            source_profile: source_profile_for_spawn,
            yolo_mode,
        })
        .await;
    if let Err(e) = spawn_result {
        // Re-check whether the session still exists in instances.
        // The user can delete a session during the spawn handshake
        // (2-3s for ACP), and the resulting error is noise for a
        // session that no longer exists. Demote to debug rather
        // than warn + AgentStartupError publish in that case.
        let still_present = state.instances.read().await.iter().any(|i| i.id == id);
        let message = format!("Failed to start cockpit agent {agent:?}: {e}");
        if still_present {
            tracing::warn!(
                target: "cockpit.supervisor",
                session = %id,
                agent = %agent,
                "auto-spawn reconciler failed: {message}"
            );
            supervisor.publish_startup_error(&id, message);
        } else {
            tracing::debug!(
                target: "cockpit.supervisor",
                session = %id,
                agent = %agent,
                "auto-spawn reconciler error after session removed (ignored): {message}"
            );
        }
    }
    ResumeOutcome::SpawnFinished
}

async fn sweep_orphan_workers(state: &Arc<AppState>, live: &HashSet<&String>) {
    // Sweep registry entries whose session no longer exists (deleted
    // while serve was down) and SIGTERM the orphan runner so the user
    // doesn't see a phantom in `aoe cockpit ps`. Only runs against
    // entries that aren't currently in our `workers` map.
    let Ok(records) = crate::cockpit::worker_registry::list() else {
        return;
    };
    for record in records {
        if live.contains(&record.session_id) {
            continue;
        }
        if state
            .cockpit_supervisor
            .is_running(&record.session_id)
            .await
        {
            continue;
        }
        tracing::info!(
            target: "cockpit.supervisor",
            session = %record.session_id,
            pid = record.pid,
            "sweeping orphan worker (no matching session on disk)"
        );
        #[cfg(unix)]
        if crate::cockpit::worker_registry::is_pid_alive(record.pid) {
            use nix::sys::signal::{kill, Signal};
            use nix::unistd::Pid;
            let _ = kill(Pid::from_raw(record.pid as i32), Signal::SIGTERM);
        }
        crate::cockpit::worker_registry::delete(&record.session_id).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::should_auto_stop;

    const HOUR_MS: i64 = 3_600_000;

    #[test]
    fn disabled_threshold_never_stops() {
        // threshold 0 = feature off; even a worker idle for a day survives.
        assert!(!should_auto_stop(HOUR_MS * 24, Some(0), 0, false));
    }

    #[test]
    fn in_flight_worker_is_never_stopped() {
        // Idle far past the threshold, but mid-turn: do not kill.
        assert!(!should_auto_stop(HOUR_MS * 24, Some(0), 3600, true));
    }

    #[test]
    fn idle_past_threshold_stops() {
        // Last event 2h ago, threshold 1h, not mid-turn: reap.
        assert!(should_auto_stop(HOUR_MS * 2, Some(0), 3600, false));
    }

    #[test]
    fn idle_within_threshold_survives() {
        // Last event 30min ago, threshold 1h: too soon.
        let now = HOUR_MS;
        let last = HOUR_MS / 2;
        assert!(!should_auto_stop(now, Some(last), 3600, false));
    }

    #[test]
    fn no_events_never_stops() {
        // A worker with no recorded events (fresh spawn) is never reaped.
        assert!(!should_auto_stop(HOUR_MS * 24, None, 3600, false));
    }

    #[test]
    fn exactly_at_threshold_stops() {
        // Boundary: elapsed == threshold reaps (>= comparison).
        assert!(should_auto_stop(3600 * 1000, Some(0), 3600, false));
    }
}
