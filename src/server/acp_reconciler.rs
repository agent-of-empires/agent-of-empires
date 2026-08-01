//! Acp worker reconciler. Runs every 2s tick (and on cold start,
//! the first tick fires immediately) to reconcile on-disk session
//! state against the supervisor's live worker pool.
//!
//! Responsibilities:
//!
//! 1. Honor the `aoe acp stop|kill|restart` side-channel.
//! 2. Sweep orphan registry entries whose session is gone.
//! 3. For every structured view-mode session without a live worker, run a
//!    resume task: reattach to an existing runner if one is alive,
//!    otherwise fresh-spawn the agent.
//!
//! The resume tasks run in parallel under a `tokio::sync::Semaphore`
//! cap of `MAX_CONCURRENT_RESUMES` (clamped to
//! `max_concurrent_workers`). The supervisor's per-agent
//! install gate (see `Supervisor::spawn`) serialises only the first
//! spawn of each agent per daemon lifetime so the claude-agent-acp
//! lazy-install race never bites; every subsequent spawn for that
//! agent runs in parallel. See #1088.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::timeout;

use super::session_service::SessionService;
use super::AppState;

/// Reconciler-side respawn budget. The reconciler is the only respawner
/// for sessions with no live in-memory handle (fresh spawns and
/// reattach-after-restart). Its sole anti-loop guard used to be the
/// `attempted` set, which the `RetryAfterAttachTimeout` arm clears every
/// tick, so a session stuck "registry-live-but-handshake-times-out" (or a
/// worker that crashes seconds after a fresh spawn) respawned forever with
/// no backoff and no visible error. We bound it the same way the
/// supervisor's in-memory drain watchdog does (`restart_history` +
/// `RESTART_WINDOW`): at most `RECONCILER_MAX_RESPAWNS_IN_WINDOW` resume
/// attempts per session inside `RECONCILER_RESPAWN_WINDOW`, then park the
/// session (publish one `AgentStartupError`) until an explicit retry. The
/// budget is deliberately looser than the supervisor's 3/60s: the
/// reconciler counts at the decision-to-act point (before the outcome is
/// known), so a healthy daemon restart plus one transient blip can spend
/// two attempts without being a loop. See #1945.
const RECONCILER_MAX_RESPAWNS_IN_WINDOW: usize = 5;
const RECONCILER_RESPAWN_WINDOW: Duration = Duration::from_secs(60);

/// Maximum acp worker resumes (spawn or attach) run in parallel on
/// `aoe serve` cold start. Node.js bootup is memory-heavy: 4 concurrent
/// claude-agent-acp processes are around 200-320MB transient. See #1088.
const MAX_CONCURRENT_RESUMES: u32 = 4;

/// Seconds added to the adapter-reported `resets_at` before rate-limit
/// auto-resume fires, absorbing clock skew and adapter jitter. The
/// minimum park window below still applies, so a buggy adapter reporting
/// a past `resets_at` cannot cause a tight respawn loop. See #1722.
const RATE_LIMIT_AUTO_RESUME_GRACE_SECS: u32 = 15;

/// Record a reconciler resume attempt for `id` at `now`, pruning entries
/// older than `RECONCILER_RESPAWN_WINDOW`, and report whether the session
/// has exhausted its respawn budget and should be parked. When the budget
/// is already spent the attempt is not recorded (the history stays pinned
/// at the cap and ages out naturally once the session is unparked). Pure
/// so the policy is unit-testable without a live daemon. See #1945.
fn record_and_check_respawn_budget(
    history: &mut HashMap<String, Vec<Instant>>,
    id: &str,
    now: Instant,
) -> bool {
    // Avoid an unconditional `id.to_string()` on the common hit path:
    // `entry(K)` takes the key by value, so it would allocate every tick.
    if !history.contains_key(id) {
        history.insert(id.to_string(), Vec::new());
    }
    let entry = history.get_mut(id).expect("inserted above when missing");
    entry.retain(|t| now.duration_since(*t) < RECONCILER_RESPAWN_WINDOW);
    if entry.len() >= RECONCILER_MAX_RESPAWNS_IN_WINDOW {
        return true;
    }
    entry.push(now);
    false
}

/// Drop every per-session reconciler budget/marker entry for `id` so an
/// explicit user retry (`aoe acp restart` or the #2109 "Update & restart")
/// starts from a clean slate: re-armed for a fresh spawn, un-parked, respawn
/// budget reset, and its capacity marker cleared so a repeat capacity block
/// re-publishes a fresh banner. The `is_running` branch deliberately does NOT
/// use this (it clears the same three budget maps but *inserts* into
/// `attempted`), so only the two reaper loops share this reset.
fn forget_session_budget(
    id: &str,
    attempted: &mut HashSet<String>,
    parked: &mut HashSet<String>,
    respawn_history: &mut HashMap<String, Vec<Instant>>,
    capacity_deferred: &mut HashSet<String>,
) {
    attempted.remove(id);
    parked.remove(id);
    respawn_history.remove(id);
    capacity_deferred.remove(id);
}

/// Build the banner published when a structured-view worker exhausts its
/// respawn budget and the session is parked. When the session's project_path
/// no longer exists on disk, every respawn is doomed for the same reason: the
/// working directory was moved or deleted, not the adapter. Embed the exact
/// `AcpError::ProjectPathMissing` Display text (`project path no longer exists:
/// <path>`) so the web banner regex routes to the moved-cwd remediation instead
/// of the misleading install-the-adapter copy. See #2260 and #1089.
fn park_message(project_path: &str) -> String {
    let base = format!(
        "Structured view worker failed to stay up after {} restart attempts in {}s; auto-respawn paused.",
        RECONCILER_MAX_RESPAWNS_IN_WINDOW,
        RECONCILER_RESPAWN_WINDOW.as_secs(),
    );
    if !std::path::Path::new(project_path).exists() {
        format!("{base} project path no longer exists: {project_path}")
    } else {
        format!("{base} Retry from the dashboard once the underlying issue is fixed.")
    }
}

/// Per-target resume outcome. Drives whether the reconciler should
/// retry on the next tick or leave `attempted` set so the same target
/// isn't poked every 2s.
#[derive(Debug, Clone)]
enum ResumeOutcome {
    /// Reattach succeeded; nothing else to do for this id.
    Attached,
    /// Reattach timed out; the orphan registry entry was swept. The next
    /// tick may try a fresh spawn cleanly, so the id is dropped from
    /// `attempted`, but only while the session is under its respawn budget
    /// (a parked session keeps the guard). See #1945.
    RetryAfterAttachTimeout,
    /// Fresh spawn finished, with or without error. `attempted` stays
    /// populated; a permanently-failing spawn (e.g. missing
    /// claude-agent-acp) does not loop forever.
    SpawnFinished,
    /// Spawn refused by `SupervisorError::CapacityFull`: transient,
    /// non-crash, and user-actionable (a slot frees when a peer worker
    /// stops), not a spawn failure. The id is re-armed (dropped from
    /// `attempted`) so the per-tick retry self-heals; the join handler
    /// refunds the budget and publishes the banner once. `message` is the
    /// `CapacityFull` Display, reused verbatim as the `AgentStartupError`
    /// body so it matches the front-end regex. See #1027.
    CapacityDeferred { message: String },
}

/// A single structured view session that needs a worker. Snapshotted from the
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
    /// `Instance.command`: the resolved launch command (from
    /// `session.agent_command_override` / `--cmd-override`). Threaded
    /// into `SpawnRequest` so structured view honors it like tmux. See #1766.
    command: String,
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
    String,
);

/// When each cadence-gated pass last ran. Grouped rather than passed as
/// three more `&mut Option<Instant>` parameters: the passes are gated on the
/// same 2s tick and the count was already at clippy's argument limit, so the
/// next one to be added would have to either bundle or silence the lint.
#[derive(Default)]
pub struct ReapCadence {
    pub idle: Option<Instant>,
    pub rate_limit: Option<Instant>,
    pub terminal_repair: Option<Instant>,
}

pub async fn reconcile_acp_workers(
    state: &Arc<AppState>,
    attempted: &mut HashSet<String>,
    cadence: &mut ReapCadence,
    respawn_history: &mut HashMap<String, Vec<Instant>>,
    parked: &mut HashSet<String>,
    capacity_deferred: &mut HashSet<String>,
) {
    // Respawn build-stale workers that were adopted to drain an in-flight
    // turn (see #1754) and have since gone idle. Runs BEFORE
    // `reap_user_stopped` so the marker + registry-delete this writes is
    // picked up by the same tick's reaper, which tears down the attached
    // handle and clears `attempted` so the resume pass below fresh-spawns
    // on the current binary.
    respawn_drained_stale_workers(state).await;

    // Detect `aoe acp stop|kill|restart` (a separate process that
    // deletes the registry entry + SIGTERMs the runner) and surface it
    // as a typed Stopped event. The daemon's protocol-layer connection
    // task blocks on `cmd_rx.recv()` while idle, so socket EOF doesn't
    // propagate to the drain task on its own, so without this poll the
    // UI stays stuck on "thinking" and the supervisor keeps a phantom
    // worker. For the `restart` case, the reaper returns the ids it
    // marked as `restart_pending`; clear them from `attempted` so the
    // spawn pass below treats them as fresh and the next 2s tick
    // reattaches with the cached `acp_session_id`.
    let restart_pending = state.acp_supervisor.reap_user_stopped().await;
    for id in &restart_pending {
        // `aoe acp restart` is an explicit user retry: give the session a
        // clean slate (re-armed, un-parked, budget + capacity marker reset).
        forget_session_budget(id, attempted, parked, respawn_history, capacity_deferred);
    }

    // Out-of-band respawn requests (web "Update & restart" after a global
    // adapter install, #2109). These sessions failed their spawn on a
    // compatibility rejection and have no live worker, so the
    // `reap_user_stopped` path above never sees them; they sit pinned in
    // `attempted`. Same clean-slate reset (like an explicit restart) so the
    // resume pass below fresh-spawns them on the freshly-installed adapter and
    // the next handshake clears the red X.
    for id in state.acp_supervisor.take_respawn_requests() {
        forget_session_budget(&id, attempted, parked, respawn_history, capacity_deferred);
    }

    // Idle auto-stop (#1689). Cadence-gated to IDLE_REAP_INTERVAL so the
    // batched activity query does not run on every 2s tick. Runs BEFORE
    // the resume snapshot below: a worker marked dormant here is excluded
    // from this same tick's respawn pass by the `!i.is_idle_dormant()`
    // filter. The idle threshold is resolved per session profile inside
    // `reap_idle_workers`; `auto_stop_idle_secs == 0` (the default)
    // disables the feature for sessions on that profile.
    if cadence
        .idle
        .is_none_or(|t| t.elapsed() >= IDLE_REAP_INTERVAL)
    {
        reap_idle_workers(state).await;
        cadence.idle = Some(Instant::now());
    }

    // Terminal-event repair (#3190). Cadence-gated like the reaps above.
    // Runs AFTER the idle reap so a session the reap just marked dormant and
    // shut down carries the reap's own `idle_auto_stop` terminal instead of
    // collecting a second, redundant one from this pass on the same tick.
    if cadence
        .terminal_repair
        .is_none_or(|t| t.elapsed() >= TERMINAL_REPAIR_INTERVAL)
    {
        repair_missing_terminal(state).await;
        cadence.terminal_repair = Some(Instant::now());
    }

    // Rate-limit auto-resume (#1722). Cadence-gated like the idle reaper:
    // reset windows are long, so probing every 2s tick is wasteful. Runs
    // BEFORE the resume snapshot so a session whose reset just elapsed is
    // un-parked (breadcrumb published + cleared from `attempted`) in time
    // for this same tick's spawn pass to bring its worker back. The pass is
    // a no-op for the default-off case: profiles that did not opt in are
    // dropped before any event-store probe.
    if cadence
        .rate_limit
        .is_none_or(|t| t.elapsed() >= RATE_LIMIT_RESUME_INTERVAL)
    {
        reap_rate_limit_resumes(state, attempted).await;
        cadence.rate_limit = Some(Instant::now());
    }

    // Snapshot per-target resume inputs under the instances read lock.
    // We then drop the lock so the parallel resume tasks (each ~3s for
    // a fresh spawn) don't pin it.
    //
    // Triaged sessions (archived or currently-snoozed) are excluded from
    // the resume targets so the reconciler does not race the web
    // archive/snooze handler's worker teardown. Without this skip, the
    // 2s tick would respawn an archived structured view worker immediately after
    // the API handler shuts it down, defeating the archive semantics.
    // Expired snoozes naturally rejoin via `is_snoozed()` returning
    // false past the deadline. See #1581.
    let raw_targets: Vec<RawTargetTuple> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.is_structured()
                    && !i.is_archived()
                    && !i.is_snoozed()
                    && !i.is_trashed()
                    && !i.is_idle_dormant()
            })
            .map(|i| {
                (
                    i.id.clone(),
                    i.tool.clone(),
                    i.agent_name.clone(),
                    i.agent_model.clone(),
                    i.project_path.clone(),
                    i.acp_session_id.clone(),
                    i.source_profile.clone(),
                    i.yolo_mode,
                    i.command.clone(),
                )
            })
            .collect()
    };

    let live: HashSet<&String> = raw_targets.iter().map(|t| &t.0).collect();
    attempted.retain(|id| live.contains(id));
    // Sweep budget state for sessions that no longer exist so the maps
    // don't grow unbounded and a recreated id starts with a clean budget.
    parked.retain(|id| live.contains(id));
    respawn_history.retain(|id, _| live.contains(id));
    capacity_deferred.retain(|id| live.contains(id));

    // ORDERING INVARIANT: this orphan sweep MUST run before the
    // resume scheduling pass below. The capacity check counts both
    // in-memory workers AND on-disk registry entries (so a fresh
    // daemon can't race the reconciler and over-spawn). If the sweep
    // ran after, dead-PID entries from a previous unclean shutdown
    // would still count toward `max_concurrent_workers` and could
    // block legitimate spawns until the next tick. Do not reorder.
    sweep_orphan_workers(state, &live).await;

    // Re-adopt live orphan runners (#1890) before the work-list loop, which
    // skips every `attempted` id. See `readopt_orphan_runners`.
    readopt_orphan_runners(state, attempted).await;

    // Retry owner for undelivered initial turns (#2897): a session persisted
    // with `pending_initial_turn` whose create fast path did not deliver it
    // (spawn failure, daemon restart, adopted runner) gets its turn drained
    // here once a worker is live. Normally a no-op: pending turns exist only
    // between a plugin create and its first successful delivery.
    drain_pending_initial_turns(state).await;

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
        command,
    ) in raw_targets
    {
        if attempted.contains(&id) {
            // A restart marker that arrives after the reaper already ran. `aoe
            // session add-project` (#3103) stops the worker first and only asks
            // for the restart once the moved workspace is durable, precisely so
            // a respawn cannot land in the directory it is moving; that ordering
            // means its marker routinely misses `reap_user_stopped`. Without
            // this the session would sit stopped until the next daemon start.
            if !crate::process::worker_registry::take_restart_marker(&id) {
                continue;
            }
            forget_session_budget(&id, attempted, parked, respawn_history, capacity_deferred);
        }
        if state.acp_supervisor.is_running(&id).await {
            // A REST-triggered spawn (POST /api/sessions or
            // /api/acp/sessions/:id/enable) already owns the worker;
            // record the id so we don't poll is_running every tick. A live
            // worker is also the self-healing signal for a crash-loop-parked
            // session: the user retried via the dashboard, so wipe the
            // budget and un-park.
            parked.remove(&id);
            respawn_history.remove(&id);
            capacity_deferred.remove(&id);
            attempted.insert(id);
            continue;
        }
        // Crash-loop park (#1945): a session whose worker keeps failing to
        // come online is held parked, with the `attempted` insert below as a
        // secondary per-tick guard. `parked` is authoritative because the
        // restart / rate-limit reapers clear `attempted` and would otherwise
        // un-park unintentionally. The park is released by the `is_running`
        // branch above (explicit user retry) or when the session leaves the
        // live set. Lost on daemon restart, which gives a genuinely-broken
        // session one more bounded burst before re-parking; acceptable.
        if parked.contains(&id) {
            attempted.insert(id);
            continue;
        }
        // Rate-limit park: if the most recent lifecycle event for this
        // session is `Stopped { reason: "rate_limited" }`, the previous
        // worker exited because the adapter hit a quota. Auto-resuming
        // would `session/load` and immediately fail the next prompt the
        // same way; on daemon restart that would undo the entire #1281
        // fix. Hold the session parked until the user explicitly retries
        // via `/acp/spawn` or hands off via `/acp/switch-agent`.
        // SQLite call wrapped in spawn_blocking to match the
        // has_in_flight_turn pattern below; the reconciler runs on the
        // tokio runtime and these queries can stall under load.
        let store = Arc::clone(&state.acp_event_store);
        let id_for_status = id.clone();
        let latest_status =
            tokio::task::spawn_blocking(move || store.latest_status_event(&id_for_status))
                .await
                .unwrap_or(None);
        if let Some(crate::acp::Event::Stopped { reason }) = latest_status {
            if reason == "rate_limited" {
                tracing::debug!(
                    target: "acp.supervisor",
                    session = %id,
                    "skipping auto-resume: latest lifecycle event is Stopped{{rate_limited}}"
                );
                attempted.insert(id);
                continue;
            }
        }
        // Respawn-budget gate (#1945). Count this resume decision before we
        // know its outcome: that catches both the reattach-timeout loop and
        // the fresh-spawn-then-crash loop (where the worker dies seconds
        // later and re-enters once `attempted` is cleared). Over budget,
        // park the session, surface one `AgentStartupError`, and skip.
        if record_and_check_respawn_budget(respawn_history, &id, Instant::now()) {
            tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                max_respawns = RECONCILER_MAX_RESPAWNS_IN_WINDOW,
                window_secs = RECONCILER_RESPAWN_WINDOW.as_secs(),
                "structured-view worker respawn budget exhausted; parking session"
            );
            if parked.insert(id.clone()) {
                state
                    .acp_supervisor
                    .publish_startup_error(&id, park_message(&project_path));
            }
            attempted.insert(id);
            continue;
        }
        let store = Arc::clone(&state.acp_event_store);
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
                        target: "acp.supervisor",
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
            command,
        });
    }

    if tasks.is_empty() {
        return;
    }

    // Resume concurrency cap. Bounded by total worker capacity so it can
    // never exceed `max_concurrent_workers`. Floor at 1 so a misconfigured
    // zero doesn't deadlock the reconciler.
    let cfg = crate::session::profile_config::resolve_config_or_warn(&state.profile);
    let resume_limit = MAX_CONCURRENT_RESUMES
        .min(cfg.acp.max_concurrent_workers)
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
                // Only re-arm a retry while the session is under budget; a
                // parked session keeps its `attempted` guard so the loop
                // can't restart. The budget gate already counted this
                // attempt before the task ran. See #1945.
                if !parked.contains(&id) {
                    attempted.remove(&id);
                }
            }
            Ok((id, ResumeOutcome::CapacityDeferred { message })) => {
                // Refund the single budget entry this tick recorded at the
                // decision gate (`record_and_check_respawn_budget`, above):
                // CapacityFull is not a crash, so it must not burn the #1945
                // budget. POP the last entry (this tick's), not `remove(&id)`,
                // which would wipe genuine prior-crash history and let a
                // crashing session escape the park budget.
                if let Some(entries) = respawn_history.get_mut(&id) {
                    entries.pop();
                    if entries.is_empty() {
                        respawn_history.remove(&id);
                    }
                }
                // Re-arm the retry so the next tick can try again once a slot
                // frees. NEVER `attempted.insert`: `attempted` is persistent
                // (only the live-set retain drops it), so keeping the id there
                // would skip the session forever and the self-heal would never
                // fire. Unlike `RetryAfterAttachTimeout` this needs no
                // `!parked` guard: an id only reaches a spawn (and thus
                // CapacityFull) after passing the parked check, so it is never
                // parked here.
                attempted.remove(&id);
                // Publish the capacity banner once per transition; the gate
                // returns true only on the first insert (mirrors
                // `parked.insert`), because `publish_startup_error` does not
                // dedup and per-tick publishing would spam the event store.
                if capacity_deferred.insert(id.clone()) {
                    state.acp_supervisor.publish_startup_error(&id, message);
                }
            }
            Ok((id, ResumeOutcome::Attached)) | Ok((id, ResumeOutcome::SpawnFinished)) => {
                // Clear the capacity marker on the successful-respawn path.
                // This is the ONLY clear the reconciler-dispatched capacity
                // case reaches: a successful respawn returns SpawnFinished and
                // leaves the id in `attempted`, so the `is_running` branch is
                // unreachable next tick. Without this clear the marker sticks
                // for the worker's life and a second capacity transition would
                // not re-publish the banner.
                capacity_deferred.remove(&id);
            }
            Err(e) => {
                // Task panicked or was cancelled. Don't keep retrying
                // the same id every tick if the task panics on every
                // run; the `attempted` insert above already protects
                // us. Log so operators see it.
                tracing::error!(
                    target: "acp.supervisor",
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

/// How often the terminal-repair pass runs. Coarser than the 2s tick so the
/// per-candidate event-log probes stay cheap, fine enough that the wrong badge
/// clears within about half a minute of the grace expiring. See #3190.
const TERMINAL_REPAIR_INTERVAL: Duration = Duration::from_secs(30);

/// How long a cost-bearing `UsageUpdated` must stand as the session's latest
/// event before the repair pass treats the turn as finished.
///
/// The adapter emits that frame as its "wrap up accounting" end-of-turn
/// marker, which is why `acp_client`'s own between-prompt watchdog trusts it
/// on a 3s grace. This backstop is deliberately an order of magnitude more
/// patient: it must not race a turn that emits the marker and then spends time
/// inside a model call before its next frame, and unlike the in-connection
/// watchdogs it writes to the canonical log, so a false positive costs more
/// than a late one. 60s still bounds the wrong status to about a minute,
/// against the hour the idle reap used to take. See #3190.
const TERMINAL_REPAIR_GRACE_SECS: u32 = 60;

/// Pure terminal-repair decision. Every input must line up before the daemon
/// writes a terminal event the agent never sent.
///
/// `terminal_usage` is the load-bearing one: the repair infers completion from
/// the adapter's own end-of-turn marker being latest, NOT from silence.
///
/// It rests on that marker meaning end-of-turn, which is the same thing
/// `acp_client`'s watchdog trusts on a 3s grace. The residual risk, accepted:
/// an adapter that emits a cost-bearing frame MID-turn and then spends over
/// the grace inside a silent model call with no open tool gets a terminal it
/// did not send. The status self-heals on the turn's next event, but unlike
/// the in-memory status the fabricated `Stopped` stays in the log, so the
/// timeline keeps a turn boundary that never happened. That is why the reason
/// string is distinct rather than `prompt_complete`. See PR #3192 review.
/// Silence alone is not evidence a turn finished (an agent can sit in a model
/// call), and a turn that died mid-stream without ever emitting the marker is
/// a worker-liveness problem with a different fix, so it is left to the idle
/// reap rather than guessed at here.
///
/// The rest are refusals: a user prompt still lacking its terminator
/// (`in_flight_turn`, which also covers a live async sub-agent), a tool the
/// agent is still running in this epoch (`open_tool_call`), or a pending
/// approval / elicitation (`awaiting_user`, which can outlive the `Waiting`
/// status because a later activity event overwrites it). See #3190.
#[allow(clippy::too_many_arguments)]
fn should_repair_terminal(
    now_ms: i64,
    last_event_ms: i64,
    terminal_usage: bool,
    grace_secs: u32,
    in_flight_turn: bool,
    open_tool_call: bool,
    awaiting_user: bool,
) -> bool {
    if !terminal_usage || in_flight_turn || open_tool_call || awaiting_user {
        return false;
    }
    now_ms.saturating_sub(last_event_ms) >= i64::from(grace_secs) * 1000
}

/// Terminal-repair pass (#3190). Publishes the `Stopped` an agent-initiated
/// turn never got, so a session whose agent is demonstrably done stops
/// rendering as Running.
///
/// Why this lives outside the connection task: the three watchdogs that are
/// supposed to emit that terminal all live inside one `run_connection_task`
/// state machine, sharing a one-shot guard and a set of atomics, and a
/// command loop that can block while also owning its own watchdog timer
/// cannot reliably watchdog itself. Two confirmed sessions ran a full
/// agent-initiated turn (a Monitor and a backgrounded Bash resuming the
/// agent after its prompt had already completed), ended on the adapter's
/// cost-bearing end-of-turn marker, and never got a terminal at all; the only
/// thing that recovered them was the 1-hour idle reap, which kills the worker
/// to get there.
///
/// Deliberately narrow: it only appends the missing event. It never stops,
/// restarts, or marks a worker dormant, so a live agent that is merely quiet
/// loses nothing but its green dot, and any further activity re-arms Running
/// through `derive_acp_status` as usual.
async fn repair_missing_terminal(state: &Arc<AppState>) {
    // Only rows the daemon currently projects as Running. `Waiting` is
    // excluded: a session parked on an approval is legitimately silent for
    // as long as the user takes.
    let candidates: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.is_structured() && i.status == crate::session::Status::Running)
            .map(|i| i.id.clone())
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    // Batched age pre-filter, so the per-candidate probes below only run for
    // sessions that could possibly qualify. Mirrors the idle reap's shape.
    let store = Arc::clone(&state.acp_event_store);
    let ids = candidates.clone();
    let latest_at = match tokio::task::spawn_blocking(move || {
        store.last_event_at_for_sessions(&ids)
    })
    .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(target: "acp.supervisor", error = %e, "terminal-repair activity query failed");
            return;
        }
    };
    let now_ms = chrono::Utc::now().timestamp_millis();
    let grace_ms = i64::from(TERMINAL_REPAIR_GRACE_SECS) * 1000;
    for id in candidates {
        let Some(last_ms) = latest_at.get(&id).copied() else {
            continue;
        };
        if now_ms.saturating_sub(last_ms) < grace_ms {
            continue;
        }
        let store = Arc::clone(&state.acp_event_store);
        let probe_id = id.clone();
        let probe = tokio::task::spawn_blocking(move || {
            let latest = store.terminal_repair_probe(&probe_id);
            (
                latest,
                store.has_in_flight_turn(&probe_id),
                store.has_open_tool_call_in_epoch(&probe_id),
                !store.unresolved_approval_nonces(&probe_id).is_empty()
                    || !store.unresolved_elicitation_nonces(&probe_id).is_empty(),
            )
        })
        .await;
        // A panicking probe is worth a line: this pass exists to explain
        // missing terminal events, so swallowing a panic inside it defeats
        // the point. The no-substantive-event case below is ordinary and
        // stays quiet.
        let (latest, in_flight, open_tool, awaiting_user) = match probe {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    error = %e,
                    "terminal-repair probe task failed; skipping this session"
                );
                continue;
            }
        };
        let Some(latest) = latest else {
            continue;
        };
        // The same condition `acp_client`'s `LifecycleSignal::TerminalUsage`
        // classifier applies, evaluated on the decoded event so the two
        // cannot drift apart in SQL.
        let terminal_usage = matches!(
            &latest.substantive,
            crate::acp::Event::UsageUpdated { usage } if usage.cost.is_some()
        );
        if !should_repair_terminal(
            now_ms,
            latest.substantive_at_ms,
            terminal_usage,
            TERMINAL_REPAIR_GRACE_SECS,
            in_flight,
            open_tool,
            awaiting_user,
        ) {
            continue;
        }
        // Conditional on the log's newest seq still being the newest
        // allocation: anything published between the probe and here (a fresh
        // prompt above all) must not be terminated by this repair. A refusal
        // just waits for the next pass. Expects `latest_seq` rather than the
        // substantive event's own seq, because the seq counter also advances
        // for ambient events (an `AcpSessionAssigned` from a resume replay),
        // and expecting the substantive one would make every later pass
        // refuse forever. See PR #3192 review.
        if state.acp_supervisor.publish_stopped_if_seq(
            &id,
            "inferred_prompt_complete",
            latest.latest_seq,
        ) {
            tracing::info!(
                target: "acp.supervisor",
                session = %id,
                after_seq = latest.latest_seq,
                quiet_ms = now_ms.saturating_sub(latest.substantive_at_ms),
                "terminal-repair: agent-initiated turn ended with no Stopped; published inferred_prompt_complete"
            );
        }
    }
}

/// Pure idle-reap decision. A structured view worker is auto-stopped only when the
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

/// Idle auto-stop pass (#1689). Shuts down structured view workers that have seen
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
    // Candidates: structured view sessions not already sunk/dormant. Snapshot
    // (id, profile) under the read lock so we don't hold it across awaits.
    let candidates: Vec<(String, String)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.is_structured()
                    && !i.is_archived()
                    && !i.is_snoozed()
                    && !i.is_trashed()
                    && !i.is_idle_dormant()
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
                        .acp
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
        if state.acp_supervisor.is_running(&id).await {
            live.push((id, profile, idle_secs));
        }
    }
    if live.is_empty() {
        return;
    }
    // One batched query for the latest event timestamp per candidate.
    let ids: Vec<String> = live.iter().map(|(id, _, _)| id.clone()).collect();
    let store = Arc::clone(&state.acp_event_store);
    let latest = match tokio::task::spawn_blocking(move || store.last_event_at_for_sessions(&ids))
        .await
    {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(target: "acp.supervisor", error = %e, "idle-reap activity query failed");
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
        let store = Arc::clone(&state.acp_event_store);
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
        let persisted =
            if let Ok(storage) = crate::session::Storage::new(&profile, state.file_watch.clone()) {
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
                target: "acp.supervisor",
                session = %id,
                "idle-reap persist failed; leaving worker alive"
            );
            continue;
        }
        match state.acp_supervisor.shutdown_idle(&id).await {
            Ok(()) | Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) => {
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    idle_secs,
                    "auto-stopped idle structured view worker"
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
                if let Ok(storage) =
                    crate::session::Storage::new(&profile, state.file_watch.clone())
                {
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
                    target: "acp.supervisor",
                    session = %id,
                    "idle-reap shutdown failed; cleared dormant marker: {e}"
                );
            }
        }
    }
}

/// Respawn build-stale workers that were adopted mid-turn (flagged via
/// `Supervisor::mark_build_respawn_pending` in `resume_one`) once their
/// in-flight turn has finished. Idle is detected with the same
/// `has_in_flight_turn` event-store probe the resume pass uses.
///
/// For each drained session this mirrors `aoe acp restart`: write the
/// restart marker so the reaper publishes `restart_pending` (the UI shows
/// "Restarting…" rather than a stop), then SIGTERM the stale runner group
/// and delete its registry entry. The caller runs the reaper immediately
/// after, which tears down the attached handle and clears `attempted`, so
/// the resume pass fresh-spawns on the current binary. See #1754.
async fn respawn_drained_stale_workers(state: &Arc<AppState>) {
    for id in state.acp_supervisor.build_respawn_pending_ids() {
        let store = Arc::clone(&state.acp_event_store);
        let id_probe = id.clone();
        let in_flight =
            match tokio::task::spawn_blocking(move || store.has_in_flight_turn(&id_probe)).await {
                Ok(v) => v,
                // Probe failed: assume still busy so a transient error
                // never hard-kills a possibly-live turn. Retried next tick.
                Err(e) => {
                    tracing::warn!(
                        target: "acp.supervisor",
                        session = %id,
                        error = %e,
                        "in-flight probe failed for draining stale worker; deferring respawn"
                    );
                    true
                }
            };
        if in_flight {
            continue;
        }
        tracing::info!(
            target: "acp.supervisor",
            session = %id,
            "build-stale structured view worker drained; respawning on current binary"
        );
        crate::process::worker_registry::mark_restart_pending(&id);
        crate::process::worker_registry::terminate(&id);
        state.acp_supervisor.clear_build_respawn_pending(&id);
    }
}

/// What `resume_one` should do with the worker registry record it found
/// for a structured view session that has no live in-memory worker yet. Split out
/// as a pure function so the build-version respawn policy (#1754) is
/// unit-testable without standing up a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdoptDecision {
    /// No usable record (dead PID / missing socket): sweep and fresh-spawn.
    FreshSpawn,
    /// Live worker on the current binary: reattach.
    Attach,
    /// Live worker on an older binary with no in-flight turn: terminate
    /// now and fresh-spawn on the current binary.
    RespawnStaleIdle,
    /// Live worker on an older binary mid-turn: adopt to keep the turn
    /// streaming, then respawn at the next idle boundary.
    AdoptStaleForDrain,
}

/// Decide whether a session currently pinned in the reconciler's
/// `attempted` set should be dropped so the resume pass can re-adopt its
/// live on-disk runner. `running` folds the in-memory worker AND any
/// in-flight resume reservation (`Supervisor::is_running`); `has_live_runner`
/// is a live registry record (PID alive + socket present). A session with a
/// live runner but no in-memory presence is the orphan-after-failed-handshake
/// state (#1890): the runner is serving on its socket but the daemon never
/// reattached, so every prompt 404s. A running session (healthy or mid-spawn)
/// is left alone. Pure so the policy is unit-testable without a daemon,
/// mirroring `adopt_decision`.
fn should_readopt_orphan_runner(running: bool, has_live_runner: bool) -> bool {
    !running && has_live_runner
}

fn adopt_decision(live: bool, build_current: bool, in_flight_turn: bool) -> AdoptDecision {
    if !live {
        AdoptDecision::FreshSpawn
    } else if build_current {
        AdoptDecision::Attach
    } else if in_flight_turn {
        AdoptDecision::AdoptStaleForDrain
    } else {
        AdoptDecision::RespawnStaleIdle
    }
}

/// How often the rate-limit auto-resume pass runs. Reset windows are
/// minutes to hours, so the 2s reconciler tick would re-probe far more
/// often than needed; this gates it to a coarse cadence. See #1722.
const RATE_LIMIT_RESUME_INTERVAL: Duration = Duration::from_secs(15);

/// Hardcoded floor on the park window, measured from when the `RateLimit`
/// event was recorded. A misbehaving adapter could report a `resets_at`
/// already in the past (or with `grace_secs == 0`); without this floor the
/// reconciler would respawn the worker on the very next pass and could
/// thrash if the adapter keeps emitting past resets. 30s preserves the
/// spirit of the #1281 "no eager restart loop" fix. See #1722.
const RATE_LIMIT_MIN_PARK_SECS: i64 = 30;

/// How long auto-resume waits when the agent reported no reset time at
/// all. Purely a retry schedule: it never lands in a `RateLimit` event's
/// `resets_at`, so no surface presents it as a reset the agent reported,
/// which is what #3152 is about. It does reach the `RateLimitAutoResumed`
/// breadcrumb, where the timestamp means "when the resume fired" (already
/// reset plus grace even in the reported case). If the limit has not
/// cleared, the retry re-parks and the next one is another interval out.
const RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS: i64 = 3600;

/// Opt-in rate-limit auto-resume pass (#1722). For structured view sessions parked
/// on `Stopped { reason: "rate_limited" }` whose profile enabled
/// `acp.rate_limit_auto_resume`, respawn the worker once the
/// adapter-reported `resets_at` (plus the configured grace, floored by
/// `RATE_LIMIT_MIN_PARK_SECS` from when the limit was recorded) has passed.
///
/// Mechanism: publish a `RateLimitAutoResumed` breadcrumb (which supersedes
/// the terminal `Stopped{rate_limited}` in `latest_status_event`) and clear
/// the id from `attempted`. The main resume loop on the same tick then sees
/// a non-park latest status and a clear `attempted` slot, so it fresh-spawns
/// the worker through the existing path. Both the in-process park (id was
/// inserted into `attempted` while the worker ran) and the daemon-restart
/// park (the main loop parks it on the first tick) are covered because the
/// candidate set is exactly `attempted` minus running workers.
///
/// Durable across daemon restart: `resets_at` is read from the persisted
/// event store, never from memory. A re-rate-limit writes a fresh
/// `RateLimit` event with a new `resets_at`, so the next auto-resume waits
/// for the new window rather than looping.
/// Wall-clock instant at which a rate-limit-parked session becomes
/// eligible for auto-resume: the later of the adapter-reported reset
/// (plus the configured grace) and a hardcoded minimum park measured from
/// when the `RateLimit` event was recorded. The floor keeps a buggy
/// adapter that reports a past `resets_at` (or a zero grace) from driving
/// a tight respawn loop. See #1722.
fn rate_limit_resume_at(
    resets_at: chrono::DateTime<chrono::Utc>,
    recorded_at_ms: i64,
    grace_secs: u32,
) -> chrono::DateTime<chrono::Utc> {
    let resets_plus_grace = resets_at + chrono::Duration::seconds(i64::from(grace_secs));
    match chrono::DateTime::from_timestamp_millis(recorded_at_ms)
        .map(|t| t + chrono::Duration::seconds(RATE_LIMIT_MIN_PARK_SECS))
    {
        Some(floor) if floor > resets_plus_grace => floor,
        _ => resets_plus_grace,
    }
}

/// Wall-clock instant at which a rate-limit-parked session with NO
/// reported reset becomes eligible for an auto-resume retry: a fixed
/// interval after the `RateLimit` event was recorded. See
/// `RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS` and #3152.
fn rate_limit_unknown_reset_retry_at(recorded_at_ms: i64) -> chrono::DateTime<chrono::Utc> {
    let retry_after = chrono::Duration::seconds(RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS);
    match chrono::DateTime::from_timestamp_millis(recorded_at_ms) {
        Some(recorded) => recorded + retry_after,
        None => chrono::Utc::now() + retry_after,
    }
}

async fn reap_rate_limit_resumes(state: &Arc<AppState>, attempted: &mut HashSet<String>) {
    // Candidates: structured view sessions currently parked (recorded in
    // `attempted`, no live worker). Snapshot (id, profile) under the read
    // lock so we don't hold it across awaits. Archived/snoozed/dormant
    // sessions are excluded for the same reasons as the resume snapshot.
    let candidates: Vec<(String, String)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.is_structured()
                    && !i.is_archived()
                    && !i.is_snoozed()
                    && !i.is_trashed()
                    && !i.is_idle_dormant()
                    && attempted.contains(&i.id)
            })
            .map(|i| (i.id.clone(), i.source_profile.clone()))
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    // Only sessions without a live worker are parked; a running worker in
    // `attempted` is the steady-state perf entry, not a park.
    let mut parked: Vec<(String, String)> = Vec::new();
    for (id, profile) in candidates {
        if !state.acp_supervisor.is_running(&id).await {
            parked.push((id, profile));
        }
    }
    if parked.is_empty() {
        return;
    }
    // Resolve the auto-resume config per distinct profile off-thread (it
    // touches disk). Sessions on a profile that did not opt in are dropped
    // before any per-session event-store probe, so the feature is free for
    // the default-off case.
    let distinct_profiles: Vec<String> = {
        let mut seen = HashSet::new();
        parked
            .iter()
            .map(|(_, p)| p.clone())
            .filter(|p| seen.insert(p.clone()))
            .collect()
    };
    let cfg_by_profile: std::collections::HashMap<String, bool> =
        tokio::task::spawn_blocking(move || {
            distinct_profiles
                .into_iter()
                .map(|p| {
                    let acp = crate::session::profile_config::resolve_config_or_warn(&p).acp;
                    (p, acp.rate_limit_auto_resume)
                })
                .collect()
        })
        .await
        .unwrap_or_default();

    let now = chrono::Utc::now();
    for (id, profile) in parked {
        let enabled = cfg_by_profile.get(&profile).copied().unwrap_or(false);
        if !enabled {
            continue;
        }
        // Confirm the session is actually parked on a rate-limit stop (not
        // some other terminal state that happens to sit in `attempted`) and
        // read the reset time, both off-thread.
        let store = Arc::clone(&state.acp_event_store);
        let id_probe = id.clone();
        let (is_rate_limit_parked, rate_limit) = match tokio::task::spawn_blocking(move || {
            let parked = matches!(
                store.latest_status_event(&id_probe),
                Some(crate::acp::Event::Stopped { reason }) if reason == "rate_limited"
            );
            (parked, store.latest_rate_limit_event(&id_probe))
        })
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "acp.supervisor",
                    session = %id,
                    error = %e,
                    "rate-limit auto-resume probe failed"
                );
                continue;
            }
        };
        if !is_rate_limit_parked {
            continue;
        }
        let Some((info, recorded_at_ms)) = rate_limit else {
            continue;
        };
        // A reported reset schedules against it; an unreported one (the
        // agent never attributed a reset to the window that rejected, see
        // #3152) falls back to a retry interval measured from the park.
        // Skipping instead would leave auto-resume, whose whole job is
        // coming back to life, doing nothing for those limits.
        let resume_at = match info.resets_at {
            Some(resets_at) => {
                rate_limit_resume_at(resets_at, recorded_at_ms, RATE_LIMIT_AUTO_RESUME_GRACE_SECS)
            }
            None => rate_limit_unknown_reset_retry_at(recorded_at_ms),
        };
        if now < resume_at {
            continue;
        }
        // Re-check liveness right before publishing: several awaits sit
        // between the candidate snapshot and here, so a manual
        // `/acp/spawn` could have brought the worker back in the gap.
        // Without this guard we would emit a spurious auto-resume
        // breadcrumb (and clear `attempted`) for an already-running
        // session. Let the manual resume win. See #1722.
        if state.acp_supervisor.is_running(&id).await {
            continue;
        }
        // Eligible: queue the interrupted prompt (if any) so the respawned
        // worker continues instead of sitting idle (#3028), then publish the
        // breadcrumb (supersedes Stopped{rate_limited}) and free the
        // `attempted` slot so the main resume loop spawns a fresh worker this
        // tick. The pending-turn drain delivers the continuation once live.
        enqueue_rate_limit_continuation(state, &id).await;
        state
            .acp_supervisor
            .publish_rate_limit_auto_resumed(&id, resume_at);
        attempted.remove(&id);
        tracing::info!(
            target: "acp.supervisor",
            session = %id,
            resets_at = ?info.resets_at,
            resume_at = %resume_at,
            "rate-limit auto-resume: park window elapsed; respawning worker"
        );
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
        command,
    } = target;

    // Reattach path: if a previous daemon detached a runner for this
    // session and the runner is still alive, dial its socket instead
    // of spawning a fresh agent. Bounded by the registry probe — no
    // network IO unless we have a live PID + socket on disk.
    if let Ok(Some(record)) = crate::process::worker_registry::load(&id) {
        let decision = adopt_decision(
            crate::process::worker_registry::is_record_live(&record),
            crate::process::worker_registry::is_build_current(&record),
            in_flight_turn,
        );
        if decision == AdoptDecision::FreshSpawn {
            // Dead PID or missing socket: sweep the orphan registry entry
            // so the fall-through below is a clean fresh spawn.
            crate::process::worker_registry::delete(&id).ok();
        } else if decision == AdoptDecision::RespawnStaleIdle {
            // The runner survived a daemon restart but is executing an
            // older binary (e.g. after `aoe update`) and has no in-flight
            // turn. Replace it now: SIGTERM the stale runner group (which
            // also deletes the registry entry) and fall through to a
            // fresh spawn on the current binary. See #1754.
            tracing::info!(
                target: "acp.supervisor",
                session = %id,
                old_build = %record.build_version,
                new_build = crate::build_info::BUILD_VERSION,
                "respawning idle build-stale structured view worker on current binary"
            );
            crate::process::worker_registry::terminate(&id);
        } else {
            // Attach or AdoptStaleForDrain: dial the live runner.
            if decision == AdoptDecision::AdoptStaleForDrain {
                // Build-stale but mid-turn: adopt now so the in-flight
                // turn keeps streaming, and flag the session so the next
                // idle boundary respawns it on the current binary instead
                // of hard-killing the turn. Preserves the #1037
                // survive-restart contract. See #1754.
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    old_build = %record.build_version,
                    new_build = crate::build_info::BUILD_VERSION,
                    "adopting build-stale structured view worker to drain in-flight turn before respawn"
                );
                state.acp_supervisor.mark_build_respawn_pending(&id);
            }
            let supervisor = Arc::clone(&state.acp_supervisor);
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
                        target: "acp.supervisor",
                        session = %id,
                        pid = record.pid,
                        in_flight_turn,
                        "reattached to existing structured view runner"
                    );
                    // The startup pass in `seed_acp_statuses`
                    // covers the cold-start case. Anything attached
                    // later (e.g. a session created after the daemon
                    // started) also needs its status seeded; the
                    // attach path's only sidebar-moving signal is the
                    // next live event, which can be many seconds
                    // away. Re-derive from history here too so the
                    // dot turns green immediately. See #1103 (A).
                    if in_flight_turn {
                        if let Some(event) = state.acp_event_store.latest_seed_status_event(&id) {
                            if let Some(intent) = crate::server::derive_acp_status(&event) {
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
                        target: "acp.supervisor",
                        session = %id,
                        "attach failed; falling back to fresh spawn: {e}"
                    );
                    crate::process::worker_registry::delete(&id).ok();
                }
                Err(_) => {
                    tracing::warn!(
                        target: "acp.supervisor",
                        session = %id,
                        "attach timed out after 3s; falling back to fresh spawn"
                    );
                    crate::process::worker_registry::delete(&id).ok();
                    return ResumeOutcome::RetryAfterAttachTimeout;
                }
            }
        }
    }

    // Fresh-spawn fallback: we are about to spin up a brand new agent
    // process. The previous one (if any) was killed before it could
    // complete the in-flight prompt, so its turn is forever orphaned.
    // Publish a synthetic Stopped now so the UI doesn't keep
    // "thinking" after restart.
    if in_flight_turn {
        state
            .acp_supervisor
            .synthesize_stopped_for_orphan(&id, "orphaned_at_restart");
    }

    let resume_target = ResumeTarget {
        id: id.clone(),
        tool,
        agent_override,
        model,
        project_path,
        stored_acp_session_id,
        source_profile,
        in_flight_turn,
        yolo_mode,
        command,
    };
    let req = match build_spawn_request(&state.session_service, &resume_target).await {
        Ok(req) => req,
        Err(()) => return ResumeOutcome::SpawnFinished,
    };
    let agent = req.agent.clone();
    let spawn_result = state.acp_supervisor.spawn(req).await;
    if let Err(e) = spawn_result {
        // CapacityFull is transient, not a spawn failure: hand it to the
        // join handler as CapacityDeferred (refund budget, re-arm, publish
        // once) instead of burning the crash budget and orphaning the
        // session. Match before the `format!` below, where the typed error
        // is otherwise erased into a String. See #1027.
        if matches!(
            e,
            crate::acp::supervisor::SupervisorError::CapacityFull { .. }
        ) {
            return ResumeOutcome::CapacityDeferred {
                message: e.to_string(),
            };
        }
        // Re-check whether the session still exists in instances.
        // The user can delete a session during the spawn handshake
        // (2-3s for ACP), and the resulting error is noise for a
        // session that no longer exists. Demote to debug rather
        // than warn + AgentStartupError publish in that case.
        let still_present = state.instances.read().await.iter().any(|i| i.id == id);
        let message = format!("Failed to start structured view agent {agent:?}: {e}");
        if still_present {
            tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                agent = %agent,
                "auto-spawn reconciler failed: {message}"
            );
            state.acp_supervisor.publish_startup_error(&id, message);
        } else {
            tracing::debug!(
                target: "acp.supervisor",
                session = %id,
                agent = %agent,
                "auto-spawn reconciler error after session removed (ignored): {message}"
            );
        }
    }
    ResumeOutcome::SpawnFinished
}

/// Spawn a detached drain for every session that still carries a persisted
/// `pending_initial_turn` and has a live worker to receive it (#2897). The
/// drain itself claims a per-session slot and runs under the instance lock,
/// so overlapping ticks and the create fast path cannot double-deliver.
/// Triaged sessions are skipped like everywhere else in the reconciler; the
/// turn stays persisted and delivers if the session is ever un-triaged.
/// Queue the rate-limit-interrupted prompt as the session's next turn so a
/// resume (manual `/acp/spawn` or auto-resume) continues the work instead of
/// leaving the agent idle. Reads the interrupted prompt from the event store
/// off the async runtime, then hands it to the pending-initial-turn drain
/// (no-op when the last turn wasn't rate-limited or a turn is already
/// queued). #3028.
pub(crate) async fn enqueue_rate_limit_continuation(state: &Arc<AppState>, id: &str) {
    let store = Arc::clone(&state.acp_event_store);
    let id_owned = id.to_string();
    let (text, attachments) = match tokio::task::spawn_blocking(move || {
        store.rate_limited_turn_prompt(&id_owned)
    })
    .await
    {
        Ok(Some(prompt)) => prompt,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                "rate-limit continuation lookup failed: {e}"
            );
            return;
        }
    };
    state
        .session_service
        .set_pending_initial_turn(id, text, attachments)
        .await;
}

async fn drain_pending_initial_turns(state: &Arc<AppState>) {
    let candidates: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                i.pending_initial_turn.is_some()
                    && i.is_structured()
                    && !i.is_archived()
                    && !i.is_snoozed()
                    && !i.is_trashed()
            })
            .map(|i| i.id.clone())
            .collect()
    };
    for id in candidates {
        if !state.acp_supervisor.is_running(&id).await {
            continue;
        }
        let service = Arc::clone(&state.session_service);
        crate::task_util::spawn_supervised(
            "acp.pending_initial_turn_drain",
            crate::task_util::PanicPolicy::Log,
            async move {
                service.drain_pending_initial_turn(&id).await;
            },
        );
    }
}

/// Build a fresh-spawn `SpawnRequest` for a resume target: pick the
/// agent, resolve the cwd, and ensure the sandbox container. On a sandbox
/// failure it publishes a startup error (so the UI banner matches the
/// reconciler path) and returns `Err(())`; callers bail. Shared by the
/// reconciler's fresh-spawn fallback and the prompt-wake resume (#1748)
/// so both paths build identical requests.
async fn build_spawn_request(
    service: &Arc<SessionService>,
    target: &ResumeTarget,
) -> Result<crate::acp::supervisor::SpawnRequest, ()> {
    let supervisor = Arc::clone(&service.acp_supervisor);

    let inst_lock = service.instance_lock(&target.id).await;
    // Re-read project_path under the per-session lock instead of trusting
    // target.project_path, which the reconciler snapshotted up to a tick ago.
    // A tied-worktree rename (rename_session / set_worktree_name) holds this
    // same lock across `git worktree move` plus the metadata write, so once we
    // hold it the move has landed and the path is final. Spawning at the stale
    // pre-move path is the crash-loop in #2260. Bail if the session vanished
    // mid-flight (e.g. deleted during the handshake); ensure_container below
    // re-acquires the same lock, so this read-and-release must not hold it.
    // Also read import_pending under the lock: if the daemon restarted before
    // an imported session's first session/load completed, the reconciler must
    // still seed the transcript from the replay. The supervisor clears any
    // partial events from the interrupted attempt after it reserves the slot.
    // See #2276.
    //
    // fork_pending is read the same way: if the daemon restarted before the
    // structured fork's first connect captured the child id, the handshake
    // must still send session/fork. It is cleared once the forked id lands
    // (Task 11), so a later reattach reads None and resumes normally.
    // acp_effort is read here too (not off the snapshotted target) so a pick made
    // while this respawn was queued still lands: the handshake re-applies it
    // through the agent's thought-level config option, and a None means the
    // session inherits whatever the configured default resolves to.
    let (cwd, seed_history_replay, fork_from, acp_mode_id, acp_effort) = {
        let _guard = inst_lock.lock().await;
        let instances = service.instances.read().await;
        let Some(inst) = instances.iter().find(|i| i.id == target.id) else {
            return Err(());
        };
        (
            PathBuf::from(&inst.project_path),
            inst.import_pending == Some(true),
            inst.fork_pending.clone(),
            inst.acp_mode_id.clone(),
            inst.acp_effort.clone(),
        )
    };
    let agent = supervisor
        .pick_agent_for_tool(
            &target.tool,
            target.agent_override.as_deref(),
            &target.source_profile,
            &cwd,
        )
        .await;
    let sandbox_info = match crate::acp::sandbox::ensure_container_for_session(
        &service.instances,
        &inst_lock,
        &target.id,
        false,
    )
    .await
    {
        Ok(info) => info,
        Err(e) => {
            let message = format!("sandbox container ensure failed: {e}");
            tracing::warn!(
                target: "acp.supervisor",
                session = %target.id,
                "reconciler container ensure failed: {message}"
            );
            supervisor.publish_startup_error(&target.id, message);
            return Err(());
        }
    };

    // Thread the session profile through regardless of sandboxing: the
    // spawn path resolves agent_acp_cmd and worker env from it, so a
    // non-sandbox session on a non-default profile must not fall back to
    // the default profile.
    Ok(crate::acp::supervisor::SpawnRequest {
        session_id: target.id.clone(),
        agent,
        cwd,
        additional_dirs: vec![],
        provider_env: vec![],
        model: target.model.clone(),
        effort: acp_effort,
        stored_acp_session_id: target.stored_acp_session_id.clone(),
        fork_from,
        sandbox_info,
        source_profile: Some(target.source_profile.clone()),
        yolo_mode: target.yolo_mode,
        acp_mode_id,
        agent_command_override: command_override_for_spawn(&target.tool, &target.command),
        seed_history_replay,
    })
}

/// Build a structured view command override from the instance's persisted
/// launch command. Returns `None` for an empty command so the spawn
/// keeps the registry default. Applicability gating (registry-backed,
/// matching binary) lives in the supervisor where the resolved
/// `AgentSpec` is available. See #1766.
pub(crate) fn command_override_for_spawn(
    tool: &str,
    command: &str,
) -> Option<crate::acp::supervisor::AgentCommandOverride> {
    let command = command.trim();
    if command.is_empty() {
        return None;
    }
    Some(crate::acp::supervisor::AgentCommandOverride {
        logical_tool: tool.to_string(),
        command: command.to_string(),
    })
}

/// Snapshot a single structured view session's resume inputs from the live
/// instance list. Returns `None` when the session is gone or is not a
/// structured view session. `in_flight_turn` is always false: this is only used
/// by the prompt-wake path (#1748), where the worker was idle-auto-stopped
/// and is by definition not mid-turn.
async fn resume_target_for_session(
    service: &Arc<SessionService>,
    id: &str,
) -> Option<ResumeTarget> {
    let instances = service.instances.read().await;
    // Filter the same triage states the reconciler skips everywhere else.
    // This runs without `instance_lock` held, so an archive or snooze can
    // win the race after dormancy was cleared; resolving to None (then
    // NotFound) keeps us from respawning a session the reconciler
    // intentionally leaves sunk. See #1748.
    let inst = instances.iter().find(|i| {
        i.id == id
            && i.is_structured()
            && !i.is_archived()
            && !i.is_snoozed()
            && !i.is_trashed()
            && !i.is_idle_dormant()
    })?;
    Some(ResumeTarget {
        id: inst.id.clone(),
        tool: inst.tool.clone(),
        agent_override: inst.agent_name.clone(),
        model: inst.agent_model.clone(),
        project_path: inst.project_path.clone(),
        stored_acp_session_id: inst.acp_session_id.clone(),
        source_profile: inst.source_profile.clone(),
        in_flight_turn: false,
        yolo_mode: inst.yolo_mode,
        command: inst.command.clone(),
    })
}

/// Result of a prompt-wake resume trigger. See `trigger_resume_background`.
pub(crate) enum ResumeTrigger {
    /// A detached resume task was started; a `pending_resumes` slot is
    /// reserved so `wait_for_worker` will block until the worker is live.
    Started,
    /// A worker is already running or another resume is already in flight.
    AlreadyResuming,
    /// The session is gone or is not a structured view session; nothing to do.
    NotFound,
}

/// Synchronously reserve a resume slot for `id`, then drive a fresh worker
/// spawn in a DETACHED task so it survives the originating HTTP request
/// being cancelled on client disconnect. Because `begin_resume` reserves
/// the `pending_resumes` slot before this returns, a subsequent
/// `send_prompt` -> `wait_for_worker` observes the reservation and blocks
/// until the worker is live instead of failing fast with a 404. The next
/// reconciler tick sees the reservation via `is_running` and skips the
/// session, so there is no double-spawn. Returns `Err(CapacityFull)` when
/// the worker cap is reached so the handler can surface 503. See #1748.
///
/// Callers MUST NOT hold the session's `instance_lock` while awaiting the
/// worker this kicks: the detached task takes that same lock inside
/// `build_spawn_request`, so a caller that holds it stalls the spawn for
/// its whole `WORKER_READY_TIMEOUT` wait and then gives up. See #3172.
pub(crate) async fn trigger_resume_background(
    service: &Arc<SessionService>,
    id: &str,
) -> Result<ResumeTrigger, crate::acp::supervisor::SupervisorError> {
    use crate::acp::supervisor::{ResumeKind, ResumeReservationOutcome};
    let reservation = match service
        .acp_supervisor
        .begin_resume(id, ResumeKind::Spawn)
        .await?
    {
        ResumeReservationOutcome::Reserved(r) => r,
        ResumeReservationOutcome::AlreadyPresent => return Ok(ResumeTrigger::AlreadyResuming),
    };
    let Some(target) = resume_target_for_session(service, id).await else {
        // Session vanished between the wake and this snapshot; drop the
        // reservation (RAII clears pending + notifies waiters) and report
        // nothing to do.
        drop(reservation);
        return Ok(ResumeTrigger::NotFound);
    };
    let service = Arc::clone(service);
    crate::task_util::spawn_supervised(
        "acp.prompt_wake_resume",
        crate::task_util::PanicPolicy::Log,
        async move {
            let req = match build_spawn_request(&service, &target).await {
                // Sandbox failure already published a startup error; the
                // reservation drops here and wakes any parked send_prompt.
                Ok(req) => req,
                Err(()) => return,
            };
            let agent = req.agent.clone();
            if let Err(e) = service.acp_supervisor.spawn_inner(req, reservation).await {
                // AlreadyRunning / SpawnCancelled are benign: a worker
                // already exists or the session was intentionally torn
                // down mid-handshake. Only surface real startup failures.
                if !matches!(
                    e,
                    crate::acp::supervisor::SupervisorError::AlreadyRunning(_)
                        | crate::acp::supervisor::SupervisorError::SpawnCancelled(_)
                ) {
                    let still_present = service
                        .instances
                        .read()
                        .await
                        .iter()
                        .any(|i| i.id == target.id);
                    if still_present {
                        let message =
                            format!("Failed to start structured view agent {agent:?}: {e}");
                        tracing::warn!(
                            target: "acp.supervisor",
                            session = %target.id,
                            agent = %agent,
                            "prompt-wake spawn failed: {message}"
                        );
                        service
                            .acp_supervisor
                            .publish_startup_error(&target.id, message);
                    }
                }
            }
        },
    );
    Ok(ResumeTrigger::Started)
}

/// Re-adopt live orphan runners. A fresh spawn whose in-memory handshake
/// fails or times out can leave its DETACHED runner alive and registered on
/// disk: the runner binds its socket and writes its registry entry BEFORE the
/// handshake completes, and `connect_via_socket`'s error/timeout path does not
/// kill it (the runner owns the agent and survives daemon death by design).
/// Such a session is left in `attempted` with no in-memory worker, so the
/// reconciler's work-list loop skips it forever and the live runner is never
/// reattached, so every prompt 404s even though `aoe acp ps` shows the worker
/// alive.
///
/// This is the live-session mirror of `sweep_orphan_workers`, which only reaps
/// runners whose session is GONE; here the session IS live, so clear it from
/// `attempted` and let the same tick's resume pass adopt the runner over its
/// socket. The respawn budget still bounds a genuinely-wedged runner: a record
/// that can't be reattached burns a budget slot per tick and parks after the
/// cap rather than looping. A worker that is in-memory or mid-spawn reports
/// `is_running`, so the healthy and in-flight cases are left untouched. See
/// #1890.
async fn readopt_orphan_runners(state: &Arc<AppState>, attempted: &mut HashSet<String>) {
    let mut readopt: Vec<String> = Vec::new();
    for id in attempted.iter() {
        let running = state.acp_supervisor.is_running(id).await;
        // Skip the registry disk read on the hot path: only a non-running
        // session can possibly be a readopt candidate.
        if running {
            continue;
        }
        let has_live_runner = matches!(
            crate::process::worker_registry::load(id),
            Ok(Some(record)) if crate::process::worker_registry::is_record_live(&record)
        );
        if should_readopt_orphan_runner(running, has_live_runner) {
            readopt.push(id.clone());
        }
    }
    for id in readopt {
        attempted.remove(&id);
    }
}

async fn sweep_orphan_workers(state: &Arc<AppState>, live: &HashSet<&String>) {
    // Sweep registry entries whose session no longer exists (deleted
    // while serve was down) and SIGTERM the orphan runner so the user
    // doesn't see a phantom in `aoe acp ps`. Only runs against
    // entries that aren't currently in our `workers` map.
    let Ok(records) = crate::process::worker_registry::list() else {
        return;
    };
    for record in records {
        if live.contains(&record.session_id) {
            continue;
        }
        if state.acp_supervisor.is_running(&record.session_id).await {
            continue;
        }
        tracing::info!(
            target: "acp.supervisor",
            session = %record.session_id,
            pid = record.pid,
            "sweeping orphan worker (no matching session on disk)"
        );
        // Group-kill with SIGKILL escalation, not a single-pid SIGTERM: the
        // orphan's node wrapper and `claude` grandchild share the runner's
        // process group, and a bare SIGTERM to just the leader pid can
        // leave them alive under PID 1 (part of the leak this fixes). The
        // escalation runs detached so one stubborn orphan can't stall the
        // sweep for the grace window. If the daemon exits within the 2s
        // grace the spawned task is dropped before its SIGKILL fires, so a
        // grandchild that ignored the SIGTERM survives with only that
        // signal; the next daemon boot re-sweeps it, so this is acceptable.
        // See #1921.
        #[cfg(unix)]
        tokio::spawn(crate::process::worker::reap_group_escalating(
            record.pid,
            std::time::Duration::from_secs(2),
        ));
        crate::process::worker_registry::delete(&record.session_id).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        adopt_decision, rate_limit_resume_at, rate_limit_unknown_reset_retry_at, should_auto_stop,
        should_readopt_orphan_runner, AdoptDecision, RATE_LIMIT_MIN_PARK_SECS,
        RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS,
    };
    use chrono::{Duration, TimeZone, Utc};

    const HOUR_MS: i64 = 3_600_000;

    // --- park banner classification (#2260) ---

    /// When the parked session's project_path is gone, the banner must carry
    /// the exact `ProjectPathMissing` Display text so the web routes to the
    /// moved-cwd remediation instead of the install-the-adapter copy.
    #[test]
    fn park_message_names_missing_project_path() {
        use super::park_message;
        let missing = "/tmp/aoe-does-not-exist-2260/worktrees/Burmese";
        let msg = park_message(missing);
        assert!(
            msg.contains(&format!("project path no longer exists: {missing}")),
            "park message must embed the ProjectPathMissing text, got: {msg}"
        );
        // Defends the web regex contract: it must not steer to the adapter copy.
        assert!(!msg.contains("Retry from the dashboard"));
    }

    /// When the project_path still exists, a park is not a moved-cwd problem,
    /// so the generic retry copy is used (no false moved-cwd banner).
    #[test]
    fn park_message_generic_when_path_present() {
        use super::park_message;
        let present = std::env::temp_dir();
        let msg = park_message(&present.to_string_lossy());
        assert!(
            !msg.contains("project path no longer exists"),
            "an existing path must not produce the moved-cwd banner, got: {msg}"
        );
        assert!(msg.contains("Retry from the dashboard"));
    }

    // --- reconciler uses the live cwd, not the stale snapshot (#2260) ---

    /// A tied-worktree rename moves the dir and updates the instance's
    /// project_path, but the reconciler may already hold a ResumeTarget
    /// snapshotted at the OLD path. build_spawn_request must re-read the
    /// current project_path under the instance_lock so the respawn lands at
    /// the new path; using the stale target path is the crash-loop in #2260.
    #[tokio::test]
    async fn build_spawn_request_uses_live_project_path_not_stale_target() {
        use super::{build_spawn_request, ResumeTarget};
        use crate::server::test_support::build_test_app_state;
        use crate::session::{Instance, View};
        use std::path::PathBuf;

        let new_path = "/tmp/aoe-2260-after-rename";
        let mut inst = Instance::new("renamed", new_path);
        inst.id = "sess-2260".to_string();
        inst.view = View::Structured; // structured, non-sandboxed
        let state = build_test_app_state(vec![inst]);

        // The reconciler snapshotted the pre-move path a tick ago.
        let target = ResumeTarget {
            id: "sess-2260".to_string(),
            tool: "claude".to_string(),
            agent_override: Some("claude".to_string()),
            model: None,
            project_path: "/tmp/aoe-2260-before-rename".to_string(),
            stored_acp_session_id: None,
            source_profile: "default".to_string(),
            in_flight_turn: false,
            yolo_mode: false,
            command: String::new(),
        };

        let req = build_spawn_request(&state.session_service, &target)
            .await
            .expect("spawn request builds for a non-sandboxed structured session");
        assert_eq!(
            req.cwd,
            PathBuf::from(new_path),
            "respawn must target the current project_path, not the stale snapshot"
        );
    }

    /// A respawn must carry the session's pinned effort, or the handshake has
    /// nothing to re-apply and the picked thought level reverts to the agent
    /// default on every worker restart. An unpinned session stays `None` so it
    /// inherits whatever the configured default resolves to.
    #[tokio::test]
    async fn build_spawn_request_carries_persisted_effort() {
        use super::{build_spawn_request, ResumeTarget};
        use crate::server::test_support::build_test_app_state;
        use crate::session::{Instance, View};

        let mut inst = Instance::new("pinned", "/tmp/aoe-effort-respawn");
        inst.id = "sess-effort".to_string();
        inst.view = View::Structured;
        inst.acp_effort = Some("high".to_string());
        let mut unpinned = Instance::new("unpinned", "/tmp/aoe-effort-respawn");
        unpinned.id = "sess-no-effort".to_string();
        unpinned.view = View::Structured;
        let state = build_test_app_state(vec![inst, unpinned]);

        let target = |id: &str| ResumeTarget {
            id: id.to_string(),
            tool: "claude".to_string(),
            agent_override: Some("claude".to_string()),
            model: None,
            project_path: "/tmp/aoe-effort-respawn".to_string(),
            stored_acp_session_id: None,
            source_profile: "default".to_string(),
            in_flight_turn: false,
            yolo_mode: false,
            command: String::new(),
        };

        let req = build_spawn_request(&state.session_service, &target("sess-effort"))
            .await
            .expect("spawn request builds");
        assert_eq!(req.effort.as_deref(), Some("high"));

        let req = build_spawn_request(&state.session_service, &target("sess-no-effort"))
            .await
            .expect("spawn request builds");
        assert_eq!(req.effort, None);
    }

    // --- reconciler respawn budget (#1945) ---

    /// A session that keeps needing a respawn trips the budget after the
    /// cap, stops re-arming while over budget, and self-heals once the
    /// window elapses. This is the guard that breaks the silent crash loop.
    #[test]
    fn respawn_budget_parks_after_cap_and_recovers_after_window() {
        use super::{record_and_check_respawn_budget, RECONCILER_MAX_RESPAWNS_IN_WINDOW};
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        let mut history: HashMap<String, Vec<Instant>> = HashMap::new();
        let id = "sess-loop";
        let now = Instant::now();

        // The first MAX attempts are allowed (under budget).
        for i in 0..RECONCILER_MAX_RESPAWNS_IN_WINDOW {
            assert!(
                !record_and_check_respawn_budget(&mut history, id, now),
                "attempt {i} should be under budget"
            );
        }
        // The next attempt trips the budget (park).
        assert!(
            record_and_check_respawn_budget(&mut history, id, now),
            "attempt past the cap should be over budget"
        );
        // Over-budget calls do not record, so the window stays pinned at
        // the cap rather than growing every tick while parked.
        assert_eq!(history[id].len(), RECONCILER_MAX_RESPAWNS_IN_WINDOW);

        // Once the window fully elapses the stale attempts prune and the
        // session is allowed to retry again. Note: in the live system a
        // parked session never reaches this function again until explicitly
        // un-parked; this exercises the pruning invariant in isolation.
        let later = now + Duration::from_secs(120);
        assert!(
            !record_and_check_respawn_budget(&mut history, id, later),
            "after the window elapses the budget should reset"
        );
        assert_eq!(history[id].len(), 1);

        // A different session shares no budget.
        assert!(!record_and_check_respawn_budget(
            &mut history,
            "other-sess",
            now
        ));
    }

    // --- live orphan runner re-adoption (#1890) ---

    /// The only state that should drop a session from `attempted` for
    /// re-adoption is "no in-memory worker / reservation, but a live runner
    /// on disk": a fresh spawn whose handshake failed while the detached
    /// runner stayed up. A running session (healthy or mid-spawn) is never
    /// disturbed, and a session with no live runner stays parked under the
    /// respawn budget instead of being poked every tick.
    #[test]
    fn readopt_only_when_runner_live_and_not_running() {
        // Orphan: live runner, nothing in memory -> re-adopt.
        assert!(should_readopt_orphan_runner(false, true));
        // Healthy / mid-spawn: an in-memory worker or reservation wins, even
        // if a registry record exists.
        assert!(!should_readopt_orphan_runner(true, true));
        // No live runner: leave the respawn budget to govern; do not clear.
        assert!(!should_readopt_orphan_runner(false, false));
        assert!(!should_readopt_orphan_runner(true, false));
    }

    // --- build-version respawn policy (#1754) ---

    /// Story 1: a live worker whose build differs from the daemon and is
    /// NOT mid-turn is respawned (terminate + fresh spawn), not adopted.
    #[test]
    fn stale_build_idle_worker_respawns() {
        assert_eq!(
            adopt_decision(true, false, false),
            AdoptDecision::RespawnStaleIdle
        );
    }

    /// Story 2: a live worker whose build differs from the daemon but is
    /// mid-turn is adopted to drain, not hard-killed. The reconciler's
    /// per-tick drain check respawns it once the turn finishes.
    #[test]
    fn stale_build_busy_worker_adopts_to_drain() {
        assert_eq!(
            adopt_decision(true, false, true),
            AdoptDecision::AdoptStaleForDrain
        );
    }

    /// A live worker on the current build is reattached regardless of
    /// in-flight state: the survive-restart contract (#1037) is unchanged
    /// for same-version restarts.
    #[test]
    fn current_build_worker_attaches() {
        assert_eq!(adopt_decision(true, true, false), AdoptDecision::Attach);
        assert_eq!(adopt_decision(true, true, true), AdoptDecision::Attach);
    }

    /// A dead record fresh-spawns no matter the build/turn state; build
    /// currency only matters for a live worker.
    #[test]
    fn dead_record_fresh_spawns() {
        assert_eq!(
            adopt_decision(false, false, false),
            AdoptDecision::FreshSpawn
        );
        assert_eq!(adopt_decision(false, true, true), AdoptDecision::FreshSpawn);
    }

    #[test]
    fn resume_at_is_reset_plus_grace_when_far_in_future() {
        // A reset an hour out dominates the 30s recorded-at floor, so the
        // resume instant is exactly resets_at + grace.
        let recorded_at = Utc.timestamp_opt(1_000_000, 0).unwrap();
        let resets_at = recorded_at + Duration::hours(1);
        let got = rate_limit_resume_at(resets_at, recorded_at.timestamp_millis(), 15);
        assert_eq!(got, resets_at + Duration::seconds(15));
    }

    // #3152: the agent reported no reset at all. Auto-resume still has to
    // retry, on a policy interval measured from the park, because otherwise
    // an enabled auto-resume would never pick the session back up.
    #[test]
    fn unknown_reset_retries_an_interval_after_the_park() {
        let recorded_at = Utc.timestamp_opt(1_500_000, 0).unwrap();
        assert_eq!(
            rate_limit_unknown_reset_retry_at(recorded_at.timestamp_millis()),
            recorded_at + Duration::seconds(RATE_LIMIT_UNKNOWN_RESET_RETRY_SECS)
        );
    }

    #[test]
    fn resume_at_floors_on_recorded_at_for_past_reset() {
        // Adapter reported a reset in the past with zero grace; without the
        // floor this would resume immediately. The floor pins it to
        // recorded_at + MIN_PARK so there is no tight respawn loop.
        let recorded_at = Utc.timestamp_opt(2_000_000, 0).unwrap();
        let resets_at = recorded_at - Duration::seconds(5); // already elapsed
        let got = rate_limit_resume_at(resets_at, recorded_at.timestamp_millis(), 0);
        assert_eq!(
            got,
            recorded_at + Duration::seconds(RATE_LIMIT_MIN_PARK_SECS)
        );
    }

    #[test]
    fn resume_at_grace_wins_when_above_floor() {
        // resets_at == recorded_at, grace 120s > 30s floor: grace wins.
        let recorded_at = Utc.timestamp_opt(3_000_000, 0).unwrap();
        let got = rate_limit_resume_at(recorded_at, recorded_at.timestamp_millis(), 120);
        assert_eq!(got, recorded_at + Duration::seconds(120));
    }

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

    // --- CapacityFull as a first-class transient (#1027) ---

    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::time::Instant;

    /// #3190. An agent that parks on off-protocol work resumes itself after
    /// its prompt already completed, and that resumed turn used to end with no
    /// terminal event at all, pinning the session at Running until the 1-hour
    /// reap killed the worker. Case 0 is the real occurrence, replayed from the
    /// affected session's log: prompt, its own `Stopped`, then agent-initiated
    /// work ending on the adapter's cost-bearing end-of-turn marker.
    ///
    /// The rest are the refusals, each one thing that must veto writing a
    /// terminal the agent never sent. Table rather than a test per case
    /// because they share the whole fixture; only the seeded log and the row's
    /// status differ.
    #[tokio::test]
    #[serial_test::serial]
    async fn terminal_repair_publishes_only_for_a_finished_agent_turn() {
        use crate::acp::state::{SessionUsage, ToolCall, UsageCost};
        use crate::acp::Event;
        use crate::server::test_support::build_test_app_state;

        fn usage(cost: bool) -> Event {
            Event::UsageUpdated {
                usage: SessionUsage {
                    used: 400_000,
                    size: 1_000_000,
                    cost: cost.then(|| UsageCost {
                        amount: 21.4,
                        currency: "USD".to_string(),
                    }),
                },
            }
        }
        fn tool_call(id: &str) -> ToolCall {
            ToolCall {
                id: id.to_string(),
                name: "Terminal".to_string(),
                kind: "execute".to_string(),
                args_preview: "{}".to_string(),
                started_at: Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            }
        }
        fn tool_started(id: &str) -> Event {
            Event::ToolCallStarted {
                tool_call: tool_call(id),
            }
        }
        fn tool_done(id: &str) -> Event {
            Event::ToolCallCompleted {
                tool_call_id: id.to_string(),
                is_error: false,
                content: String::new(),
                output: Vec::new(),
                completed_at: Utc::now(),
                async_subagent: false,
            }
        }
        let stopped = |reason: &str| Event::Stopped {
            reason: reason.to_string(),
        };
        // The agent-initiated turn, shared by every case: no UserPromptSent
        // behind it, ending on the cost-bearing marker.
        let finished_agent_turn = |extra: Vec<Event>| {
            let mut evs = vec![
                Event::UserPromptSent {
                    text: "continue".to_string(),
                    attachments: Vec::new(),
                },
                stopped("prompt_complete"),
                tool_started("t1"),
                tool_done("t1"),
                Event::AgentMessageChunk {
                    text: "Done.".to_string(),
                },
            ];
            evs.extend(extra);
            evs.push(usage(true));
            evs
        };

        struct Case {
            name: &'static str,
            events: Vec<Event>,
            status: crate::session::Status,
            /// Age of every seeded event, so a case can sit inside the grace.
            age_secs: i64,
            expect_repair: bool,
        }
        let cases = vec![
            Case {
                name: "finished agent-initiated turn",
                events: finished_agent_turn(Vec::new()),
                status: crate::session::Status::Running,
                age_secs: 120,
                expect_repair: true,
            },
            Case {
                // The seq counter advances for ambient events too, so a
                // repair that expected the substantive event's own seq
                // refused forever on any session a resume had replayed
                // into. See PR #3192 review.
                name: "ambient event trails the marker",
                events: {
                    let mut evs = finished_agent_turn(Vec::new());
                    evs.push(Event::AcpSessionAssigned {
                        acp_session_id: "acp-1".to_string(),
                    });
                    evs
                },
                status: crate::session::Status::Running,
                age_secs: 120,
                expect_repair: true,
            },
            Case {
                name: "still inside the grace window",
                events: finished_agent_turn(Vec::new()),
                status: crate::session::Status::Running,
                age_secs: 5,
                expect_repair: false,
            },
            Case {
                name: "latest event is not the end-of-turn marker",
                // A cost-free usage frame is ordinary mid-turn accounting.
                events: {
                    let mut evs = finished_agent_turn(Vec::new());
                    evs.push(usage(false));
                    evs
                },
                status: crate::session::Status::Running,
                age_secs: 120,
                expect_repair: false,
            },
            Case {
                name: "user prompt still lacks its terminator",
                events: vec![
                    Event::UserPromptSent {
                        text: "go".to_string(),
                        attachments: Vec::new(),
                    },
                    usage(true),
                ],
                status: crate::session::Status::Running,
                age_secs: 120,
                expect_repair: false,
            },
            Case {
                name: "tool still open in this epoch",
                events: finished_agent_turn(vec![tool_started("t2")]),
                status: crate::session::Status::Running,
                age_secs: 120,
                expect_repair: false,
            },
            Case {
                // The veto that keeps the daemon from terminating a session
                // genuinely blocked on the user. It has to be reachable with
                // status Running, because an approval can outlive the Waiting
                // status: a later activity event overwrites it. The row below
                // seeds the approval BEFORE the marker so the marker is still
                // latest, which is what isolates this veto from the
                // not-the-marker one. See PR #3192 review.
                name: "unresolved approval, marker still latest",
                events: finished_agent_turn(vec![Event::ApprovalRequested {
                    approval: crate::acp::approvals::Approval {
                        nonce: crate::acp::approvals::Nonce("n-1".to_string()),
                        tool_call: tool_call("t-approval"),
                        destructive: false,
                        requested_at: Utc::now(),
                        resolved: None,
                    },
                }]),
                status: crate::session::Status::Running,
                age_secs: 120,
                expect_repair: false,
            },
            Case {
                name: "waiting on the user",
                events: finished_agent_turn(Vec::new()),
                status: crate::session::Status::Waiting,
                age_secs: 120,
                expect_repair: false,
            },
        ];

        for case in cases {
            let id = "acp-terminal-repair";
            let project = tempfile::TempDir::new().unwrap();
            let mut inst = structured_instance(id, &project.path().to_string_lossy());
            inst.status = case.status;
            let state = build_test_app_state(vec![inst]);
            let at_ms = Utc::now().timestamp_millis() - case.age_secs * 1000;
            let last_seq = case.events.len() as u64;
            for (idx, event) in case.events.iter().enumerate() {
                state
                    .acp_event_store
                    .record_at(id, idx as u64 + 1, event, at_ms)
                    .unwrap();
            }
            // Mirror daemon startup: the seq counter is seeded from the log,
            // so the repair's compare-and-publish has something to compare.
            state
                .acp_supervisor
                .hydrate_seqs([(id.to_string(), last_seq)]);

            super::repair_missing_terminal(&state).await;

            let repaired: Vec<u64> = state
                .acp_event_store
                .replay_from(id, 0)
                .into_iter()
                .filter(|(_, e)| {
                    matches!(e, Event::Stopped { reason } if reason == "inferred_prompt_complete")
                })
                .map(|(seq, _)| seq)
                .collect();
            if case.expect_repair {
                assert_eq!(
                    repaired,
                    vec![last_seq + 1],
                    "{}: expected exactly one inferred terminal, appended after the marker",
                    case.name
                );
            } else {
                assert!(
                    repaired.is_empty(),
                    "{}: must not write a terminal the agent never sent",
                    case.name
                );
            }
        }
    }

    fn structured_instance(id: &str, project_path: &str) -> crate::session::Instance {
        use crate::session::{Instance, View};
        let mut inst = Instance::new(id, project_path);
        inst.id = id.to_string();
        inst.view = View::Structured;
        // Bogus agent: once a slot frees, the fresh spawn fails fast with
        // UnknownAgent (resolved before any process or socket work) so
        // resume_one returns SpawnFinished without launching a real runner.
        // At capacity the agent is irrelevant, since begin_resume returns
        // CapacityFull before spawn_inner runs.
        inst.agent_name = Some("aoe-no-such-agent-1027".to_string());
        inst
    }

    /// Isolate HOME so the worker registry (and thus the reconciler's orphan
    /// sweep / capacity count) can't see the developer's real dev-mode
    /// entries. Returns the temp dirs so the caller keeps them alive.
    async fn capacity_test_state(
        id: &str,
    ) -> (
        Arc<crate::server::AppState>,
        tempfile::TempDir,
        tempfile::TempDir,
    ) {
        use crate::server::test_support::build_test_app_state;
        let home = tempfile::TempDir::new().unwrap();
        // SAFETY: reconciler capacity tests are `#[serial]`, so no other test
        // races this process-global env mutation.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
        }
        let project = tempfile::TempDir::new().unwrap();
        let inst = structured_instance(id, &project.path().to_string_lossy());
        let state = build_test_app_state(vec![inst]);
        (state, home, project)
    }

    async fn run_tick(
        state: &Arc<crate::server::AppState>,
        attempted: &mut HashSet<String>,
        respawn_history: &mut HashMap<String, Vec<Instant>>,
        parked: &mut HashSet<String>,
        capacity_deferred: &mut HashSet<String>,
    ) {
        // Pre-stamped so the cadence-gated passes sit out these ticks; the
        // capacity tests below exercise the spawn path only.
        let mut cadence = super::ReapCadence {
            idle: Some(Instant::now()),
            rate_limit: Some(Instant::now()),
            terminal_repair: Some(Instant::now()),
        };
        super::reconcile_acp_workers(
            state,
            attempted,
            &mut cadence,
            respawn_history,
            parked,
            capacity_deferred,
        )
        .await;
    }

    fn capacity_startup_errors(state: &Arc<crate::server::AppState>, id: &str) -> usize {
        state
            .acp_event_store
            .replay_from(id, 0)
            .into_iter()
            .filter(|(_, e)| {
                matches!(e, crate::acp::Event::AgentStartupError { message }
                    if message.contains("capacity full"))
            })
            .count()
    }

    /// A restart marker written AFTER the reaper already ran must still be
    /// honoured. `aoe session add-project` (#3103) deletes the registry entry
    /// and SIGTERMs first, and only writes the marker once the moved workspace
    /// is durable, so on a slow conversion the marker routinely lands after
    /// `reap_user_stopped` has already classified the teardown as
    /// `user_stopped` and pinned the id in `attempted`. Without the late-marker
    /// branch the session sits stopped until the next daemon start, and the
    /// stale marker file is left behind to poison a later `aoe acp stop`.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_late_restart_marker_clears_the_budget_and_is_consumed() {
        let (state, _home, _project) = capacity_test_state("s-late-marker").await;

        let mut attempted = HashSet::new();
        let mut respawn_history: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut parked = HashSet::new();
        let mut capacity_deferred = HashSet::new();

        // The state the reaper leaves behind when it wins the race.
        attempted.insert("s-late-marker".to_string());
        crate::process::worker_registry::mark_restart_pending("s-late-marker");

        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;

        // The marker must be consumed, not left behind to poison a later stop.
        assert!(
            !crate::process::worker_registry::take_restart_marker("s-late-marker"),
            "the tick must consume the late marker"
        );
        // And the budget clear must actually let the spawn pass run: the bogus
        // agent fails fast with UnknownAgent, which records one startup error.
        // Without the late-marker branch the loop `continue`s and records none.
        assert_eq!(
            state.acp_event_store.replay_from("s-late-marker", 0).len(),
            1,
            "clearing the budget must let the spawn pass attempt a respawn"
        );
        assert!(
            !crate::process::worker_registry::take_restart_marker("s-late-marker"),
            "the marker must be consumed by the tick, not left to poison a later stop"
        );
    }

    /// The other half: with no marker, an id in `attempted` stays skipped.
    /// Without this the late-marker branch would re-arm every parked session on
    /// every tick and defeat the crash-loop budget entirely.
    #[tokio::test]
    #[serial_test::serial]
    async fn no_marker_leaves_an_attempted_id_skipped() {
        let (state, _home, _project) = capacity_test_state("s-no-marker").await;

        let mut attempted = HashSet::new();
        let mut respawn_history: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut parked = HashSet::new();
        let mut capacity_deferred = HashSet::new();

        attempted.insert("s-no-marker".to_string());

        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;

        assert!(
            attempted.contains("s-no-marker"),
            "without a marker the id must stay pinned; otherwise the respawn budget is void"
        );
        assert!(
            state
                .acp_event_store
                .replay_from("s-no-marker", 0)
                .is_empty(),
            "a pinned id must not reach the spawn pass"
        );
    }

    /// The core of the fix: a CapacityFull spawn must re-arm `attempted`
    /// (remove, never insert) so the SAME process retries on the next tick.
    /// Testing via a daemon restart would mask this: restart wipes the
    /// in-memory `attempted`, hiding the "stuck forever" bug.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_deferred_rearms_attempted_and_retries_next_tick() {
        let (state, _home, _project) = capacity_test_state("s-cap").await;
        state.acp_supervisor.test_insert_worker("occupant").await;

        let mut attempted = HashSet::new();
        let mut respawn_history: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut parked = HashSet::new();
        let mut capacity_deferred = HashSet::new();

        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;
        assert!(
            !attempted.contains("s-cap"),
            "CapacityDeferred must re-arm the retry, not pin the id in attempted"
        );
        assert!(
            capacity_deferred.contains("s-cap"),
            "the capacity marker must be set after the deferral"
        );
        assert!(
            !parked.contains("s-cap"),
            "CapacityFull must not park the session (that is the crash-loop guard)"
        );

        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;
        assert!(
            !attempted.contains("s-cap"),
            "the next tick must retry (attempted stays clear), not skip forever"
        );
    }

    /// The capacity banner is published once per transition, not once per
    /// tick: `publish_startup_error` does not dedup, so without the
    /// `capacity_deferred` gate a session stuck at capacity would spam the
    /// event store every 2s.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_deferred_publishes_once_across_ticks() {
        let (state, _home, _project) = capacity_test_state("s-once").await;
        state.acp_supervisor.test_insert_worker("occupant").await;

        let mut attempted = HashSet::new();
        let mut respawn_history: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut parked = HashSet::new();
        let mut capacity_deferred = HashSet::new();

        for _ in 0..3 {
            run_tick(
                &state,
                &mut attempted,
                &mut respawn_history,
                &mut parked,
                &mut capacity_deferred,
            )
            .await;
        }

        assert_eq!(
            capacity_startup_errors(&state, "s-once"),
            1,
            "capacity banner must publish exactly once across ticks, not per tick"
        );
    }

    /// The budget refund pops only this tick's decision entry; genuine
    /// prior-crash history survives so a truly crashing session can't use a
    /// CapacityFull to escape the #1945 park budget.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_deferred_pop_preserves_prior_crash_history() {
        let (state, _home, _project) = capacity_test_state("s-hist").await;
        state.acp_supervisor.test_insert_worker("occupant").await;

        let mut attempted = HashSet::new();
        // Two prior crash entries, below the park cap so the session still
        // reaches the spawn (and thus CapacityFull) this tick.
        let now = Instant::now();
        let mut respawn_history: HashMap<String, Vec<Instant>> = HashMap::new();
        respawn_history.insert("s-hist".to_string(), vec![now, now]);
        let mut parked = HashSet::new();
        let mut capacity_deferred = HashSet::new();

        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;

        assert_eq!(
            respawn_history.get("s-hist").map(Vec::len).unwrap_or(0),
            2,
            "only this tick's decision entry may be popped; prior crashes survive"
        );
    }

    /// When a peer worker stops and the slot frees, the next tick re-attempts
    /// the deferred session and clears the capacity marker on the
    /// SpawnFinished path (the critical clear, since a re-attempt leaves the id
    /// in `attempted` and never revisits the is_running branch). The re-attempt
    /// here fails fast (bogus agent) but still routes through SpawnFinished, so
    /// it exercises the exact clear path a real respawn would.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_deferred_clears_marker_when_slot_frees() {
        let (state, _home, _project) = capacity_test_state("s-free").await;
        state.acp_supervisor.test_insert_worker("occupant").await;

        let mut attempted = HashSet::new();
        let mut respawn_history: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut parked = HashSet::new();
        let mut capacity_deferred = HashSet::new();

        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;
        assert!(
            capacity_deferred.contains("s-free"),
            "precondition: the session is capacity-deferred after the first tick"
        );

        // A peer worker stops: the slot frees.
        state.acp_supervisor.test_remove_worker("occupant").await;
        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;
        assert!(
            !capacity_deferred.contains("s-free"),
            "a freed slot must re-attempt the deferred session and clear the marker"
        );
        assert_eq!(
            capacity_startup_errors(&state, "s-free"),
            1,
            "clearing the marker must not re-publish the capacity banner"
        );
    }

    /// The second (out-of-band) clear site: a deferred session whose worker
    /// comes online via a REST spawn is picked up by the `is_running` branch,
    /// which clears the capacity marker. Covers the path the reconciler's own
    /// respawn (SpawnFinished) never reaches.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_deferred_cleared_by_is_running_branch() {
        let (state, _home, _project) = capacity_test_state("s-oob").await;
        state.acp_supervisor.test_insert_worker("occupant").await;

        let mut attempted = HashSet::new();
        let mut respawn_history: HashMap<String, Vec<Instant>> = HashMap::new();
        let mut parked = HashSet::new();
        let mut capacity_deferred = HashSet::new();

        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;
        assert!(
            capacity_deferred.contains("s-oob"),
            "precondition: the session is capacity-deferred after the first tick"
        );

        // A REST spawn brings the deferred session's own worker online.
        state.acp_supervisor.test_insert_worker("s-oob").await;
        run_tick(
            &state,
            &mut attempted,
            &mut respawn_history,
            &mut parked,
            &mut capacity_deferred,
        )
        .await;
        assert!(
            !capacity_deferred.contains("s-oob"),
            "the is_running branch must clear the marker for an out-of-band worker"
        );
    }

    /// §2 message selection, shared by both the create-path (`create_session`)
    /// and the enable-path (`acp_enable`) via `structured_spawn_error_message`:
    /// a CapacityFull spawn surfaces the capacity Display (matching the
    /// front-end capacity regex) so the session shows the capacity banner, while
    /// any other error keeps the generic crash-style message.
    #[test]
    fn structured_spawn_error_message_prefers_capacity_display_over_generic() {
        use crate::acp::supervisor::SupervisorError;
        use crate::server::api::structured_spawn_error_message;

        let capacity = SupervisorError::CapacityFull {
            current: 1,
            limit: 1,
        };
        let msg = structured_spawn_error_message(&capacity, "claude-code");
        assert!(
            msg.contains("capacity full") && msg.contains("max_concurrent_workers"),
            "capacity errors must surface the capacity Display, got: {msg}"
        );
        assert!(
            !msg.contains("Failed to start structured view agent"),
            "capacity errors must not use the generic crash-style message"
        );

        let generic = SupervisorError::UnknownAgent("bogus".to_string());
        let generic_msg = structured_spawn_error_message(&generic, "bogus");
        assert!(
            generic_msg.contains("Failed to start structured view agent"),
            "non-capacity errors keep the generic message, got: {generic_msg}"
        );
    }
}
