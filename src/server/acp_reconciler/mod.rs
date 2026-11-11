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

pub(crate) use rate_limit::install_rate_limit_continuation;
pub(crate) use rate_limit::ContinuationOutcome;
pub(crate) use resume::{trigger_resume_background, ResumeTrigger};

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
    /// Attachment, retirement or launch prep lost a race; retry next tick unless parked.
    Retry,
    /// Spawn finished with or without error; `attempted` stays set so a
    /// permanently failing spawn does not loop.
    SpawnFinished,
    /// `CapacityFull` is transient, not a crash: re-arm and publish the banner
    /// once. `message` is its Display, matched by the front-end regex (#1027).
    CapacityDeferred {
        message: String,
        source_profile: String,
        lifecycle_generation: u64,
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
    i.is_structured()
        && i.terminal_handoff.is_none()
        && !i.is_archived()
        && !i.is_snoozed()
        && !i.is_trashed()
}

/// Eligible for a reconciler-driven worker.
fn is_resumable(i: &Instance) -> bool {
    is_untriaged_structured(i) && !i.is_idle_dormant() && i.launch_is_finalized()
}

/// `(user-prompt turn, agent-initiated turn)` open in the log; `None` when unreadable.
/// Only the first goes to attach: a runner replays the completion of its last user
/// prompt on every attach, which would close an agent-initiated turn mid-flight.
fn open_turn_kinds(store: &EventStore, id: &str) -> Option<(bool, bool)> {
    Some((
        store.try_in_flight_turn(id)?,
        store.has_open_agent_turn(id)?,
    ))
}

/// `query_store` after the writer has stored every queued worker event: a probe that
/// gates a stop or replacement must not miss work still in the write queue.
async fn query_flushed<T: Send + 'static>(
    supervisor: &Arc<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
    store: &Arc<EventStore>,
    id: &str,
    what: &str,
    f: impl FnOnce(&EventStore, &str) -> T + Send + 'static,
) -> Option<T> {
    let supervisor = Arc::clone(supervisor);
    query_store(store, id, what, move |s, id| {
        supervisor.flush_events();
        f(s, id)
    })
    .await
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
    crate::server::api::recover_pending_terminal_handoffs(
        state,
        crate::server::api::HandoffRecovery::Maintenance,
    )
    .await;

    // Before the reaper, so this tick's reaper tears down the drained handle.
    resume::respawn_drained_stale_workers(state).await;

    // `aoe acp stop|kill|restart` runs out of process; the reaper surfaces it
    // and returns restart ids, which, like respawn requests (#2109), get a clean slate.
    for id in supervisor.reap_user_stopped().await {
        forget_session_budget(&id, attempted, parked, respawn_history, capacity_deferred);
    }
    for id in supervisor.take_respawn_requests() {
        // A teardown in flight settles later; the request waits for it, or the
        // loop below would pin the session as attempted.
        if matches!(
            supervisor.worker_state(&id).await,
            AcpWorkerState::Resuming | AcpWorkerState::Stopping
        ) {
            supervisor.request_respawn(&id);
            continue;
        }
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
    let (targets, with_queued_prompts, queued_not_stopped): (
        Vec<ResumeTarget>,
        HashSet<String>,
        HashSet<String>,
    ) = {
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
            instances
                .iter()
                .filter(|i| {
                    !i.queued_prompts.is_empty() && i.status != crate::session::Status::Stopped
                })
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
        // A session with a queue and no worker is retried under the normal
        // budget, whatever its status short of Stopped (a failed spawn leaves
        // Error): an earlier attempt must not strand the queue (#5200).
        let retry_for_queue = attempted.contains(&id)
            && queued_not_stopped.contains(&id)
            && supervisor.worker_state(&id).await == AcpWorkerState::Absent;
        if attempted.contains(&id) {
            // A restart marker can land after the reaper ran: add-project (#3103)
            // writes it only once the moved workspace is durable.
            let restart_marker = supervisor.worker_state(&id).await != AcpWorkerState::Stopping
                && supervisor.take_late_restart_marker(&id);
            if !retry_for_queue && !restart_marker {
                continue;
            }
            if restart_marker {
                forget_session_budget(&id, attempted, parked, respawn_history, capacity_deferred);
            }
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
        // Probed before the respawn budget is charged: a failure retries on the next pass,
        // unpinned and uncharged. Replacing a dead runner
        // without closing its orphaned turn would leave the session Running.
        let Some((in_flight, agent_turn)) = query_flushed(
            &state.acp_supervisor,
            &state.acp_event_store,
            &id,
            "open turn",
            open_turn_kinds,
        )
        .await
        .flatten() else {
            continue;
        };
        target.in_flight_turn = in_flight;
        target.agent_turn_open = agent_turn;
        // A failed probe counts as busy so a stale runner is not replaced under live work.
        target.holds_live_work = in_flight
            || agent_turn
            || query_flushed(
                &state.acp_supervisor,
                &state.acp_event_store,
                &id,
                "live work",
                |s, id| s.holds_live_work(id),
            )
            .await
            .flatten()
            .unwrap_or(true);
        if record_and_check_respawn_budget(respawn_history, &id, Instant::now()) {
            tracing::warn!(
                target: "acp.supervisor",
                session = %id,
                max_respawns = RECONCILER_MAX_RESPAWNS_IN_WINDOW,
                window_secs = RECONCILER_RESPAWN_WINDOW.as_secs(),
                "structured-view worker respawn budget exhausted; parking session"
            );
            if parked.insert(id.clone()) {
                crate::server::api::publish_startup_error_if_current(
                    state,
                    &id,
                    &target.source_profile,
                    target.lifecycle_generation,
                    park_message(&target.project_path),
                )
                .await;
            }
            attempted.insert(id);
            continue;
        }
        attempted.insert(id);
        tasks.push(target);
    }
    if tasks.is_empty() {
        return;
    }

    let resume_limit = MAX_CONCURRENT_RESUMES
        .min(state.acp_supervisor.max_concurrent_workers())
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
            Ok((id, ResumeOutcome::Retry)) => {
                if !parked.contains(&id) {
                    attempted.remove(&id);
                }
            }
            Ok((
                id,
                ResumeOutcome::CapacityDeferred {
                    message,
                    source_profile,
                    lifecycle_generation,
                },
            )) => {
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
                    crate::server::api::publish_startup_error_if_current(
                        state,
                        &id,
                        &source_profile,
                        lifecycle_generation,
                        message,
                    )
                    .await;
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
            .filter(|i| i.pending_initial_turn.is_some() && is_resumable(i))
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
                    && matches!(
                        i.status,
                        crate::session::Status::Idle | crate::session::Status::Running
                    )
                    && is_untriaged_structured(i)
            })
            .map(|i| (i.id.clone(), i.is_idle_dormant()))
            .collect()
    };
    for (id, dormant) in candidates {
        let service = Arc::clone(&state.session_service);
        if !service.fold_control_state(&id).await.accepts_queue_drain() {
            continue;
        }
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

struct OrphanCleanupClaim {
    state: Arc<AppState>,
    identity: (String, u32, u64),
}

impl Drop for OrphanCleanupClaim {
    fn drop(&mut self) {
        self.state
            .acp_orphan_cleanups
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.identity);
    }
}

/// Reaps registry entries whose session no longer exists.
async fn sweep_orphan_workers(state: &Arc<AppState>, live: &HashSet<&String>) {
    let Ok(records) = crate::process::worker_registry::list() else {
        return;
    };
    let pending_handoffs: HashSet<String> = state
        .instances
        .read()
        .await
        .iter()
        .filter(|row| row.terminal_handoff.is_some())
        .map(|row| row.id.clone())
        .collect();
    for record in records {
        if pending_handoffs.contains(&record.session_id)
            || live.contains(&record.session_id)
            || state.acp_supervisor.is_owned(&record.session_id).await
        {
            continue;
        }
        let identity = (record.session_id.clone(), record.pid, record.generation);
        if !state
            .acp_orphan_cleanups
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(identity.clone())
        {
            continue;
        }
        let claim = Arc::new(OrphanCleanupClaim {
            state: Arc::clone(state),
            identity,
        });
        tracing::info!(target: "acp.supervisor", session = %record.session_id, pid = record.pid,
            "orphan acp worker; settling owned native runtime");
        tokio::spawn(async move {
            let session_id = record.session_id.clone();
            if let Err(error) =
                crate::process::worker_registry::terminate_record_and_wait(record, claim).await
            {
                tracing::warn!(target: "acp.supervisor", session = %session_id, %error,
                    "orphan native cleanup unproven; retaining registry route");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::test_fixtures::{capacity_startup_errors, startup_errors, test_state, Tick};
    use super::*;

    #[test]
    #[serial_test::serial]
    fn orphan_claim_survives_cancellation_until_blocking_cleanup_finishes() {
        use std::os::unix::fs::PermissionsExt as _;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .max_blocking_threads(1)
            .build()
            .unwrap();
        runtime.block_on(async {
            let (_app, state, project) = test_state("live-session");
            let log = project.path().join("cleanup-calls");
            let program = project.path().join("unavailable-prime");
            std::fs::write(
                &program,
                format!(
                    "#!/bin/sh\nprintf 'entered\n' >> {}\nexit 1\n",
                    crate::session::environment::shell_escape(&log.to_string_lossy())
                ),
            )
            .unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            let native =
                crate::process::prime::PrimeRuntime::new(program, project.path().into(), None)
                    .unwrap();
            native.prepare().unwrap();
            struct RuntimeDirectory(std::path::PathBuf);
            impl Drop for RuntimeDirectory {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            let _directory = RuntimeDirectory(native.directory.clone());
            let mut record = crate::process::worker_registry::WorkerRecord::new(
                "orphan-prime".into(),
                i32::MAX as u32,
                crate::process::worker_registry::socket_path_for("orphan-prime").unwrap(),
                "prime-agent".into(),
                "prime-agent".into(),
                project.path().into(),
                None,
                vec![],
                vec![],
                None,
                None,
            );
            record.prime_runtime = Some(native);
            crate::process::worker_registry::save(&record).unwrap();
            let identity = (record.session_id.clone(), record.pid, record.generation);
            state
                .acp_orphan_cleanups
                .lock()
                .unwrap()
                .insert(identity.clone());
            let claim = Arc::new(OrphanCleanupClaim {
                state: state.clone(),
                identity: identity.clone(),
            });
            let weak = Arc::downgrade(&claim);
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            });
            ready_rx.recv().unwrap();
            let task = tokio::spawn(crate::process::worker_registry::terminate_record_and_wait(
                record.clone(),
                claim,
            ));
            tokio::time::timeout(Duration::from_secs(2), async {
                while weak.strong_count() != 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            assert!(state
                .acp_orphan_cleanups
                .lock()
                .unwrap()
                .contains(&identity));
            for _ in 0..2 {
                sweep_orphan_workers(&state, &HashSet::new()).await;
            }
            release_tx.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(2), async {
                while weak.strong_count() != 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(crate::process::worker_registry::load(&record.session_id)
                .unwrap()
                .is_some());
            let duplicate = tokio::time::timeout(Duration::from_millis(100), async {
                loop {
                    if std::fs::read_to_string(&log)
                        .unwrap_or_default()
                        .lines()
                        .count()
                        > 1
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            assert!(
                duplicate.is_err(),
                "another tick ran native cleanup concurrently"
            );
            assert!(!state
                .acp_orphan_cleanups
                .lock()
                .unwrap()
                .contains(&identity));
        });
    }

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

    /// An unreadable event log neither spawns nor pins the session: it stays
    /// eligible, uncharged, and resumes on the first pass after the log recovers.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_open_turn_probe_retries_after_recovery() {
        let id = "s-probe-fails";
        let (_home, state, _project) = test_state(id);
        let mut tick = Tick::default();

        state.acp_event_store.test_set_unavailable(true);
        tick.run(&state).await;
        assert!(
            !tick.attempted.contains(id),
            "a failed probe is not an attempt"
        );
        assert!(
            !tick.respawn_history.contains_key(id),
            "and charges no budget"
        );

        state.acp_event_store.test_set_unavailable(false);
        tick.run(&state).await;
        assert!(tick.attempted.contains(id), "the next pass resumes it");
    }

    /// A runner retains the completion of its last user prompt and replays it on attach. An
    /// agent-initiated turn after that prompt must not be classified as a prompt in flight
    /// (which would let the replay close it), and only its own terminal releases the hold.
    #[test]
    fn an_agent_turn_after_a_completed_prompt_is_not_a_prompt_in_flight() {
        use crate::acp::Event;
        let (_home, state, _project) = test_state("s-reattach");
        let id = "s-reattach";
        let store = &state.acp_event_store;
        let events = [
            Event::UserPromptSent {
                text: "go".into(),
                attachments: Vec::new(),
                prompt_id: None,
                merged_prompt_ids: Vec::new(),
                synthesized: false,
                origin_plugin: None,
            },
            Event::Stopped {
                reason: "prompt_complete".into(),
            },
            Event::AgentMessageChunk {
                text: "woken".into(),
                message_id: None,
            },
        ];
        for (i, event) in events.iter().enumerate() {
            store.record(id, i as u64 + 1, event).unwrap();
        }
        assert_eq!(open_turn_kinds(store, id), Some((false, true)));
        assert_eq!(store.holds_live_work(id), Some(true));

        let stop = Event::Stopped {
            reason: "agent_idle".into(),
        };
        store.record(id, 4, &stop).unwrap();
        assert_eq!(open_turn_kinds(store, id), Some((false, false)));
        assert_eq!(store.holds_live_work(id), Some(false));
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

    /// A respawn request made while the worker is still mid-teardown is kept
    /// until it settles; spent early, the tick would pin the session as attempted
    /// and nothing would bring the worker back.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_respawn_request_waits_out_a_teardown_in_flight() {
        let id = "s-req";
        let (_home, state, _project) = test_state(id);
        let mut tick = Tick::default();
        tick.run(&state).await;
        let attempts = startup_errors(&state, id);
        assert!(tick.attempted.contains(id), "precondition");

        let lease = state.acp_supervisor.test_hold_stopping(id).unwrap();
        state.acp_supervisor.request_respawn(id);
        tick.run(&state).await;
        assert_eq!(startup_errors(&state, id), attempts, "nothing spawns yet");

        state.acp_supervisor.test_settle_stopping(&lease);
        tick.run(&state).await;
        assert_eq!(
            startup_errors(&state, id),
            attempts + 1,
            "the kept request respawns the session once the teardown ends"
        );
    }

    #[tokio::test]
    async fn queue_drain_preserves_a_deliberately_stopped_dormant_session() {
        use crate::session::{Instance, Status, View};

        let _app_dir = crate::session::test_support::isolate_app_dir();
        let id = "stopped-with-queue";
        let mut inst = Instance::new(id, "/tmp/aoe-stopped-with-queue");
        inst.id = id.into();
        inst.view = View::Structured;
        inst.status = Status::Stopped;
        inst.mark_idle_dormant();
        inst.queued_prompts.push(test_fixtures::queued_prompt());
        let state = crate::server::test_support::build_test_app_state(vec![inst]);
        let mut tick = test_fixtures::Tick::default();
        for _ in 0..3 {
            tick.run(&state).await;
        }
        let instances = state.instances.read().await;
        assert!(
            instances[0].is_idle_dormant(),
            "a deliberate stop must stay dormant"
        );
        assert!(
            !tick.attempted.contains(id),
            "queue drain must not attempt a stopped session"
        );
        assert_eq!(instances[0].queued_prompts.len(), 1);
    }

    #[tokio::test]
    async fn queue_drain_ignores_background_work_but_preserves_foreground_barriers() {
        use crate::acp::state::test_support::{chunk, prompt, stopped};
        use crate::acp::state::Event;
        use crate::acp::supervisor::{BroadcastSink, ChannelSink};
        use crate::session::{Instance, Status, View};

        let _app_dir = crate::session::test_support::isolate_app_dir();
        let task = Event::BackgroundTaskStarted {
            task_id: "task".into(),
            tool_call_id: None,
            description: "build".into(),
            workflow: None,
            started_at: None,
        };
        let agent = Event::BackgroundAgentLaunched {
            agent_id: "agent".into(),
            tool_call_id: "tool".into(),
            description: "research".into(),
            prompt: "research".into(),
            model: "model".into(),
            output_file: String::new(),
            started_at: chrono::Utc::now(),
            workflow: None,
        };
        for (name, events, text, drains) in [
            (
                "task",
                vec![prompt("go"), task.clone(), stopped("end_turn")],
                "follow up",
                true,
            ),
            (
                "agent",
                vec![prompt("go"), agent, stopped("end_turn")],
                "follow up",
                true,
            ),
            ("main", vec![prompt("go"), task.clone()], "follow up", false),
            (
                "notification",
                vec![task.clone(), chunk("result")],
                "follow up",
                false,
            ),
            (
                "cancel",
                vec![
                    task.clone(),
                    Event::CancelRequested {
                        escalates_at: chrono::Utc::now(),
                    },
                ],
                "follow up",
                false,
            ),
            (
                "compact",
                vec![task.clone(), Event::ConversationCompactionStarted],
                "follow up",
                false,
            ),
            ("clear", vec![task.clone()], "/clear", false),
        ] {
            let mut inst = Instance::new(name, "/tmp/aoe-queue-background");
            inst.id = name.into();
            inst.view = View::Structured;
            inst.agent_name = Some("claude".into());
            // The display stays busy even after the foreground turn ends.
            inst.status = Status::Running;
            let state = crate::server::test_support::build_test_app_state(vec![inst]);
            let commands = state
                .acp_supervisor
                .test_insert_worker_cmd_recording(name)
                .await;
            let sink = ChannelSink::new(
                state.acp_events_tx.clone(),
                Arc::clone(&state.acp_event_store),
                Arc::clone(&state.acp_control_cache),
            );
            for (seq, event) in events.iter().enumerate() {
                assert!(sink.publish_persisted(name, seq as u64 + 1, event));
            }
            state
                .session_service
                .enqueue_prompt(name, "q".into(), text.into(), Vec::new(), None, None)
                .await
                .unwrap();
            if drains {
                drain_queued_prompts(&state).await;
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !state
                        .session_service
                        .queued_prompts_snapshot(name)
                        .await
                        .is_empty()
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("reconciler must deliver while background work runs");
            } else {
                // Await the delivery path itself before asserting non-delivery.
                state.session_service.drain_queued_prompts_once(name).await;
                assert_eq!(
                    state
                        .session_service
                        .queued_prompts_snapshot(name)
                        .await
                        .len(),
                    1,
                    "{name}"
                );
            }
            state.acp_supervisor.test_flush_worker_commands(name).await;
            assert_eq!(
                commands
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|cmd| **cmd == "prompt")
                    .count(),
                usize::from(drains),
                "{name}"
            );
            if name == "clear" {
                assert!(sink.publish_persisted(
                    name,
                    2,
                    &Event::BackgroundTaskFinished {
                        task_id: "task".into(),
                        ended_at: None
                    }
                ));
                state.session_service.drain_queued_prompts_once(name).await;
                assert!(state
                    .session_service
                    .queued_prompts_snapshot(name)
                    .await
                    .is_empty());
                state.acp_supervisor.test_flush_worker_commands(name).await;
                assert!(commands.lock().unwrap().contains(&"reset_session"));
            }
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

    /// A resume attempt that launch admission cancelled (row still `Stopped`)
    /// pins the id; once the row reads `Idle` with a queue, the pin must lift
    /// so the queue is not stranded behind a worker nothing starts (#5200).
    #[tokio::test]
    #[serial_test::serial]
    async fn a_pinned_idle_session_with_a_queue_is_resumed() {
        use crate::session::Status;

        let cases = [
            ("idle with a queue", Status::Idle, 1),
            ("error with a queue", Status::Error, 1),
            ("deliberately stopped with a queue", Status::Stopped, 0),
        ];
        for (name, status, expected_attempts) in cases {
            let id = "sess-pinned";
            let (_home, state, _project) = test_fixtures::test_state(id);
            {
                let mut instances = state.instances.write().await;
                instances[0].status = status;
                instances[0]
                    .queued_prompts
                    .push(test_fixtures::queued_prompt());
            }

            let mut tick = test_fixtures::Tick::default();
            tick.attempted.insert(id.to_string());
            tick.run(&state).await;

            assert_eq!(
                test_fixtures::startup_errors(&state, id),
                expected_attempts,
                "{name}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn explicit_start_rearms_a_stopped_session_without_a_queue() {
        use crate::session::Status;
        use axum::{
            extract::{Path, State},
            response::IntoResponse,
        };

        let id = "sess-explicit-start";
        let (_home, state, _project) = test_fixtures::test_state(id);
        let stopped = {
            let mut instances = state.instances.write().await;
            instances[0].status = Status::Stopped;
            instances[0].clone()
        };
        crate::server::test_support::seed_instances_on_disk_for_test(
            &stopped.source_profile,
            vec![stopped.clone()],
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let mut tick = test_fixtures::Tick::default();
        tick.run(&state).await;
        assert!(
            tick.attempted.contains(id),
            "stopped admission was attempted"
        );
        assert_eq!(test_fixtures::startup_errors(&state, id), 0);

        let response = crate::server::api::sessions::start_session(
            State(Arc::clone(&state)),
            Path(id.to_string()),
            Ok(None),
        )
        .await
        .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        tick.run(&state).await;
        assert_eq!(
            test_fixtures::startup_errors(&state, id),
            1,
            "explicit start must retry the fake agent without a queued prompt"
        );
        tick.run(&state).await;
        assert_eq!(
            test_fixtures::startup_errors(&state, id),
            1,
            "one explicit start must not create a retry loop"
        );
    }

    /// A launch-prep race (here: memory and disk disagree on the row) must not
    /// pin the session out of the resume set, or a queued prompt never drains.
    #[tokio::test]
    #[serial_test::serial]
    async fn launch_prep_race_does_not_pin_the_session() {
        let id = "sess-race";
        let (_home, state, _project) = test_fixtures::test_state(id);
        state.instances.write().await[0].title = "renamed in memory only".to_string();

        let mut tick = test_fixtures::Tick::default();
        tick.run(&state).await;

        assert!(!tick.attempted.contains(id));
    }

    /// A durable `aoe acp stop` fence refuses automatic resume; repeated ticks
    /// must not count that as a crash loop, and explicit resume still clears it.
    #[tokio::test]
    #[serial_test::serial]
    async fn durable_user_stop_does_not_crash_loop() {
        use crate::process::worker_registry::{self, ResumeAuthority, WorkerRecord};

        let id = "sess-stopped";
        let (_home, state, project) = test_fixtures::test_state(id);
        let record = WorkerRecord::new(
            id.into(),
            std::process::id(),
            worker_registry::socket_path_for(id).unwrap(),
            "aoe-agent".into(),
            "aoe-agent".into(),
            project.path().to_path_buf(),
            None,
            vec![],
            vec![],
            None,
            None,
        );
        worker_registry::save(&record).unwrap();
        worker_registry::request_stop(&record).unwrap();
        worker_registry::retire_selected(&record).unwrap();

        let mut tick = test_fixtures::Tick::default();
        for _ in 0..=RECONCILER_MAX_RESPAWNS_IN_WINDOW {
            tick.run(&state).await;
        }

        assert_eq!(test_fixtures::startup_errors(&state, id), 0);
        assert!(tick.parked.is_empty());
        let observed = worker_registry::stop_intent(id).unwrap();
        worker_registry::admit_resume(id, ResumeAuthority::Explicit, &observed).unwrap();
    }
}
