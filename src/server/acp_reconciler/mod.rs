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
use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
use crate::acp::event_store::{ControlKnowledge, EventStore};
use crate::daemon::AcpWorkerState;
use crate::session::Instance;

#[cfg(test)]
pub(crate) use rate_limit::install_rate_limit_continuation;
#[cfg(test)]
pub(crate) use rate_limit::{failover_in_flight, reap_rate_limit_resumes};
pub(crate) use rate_limit::{failover_target, FailoverSettings};
pub(crate) use rate_limit::{
    install_prepared_continuation, resolve_continuation, ContinuationOutcome, PreparedContinuation,
};
pub(crate) use rate_limit::{resume_rate_limited, QuotaDelivery, QuotaResume};
pub(crate) use resume::ensure_rate_limit_worker;
pub(crate) use resume::{trigger_resume_background, ResumeTrigger};

use resume::{resume_one, ResumeTarget};

/// Crash-loop budget (#1945): temporary admission and capacity refusals refund their entry.
/// Exhaustion parks a session until explicit retry.
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

fn refund_respawn_attempt(history: &mut HashMap<String, Vec<Instant>>, id: &str) {
    if let Some(entries) = history.get_mut(id) {
        entries.pop();
        if entries.is_empty() {
            history.remove(id);
        }
    }
}

/// Clean slate for `id`: re-armed, un-parked, budget and capacity marker reset.
fn forget_session_budget(
    id: &str,
    attempted: &mut HashSet<String>,
    parked: &mut HashSet<String>,
    respawn_history: &mut HashMap<String, Vec<Instant>>,
    resume_deferred: &mut HashMap<String, ResumeDeferral>,
) {
    attempted.remove(id);
    parked.remove(id);
    respawn_history.remove(id);
    resume_deferred.remove(id);
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

/// Banner for a session whose queued prompts wait behind a durable stop.
const STOP_FENCE_MESSAGE: &str = "Structured view worker was stopped (aoe acp stop); queued prompts wait until the session is started again.";

#[derive(Debug, Clone)]
pub struct ResumeDeferral {
    entry: DeferredResume,
}

#[derive(Debug, Clone)]
enum DeferredResume {
    Capacity,
    Unavailable {
        scope: crate::acp::state::NativeObservationScope,
        refusal: crate::process::worker_registry::ControlAdmissionRefusal,
        originating_epoch: Option<u64>,
    },
}

impl ResumeDeferral {
    pub(crate) fn capacity() -> Self {
        Self {
            entry: DeferredResume::Capacity,
        }
    }
}

#[derive(Debug, Clone)]
enum ResumeOutcome {
    Attached,
    /// Attachment, retirement or launch prep lost a race; retry next tick unless parked.
    Retry,
    /// No admission occurred; refund only the attempt charged by this tick.
    RegistryBusy,
    /// A purge fenced admission; re-arm without charging the crash-loop budget.
    AdmissionDeferred,
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
    Unavailable {
        scope: Option<crate::acp::state::NativeObservationScope>,
        refusal: crate::process::worker_registry::ControlAdmissionRefusal,
        originating_epoch: Option<u64>,
    },
}

#[cfg(test)]
type ResumeDispatchGate = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static RESUME_DISPATCH_GATE: std::sync::Mutex<Option<ResumeDispatchGate>> =
    std::sync::Mutex::new(None);

fn refund_tick_budget(respawn_history: &mut HashMap<String, Vec<Instant>>, id: &str) {
    if let Some(entries) = respawn_history.get_mut(id) {
        entries.pop();
        if entries.is_empty() {
            respawn_history.remove(id);
        }
    }
}

async fn defer_control_refusal(
    state: &Arc<AppState>,
    id: &str,
    scope: crate::acp::state::NativeObservationScope,
    refusal: crate::process::worker_registry::ControlAdmissionRefusal,
    originating_epoch: Option<u64>,
    deferred: &mut HashMap<String, ResumeDeferral>,
) {
    let same = deferred.get(id).is_some_and(|entry| match &entry.entry {
        DeferredResume::Unavailable {
            scope: previous,
            refusal: previous_refusal,
            ..
        } => previous.context == scope.context && *previous_refusal == refusal,
        DeferredResume::Capacity => false,
    });
    if !same
        && state
            .acp_supervisor
            .publish_control_refusal(id, &scope, refusal, originating_epoch)
            .await
    {
        deferred.insert(
            id.to_owned(),
            ResumeDeferral {
                entry: DeferredResume::Unavailable {
                    scope,
                    refusal,
                    originating_epoch,
                },
            },
        );
    }
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
pub(super) fn is_resumable(i: &Instance) -> bool {
    is_untriaged_structured(i) && !i.is_idle_dormant() && i.launch_is_finalized()
}

/// Scoped `(user-prompt turn, agent-initiated turn)` knowledge in the log.
/// Only the first goes to attach: cached prompt completion cannot close an agent turn.
fn open_turn_kinds(store: &EventStore, id: &str) -> anyhow::Result<ControlKnowledge<(bool, bool)>> {
    match (
        store.has_in_flight_turn(id)?,
        store.has_open_agent_turn(id)?,
    ) {
        (Known(prompt), Known(agent)) => Ok(Known((prompt, agent))),
        _ => Ok(Indeterminate),
    }
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

async fn query_control<T: Send + 'static>(
    store: &Arc<EventStore>,
    id: &str,
    what: &str,
    query: impl FnOnce(&EventStore, &str) -> anyhow::Result<T> + Send + 'static,
) -> Result<T, crate::acp::supervisor::SupervisorError> {
    use crate::acp::supervisor::SupervisorError;
    match query_store(store, id, what, query).await {
        Some(Ok(value)) => Ok(value),
        Some(Err(error)) => {
            tracing::warn!(target: "acp.supervisor", session = %id, %error, "{what} query refused");
            Err(SupervisorError::NativeStoreUnavailable)
        }
        None => Err(SupervisorError::NativeStoreUnavailable),
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
    resume_deferred: &mut HashMap<String, ResumeDeferral>,
    stop_notified: &mut HashSet<String>,
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
        forget_session_budget(&id, attempted, parked, respawn_history, resume_deferred);
    }
    crate::server::acp_events::commit_background_bindings(state).await;
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
        forget_session_budget(&id, attempted, parked, respawn_history, resume_deferred);
    }
    for id in supervisor.take_startup_requeues() {
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
    if due(
        &mut cadence.rate_limit,
        rate_limit::RATE_LIMIT_RESUME_INTERVAL,
    ) {
        rate_limit::reap_rate_limit_resumes(state, attempted, parked).await;
    }

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
    resume_deferred.retain(|id, _| live.contains(id));
    stop_notified
        .retain(|id| live.contains(id) && crate::process::worker_registry::stop_requested(id));

    // Must precede scheduling: capacity counts registry entries, so dead
    // entries would block legitimate spawns.
    sweep_orphan_workers(state, &live).await;
    readopt_orphan_runners(state, attempted).await;
    drain_pending_initial_turns(state).await;
    drain_queued_prompts(state).await;

    let login = &state.session_service.sandbox_login;
    for id in release_logged_in(state, login.held_ids()).await {
        if with_queued_prompts.contains(&id)
            && !crate::process::worker_registry::stop_requested(&id)
        {
            match query_control(
                &state.acp_event_store,
                &id,
                "login queue release",
                |store, id| store.rate_limit_park(id),
            )
            .await
            {
                Ok(Known(Some(park))) => {
                    let Some(submission) = state.session_service.try_prompt_submission(&id).await
                    else {
                        continue;
                    };
                    rate_limit::schedule_quota_release(
                        state.clone(),
                        id.clone(),
                        submission,
                        rate_limit::QuotaResume::LoginQueue { park_seq: park.seq },
                    );
                    continue;
                }
                Ok(Known(None)) => {}
                Ok(Indeterminate) | Err(_) => continue,
            }
        }
        login.release(&id);
        forget_session_budget(&id, attempted, parked, respawn_history, resume_deferred);
    }

    let mut tasks = Vec::new();
    for mut target in targets {
        let id = target.id.clone();
        match supervisor.resume_deferred(&id).await {
            Ok(false) => {}
            Ok(true) => continue,
            Err(error) => {
                tracing::debug!(target: "acp.supervisor", session = %id, %error,
                    "retained construction remains governed by its existing lease");
                continue;
            }
        }
        // A session with a queue and no worker is retried under the normal
        // budget, whatever its status short of Stopped (a failed spawn leaves
        // Error): an earlier attempt must not strand the queue (#5200).
        let retry_for_queue = attempted.contains(&id)
            && queued_not_stopped.contains(&id)
            && supervisor.worker_state(&id).await == AcpWorkerState::Absent;
        let worker_state = supervisor.worker_state(&id).await;
        let previous = resume_deferred
            .get(&id)
            .and_then(|entry| match &entry.entry {
                DeferredResume::Unavailable {
                    scope,
                    refusal,
                    originating_epoch,
                } => Some((scope.clone(), *refusal, *originating_epoch)),
                DeferredResume::Capacity => None,
            });
        if worker_state == AcpWorkerState::Absent
            || (worker_state == AcpWorkerState::Resuming && previous.is_some())
        {
            if let Err(error) = target
                .freeze_native_scope(previous.as_ref().map(|(scope, _, _)| scope.clone()))
                .await
            {
                tracing::debug!(target: "acp.supervisor", session = %id, %error,
                    "native scope changed before resume admission");
                continue;
            }
            let scope = target
                .native_scope
                .as_ref()
                .expect("frozen native scope")
                .clone();
            let previous = previous.filter(|(old, _, _)| old.context == scope.context);
            let probe_id = id.clone();
            let refusal = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                let registry = crate::process::worker_registry::try_registry_lock(&probe_id)?
                    .ok_or(crate::process::worker_registry::RegistryBusy)?;
                if let Some((_, refusal, epoch)) = previous {
                    if registry.refusal_still_applies(&refusal) {
                        return Ok(Some((refusal, epoch)));
                    }
                }
                Ok(registry
                    .control_admission()
                    .err()
                    .map(|refusal| (refusal, None)))
            })
            .await;
            match refusal {
                Ok(Ok(Some((refusal, epoch)))) => {
                    defer_control_refusal(state, &id, scope, refusal, epoch, resume_deferred).await;
                    attempted.remove(&id);
                    continue;
                }
                Ok(Ok(None)) => {
                    if resume_deferred.get(&id).is_some_and(|entry| {
                        matches!(&entry.entry, DeferredResume::Unavailable { .. })
                    }) {
                        resume_deferred.remove(&id);
                        attempted.remove(&id);
                    }
                }
                Ok(Err(error)) => {
                    tracing::debug!(target: "acp.supervisor", session = %id, %error,
                        "control admission deferred without charging resume budget");
                    continue;
                }
                Err(error) => {
                    tracing::warn!(target: "acp.supervisor", session = %id, %error,
                        "control admission observation failed");
                    continue;
                }
            }
        }
        // A stop fences automatic resumes, but is not a crash-loop park.
        if crate::process::worker_registry::stop_requested(&id) {
            if with_queued_prompts.contains(&id)
                && !stop_notified.contains(&id)
                && crate::server::api::publish_startup_error_if_current(
                    state,
                    &id,
                    &target.source_profile,
                    target.lifecycle_generation,
                    STOP_FENCE_MESSAGE.to_string(),
                )
                .await
            {
                // Logged once per notice; a refused publication retries next tick.
                tracing::info!(
                    target: "acp.supervisor",
                    session = %id,
                    "holding queued prompts: the worker was stopped and waits for an explicit start"
                );
                stop_notified.insert(id.clone());
            }
            attempted.insert(id);
            continue;
        }
        match query_control(
            &state.acp_event_store,
            &id,
            "rate-limit hold",
            |store, id| store.rate_limit_park(id),
        )
        .await
        {
            Ok(Known(Some(park))) => {
                if park.cap_reached
                    && with_queued_prompts.contains(&id)
                    && !login.is_held(&id)
                    && !parked.contains(&id)
                {
                    rate_limit::schedule_capped_queue(state, &id, park.seq).await;
                }
                attempted.insert(id.clone());
                continue;
            }
            Ok(Known(None)) => {}
            Ok(Indeterminate) => target.historical_control_known = false,
            Err(_) => continue,
        }
        if attempted.contains(&id) {
            // A restart marker can land after the reaper ran: add-project (#3103)
            // writes it only once the moved workspace is durable.
            let restart_marker = supervisor.worker_state(&id).await != AcpWorkerState::Stopping
                && supervisor.take_late_restart_marker(&id);
            if !retry_for_queue && !restart_marker {
                continue;
            }
            if restart_marker {
                forget_session_budget(&id, attempted, parked, respawn_history, resume_deferred);
            }
        }
        match supervisor.worker_state(&id).await {
            AcpWorkerState::Running | AcpWorkerState::Resuming => {
                // Current ownership clears crash bookkeeping only after quota hold ends.
                forget_session_budget(&id, attempted, parked, respawn_history, resume_deferred);
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
        // Waiting on the sandbox login spends no respawn budget.
        if login.is_held(&id) {
            attempted.insert(id);
            continue;
        }
        match query_flushed(
            &state.acp_supervisor,
            &state.acp_event_store,
            &id,
            "open turn",
            open_turn_kinds,
        )
        .await
        {
            Some(Ok(Known((prompt, agent)))) => {
                target.in_flight_turn = prompt;
                target.agent_turn_open = agent;
            }
            Some(Ok(Indeterminate)) => target.historical_control_known = false,
            Some(Err(error)) => {
                tracing::warn!(target: "acp.supervisor", session = %id, %error, "open turn query refused");
                continue;
            }
            None => continue,
        }
        match query_flushed(
            &state.acp_supervisor,
            &state.acp_event_store,
            &id,
            "live work",
            |store, id| store.holds_live_work(id),
        )
        .await
        {
            Some(Ok(Known(live))) => target.holds_live_work = live,
            Some(Ok(Indeterminate)) => {
                target.historical_control_known = false;
                target.holds_live_work = true;
            }
            Some(Err(error)) => {
                tracing::warn!(target: "acp.supervisor", session = %id, %error, "live work query refused");
                continue;
            }
            None => continue,
        }
        if target.historical_control_known
            && record_and_check_respawn_budget(respawn_history, &id, Instant::now())
        {
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
    #[cfg(test)]
    let gate = { RESUME_DISPATCH_GATE.lock().unwrap().take() };
    #[cfg(test)]
    if let Some((entered, release)) = gate {
        let _ = entered.send(());
        let _ = release.await;
    }
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
            Ok((id, ResumeOutcome::RegistryBusy)) => {
                refund_tick_budget(respawn_history, &id);
                attempted.remove(&id);
            }
            Ok((id, ResumeOutcome::Retry)) => {
                if !parked.contains(&id) {
                    attempted.remove(&id);
                }
            }
            Ok((id, ResumeOutcome::AdmissionDeferred)) => {
                refund_respawn_attempt(respawn_history, &id);
                attempted.remove(&id);
            }
            Ok((
                id,
                ResumeOutcome::CapacityDeferred {
                    message,
                    source_profile,
                    lifecycle_generation,
                },
            )) => {
                refund_tick_budget(respawn_history, &id);
                // Never pin: `attempted` would skip the session forever.
                attempted.remove(&id);
                // `publish_startup_error` does not dedup, so publish once per transition.
                if !resume_deferred
                    .get(&id)
                    .is_some_and(|entry| matches!(&entry.entry, DeferredResume::Capacity))
                {
                    resume_deferred.insert(id.clone(), ResumeDeferral::capacity());
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
            Ok((
                id,
                ResumeOutcome::Unavailable {
                    scope,
                    refusal,
                    originating_epoch,
                },
            )) => {
                refund_tick_budget(respawn_history, &id);
                attempted.remove(&id);
                if let Some(scope) = scope {
                    defer_control_refusal(
                        state,
                        &id,
                        scope,
                        refusal,
                        originating_epoch,
                        resume_deferred,
                    )
                    .await;
                }
            }
            Ok((id, ResumeOutcome::Attached | ResumeOutcome::SpawnFinished)) => {
                resume_deferred.remove(&id);
            }
            Err(e) => {
                tracing::error!(target: "acp.supervisor", "resume task panicked: {e}");
            }
        }
    }
}

/// Held ids whose launch would now pass the login gate, or whose row is gone or
/// triaged (archived, snoozed, trashed) and so no longer waits on a launch.
async fn release_logged_in(state: &Arc<AppState>, held: Vec<String>) -> Vec<String> {
    if held.is_empty() {
        return Vec::new();
    }
    let rows: Vec<(String, Option<Instance>)> = {
        let instances = state.instances.read().await;
        held.into_iter()
            .map(|id| {
                let row = instances
                    .iter()
                    .find(|i| i.id == id && is_untriaged_structured(i))
                    .cloned();
                (id, row)
            })
            .collect()
    };
    // The release check reads only what the session already holds (its fresh hook
    // resolution, never a hook run per tick): a held session whose `before_start` hook starts
    // emitting a key is released by the next send or launch attempt, which re-resolves the
    // hook, not by this tick.
    let release_state = Arc::clone(state);
    tokio::task::spawn_blocking(move || {
        rows.into_iter()
            .filter(|(_, row)| {
                row.as_ref().is_none_or(|i| {
                    // A store that cannot be read keeps the session held.
                    let config = crate::server::session_store::NativeSessionStore::open(
                        release_state.clone(),
                        &i.source_profile,
                        None,
                    )
                    .and_then(|native| {
                        use crate::session::SessionStore;
                        (&native as &dyn SessionStore)
                            .launch_configuration(std::path::Path::new(&i.project_path))
                    });
                    config.is_ok_and(|config| i.ensure_sandbox_login_with(&config).is_ok())
                })
            })
            .map(|(id, _)| id)
            .collect()
    })
    .await
    .unwrap_or_default()
}

/// Spawns the pending-initial-turn drain (#2897) for sessions with a live worker,
/// and the terminal drain for terminal sessions.
async fn drain_pending_initial_turns(state: &Arc<AppState>) {
    let candidates: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                is_resumable(i)
                    && i.pending_initial_turn
                        .as_ref()
                        .is_some_and(|turn| state.session_service.turn_is_deliverable(&i.id, turn))
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
            async move { service.drain_pending_initial_turn(&id).await },
        );
    }
    // Terminal rows whose create-time delivery was cut short by a restart.
    let terminal: Vec<String> = {
        let instances = state.instances.read().await;
        instances
            .iter()
            .filter(|i| {
                !i.is_structured()
                    && i.pending_initial_turn.as_ref().is_some_and(|turn| {
                        turn.is_creation_prompt()
                            && state.session_service.turn_is_deliverable(&i.id, turn)
                    })
            })
            .map(|i| i.id.clone())
            .collect()
    };
    for id in terminal {
        let service = Arc::clone(&state.session_service);
        crate::task_util::spawn_supervised(
            "tmux.pending_initial_turn_drain",
            crate::task_util::PanicPolicy::Log,
            async move { service.drain_terminal_initial_turn(&id).await },
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
                        crate::session::Status::Idle
                            | crate::session::Status::Running
                            | crate::session::Status::Error
                    )
                    && is_untriaged_structured(i)
            })
            .map(|i| (i.id.clone(), i.is_idle_dormant()))
            .collect()
    };
    for (id, dormant) in candidates {
        let service = Arc::clone(&state.session_service);
        if !matches!(service.fold_control_state(&id).await, Ok(control) if control.accepts_queue_drain())
        {
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
    use crate::acp::Event;

    #[derive(Clone, Copy)]
    enum ConstructorOutcome {
        Resume,
        SupervisorStop,
        HttpStop,
        CliStop,
    }

    async fn owned_constructor_registry_deferral_recovers(
        installation_ready: bool,
        outcome: ConstructorOutcome,
    ) {
        use crate::acp::runner_lifecycle::{ProcessControl as _, SystemProcessControl};
        use crate::acp::supervisor::test_support::{
            pause_owned_installation, runner_record, ScriptedAcpAgent,
        };
        let stop_deferred = !matches!(outcome, ConstructorOutcome::Resume);
        let _home = crate::session::test_support::isolate_app_dir();
        crate::session::config::update_app_state(|state| state.has_acknowledged_agent_hooks = true)
            .expect("acknowledge fixture hooks");
        let project = tempfile::tempdir().unwrap();
        let spec = crate::server::session_spawn::StructuredSessionSpec {
            title: Some("owned-constructor-registry-deferral".into()),
            title_typed: false,
            size: None,
            path: project.path().to_string_lossy().into_owned(),
            group: String::new(),
            parent_session_id: None,
            tool: "claude".into(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            sandbox: false,
            sandbox_image: None,
            pull_latest_image: false,
            yolo_mode: false,
            extra_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            extra_args: String::new(),
            command_override: String::new(),
            extra_repo_paths: Vec::new(),
            workspace: false,
            repo_base_branches: Vec::new(),
            scratch: false,
            trust_hooks: None,
            trust_review: None,
            skip_on_create: false,
            custom_instruction: None,
            callback_url: None,
            idempotency_key: None,
            profile: "default".into(),
            created_by_plugin: None,
            plugin_create_idempotency: None,
            pending_initial_turn: None,
            acp_mode_id: Some("plan".into()),
            view: crate::session::View::Structured,
            agent_name: Some("claude-code".into()),
            agent_model: None,
            agent_effort: None,
            import_acp_session_id: None,
            import_binding: None,
            fork_seed: None,
            cli: None,
            progress: None,
        };
        crate::server::test_support::seed_instances_on_disk_for_test("default", Vec::new());
        let agent = ScriptedAcpAgent::new();
        let state = crate::server::test_support::build_test_app_state_with_launcher(
            Vec::new(),
            agent.launcher(true),
        );
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        struct Listener(tokio::task::JoinHandle<()>);
        impl Drop for Listener {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _listener = if stop_deferred {
            let subscribers = state.acp_events_tx.receiver_count();
            let listener = Listener(tokio::spawn(crate::server::acp_events::acp_event_listener(
                state.clone(),
            )));
            tokio::time::timeout(Duration::from_secs(5), async {
                while state.acp_events_tx.receiver_count() <= subscribers {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the real observation listener subscribed before construction");
            Some(listener)
        } else {
            None
        };
        let (registration, installation, release) = if installation_ready {
            let (waiting, release) = pause_owned_installation();
            (None, Some(waiting), release)
        } else {
            let (entered, waiting) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            *crate::acp::acp_client::OWNED_REGISTRATION_GATE
                .lock()
                .unwrap() = Some((entered, released));
            (Some(waiting), None, release)
        };
        let task = tokio::spawn({
            let state = state.clone();
            async move {
                state
                    .session_service
                    .create_structured_session(spec, None, None, None)
                    .await
                    .expect("real absent-row creation carries its original New permission")
            }
        });
        let (owner, original_client) = if let Some(waiting) = registration {
            let owner = tokio::time::timeout(Duration::from_secs(15), waiting)
                .await
                .expect("real owned child reached registration-observation boundary")
                .unwrap();
            (owner, None)
        } else {
            let client = tokio::time::timeout(Duration::from_secs(15), installation.unwrap())
                .await
                .expect("real SDK client reached installation boundary")
                .unwrap();
            let owner = agent.owned_runner();
            assert_eq!(client.runner_pid(), Some(owner.pid));
            (owner, Some(client))
        };
        let created_id = state.instances.read().await[0].id.clone();
        let id = created_id.as_str();
        let prior = vec![Instant::now() - Duration::from_secs(1)];
        let mut tick = Tick::default();
        tick.respawn_history.insert(id.into(), prior.clone());
        let native = agent.started().await;
        if installation_ready {
            agent.wait_for_call("session/new", 1).await;
        }
        if stop_deferred && installation_ready {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let (registry, record) = agent.registered_runner(id).await;
                    let established = record.stored_acp_session_id.as_deref().is_some_and(|sid| {
                        original_client
                            .as_ref()
                            .and_then(|client| client.establishment_projection())
                            .and_then(|projection| projection.receipt_snapshot())
                            .is_some_and(|receipt| {
                                receipt.owner == owner
                                    && receipt.session_id() == Some(sid)
                                    && receipt.replay.closed
                                    && !receipt.replay.lost
                            })
                    });
                    drop(registry);
                    if established {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the original native session-ready receipt arrived before the refused install");
        }
        if installation_ready
            && matches!(
                outcome,
                ConstructorOutcome::HttpStop | ConstructorOutcome::CliStop
            )
        {
            assert!(
                !state
                    .acp_event_store
                    .replay_from(id, 0)
                    .expect("read retained events")
                    .iter()
                    .any(|stored| matches!(
                        stored.event.as_ref(),
                        Event::AcpSessionAssigned { .. }
                    )),
                "the genuine SDK assignment must still be only in the original inbound receiver"
            );
        }
        let (registry, registered) = agent.registered_runner(id).await;
        assert!(owner.owns_record(&registered));
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(20), task)
            .await
            .expect("the original creation request returned")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(20), state.session_service.work.drain())
            .await
            .expect("the original bounded construction attempt returned");
        let after_attempt = tick.respawn_history.get(id).cloned();
        assert!(SystemProcessControl::default().is_alive(owner));
        let calls_while_refused = agent.calls();
        tokio::time::timeout(Duration::from_secs(20), tick.run(&state))
            .await
            .unwrap();
        let after_uncharged_tick = tick.respawn_history.get(id).cloned();
        drop(registry);
        if stop_deferred {
            let before_stop_generation = state.instances.read().await[0].lifecycle_generation;
            let mut cli_stop = None;
            let stopped = tokio::time::timeout(Duration::from_secs(8), async {
                match outcome {
                    ConstructorOutcome::SupervisorStop => state
                        .acp_supervisor
                        .shutdown_and_wait(id, Duration::from_secs(5))
                        .await
                        .map_err(|error| error.to_string()),
                    ConstructorOutcome::HttpStop => {
                        let response = crate::server::api::sessions::stop_session(
                            axum::extract::State(state.clone()),
                            axum::extract::Path(id.to_owned()),
                        )
                        .await;
                        let response = axum::response::IntoResponse::into_response(response);
                        if response.status().is_success() {
                            Ok(())
                        } else {
                            let status = response.status();
                            let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                                .await
                                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                                .unwrap_or_else(|error| {
                                    format!("<response body read failed: {error}>")
                                });
                            Err(format!("HTTP Stop returned {status}: {body}"))
                        }
                    }
                    ConstructorOutcome::CliStop => {
                        let count = crate::cli::acp::stop_all_workers(5)
                            .await
                            .map_err(|error| error.to_string())?;
                        assert_eq!(count, 1, "the real CLI stop owns only this isolated runner");
                        cli_stop = Some(
                            crate::process::worker_registry::stop_intent(id)
                                .unwrap()
                                .expect("CLI published the exact original stop intent"),
                        );
                        Ok(())
                    }
                    ConstructorOutcome::Resume => unreachable!("stop branch"),
                }
            })
            .await;
            let cli_custody_settled = if matches!(outcome, ConstructorOutcome::CliStop) {
                Some(
                    tokio::time::timeout(Duration::from_secs(5), async {
                        loop {
                            tick.run(&state).await;
                            if !state.acp_supervisor.is_owned(id).await {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await
                    .is_ok(),
                )
            } else {
                None
            };
            let still_alive = SystemProcessControl::default().is_alive(owner);
            let native_reaped = if matches!(&stopped, Ok(Ok(()))) {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !agent.native_reaped() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .is_ok()
            } else {
                agent.native_reaped()
            };
            if matches!(outcome, ConstructorOutcome::SupervisorStop) {
                // Physical Stop leaves the canonical binding commit to maintenance.
                crate::server::acp_events::commit_background_bindings(&state).await;
            }
            let record_absent = runner_record(id).await.is_none();
            let calls = agent.calls();
            let expected_sid = registered.stored_acp_session_id.as_deref();
            let selected_identity_preserved =
                if matches!(outcome, ConstructorOutcome::SupervisorStop) {
                    None
                } else {
                    Some(
                        tokio::time::timeout(Duration::from_secs(5), async {
                            loop {
                                let cached = state
                                    .instances
                                    .read()
                                    .await
                                    .iter()
                                    .find(|row| row.id == id)
                                    .and_then(|row| row.acp_session_id.clone());
                                let durable = crate::session::Storage::new_unwatched("default")
                                    .unwrap()
                                    .load()
                                    .unwrap()
                                    .into_iter()
                                    .find(|row| row.id == id)
                                    .and_then(|row| row.acp_session_id);
                                if cached.as_deref() == expected_sid
                                    && durable.as_deref() == expected_sid
                                {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(20)).await;
                            }
                        })
                        .await
                        .is_ok(),
                    )
                };
            let stopped_events = state
                .acp_event_store
                .replay_from(id, 0)
                .expect("read retained events");
            let retired_identity_checkpointed = original_client
                .as_ref()
                .and_then(|client| client.establishment_projection())
                .and_then(|projection| projection.receipt_snapshot())
                .is_some_and(|receipt| {
                    receipt.owner == owner
                        && receipt.session_id() == expected_sid
                        && state.acp_event_store.establishment_seal(&receipt).is_ok()
                        && crate::session::Storage::open_reference("default")
                            .and_then(|storage| storage.load())
                            .unwrap()
                            .iter()
                            .find(|row| row.id == id)
                            .is_some_and(|row| {
                                row.acp_session_id.as_deref() == expected_sid
                                    && row.acp_establishment
                                        == Some(
                                            crate::acp::establishment::AcpEstablishment::Applied {
                                                attempt_id: receipt.attempt.id,
                                            },
                                        )
                            })
                });
            assert!(!stopped_events.iter().any(|stored| matches!(
                stored.event.as_ref(),
                Event::AcpSessionAssigned {
                    connection_ready: true,
                    ..
                }
            )));
            let any_assignment_before_cleanup = stopped_events
                .iter()
                .any(|stored| matches!(stored.event.as_ref(), Event::AcpSessionAssigned { .. }));
            let stop_generation_advanced = if matches!(outcome, ConstructorOutcome::HttpStop) {
                Some(
                    crate::session::Storage::new_unwatched("default")
                        .unwrap()
                        .load()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .lifecycle_generation
                        > before_stop_generation,
                )
            } else {
                None
            };
            let cli_stop_preserved = cli_stop.as_ref().map(|original| {
                crate::process::worker_registry::stop_intent(id)
                    .unwrap()
                    .as_ref()
                    == Some(original)
            });
            let memory_sid = state
                .instances
                .read()
                .await
                .iter()
                .find(|row| row.id == id)
                .and_then(|row| row.acp_session_id.clone());
            let disk_sid = crate::session::Storage::open_reference("default")
                .and_then(|storage| storage.load())
                .map(|rows| {
                    rows.into_iter()
                        .find(|row| row.id == id)
                        .and_then(|row| row.acp_session_id)
                })
                .map_err(|error| format!("{error:#}"));
            let health = state.canonical_health.read().await.clone();
            let diagnostics = format!("expected_sid={expected_sid:?}; memory_sid={memory_sid:?}; disk_sid={disk_sid:?}; health={health:?}");
            agent.settle(&state.acp_supervisor, id).await;
            drop(agent);
            assert!(matches!(stopped, Ok(Ok(()))),
                    "Stop must claim a deferred owned constructor without a vanished producer: {stopped:?}; {diagnostics}");
            assert!(
                !still_alive,
                "the accepted stop must physically retire the original runner"
            );
            assert!(
                native_reaped,
                "Stop must retire and reap the captured native process before fixture cleanup"
            );
            assert!(
                record_absent,
                "Stop must settle the original runner's registry record"
            );
            if let Some(advanced) = stop_generation_advanced {
                assert!(
                    advanced,
                    "the actual HTTP Stop must commit its newer native generation"
                );
            }
            if let Some(settled) = cli_custody_settled {
                assert!(settled, "the real daemon tick must settle a CLI-killed deferred constructor without an installed handle");
                assert_eq!(
                    cli_stop_preserved,
                    Some(true),
                    "settling the dead original must preserve its exact CLI stop token"
                );
                assert!(
                    calls.iter().all(|call| !matches!(
                        call["method"].as_str(),
                        Some("session/load" | "session/prompt")
                    )),
                    "CLI settlement must not replay native work"
                );
            }
            if expected_sid.is_some() {
                assert!(retired_identity_checkpointed,
                        "the real retirement consumer must checkpoint the exact original Native receipt and SID without readiness");
            } else {
                assert!(
                    !any_assignment_before_cleanup,
                    "a never-established constructor must not fabricate a conversation receipt"
                );
            }
            if let Some(preserved) = selected_identity_preserved {
                assert!(preserved,
                        "the native controller must preserve the original queued SID in memory and on disk before fixture cleanup; {diagnostics}");
            }
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["method"] == "session/new")
                    .count(),
                if installation_ready { 1 } else { 0 }
            );
        } else {
            tokio::time::timeout(Duration::from_secs(20), tick.run(&state))
                .await
                .unwrap();
            let sent = tokio::time::timeout(
                Duration::from_secs(3),
                state.acp_supervisor.send_prompt(
                    id,
                    "same owned conversation after registry refusal",
                    &[],
                ),
            )
            .await;
            let delivered = matches!(&sent, Ok(Ok(())));
            if delivered {
                agent.wait_for_call("session/prompt", 1).await;
                tokio::time::timeout(Duration::from_secs(5), async {
                        while !state.acp_event_store.replay_from(id, 0).expect("read retained events").iter().any(|stored| matches!(stored.event.as_ref(), Event::Stopped { reason } if reason == "prompt_complete")) {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                        }
                    })
                    .await
                    .unwrap();
                agent.wait_for_call("session/set_mode", 1).await;
                if let Some(original) = original_client.as_ref() {
                    state
                        .acp_supervisor
                        .test_assert_installed_client(id, original)
                        .await;
                }
                assert_eq!(agent.started().await, native);
            }
            let retained = runner_record(id).await;
            let calls = agent.calls();
            let active_owner = state.acp_supervisor.running_identity(id);
            if delivered {
                state
                    .acp_supervisor
                    .shutdown_and_wait(id, Duration::from_secs(5))
                    .await
                    .unwrap();
            }
            agent.settle(&state.acp_supervisor, id).await;
            drop(agent);
            assert!(
                delivered,
                "the next real reconcile must install a usable same-actor worker: {sent:?}"
            );
            assert_eq!(active_owner, Some(owner));
            let retained = retained.unwrap();
            assert!(owner.owns_record(&retained));
            let sid = retained
                .stored_acp_session_id
                .as_deref()
                .expect("genuine initial session established");
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["method"] == "initialize")
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["method"] == "session/new")
                    .count(),
                1
            );
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["method"] == "session/load")
                    .count(),
                0
            );
            let modes: Vec<_> = calls
                .iter()
                .filter(|call| call["method"] == "session/set_mode")
                .collect();
            assert_eq!(modes.len(), 1);
            assert_eq!(modes[0]["params"]["modeId"], "plan");
            let prompts: Vec<_> = calls
                .iter()
                .filter(|call| call["method"] == "session/prompt")
                .collect();
            assert_eq!(prompts.len(), 1);
            assert_eq!(prompts[0]["params"]["sessionId"], sid);
            assert!(calls
                .iter()
                .all(|call| call["native_pid"].as_u64() == Some(native.0 as u64)));
        }
        assert_eq!(
            after_attempt,
            Some(prior.clone()),
            "creation does not spend the reconciler's prior retry budget"
        );
        assert_eq!(
            after_uncharged_tick,
            Some(prior),
            "deferred ownership is not a successful retry or a history reset"
        );
        assert_eq!(
            calls_while_refused
                .iter()
                .filter(|call| call["method"] == "session/new")
                .count(),
            if installation_ready { 1 } else { 0 },
            "registration continuation must preserve the not-yet-established original New"
        );
        assert!(!state
            .acp_event_store
            .replay_from(id, 0)
            .expect("read retained events")
            .iter()
            .any(|stored| matches!(stored.event.as_ref(), Event::SessionContextReset { .. })));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn owned_registration_contention_reconciles_original_authorized_new_once() {
        owned_constructor_registry_deferral_recovers(false, ConstructorOutcome::Resume).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn owned_installation_contention_reconciles_original_client_and_mode_once() {
        owned_constructor_registry_deferral_recovers(true, ConstructorOutcome::Resume).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn stop_physically_retires_a_deferred_owned_constructor_without_reinstallation() {
        owned_constructor_registry_deferral_recovers(true, ConstructorOutcome::SupervisorStop)
            .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn http_stop_preserves_original_inbound_assignment_across_its_native_commit() {
        owned_constructor_registry_deferral_recovers(true, ConstructorOutcome::HttpStop).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cli_stop_settles_handleless_owned_constructor_and_preserves_its_native_binding() {
        owned_constructor_registry_deferral_recovers(true, ConstructorOutcome::CliStop).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cli_stop_settles_handleless_registration_without_establishing_a_conversation() {
        owned_constructor_registry_deferral_recovers(false, ConstructorOutcome::CliStop).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn registry_contention_preserves_prior_budget_and_refunds_only_charged_dispatch() {
        for charged in [false, true] {
            let id = "registry-budget";
            let (_home, state, _project) = test_state(id);
            let mut tick = Tick::default();
            let prior = vec![
                Instant::now() - Duration::from_secs(2),
                Instant::now() - Duration::from_secs(1),
            ];
            tick.respawn_history.insert(id.to_owned(), prior.clone());
            let entry = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(
                    crate::process::worker_registry::record_path(id)
                        .unwrap()
                        .with_extension("lock"),
                )
                .unwrap();
            let (release, entered) = if charged {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                *RESUME_DISPATCH_GATE.lock().unwrap() = Some((entered_tx, release_rx));
                (Some(release_tx), Some(entered_rx))
            } else {
                fs2::FileExt::lock_exclusive(&entry).unwrap();
                (None, None)
            };
            let tick_state = Arc::clone(&state);
            let task = tokio::spawn(async move {
                tick.run(&tick_state).await;
                tick
            });
            if let Some(entered) = entered {
                tokio::time::timeout(Duration::from_secs(5), entered)
                    .await
                    .unwrap()
                    .unwrap();
                fs2::FileExt::lock_exclusive(&entry).unwrap();
                release.unwrap().send(()).unwrap();
            }
            let tick = tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                tick.respawn_history.get(id),
                Some(&prior),
                "charged={charged}"
            );
            assert!(
                !tick.attempted.contains(id),
                "the refused observation remains retryable"
            );
            assert!(
                !tick.parked.contains(id),
                "contention is not a crash-loop decision"
            );
            assert_eq!(startup_errors(&state, id), 0);
            assert_eq!(
                state.acp_supervisor.worker_state(id).await,
                AcpWorkerState::Absent
            );
            assert!(
                state
                    .acp_event_store
                    .replay_from(id, 0)
                    .expect("read retained events")
                    .is_empty(),
                "registry contention does not fabricate a native-loss event"
            );
            drop(entry);
        }
    }

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

    #[tokio::test]
    #[serial_test::serial]
    async fn retained_legacy_refusal_never_dials_or_spends_reconciler_budget() {
        use crate::acp::event_store::ControlKnowledge::{Indeterminate, Known};
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::process::worker_registry;
        struct RestoreRecord {
            path: std::path::PathBuf,
            bytes: Vec<u8>,
        }
        impl Drop for RestoreRecord {
            fn drop(&mut self) {
                crate::session::atomic_write(&self.path, &self.bytes).unwrap();
            }
        }
        with_managed(|scope| {
            Box::pin(async move {
                for shape in ["missing", "null", "v3"] {
                    let mut live = crate::server::test_support::managed_control_session(
                        scope, shape, "claude", None,
                    )
                    .await;
                    let id = live.id().to_owned();
                    live.park("interrupted explicit intent").await;
                    let park = match live.state.acp_event_store.rate_limit_park(&id).unwrap() {
                        Known(Some(park)) => park,
                        other => panic!("real quota park missing: {other:?}"),
                    };
                    live.state
                        .acp_event_store
                        .test_conn()
                        .execute(
                            "DELETE FROM acp_events WHERE session_id=?1 AND seq<?2",
                            rusqlite::params![id, i64::try_from(park.seq).unwrap()],
                        )
                        .unwrap();
                    live.state.acp_event_store.control_cache().forget(&id);
                    let budget = live
                        .state
                        .acp_event_store
                        .rate_limit_redelivery_streak(&id)
                        .unwrap();
                    assert_eq!(budget, Known(0));
                    let methods = live.native.requests();
                    live.client = scope
                        .reconcile_after(live.client, |state, id| async move {
                            // Closing the SDK socket precedes the Native runner's
                            // final registry write. Install legacy bytes only after
                            // that real detach has completed.
                            tokio::time::timeout(Duration::from_secs(10), async {
                                loop {
                                    let record = worker_registry::load(&id).unwrap().unwrap();
                                    if record.detached_at.is_some() {
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_millis(10)).await;
                                }
                            })
                            .await
                            .expect("original Native runner recorded its detach");
                            let path = worker_registry::record_path(&id).unwrap();
                            let bytes = std::fs::read(&path).unwrap();
                            let restore = RestoreRecord {
                                path: path.clone(),
                                bytes: bytes.clone(),
                            };
                            let mut data: serde_json::Value =
                                serde_json::from_slice(&bytes).unwrap();
                            match shape {
                                "missing" => {
                                    data.as_object_mut()
                                        .unwrap()
                                        .remove("control_protocol_version");
                                }
                                "null" => {
                                    data["control_protocol_version"] = serde_json::Value::Null
                                }
                                "v3" => data["control_protocol_version"] = serde_json::json!(3),
                                _ => unreachable!(),
                            }
                            crate::session::atomic_write(
                                &path,
                                &serde_json::to_vec(&data).unwrap(),
                            )
                            .unwrap();
                            let incompatible = std::fs::read(&path).unwrap();
                            let mut tick = Tick::default();
                            let prior = vec![Instant::now()];
                            tick.respawn_history.insert(id.clone(), prior.clone());
                            for _ in 0..2 {
                                tick.run(&state).await;
                                assert_eq!(state.acp_supervisor.running_identity(&id), None);
                                assert!(state
                                    .acp_supervisor
                                    .current_native_availability(&id)
                                    .await
                                    .is_none_or(|availability| availability.owner.is_none()));
                                assert_eq!(tick.respawn_history.get(&id), Some(&prior));
                                assert_eq!(
                                    state
                                        .acp_event_store
                                        .rate_limit_redelivery_streak(&id)
                                        .unwrap(),
                                    budget
                                );
                                assert!(matches!(
                                    state.acp_event_store.rate_limit_park(&id).unwrap(),
                                    Indeterminate
                                ));
                                assert!(state
                                    .session_service
                                    .queued_prompts_snapshot(&id)
                                    .await
                                    .is_empty());
                                assert!(state
                                    .instances
                                    .read()
                                    .await
                                    .iter()
                                    .find(|row| row.id == id)
                                    .unwrap()
                                    .pending_initial_turn
                                    .is_none());
                                let events = state.acp_event_store.replay_from(&id, 0).unwrap();
                                assert!(!events.iter().any(|stored| matches!(
                                    stored.event.as_ref(),
                                    Event::RateLimitAutoResumed { .. }
                                        | Event::UserPromptSent { .. }
                                )));
                                assert_eq!(std::fs::read(&path).unwrap(), incompatible);
                            }
                            drop(restore);
                            tick.run(&state).await;
                        })
                        .await;
                    live.client.ready().await;
                    live.state = live.client.app_state();
                    assert_eq!(live.native.requests(), methods);
                    assert_eq!(
                        live.state
                            .acp_event_store
                            .rate_limit_redelivery_streak(&id)
                            .unwrap(),
                        budget
                    );
                    assert!(matches!(
                        live.state.acp_event_store.rate_limit_park(&id).unwrap(),
                        Indeterminate
                    ));
                    live.client.shutdown().await.expect(
                        "settle this case's retained Native actor before creating the next",
                    );
                }
            })
        })
        .await;
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
                crate::process::worker_registry::mark_restart_pending(id, 5).unwrap();
            }
            tick.run(&state).await;

            assert!(crate::process::worker_registry::peek_restart_marker(id).is_none());
            // The bogus agent fails fast, so a spawn attempt records one startup error.
            let expected_events = usize::from(with_marker);
            assert_eq!(
                state
                    .acp_event_store
                    .replay_from(id, 0)
                    .expect("read retained events")
                    .len(),
                expected_events,
                "with_marker={with_marker}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn temporary_purge_fences_rearm_but_explicit_stops_remain_terminal() {
        for explicit_stop in [false, true] {
            let id = "s-purge-fence";
            let (_home, state, _project) = test_state(id);
            let admission = state.acp_supervisor.fence_spawn_admission(id);
            let stop = if explicit_stop {
                Some(
                    state
                        .acp_supervisor
                        .begin_stop_request(id, "user_stopped", None, false)
                        .await,
                )
            } else {
                None
            };
            let mut tick = Tick::default();
            for _ in 0..RECONCILER_MAX_RESPAWNS_IN_WINDOW + 2 {
                tick.run(&state).await;
            }
            assert_eq!(startup_errors(&state, id), 0);
            assert!(!tick.parked.contains(id));
            assert_eq!(
                tick.respawn_history.get(id).map_or(0, Vec::len),
                usize::from(explicit_stop)
            );

            drop(stop);
            drop(admission);
            tick.run(&state).await;
            let errors = state
                .acp_event_store
                .replay_from(id, 0)
                .unwrap()
                .into_iter()
                .filter_map(|stored| match stored.event.as_ref() {
                    crate::acp::Event::AgentStartupError { message, .. } => Some(message.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if explicit_stop {
                assert!(errors.is_empty());
            } else {
                assert!(matches!(
                    errors.as_slice(),
                    [message] if message.contains("aoe-no-such-agent-1027")
                ));
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn an_unconfirmed_attach_preserves_the_runner_without_charging_budget() {
        let id = "s-purge-at-fallback";
        let (_home, state, project) = test_state(id);
        let sockets = tempfile::TempDir::with_prefix_in("aoe-attach-", "/tmp").unwrap();
        let socket = sockets.path().join("runner.sock");
        let listener =
            tokio::net::UnixListener::bind(crate::process::worker::control_socket_sibling(&socket))
                .unwrap();
        let mut child = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut record = crate::process::worker_registry::WorkerRecord::new(
            id.into(),
            child.id().unwrap(),
            socket,
            "test-agent-acp".into(),
            "test-agent".into(),
            project.path().to_path_buf(),
            None,
            vec![],
            vec![],
            Some("native-id".into()),
            Some("default".into()),
        )
        .with_generation(17);
        record.control_protocol_version =
            Some(crate::acp::control_protocol::CONTROL_PROTOCOL_VERSION);
        crate::process::worker_registry::save(&record).unwrap();
        let clear = crate::acp::Event::SessionCleared;
        let metadata = crate::acp::event_sender::EventMetadata::host(&clear);
        state
            .acp_event_store
            .record(id, 1, &clear, &metadata)
            .unwrap();

        let (entered, arrived) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        *RESUME_DISPATCH_GATE.lock().unwrap() = Some((entered, released));
        let mut tick = Tick::default();
        tokio::time::timeout(Duration::from_secs(5), async {
            let ((), ()) = tokio::join!(tick.run(&state), async {
                arrived
                    .await
                    .expect("reconciliation dispatches the retained runner");
                release.send(()).unwrap();
            });
        })
        .await
        .expect("reconciliation settles without an establishment seal");
        tokio::select! {
            biased;
            accepted = listener.accept() => panic!("uncertified attach reached socket: {accepted:?}"),
            () = std::future::ready(()) => {}
        }
        let retained = crate::process::worker_registry::load(id).unwrap().unwrap();
        assert_eq!(retained.runner_identity, record.runner_identity);
        assert_eq!(retained.generation, record.generation);
        assert!(!tick.attempted.contains(id));
        assert!(!tick.respawn_history.contains_key(id));
        assert_eq!(startup_errors(&state, id), 0);

        assert!(child.try_wait().unwrap().is_none());
        child.kill().await.unwrap();
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
    #[tokio::test]
    #[serial_test::serial]
    async fn an_agent_turn_after_a_completed_prompt_is_not_a_prompt_in_flight() {
        use crate::acp::Event;
        crate::acp::supervisor::test_support::managed::with_managed(|scope| Box::pin(async move {
            let mut live = crate::server::test_support::managed_control_session(
                scope, "independent-agent-turn", "claude", None,
            ).await;
            let id = live.id().to_owned();
            let state = live.state.clone();
            let store = &state.acp_event_store;
            let prompt = live.start_prompt("go").await;
            live.finish_prompt(&prompt, None).await;
            let run = uuid::Uuid::new_v4();
            live.native.send(serde_json::json!({
                "jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc"}
            })).await;
            live.native.send(serde_json::json!({
                "jsonrpc":"2.0","method":"session/update","params":{"sessionId":live.sid,
                    "update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"woken"}}}
            })).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !matches!(live.client.next_event().await.expect("genuine own output"),
                    Event::AgentMessageChunk { text, .. } if text == "woken") {}
            }).await.unwrap();
            assert_eq!(open_turn_kinds(store, &id).unwrap(), Known((false, true)));
            assert_eq!(store.holds_live_work(&id).unwrap(), Known(true));
            live.native.send(serde_json::json!({
                "jsonrpc":"2.0","method":"_omp/autonomous_turn/end",
                "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc","stopReason":"end_turn"}
            })).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                while !matches!(live.client.next_event().await.expect("genuine own terminal"),
                    Event::Stopped { reason } if reason == "agent_idle") {}
            }).await.unwrap();
            assert_eq!(open_turn_kinds(store, &id).unwrap(), Known((false, false)));
            assert_eq!(store.holds_live_work(&id).unwrap(), Known(false));
        })).await;
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
        state.acp_supervisor.note_startup_requeue(id);

        tick.run(&state).await;

        assert_eq!(
            state
                .acp_event_store
                .replay_from(id, 0)
                .expect("read retained events")
                .len(),
            1
        );
        assert_eq!(tick.respawn_history[id].len(), 2, "counted, not wiped");
        assert!(state.acp_supervisor.take_startup_requeues().is_empty());
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
            assert!(tick.resume_deferred.contains_key("s-cap"));
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
            assert!(tick.resume_deferred.contains_key(id), "precondition");

            if slot_freed {
                state.acp_supervisor.test_remove_worker("occupant").await;
            } else {
                state.acp_supervisor.test_insert_worker(id).await;
            }
            tick.run(&state).await;
            assert!(
                !tick.resume_deferred.contains_key(id),
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
    #[serial_test::serial]
    async fn queue_drain_ignores_background_work_but_preserves_foreground_barriers() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::acp::Event;
        use crate::server::test_support;
        use std::io::Write as _;

        with_managed(|scope| Box::pin(async move {
            for (name, text, drains) in [
                ("task", "follow up", true),
                ("agent", "follow up", true),
                ("main", "follow up", false),
                ("notification", "follow up", false),
                ("cancel", "follow up", false),
                ("compact", "follow up", false),
                ("clear", "/clear", false),
            ] {
                let agent = if name == "agent" { "codex" } else { "claude" };
                let mut live = test_support::managed_control_session(
                    scope, name, agent, None,
                ).await;
                let id = live.id().to_owned();
                let state = live.state.clone();
                let row = state.instances.read().await.iter()
                    .find(|row| row.id == id).unwrap().clone();
                let task_transcript = if name != "agent" {
                let binding = live.client.actual().native_store.as_ref()
                    .filter(|binding| binding.agent == "claude" && binding.filesystem == "host")
                    .expect("the actual original owns its Claude resource store").clone();
                let store_root = binding.stores.first().unwrap();
                let task_transcript = store_root.join("projects")
                    .join(crate::session::capture::encode_claude_project_path(&row.project_path))
                    .join(format!("{}.jsonl", live.sid));
                std::fs::create_dir_all(task_transcript.parent().unwrap()).unwrap();
                std::fs::write(&task_transcript, "").unwrap();
                    Some(task_transcript)
                } else {
                    None
                };
                let output = scope.tempdir();
                let agent_transcript = output.path().join("agent.jsonl");
                std::fs::write(&agent_transcript, "").unwrap();
                let request = live.start_prompt("go").await;
                let allocation = live.start_background(name == "agent", &agent_transcript).await;
                assert!(state.acp_event_store.replay_from(&id, 0).unwrap().iter().any(|stored| {
                    stored.metadata.resource_ref == Some(allocation)
                        && stored.metadata.resource_witness.as_ref()
                            .is_some_and(|witness| witness.allocation == allocation)
                }), "{name}: background work is a real first-I/O allocation, not history");
                if !matches!(name, "main" | "cancel") {
                    live.finish_prompt(&request, None).await;
                    let control = live.wait_control(|control| !control.turn_active).await;
                    assert!(control.has_active_background_agent(), "{name}");
                    assert_eq!(control.sidebar_status(), crate::session::Status::Running,
                        "the display remains busy for the actual background allocation");
                }
                if name == "notification" {
                    let run = uuid::Uuid::new_v4();
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                        "params":{"version":1,"sessionId":live.sid,"runId":run,"source":"irc"}
                    })).await;
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"session/update","params":{
                            "sessionId":live.sid,"update":{
                                "sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text","text":"result"}
                            }
                        }
                    })).await;
                    tokio::time::timeout(Duration::from_secs(10), async {
                        while !matches!(live.client.next_event().await.expect("actual own output"),
                            Event::AgentMessageChunk { text, .. } if text == "result") {}
                    }).await.unwrap();
                    let control = live.wait_control(|control| control.native_autonomous_active).await;
                    assert_eq!(control.sidebar_status(), crate::session::Status::Running);
                }
                if name == "cancel" {
                    live.client.cancel_prompt().await.unwrap();
                    live.wait_control(|control| control.cancelling).await;
                    tokio::time::timeout(Duration::from_secs(10), async {
                        while !live.native.requests().iter().any(|method| method == "session/cancel") {
                            tokio::task::yield_now().await;
                        }
                    }).await.expect("the actual original received cancel");
                }
                if name == "compact" {
                    let run = uuid::Uuid::new_v4();
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"_omp/autonomous_turn/start",
                        "params":{"version":1,"sessionId":live.sid,"runId":run,
                            "source":"scheduled_continuation"}
                    })).await;
                    live.native.send(serde_json::json!({
                        "jsonrpc":"2.0","method":"session/update","params":{
                            "sessionId":live.sid,"update":{
                                "sessionUpdate":"compaction_update",
                                "compactionId":"queue-background-compact", "status":"in_progress"
                            }
                        }
                    })).await;
                    live.wait_control(|control| control.compacting).await;
                }
                // Preserve the original test's busy display input. Native control and
                // readiness continue to come only from this original's real events.
                state.instances.write().await.iter_mut().find(|row| row.id == id).unwrap()
                    .status = crate::session::Status::Running;
                let prompt_count = || live.native.requests().iter()
                    .filter(|method| method.as_str() == "session/prompt").count();
                let before = prompt_count();
                state.session_service.enqueue_prompt(
                    &id, "q".into(), text.into(), Vec::new(), None, crate::server::session_service::QueueSender::default(),
                ).await.unwrap();
                if drains {
                    drain_queued_prompts(&state).await;
                    tokio::time::timeout(Duration::from_secs(10), async {
                        while !state.session_service.queued_prompts_snapshot(&id).await.is_empty() {
                            tokio::task::yield_now().await;
                        }
                    }).await.expect("reconciler must deliver while background work runs");
                    let delivered = live.next_prompt().await;
                    assert_eq!(delivered["params"]["prompt"][0]["text"], text, "{name}");
                } else {
                    // Await the real delivery path before asserting non-delivery.
                    state.session_service.drain_queued_prompts_once(&id).await;
                    assert_eq!(state.session_service.queued_prompts_snapshot(&id).await.len(), 1, "{name}");
                }
                let after = live.native.requests().iter()
                    .filter(|method| method.as_str() == "session/prompt").count();
                assert_eq!(after - before, usize::from(drains), "{name}");
                if name == "clear" {
                    // End the same actual task through its original transcript. Its
                    // real tailer publishes the matching resource_ref terminal.
                    let line = serde_json::json!({
                        "type":"queue-operation", "operation":"enqueue",
                        "timestamp":crate::time::Timestamp::now().to_rfc3339(),
                        "content":"<task-notification>\n<task-id>task</task-id>\n<tool-use-id>tool-background</tool-use-id>\n<status>completed</status>\n</task-notification>"
                    });
                    let mut file = std::fs::OpenOptions::new().append(true)
                        .open(task_transcript.as_ref().unwrap()).unwrap();
                    writeln!(file, "{line}").unwrap();
                    file.flush().unwrap();
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let event = live.client.next_event().await.expect("original task observer");
                            if matches!(event, Event::BackgroundTaskFinished { task_id, .. } if task_id == "task") {
                                assert!(state.acp_event_store.replay_from(&id, 0).unwrap().iter().any(|stored| {
                                    stored.metadata.resource_ref == Some(allocation)
                                        && matches!(stored.event.as_ref(), Event::BackgroundTaskFinished { task_id, .. }
                                            if task_id == "task")
                                }));
                                break;
                            }
                        }
                    }).await.expect("the original allocation's tailer publishes its actual end");
                    live.wait_control(|control| !control.has_active_background_agent()).await;
                    let resets_before = live.native.requests().iter()
                        .filter(|method| method.as_str() == "session/new").count();
                    state.session_service.drain_queued_prompts_once(&id).await;
                    assert!(state.session_service.queued_prompts_snapshot(&id).await.is_empty());
                    let resets_after = live.native.requests().iter()
                        .filter(|method| method.as_str() == "session/new").count();
                    assert_eq!(resets_after, resets_before + 1,
                        "the queued /clear issues one genuine Native reset");
                }
                live.client.shutdown().await.unwrap();
            }
        })).await;
    }

    /// Wake-on-drain: a dormant session with a queue must be woken, or its
    /// queue sits behind a worker the resume pass never respawns.
    #[tokio::test]
    #[serial_test::serial]
    async fn drain_queued_prompts_wakes_a_dormant_session_with_a_queue() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::acp::Event;
        use crate::server::test_support;

        with_managed(|scope| {
            Box::pin(async move {
                crate::session::config::update_config(|config| {
                    config.acp.auto_stop_idle_secs = 60;
                })
                .unwrap();
                test_support::refresh_canonical_metadata_for_test(&scope.state).await;
                let mut original = test_support::managed_control_session(
                    scope,
                    "queue-drain-wake",
                    "claude",
                    None,
                )
                .await;
                let id = original.id().to_owned();
                let state = original.state.clone();
                let sid = original.sid.clone();
                let old_turn = original
                    .start_prompt("completed work must not replay")
                    .await;
                original.finish_prompt(&old_turn, None).await;
                original.make_quota_due();
                original.client.lease.test_backdate_idle_baseline(
                    (crate::time::Timestamp::now() - crate::time::Duration::hours(1)).as_millis(),
                );
                idle::reap_idle_workers(&state).await;
                assert!(!state.acp_supervisor.is_running(&id).await);
                let storage = crate::session::Storage::open_reference("test").unwrap();
                for row in [
                    state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .clone(),
                    storage
                        .load()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap(),
                ] {
                    assert!(
                        row.is_idle_dormant(),
                        "real idle retirement precedes queued input"
                    );
                    row.require_resolved_acp_selection().unwrap();
                    assert_eq!(row.acp_session_id.as_deref(), Some(sid.as_str()));
                }
                original.client.shutdown().await.unwrap();
                let submission = state.session_service.prompt_submission(&id).await;
                let queued = state
                    .session_service
                    .enqueue_prompt(
                        &id,
                        "queue-after-native-idle".into(),
                        "new explicit queued input".into(),
                        Vec::new(),
                        None,
                        crate::server::session_service::QueueSender::default(),
                    )
                    .await
                    .unwrap();
                drop(submission);
                let row = state
                    .instances
                    .read()
                    .await
                    .iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .clone();
                assert!(row.is_idle_dormant(), "enqueue does not perform the wake");
                let expected_sid = sid.clone();
                let (sent, mut prompts) = tokio::sync::mpsc::unbounded_channel();
                let (config, native) = scope.native_peer(
                    std::path::PathBuf::from(&row.project_path),
                    "claude",
                    "claude",
                    move |request| match request["method"].as_str().unwrap() {
                        "initialize" => Some(serde_json::json!({
                            "jsonrpc":"2.0","id":request["id"],
                            "result":{"protocolVersion":1,"agentCapabilities":{"loadSession":true}}
                        })),
                        "session/load" => {
                            assert_eq!(request["params"]["sessionId"], expected_sid);
                            Some(
                                serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":{}}),
                            )
                        }
                        "session/prompt" => {
                            sent.send(request.clone()).unwrap();
                            None
                        }
                        "session/cancel" => None,
                        other => {
                            panic!("queued input must load the original conversation: {other}")
                        }
                    },
                );
                let events = scope.bind_external_launch(&id, config);
                drain_queued_prompts(&state).await;
                tokio::time::timeout(Duration::from_secs(5), async {
                    while state
                        .instances
                        .read()
                        .await
                        .iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .is_idle_dormant()
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("qualified dormant Native wakes for new queued input");
                assert_eq!(
                    storage
                        .load()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap()
                        .queued_prompts,
                    vec![queued]
                );
                let mut tick = test_fixtures::Tick::default();
                tick.run(&state).await;
                let mut resumed = scope.observe_external_resume(id.clone(), events).await;
                resumed.ready().await;
                drain_queued_prompts(&state).await;
                let request = tokio::time::timeout(Duration::from_secs(5), prompts.recv())
                    .await
                    .expect("queued input reaches the actual resumed Native")
                    .unwrap();
                assert_eq!(request["params"]["sessionId"], sid);
                assert_eq!(
                    request["params"]["prompt"][0]["text"],
                    "new explicit queued input"
                );
                native
                    .send(serde_json::json!({
                        "jsonrpc":"2.0","id":request["id"],"result":{"stopReason":"end_turn"}
                    }))
                    .await;
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        if matches!(
                            resumed.next_event().await.unwrap(),
                            Event::Stopped { reason } if reason == "prompt_complete"
                        ) {
                            break;
                        }
                    }
                    while !state
                        .session_service
                        .queued_prompts_snapshot(&id)
                        .await
                        .is_empty()
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("actual completion and queue retirement settle");
                tick.run(&state).await;
                state.session_service.drain_queued_prompts_once(&id).await;
                let methods = native.requests();
                for method in ["initialize", "session/load", "session/prompt"] {
                    assert_eq!(
                        methods.iter().filter(|got| got.as_str() == method).count(),
                        1
                    );
                }
                assert!(!methods.iter().any(|method| method == "session/new"));
                assert!(prompts.try_recv().is_err());
                assert_eq!(
                    original
                        .native
                        .requests()
                        .iter()
                        .filter(|method| *method == "session/prompt")
                        .count(),
                    1,
                    "the completed original turn is not replayed"
                );
                assert!(!tick.parked.contains(&id));
                assert!(storage
                    .load()
                    .unwrap()
                    .into_iter()
                    .find(|row| row.id == id)
                    .unwrap()
                    .queued_prompts
                    .is_empty());
            })
        })
        .await;
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
            crate::server::test_support::select_retained_for_test(&mut instances[0]);
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
        {
            let instances = state.instances.read().await;
            assert_eq!(
                instances[0].lifecycle_generation,
                stopped.lifecycle_generation
            );
            assert_eq!(instances[0].resume_intent, stopped.resume_intent);
            assert_eq!(instances[0].acp_establishment, stopped.acp_establishment);
        }
        let persisted = crate::session::Storage::new_unwatched(&stopped.source_profile)
            .unwrap()
            .load_instances_complete()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        assert_eq!(persisted.lifecycle_generation, stopped.lifecycle_generation);
        assert_eq!(persisted.resume_intent, stopped.resume_intent);
        assert_eq!(persisted.acp_establishment, stopped.acp_establishment);

        let mut publications = state.acp_events_tx.subscribe();
        let response = crate::server::api::sessions::start_session(
            State(Arc::clone(&state)),
            Path(id.to_string()),
            Ok(None),
        )
        .await
        .into_response();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        tick.run(&state).await;
        let startup = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = publications
                    .recv()
                    .await
                    .expect("actual startup publication");
                if frame.session_id == id
                    && matches!(frame.event.as_ref(), Event::AgentStartupError { .. })
                {
                    break frame;
                }
            }
        })
        .await
        .expect("the explicit background attempt reports its real startup failure");
        let Event::AgentStartupError { message, .. } = startup.event.as_ref() else {
            unreachable!("selected the actual startup failure");
        };
        let expected_error = crate::acp::supervisor::SupervisorError::UnknownAgent(
            stopped
                .agent_name
                .clone()
                .expect("the fixture selects an unknown agent"),
        )
        .to_string();
        assert!(
            message.contains(&expected_error),
            "explicit start must reach agent resolution, not a selection refusal: {message}"
        );
        assert_eq!(
            test_fixtures::startup_errors(&state, id),
            1,
            "explicit start must retry the fake agent without a queued prompt"
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while state.acp_supervisor.worker_state(id).await != AcpWorkerState::Absent {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the explicit construction settles before checking the no-loop tick");
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

    /// A stop holds the queue without spending crash-loop budget or suppressing
    /// retries after an explicit start clears the fence.
    #[tokio::test]
    #[serial_test::serial]
    async fn durable_user_stop_with_a_queue_does_not_crash_loop() {
        use crate::process::worker_registry::{self, WorkerRecord};
        use crate::session::Status;

        for status in [Status::Idle, Status::Error] {
            let id = "sess-stopped-queued";
            let (_home, state, project) = test_fixtures::test_state(id);
            {
                let mut instances = state.instances.write().await;
                instances[0].status = status;
                instances[0]
                    .queued_prompts
                    .push(test_fixtures::queued_prompt());
            }
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
            *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Degraded {
                code: crate::daemon::ReloadFailureCode::Metadata,
                profiles: vec!["default".into()],
            };
            for _ in 0..2 {
                tick.run(&state).await;
            }
            assert_eq!(count_messages(&state, id, STOP_FENCE_MESSAGE), 0);
            assert!(!tick.stop_notified.contains(id));
            assert!(tick.parked.is_empty());
            assert!(tick.respawn_history.get(id).is_none_or(Vec::is_empty));

            *state.canonical_health.write().await = crate::daemon::RuntimeHealth::Healthy;
            for _ in 0..=2 * RECONCILER_MAX_RESPAWNS_IN_WINDOW {
                tick.run(&state).await;
            }

            assert!(
                tick.respawn_history.get(id).is_none_or(Vec::is_empty),
                "{status:?}: a fenced session must not charge the budget"
            );
            assert_eq!(
                test_fixtures::startup_errors(&state, id),
                1,
                "{status:?}: one banner for the stop, no crash-loop park"
            );
            assert_eq!(
                count_messages(&state, id, STOP_FENCE_MESSAGE),
                1,
                "{status:?}: the banner names the stop"
            );
            assert!(tick.parked.is_empty());

            // Fresh reconciler state models a daemon restart with the same fence.
            let mut tick = test_fixtures::Tick::default();
            for _ in 0..=2 * RECONCILER_MAX_RESPAWNS_IN_WINDOW {
                tick.run(&state).await;
            }
            assert!(
                worker_registry::stop_requested(id),
                "{status:?}: the fence survives the restart"
            );
            assert!(
                tick.respawn_history.get(id).is_none_or(Vec::is_empty),
                "{status:?}: a restart must not charge the budget either"
            );
            assert_eq!(
                test_fixtures::startup_errors(&state, id),
                2,
                "{status:?}: one more banner after the restart, no crash-loop park"
            );
            assert_eq!(
                count_messages(&state, id, STOP_FENCE_MESSAGE),
                2,
                "{status:?}: each process shows the stop banner once"
            );

            // Admission clears the fence; even a failed launch must not leave
            // the queue held by the old stop notice.
            let observed = worker_registry::stop_intent(id).unwrap();
            worker_registry::admit_resume(
                id,
                worker_registry::ResumeAuthority::Explicit,
                &observed,
            )
            .unwrap();
            tick.run(&state).await;
            assert_eq!(
                tick.respawn_history.get(id).map_or(0, Vec::len),
                1,
                "{status:?}: the explicit start is a real attempt"
            );
            assert_eq!(test_fixtures::startup_errors(&state, id), 3, "{status:?}");
            assert!(!tick.stop_notified.contains(id));
            tick.run(&state).await;
            assert_eq!(tick.respawn_history.get(id).map_or(0, Vec::len), 2);
        }
    }

    fn count_messages(state: &AppState, id: &str, text: &str) -> usize {
        state
            .acp_event_store
            .replay_from(id, 0)
            .unwrap()
            .into_iter()
            .filter(|stored| {
                matches!(stored.event.as_ref(), crate::acp::Event::AgentStartupError { message, .. } if message == text)
            })
            .count()
    }
}
