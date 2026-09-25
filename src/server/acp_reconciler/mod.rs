//! Structured view worker reconciler: each 2s tick brings on-disk session state and the supervisor's worker pool into agreement.

mod idle;
mod rate_limit;
mod resume;
#[cfg(test)]
mod test_fixtures;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::AppState;
use crate::acp::event_store::EventStore;
use crate::daemon::AcpWorkerState;
use crate::session::Instance;

pub(crate) use rate_limit::enqueue_rate_limit_continuation;
pub(crate) use resume::{command_override_for_spawn, trigger_resume_background, ResumeTrigger};

use resume::{resume_one, ResumeTarget};

/// Crash-loop budget (#1945): at most this many resume decisions per session
/// per window before it is parked until an explicit retry. Looser than the
/// supervisor's 3/60s because it counts before the outcome is known.
const RECONCILER_MAX_RESPAWNS_IN_WINDOW: usize = 5;
const RECONCILER_RESPAWN_WINDOW: Duration = Duration::from_secs(60);

/// Parallel resume cap; each claude-agent-acp boot is memory-heavy (#1088).
const MAX_CONCURRENT_RESUMES: u32 = 4;

/// Records a resume attempt and reports whether the respawn budget is spent.
/// An over-budget attempt is not recorded, so the history stays pinned at the cap.
fn record_and_check_respawn_budget(
    history: &mut HashMap<String, Vec<Instant>>,
    id: &str,
    now: Instant,
) -> bool {
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

/// Clean slate for `id`: re-armed, un-parked, budget and capacity marker reset.
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

/// Banner for a parked session. A missing project path embeds the
/// `ProjectPathMissing` Display text so the web routes to the moved-cwd remediation (#2260).
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

#[derive(Debug, Clone)]
enum ResumeOutcome {
    Attached,
    /// The orphan registry entry was swept; retry next tick unless parked.
    RetryAfterAttachTimeout,
    /// Spawn finished with or without error; `attempted` stays set so a
    /// permanently failing spawn does not loop.
    SpawnFinished,
    /// `CapacityFull` is transient, not a crash: re-arm and publish the banner
    /// once. `message` is its Display, matched by the front-end regex (#1027).
    CapacityDeferred {
        message: String,
    },
}

/// When each cadence-gated pass last ran.
#[derive(Default)]
pub struct ReapCadence {
    pub idle: Option<Instant>,
    pub rate_limit: Option<Instant>,
    pub terminal_repair: Option<Instant>,
}

fn due(last: &mut Option<Instant>, interval: Duration) -> bool {
    if last.is_some_and(|t| t.elapsed() < interval) {
        return false;
    }
    *last = Some(Instant::now());
    true
}

/// Structured and not archived, snoozed or trashed.
fn is_untriaged_structured(i: &Instance) -> bool {
    i.is_structured() && !i.is_archived() && !i.is_snoozed() && !i.is_trashed()
}

/// Eligible for a reconciler-driven worker.
fn is_resumable(i: &Instance) -> bool {
    is_untriaged_structured(i) && !i.is_idle_dormant()
}

/// Runs a blocking event-store query for `id` off the runtime; `None` (logged) if the task panicked.
async fn query_store<T: Send + 'static>(
    store: &Arc<EventStore>,
    id: &str,
    what: &str,
    f: impl FnOnce(&EventStore, &str) -> T + Send + 'static,
) -> Option<T> {
    let store = Arc::clone(store);
    let owned = id.to_string();
    match tokio::task::spawn_blocking(move || f(&store, &owned)).await {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(target: "acp.supervisor", session = %id, error = %e, "{what} probe failed");
            None
        }
    }
}

/// Resolves a config value once per distinct profile, off the runtime.
async fn resolve_per_profile<T: Send + 'static>(
    profiles: impl Iterator<Item = String>,
    pick: fn(&crate::session::config::Config) -> T,
) -> HashMap<String, T> {
    let distinct: HashSet<String> = profiles.collect();
    tokio::task::spawn_blocking(move || {
        distinct
            .into_iter()
            .map(|p| {
                let value =
                    pick(&crate::session::config::profile_config::resolve_config_or_warn(&p));
                (p, value)
            })
            .collect()
    })
    .await
    .unwrap_or_default()
}

pub async fn reconcile_acp_workers(
    state: &Arc<AppState>,
    attempted: &mut HashSet<String>,
    cadence: &mut ReapCadence,
    respawn_history: &mut HashMap<String, Vec<Instant>>,
    parked: &mut HashSet<String>,
    capacity_deferred: &mut HashSet<String>,
) {
    let supervisor = &state.acp_supervisor;
    supervisor.retry_pending_teardowns().await;

    // Before the reaper, so this tick's reaper tears down the drained handle.
    resume::respawn_drained_stale_workers(state).await;

    // `aoe acp stop|kill|restart` runs out of process; the reaper surfaces it
    // and returns restart ids, which, like respawn requests (#2109), get a clean slate.
    for id in supervisor
        .reap_user_stopped()
        .await
        .into_iter()
        .chain(supervisor.take_respawn_requests())
    {
        forget_session_budget(&id, attempted, parked, respawn_history, capacity_deferred);
    }
    // Pre-session startup failures re-arm under the ordinary budget.
    for id in supervisor.take_startup_failures() {
        attempted.remove(&id);
    }
    for id in supervisor.take_respawned_in_place() {
        resume::requeue_interrupted_monitor(state, &id).await;
    }

    // The idle reap runs before the snapshot so a newly dormant session is not
    // respawned, and before terminal repair so it keeps its own terminal.
    if due(&mut cadence.idle, idle::IDLE_REAP_INTERVAL) {
        idle::reap_idle_workers(state).await;
    }
    if due(&mut cadence.terminal_repair, idle::TERMINAL_REPAIR_INTERVAL) {
        idle::repair_missing_terminal(state).await;
    }
    // Before the snapshot so a released park respawns this same tick.
    let released_from_park = if due(
        &mut cadence.rate_limit,
        rate_limit::RATE_LIMIT_RESUME_INTERVAL,
    ) {
        rate_limit::reap_rate_limit_resumes(state, attempted, parked).await
    } else {
        HashSet::new()
    };

    // Triaged sessions are excluded so an archive/snooze teardown is not undone (#1581).
    let (targets, with_queued_prompts): (Vec<ResumeTarget>, HashSet<String>) = {
        let instances = state.instances.read().await;
        (
            instances
                .iter()
                .filter(|i| is_resumable(i))
                .map(ResumeTarget::from_instance)
                .collect(),
            instances
                .iter()
                .filter(|i| !i.queued_prompts.is_empty())
                .map(|i| i.id.clone())
                .collect(),
        )
    };
    let live: HashSet<&String> = targets.iter().map(|t| &t.id).collect();
    attempted.retain(|id| live.contains(id));
    parked.retain(|id| live.contains(id));
    respawn_history.retain(|id, _| live.contains(id));
    capacity_deferred.retain(|id| live.contains(id));

    // Must precede scheduling: capacity counts registry entries, so dead
    // entries would block legitimate spawns.
    sweep_orphan_workers(state, &live).await;
    readopt_orphan_runners(state, attempted).await;
    drain_pending_initial_turns(state).await;
    drain_queued_prompts(state).await;

    let mut tasks = Vec::new();
    for mut target in targets {
        let id = target.id.clone();
        if attempted.contains(&id) {
            // A restart marker can land after the reaper ran: add-project (#3103)
            // writes it only once the moved workspace is durable.
            if supervisor.worker_state(&id).await == AcpWorkerState::Stopping
                || !supervisor.take_late_restart_marker(&id)
            {
                continue;
            }
            forget_session_budget(&id, attempted, parked, respawn_history, capacity_deferred);
        }
        match supervisor.worker_state(&id).await {
            AcpWorkerState::Running | AcpWorkerState::Resuming => {
                // Already owned; a live worker is also the user's retry, so un-park.
                forget_session_budget(&id, attempted, parked, respawn_history, capacity_deferred);
                attempted.insert(id);
                continue;
            }
            AcpWorkerState::Stopping => {
                attempted.insert(id);
                continue;
            }
            AcpWorkerState::Absent => {}
        }
        if parked.contains(&id) {
            attempted.insert(id);
            continue;
        }
        // A rate-limit park (#3514) is left to the auto-resume pass; a cap park
        // (#3688) is released by a queued prompt.
        if !released_from_park.contains(&id) {
            let park = query_store(&state.acp_event_store, &id, "rate-limit park", |s, id| {
                s.rate_limit_park(id)
            })
            .await
            .flatten();
            if let Some(park) =
                park.filter(|p| !p.cap_reached || !with_queued_prompts.contains(&id))
            {
                tracing::debug!(
                    target: "acp.supervisor",
                    session = %id,
                    cap_reached = park.cap_reached,
                    "holding respawn: session is parked on a rate limit"
                );
                attempted.insert(id);
                continue;
            }
        }
        if record_and_check_respawn_budget(respawn_history, &id, Instant::now()) {
            tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                max_respawns = RECONCILER_MAX_RESPAWNS_IN_WINDOW,
                window_secs = RECONCILER_RESPAWN_WINDOW.as_secs(),
                "structured-view worker respawn budget exhausted; parking session"
            );
            if parked.insert(id.clone()) {
                supervisor.publish_startup_error(&id, park_message(&target.project_path));
            }
            attempted.insert(id);
            continue;
        }
        target.in_flight_turn =
            query_store(&state.acp_event_store, &id, "in-flight turn", |s, id| {
                s.has_in_flight_turn(id)
            })
            .await
            .unwrap_or(false);
        attempted.insert(id);
        tasks.push(target);
    }
    if tasks.is_empty() {
        return;
    }

    let cfg = crate::session::config::profile_config::resolve_config_or_warn(&state.profile);
    let resume_limit = MAX_CONCURRENT_RESUMES
        .min(cfg.acp.max_concurrent_workers)
        .max(1);
    let semaphore = Arc::new(Semaphore::new(resume_limit as usize));
    let mut set: JoinSet<(String, ResumeOutcome)> = JoinSet::new();
    for target in tasks {
        let state = Arc::clone(state);
        let sem = Arc::clone(&semaphore);
        set.spawn(async move {
            let Ok(_permit) = sem.acquire().await else {
                return (target.id, ResumeOutcome::SpawnFinished);
            };
            let id = target.id.clone();
            (id, resume_one(state, target).await)
        });
    }

    while let Some(result) = set.join_next().await {
        match result {
            Ok((id, ResumeOutcome::RetryAfterAttachTimeout)) => {
                if !parked.contains(&id) {
                    attempted.remove(&id);
                }
            }
            Ok((id, ResumeOutcome::CapacityDeferred { message })) => {
                // Refund only this tick's budget entry so prior crash history survives.
                if let Some(entries) = respawn_history.get_mut(&id) {
                    entries.pop();
                    if entries.is_empty() {
                        respawn_history.remove(&id);
                    }
                }
                // Never pin: `attempted` would skip the session forever.
                attempted.remove(&id);
                // `publish_startup_error` does not dedup, so publish once per transition.
                if capacity_deferred.insert(id.clone()) {
                    supervisor.publish_startup_error(&id, message);
                }
            }
            Ok((id, ResumeOutcome::Attached | ResumeOutcome::SpawnFinished)) => {
                capacity_deferred.remove(&id);
            }
            Err(e) => {
                tracing::error!(target: "acp.supervisor", "resume task panicked: {e}");
            }
        }
    }
}

/// Spawns the pending-initial-turn drain (#2897) for sessions with a live worker.
async fn drain_pending_initial_turns(state: &Arc<AppState>) {
    let candidates: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| i.pending_initial_turn.is_some() && is_untriaged_structured(i))
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
            async move { service.drain_pending_initial_turn(&id).await },
        );
    }
}

/// Delivers idle sessions' server-owned prompt queues with no client tab open.
/// The drain must never hold `instance_lock` while waiting on a resume (#3621).
async fn drain_queued_prompts(state: &Arc<AppState>) {
    let candidates: Vec<(String, bool)> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                !i.queued_prompts.is_empty()
                    && i.status == crate::session::Status::Idle
                    && is_untriaged_structured(i)
            })
            .map(|i| (i.id.clone(), i.is_idle_dormant()))
            .collect()
    };
    for (id, dormant) in candidates {
        let service = Arc::clone(&state.session_service);
        if state.acp_supervisor.is_running(&id).await {
            crate::task_util::spawn_supervised(
                "acp.queue_drain",
                crate::task_util::PanicPolicy::Log,
                async move { service.drain_queued_prompts_once(&id).await },
            );
        } else if dormant {
            // Clear dormancy so the resume pass respawns it under its budget (#3172).
            crate::task_util::spawn_supervised(
                "acp.queue_drain_wake",
                crate::task_util::PanicPolicy::Log,
                async move { service.wake_dormant_for_queue_drain(&id).await },
            );
        }
    }
}

/// Un-pins sessions whose detached runner is alive on disk with no in-memory
/// worker, so this tick's resume pass reattaches it (#1890).
async fn readopt_orphan_runners(state: &Arc<AppState>, attempted: &mut HashSet<String>) {
    let mut readopt = Vec::new();
    for id in attempted.iter() {
        if state.acp_supervisor.is_owned(id).await {
            continue;
        }
        if matches!(
            crate::process::worker_registry::load(id),
            Ok(Some(record)) if crate::process::worker_registry::is_record_live(&record)
        ) {
            readopt.push(id.clone());
        }
    }
    for id in readopt {
        attempted.remove(&id);
    }
}

/// Reaps registry entries whose session no longer exists.
async fn sweep_orphan_workers(state: &Arc<AppState>, live: &HashSet<&String>) {
    let Ok(records) = crate::process::worker_registry::list() else {
        return;
    };
    for record in records {
        if live.contains(&record.session_id)
            || state.acp_supervisor.is_owned(&record.session_id).await
        {
            continue;
        }
        tracing::info!(
            target: "acp.supervisor",
            session = %record.session_id,
            pid = record.pid,
            "sweeping orphan worker (no matching session on disk)"
        );
        // Group kill with escalation, detached so one stubborn orphan cannot stall the sweep (#1921).
        #[cfg(unix)]
        tokio::spawn(crate::process::worker::reap_group_escalating(
            record.pid,
            Duration::from_secs(2),
        ));
        crate::process::worker_registry::delete(&record.session_id).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::{capacity_startup_errors, test_state, Tick};
    use super::*;

    #[test]
    fn park_message_classifies_missing_project_path() {
        let missing = "/tmp/aoe-does-not-exist-2260/worktrees/Burmese";
        let msg = park_message(missing);
        assert!(msg.contains(&format!("project path no longer exists: {missing}")));
        assert!(!msg.contains("Retry from the dashboard"));

        let msg = park_message(&std::env::temp_dir().to_string_lossy());
        assert!(!msg.contains("project path no longer exists"));
        assert!(msg.contains("Retry from the dashboard"));

        use crate::acp::supervisor::SupervisorError;
        use crate::server::api::structured_spawn_error_message;

        let capacity = SupervisorError::CapacityFull {
            current: 1,
            limit: 1,
        };
        let msg = structured_spawn_error_message(&capacity, "claude-code");
        assert!(msg.contains("capacity full") && msg.contains("max_concurrent_workers"));
        assert!(!msg.contains("Failed to start structured view agent"));

        let generic = SupervisorError::UnknownAgent("bogus".to_string());
        assert!(structured_spawn_error_message(&generic, "bogus")
            .contains("Failed to start structured view agent"));
    }

    #[test]
    fn respawn_budget_parks_after_cap_and_recovers_after_window() {
        let mut history = HashMap::new();
        let id = "sess-loop";
        let now = Instant::now();
        for _ in 0..RECONCILER_MAX_RESPAWNS_IN_WINDOW {
            assert!(!record_and_check_respawn_budget(&mut history, id, now));
        }
        assert!(record_and_check_respawn_budget(&mut history, id, now));
        // Over-budget calls do not record.
        assert_eq!(history[id].len(), RECONCILER_MAX_RESPAWNS_IN_WINDOW);

        let later = now + Duration::from_secs(120);
        assert!(!record_and_check_respawn_budget(&mut history, id, later));
        assert_eq!(history[id].len(), 1);
        assert!(!record_and_check_respawn_budget(&mut history, "other", now));
    }

    /// A late restart marker (#3103) re-arms and is consumed; without one an
    /// attempted id stays pinned, or the crash-loop budget would be void.
    #[tokio::test]
    #[serial_test::serial]
    async fn attempted_id_rearms_only_on_a_late_restart_marker() {
        for with_marker in [true, false] {
            let id = "s-late-marker";
            let (_home, state, _project) = test_state(id);
            let mut tick = Tick::default();
            tick.attempted.insert(id.to_string());
            if with_marker {
                crate::process::worker_registry::mark_restart_pending(id, 5);
            }
            tick.run(&state).await;

            assert!(crate::process::worker_registry::peek_restart_marker(id).is_none());
            // The bogus agent fails fast, so a spawn attempt records one startup error.
            let expected_events = usize::from(with_marker);
            assert_eq!(
                state.acp_event_store.replay_from(id, 0).len(),
                expected_events,
                "with_marker={with_marker}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_startup_failure_rearms_the_pinned_session_under_budget() {
        let id = "s-startup-failed";
        let (_home, state, _project) = test_state(id);
        let mut tick = Tick::default();
        tick.attempted.insert(id.to_string());
        tick.respawn_history
            .insert(id.to_string(), vec![Instant::now()]);
        state.acp_supervisor.note_startup_failure(id);

        tick.run(&state).await;

        assert_eq!(state.acp_event_store.replay_from(id, 0).len(), 1);
        assert_eq!(tick.respawn_history[id].len(), 2, "counted, not wiped");
        assert!(state.acp_supervisor.take_startup_failures().is_empty());
    }

    /// CapacityFull re-arms instead of pinning (a restart would hide a stuck
    /// id), never parks, keeps prior crash history, and publishes its banner
    /// once across ticks.
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_deferred_rearms_and_publishes_once() {
        let (_home, state, _project) = test_state("s-cap");
        state.acp_supervisor.test_insert_worker("occupant").await;
        let mut tick = Tick::default();
        let now = Instant::now();
        tick.respawn_history
            .insert("s-cap".to_string(), vec![now, now]);
        for _ in 0..3 {
            tick.run(&state).await;
            assert!(!tick.attempted.contains("s-cap"));
            assert!(tick.capacity_deferred.contains("s-cap"));
            assert!(!tick.parked.contains("s-cap"));
        }
        assert_eq!(tick.respawn_history.get("s-cap").map(Vec::len), Some(2));
        assert_eq!(capacity_startup_errors(&state, "s-cap"), 1);
    }

    /// The capacity marker clears both when a slot frees (the SpawnFinished
    /// path) and when an out-of-band worker comes online (the running branch).
    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_marker_clears_when_the_session_gets_a_slot() {
        for slot_freed in [true, false] {
            let id = "s-free";
            let (_home, state, _project) = test_state(id);
            state.acp_supervisor.test_insert_worker("occupant").await;
            let mut tick = Tick::default();
            tick.run(&state).await;
            assert!(tick.capacity_deferred.contains(id), "precondition");

            if slot_freed {
                state.acp_supervisor.test_remove_worker("occupant").await;
            } else {
                state.acp_supervisor.test_insert_worker(id).await;
            }
            tick.run(&state).await;
            assert!(
                !tick.capacity_deferred.contains(id),
                "slot_freed={slot_freed}"
            );
            assert_eq!(capacity_startup_errors(&state, id), 1);
        }
    }

    /// Wake-on-drain: a dormant session with a queue must be woken, or its
    /// queue sits behind a worker the resume pass never respawns.
    #[tokio::test]
    async fn drain_queued_prompts_wakes_a_dormant_session_with_a_queue() {
        let _app_dir = crate::session::test_support::isolate_app_dir();
        use crate::session::{Instance, Status, View};

        let mut inst = Instance::new("queue", "/tmp/aoe-drain-wake");
        inst.id = "sess-dw".to_string();
        inst.view = View::Structured;
        inst.status = Status::Idle;
        inst.mark_idle_dormant();
        inst.queued_prompts.push(test_fixtures::queued_prompt());
        let state = crate::server::test_support::build_test_app_state(vec![inst]);

        drain_queued_prompts(&state).await;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while state.instances.read().await[0].is_idle_dormant() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "session was not woken"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
