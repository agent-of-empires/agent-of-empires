//! The drain task: pumps a worker's events into the sink and respawns the
//! worker when its connection ends, within the restart budget.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::agents::log_wrapper_substitution;
use super::launch::{
    admit_stored_row, apply_claude_store_pin, apply_declared_config_dir,
    await_resume_to_completion, before_session_env, discovery_key, overlay_env, publish_rejection,
    refresh_spawn_model_effort, resolve_mcp_servers, McpPaths,
};
use super::teardown::{settle_sweeping_if_owed, tear_down_runner, wait_for_exit};
use super::{
    lock_recover, publish_next_persisted, publish_with_next_seq, BroadcastSink, Launcher,
    PendingContextReset, SeqMap, SharedSet, Supervisor, SupervisorError, WorkerKind, Workers,
    MAX_RESPAWNS_IN_WINDOW, RESPAWN_BACKOFF, RESTART_WINDOW,
};
use crate::acp::acp_client::{AcpClient, AcpError, ControlLoss, SpawnConfig};
use crate::acp::runner_lifecycle::{
    InstallError, Lease, LifecycleTable, PrimeOwnership, ProcessControl, RunnerIdentity,
};

/// Bounds recovery of a live runner whose control channel closed.
const CONTROL_REATTACH_ATTEMPTS: u32 = 3;
const CONTROL_REATTACH_DEADLINE: Duration = Duration::from_secs(3);
use crate::acp::state::{AcpSessionId, Event};
use crate::process::worker_registry;

impl<S: BroadcastSink> Supervisor<S> {
    pub(super) fn start_drain_task(
        &self,
        session_id: String,
        lease: Lease,
        inbound: mpsc::Receiver<Event>,
        context_reset: Option<PendingContextReset>,
    ) -> JoinHandle<()> {
        let drain = Drain {
            session_id,
            sink: Arc::clone(&self.sink),
            workers: Arc::clone(&self.workers),
            next_seqs: Arc::clone(&self.next_seqs),
            incompatible_binaries: Arc::clone(&self.incompatible_binaries),
            lifecycle: Arc::clone(&self.lifecycle),
            process_control: Arc::clone(&self.process_control),
            launcher: Arc::clone(&self.launcher),
            notify: Arc::clone(&self.worker_notify),
            startup_failures: Arc::clone(&self.startup_failures),
            pending_context_resets: Arc::clone(&self.pending_context_resets),
            context_reset,
            respawned_in_place: Arc::clone(&self.respawned_in_place),
            recovery: None,
        };
        crate::task_util::spawn_supervised(
            "supervisor.drain",
            crate::task_util::PanicPolicy::Log,
            drain.run(lease, inbound),
        )
    }
}

struct Drain<S> {
    session_id: String,
    sink: Arc<S>,
    workers: Workers,
    next_seqs: Arc<SeqMap>,
    incompatible_binaries: Arc<std::sync::Mutex<HashMap<String, String>>>,
    lifecycle: Arc<std::sync::Mutex<LifecycleTable>>,
    process_control: Arc<dyn ProcessControl>,
    launcher: Launcher,
    notify: Arc<tokio::sync::Notify>,
    startup_failures: SharedSet,
    pending_context_resets: SharedSet,
    context_reset: Option<PendingContextReset>,
    respawned_in_place: SharedSet,
    recovery: Option<Recovery>,
}

impl<S> Drain<S> {
    fn reservation_for_lease(&self, lease: Lease) -> super::ResumeReservation {
        super::ResumeReservation {
            lease,
            expected_lifecycle_generation: None,
            attach_record: None,
            lifecycle: Arc::clone(&self.lifecycle),
            notify: Arc::clone(&self.notify),
        }
    }
}

/// What a worker's event stream said before it closed.
#[derive(Default)]
struct StreamEnd {
    agent_unresponsive: bool,
    rate_limited: bool,
    startup_failed: bool,
    established: bool,
}

/// One recovery of a runner whose control channel closed, spanning attempts.
#[derive(Clone, Copy)]
struct Recovery {
    loss: ControlLoss,
    attempts: u32,
}

#[derive(Debug)]
enum RestartDecision {
    Respawn(Box<SpawnConfig>),
    BudgetBurned,
    /// An `Attached` worker's connection died. There is no spawn config to
    /// respawn from, and parking would strand an adopted session behind a
    /// banner nobody may be watching, so the handle is dropped and a startup
    /// failure recorded for the reconciler to fresh-spawn from.
    LeaveToReconciler,
    /// The handle was removed (shutdown or delete).
    Gone,
    /// The registry entry was deleted under a live handle (`aoe acp stop|kill`).
    UserStopped,
}

impl<S: BroadcastSink> Drain<S> {
    fn publish(&self, event: Event) {
        publish_next_persisted(
            self.sink.as_ref(),
            &self.next_seqs,
            &self.session_id,
            &event,
        );
    }

    async fn run(mut self, mut lease: Lease, mut inbound: mpsc::Receiver<Event>) {
        loop {
            let end = self.pump(&mut inbound, lease.epoch()).await;
            warn!(
                target: "acp.supervisor",
                session = %self.session_id,
                agent_unresponsive = end.agent_unresponsive,
                "drain channel closed (agent connection task ended); evaluating respawn"
            );
            if end.agent_unresponsive {
                let identity = lock_recover(&self.lifecycle)
                    .running(&self.session_id)
                    .filter(|(current, _)| current == &lease)
                    .and_then(|(_, identity)| identity);
                kill_wedged_runner(&*self.process_control, &self.session_id, identity).await;
            }
            if end.rate_limited {
                info!(
                    target: "acp.supervisor",
                    session = %self.session_id,
                    "rate-limited; dropping worker handle without respawn"
                );
                self.drop_handle(&lease, None).await;
                return;
            }
            if end.startup_failed && self.recovery.is_none() {
                info!(
                    target: "acp.supervisor",
                    session = %self.session_id,
                    "startup failed before a session was established; leaving the retry to the reconciler"
                );
                lock_recover(&self.startup_failures).insert(self.session_id.clone());
                self.drop_handle(&lease, None).await;
                return;
            }
            if !end.agent_unresponsive {
                if let Some(next_inbound) = self.recover_control_loss(&lease, &end).await {
                    inbound = next_inbound;
                    continue;
                }
            }
            // Recovery belongs to this connection and cannot carry into a new lease.
            self.recovery = None;
            let Some(config) = self.approve_respawn(&lease).await else {
                return;
            };
            match self.respawn(&lease, config).await {
                Some((next_lease, next_inbound)) => {
                    lease = next_lease;
                    inbound = next_inbound;
                }
                None => return,
            }
        }
    }

    /// Publish all events until the worker's channel closes. Resume confirmation
    /// is timed inside the client, before it starts committing the replay.
    async fn pump(&mut self, inbound: &mut mpsc::Receiver<Event>, generation: u64) -> StreamEnd {
        let mut end = StreamEnd::default();
        let mut established = false;
        while let Some(event) = inbound.recv().await {
            // A runner stopped on purpose closes its connection with an error that is no
            // failure: not published, and not a startup failure before the session is assigned.
            if matches!(event, Event::AgentStartupError { .. }) && self.restart_intended() {
                continue;
            }
            match &event {
                Event::Stopped { reason } => match reason.as_str() {
                    "agent_unresponsive" | "prompt_orphaned" | "user_forced" => {
                        end.agent_unresponsive = true
                    }
                    "rate_limited" => end.rate_limited = true,
                    "stored_session_rejected" => end.startup_failed = true,
                    _ => {}
                },
                Event::AgentStartupError { .. } if !established => {
                    end.startup_failed = true;
                    // A refused live resume consumes a recovery attempt. Publish the
                    // original loss once only after the bounded recovery is exhausted.
                    if self.recovery.is_some() {
                        continue;
                    }
                }
                Event::AcpSessionAssigned { acp_session_id } => {
                    if let Some(pending) = self.context_reset.take() {
                        publish_with_next_seq(&self.next_seqs, &self.session_id, |seq| {
                            self.sink.publish_from_worker(
                                &self.session_id,
                                seq,
                                &Event::SessionContextReset {
                                    reason: pending.reason,
                                },
                                generation,
                            )
                        });
                        let id = self.session_id.clone();
                        let assigned = acp_session_id.clone();
                        let acknowledged = tokio::task::spawn_blocking(move || {
                            crate::migrations::v033_isolate_sandbox_content::acknowledge_context_reset(
                                &pending.profile,
                                &id,
                                crate::migrations::v033_isolate_sandbox_content::NativeContextView::Structured,
                                generation,
                                &pending.transactions,
                                Some(&assigned),
                            )
                        }).await;
                        let failure = match acknowledged {
                            Ok(Ok(())) => None,
                            Ok(Err(error)) => Some(error.to_string()),
                            Err(error) => Some(error.to_string()),
                        };
                        if let Some(error) = failure {
                            end.startup_failed = true;
                            publish_with_next_seq(&self.next_seqs, &self.session_id, |seq| {
                                self.sink.publish_from_worker(
                                    &self.session_id,
                                    seq,
                                    &Event::AgentStartupError {
                                        message: format!(
                                            "Could not commit the isolated native context: {error}"
                                        ),
                                    },
                                    generation,
                                )
                            });
                            let client = self
                                .workers
                                .lock()
                                .await
                                .get(&self.session_id)
                                .filter(|handle| handle.lease.epoch() == generation)
                                .map(|handle| Arc::clone(&handle.client));
                            if let Some(client) = client {
                                let _ = client.shutdown().await;
                            }
                            return end;
                        }
                    }
                    if self.clear_pending_context_reset() {
                        self.notify.notify_waiters();
                    }
                    established = true;
                    self.recovery = None;
                    let mut workers = self.workers.lock().await;
                    if let Some(handle) = workers.get_mut(&self.session_id) {
                        if handle.lease.epoch() != generation {
                            continue;
                        }
                        handle.native_session_id = Some(acp_session_id.clone());
                        if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                            info!(
                                target: "acp.supervisor",
                                session = %self.session_id,
                                acp_session_id = %acp_session_id,
                                "caching agent-assigned id for future respawn"
                            );
                            spawn_config.stored_acp_session_id = Some(acp_session_id.clone());
                            spawn_config.seed_history_replay = false;
                        }
                    }
                }
                Event::SessionContextReset { reason } => {
                    let mut workers = self.workers.lock().await;
                    if let Some(handle) = workers.get_mut(&self.session_id) {
                        if handle.lease.epoch() != generation {
                            continue;
                        }
                        handle.native_session_id = None;
                        if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                            info!(
                                target: "acp.supervisor",
                                session = %self.session_id,
                                %reason,
                                "clearing cached id and any pending fork after a context reset"
                            );
                            spawn_config.stored_acp_session_id = None;
                            spawn_config.fork_from = None;
                        }
                    }
                }
                _ => {}
            }
            // Tagged so a frame queued by a replaced worker cannot mutate runtime state.
            publish_with_next_seq(&self.next_seqs, &self.session_id, |seq| {
                self.sink
                    .publish_from_worker(&self.session_id, seq, &event, generation)
            });
        }
        end.established = established;
        end
    }

    /// Whether a restart is pending for the runner on record. A marker naming another
    /// generation belongs to a different runner, so a replacement's genuine error passes.
    fn restart_intended(&self) -> bool {
        let Some(marker) = worker_registry::peek_restart_marker(&self.session_id) else {
            return false;
        };
        let generation = worker_registry::load(&self.session_id)
            .ok()
            .flatten()
            .map_or(0, |record| record.generation);
        marker == generation
    }

    fn clear_pending_context_reset(&self) -> bool {
        lock_recover(&self.pending_context_resets).remove(&self.session_id)
    }

    /// Respawn retags its handle before awaiting; replacements never transfer ownership here.
    async fn retain_retiring_handle(&self, lease: &Lease) -> bool {
        let mut workers = self.workers.lock().await;
        if lock_recover(&self.lifecycle)
            .current_lease(&self.session_id)
            .as_ref()
            != Some(lease)
        {
            return false;
        }
        if workers
            .get(&self.session_id)
            .is_some_and(|handle| handle.lease != *lease)
        {
            return false;
        }
        if let Some(handle) = workers.remove(&self.session_id) {
            super::teardown::retain_worker(
                lease,
                handle,
                lease.spawned_runner().or_else(|| lease.previous_runner()),
            );
        }
        true
    }

    /// The pump has stopped; no event follows this settlement.
    async fn settle_retirement(
        &self,
        lease: &Lease,
        settlement: crate::acp::runner_lifecycle::Settlement,
    ) {
        settle_sweeping_if_owed(
            &self.process_control,
            &self.lifecycle,
            &self.notify,
            super::teardown::StopPublication {
                workers: &self.workers,
                sink: &*self.sink,
                seqs: &self.next_seqs,
            },
            lease,
            settlement,
            super::teardown::CompletionPublisher::CurrentDrain,
        )
        .await;
    }

    /// Keep ownership until a native, requested, or unprovable teardown settles.
    async fn drop_handle(&self, lease: &Lease, stopped_reason: Option<&str>) {
        let unproven_record = match worker_registry::load_strict(&self.session_id) {
            Ok(Some(record)) => record.agent_key == "prime-agent" || record.prime_runtime.is_some(),
            Ok(None) => false,
            Err(_) => true,
        };
        let mut guard = self.workers.lock().await;
        let requires_settlement = unproven_record || stopped_reason.is_some() || worker_registry::stop_requested(&self.session_id)
                || guard.get(&self.session_id).is_some_and(|handle| matches!(&handle.kind, WorkerKind::Runner { spawn_config } if spawn_config.uses_native_prime()));
        let dropped = if requires_settlement {
            let decision = {
                let mut lifecycle = lock_recover(&self.lifecycle);
                if !lifecycle
                    .running(&self.session_id)
                    .is_some_and(|(current, _)| &current == lease)
                {
                    return;
                }
                if guard
                    .get(&self.session_id)
                    .is_some_and(|handle| handle.lease != *lease)
                {
                    return;
                }
                match stopped_reason.or_else(|| {
                    worker_registry::stop_requested(&self.session_id).then_some("user_stopped")
                }) {
                    Some(reason) => lifecycle.begin_stop(&self.session_id, reason, false),
                    None => lifecycle.begin_connection_retirement(lease),
                }
            };
            let crate::acp::runner_lifecycle::StopDecision::TearDown { lease, identity } = decision
            else {
                return;
            };
            if let Some(handle) = guard.remove(&self.session_id) {
                super::teardown::retain_worker(&lease, handle, identity);
            }
            drop(guard);
            super::teardown::retire_registered_clients(&self.lifecycle, &lease).await;
            let settlement =
                super::teardown::tear_down_lease(&*self.process_control, &lease, false).await;
            self.settle_retirement(&lease, settlement).await;
            true
        } else {
            // Non-destructive park remains the old synchronous fold-and-release path.
            let current = lock_recover(&self.lifecycle)
                .running(&self.session_id)
                .is_some_and(|(running, _)| &running == lease)
                && guard
                    .get(&self.session_id)
                    .is_none_or(|handle| handle.lease == *lease);
            if current {
                guard.remove(&self.session_id);
                self.detach_stopped_worker_work();
                lock_recover(&self.lifecycle).release_running(lease);
            }
            drop(guard);
            current
        };
        if dropped && self.clear_pending_context_reset() {
            self.notify.notify_waiters();
        }
    }

    /// The closed worker's tailer and task tracking died with it, so the
    /// sub-agents and tasks it left open will never report their end.
    fn detach_stopped_worker_work(&self) {
        super::publish::detach_orphaned_background_agents_on(
            &*self.sink,
            &self.next_seqs,
            &self.session_id,
            "the worker that was tracking this sub-agent stopped; tracking stopped",
        );
    }

    /// A runner that closed only its control channel still owns its agent and
    /// turn, so dial it again under the same lease rather than replace it.
    /// Reports the loss when the worker cannot be recovered that way.
    async fn recover_control_loss(
        &mut self,
        lease: &Lease,
        end: &StreamEnd,
    ) -> Option<mpsc::Receiver<Event>> {
        // Take before fallible lookups so early exits cannot retain recovery state.
        let recovery = self.recovery.take().filter(|_| !end.established);
        let (client, observed) = {
            let workers = self.workers.lock().await;
            let handle = workers
                .get(&self.session_id)
                .filter(|handle| handle.lease == *lease)?;
            if matches!(&handle.kind, WorkerKind::Runner { spawn_config } if spawn_config.uses_native_prime())
            {
                return None;
            }
            (Arc::clone(&handle.client), handle.client.control_loss())
        };
        // A failed attempt continues its recovery; anything else ends it.
        let (loss, attempts) = match recovery {
            Some(recovery) if end.startup_failed || observed.is_some() => {
                (recovery.loss, recovery.attempts)
            }
            Some(recovery) => {
                self.report_control_loss(&client, recovery.loss);
                return None;
            }
            None => (observed?, 0),
        };
        let reattached = self
            .reattach_same_runner(lease, &client, loss, attempts)
            .await;
        match reattached {
            Some((inbound, recovery)) => {
                self.recovery = Some(recovery);
                Some(inbound)
            }
            None => {
                self.report_control_loss(&client, loss);
                None
            }
        }
    }

    fn report_control_loss(&self, client: &AcpClient, loss: ControlLoss) {
        if worker_registry::stop_requested(&self.session_id) {
            return;
        }
        // A completion published before the channel closed already ended the turn.
        if client.claim_lost_turn(loss) {
            self.publish(Event::Stopped {
                reason: "runner_control_lost".into(),
            });
        }
        self.publish(Event::AgentStartupError {
            message: "ACP connection failed: runner control channel closed".into(),
        });
    }

    /// Dials the runner until one attempt is installed. The client bounds the
    /// wait for resume confirmation while the drain publishes its replay.
    async fn reattach_same_runner(
        &self,
        lease: &Lease,
        lost: &Arc<AcpClient>,
        loss: ControlLoss,
        attempts: u32,
    ) -> Option<(mpsc::Receiver<Event>, Recovery)> {
        let session_id = &self.session_id;
        if lease.prime() != PrimeOwnership::NotPrime {
            return None;
        }
        let identity = lock_recover(&self.lifecycle)
            .running(session_id)
            .filter(|(current, _)| current == lease)
            .and_then(|(_, identity)| identity)
            .filter(|identity| identity.birth_fingerprint.is_some())?;
        for attempt in attempts + 1..=CONTROL_REATTACH_ATTEMPTS {
            if attempt > 1 {
                tokio::time::sleep(RESPAWN_BACKOFF).await;
            }
            if worker_registry::stop_requested(session_id) {
                return None;
            }
            let record = match worker_registry::load_strict(session_id) {
                Ok(Some(record))
                    if identity.owns_record(&record)
                        && worker_registry::is_record_live(&record) =>
                {
                    record
                }
                _ => {
                    info!(
                        target: "acp.supervisor",
                        session = %session_id,
                        pid = identity.pid,
                        "runner that lost its control channel is gone or replaced; not reattaching"
                    );
                    return None;
                }
            };
            let stored_acp_session_id = record.stored_acp_session_id?;
            // Charge only a verified live runner, so a dead one pays just its respawn.
            if attempt == 1 && !self.admit_reattach(lease).await {
                return None;
            }
            let resume_by = tokio::time::Instant::now() + CONTROL_REATTACH_DEADLINE;
            let attempt_result = tokio::time::timeout_at(
                resume_by,
                lost.reattach(
                    stored_acp_session_id,
                    loss.turn_in_flight,
                    resume_by,
                    lease.clone(),
                ),
            )
            .await
            .unwrap_or(Err(AcpError::AttachTimedOut));
            match attempt_result {
                Ok(next) => {
                    let inbound = self
                        .install_reattached(lease, identity, lost, next, loss)
                        .await?;
                    let recovery = Recovery {
                        loss,
                        attempts: attempt,
                    };
                    return Some((inbound, recovery));
                }
                Err(error) => warn!(
                    target: "acp.supervisor",
                    session = %session_id,
                    attempt,
                    %error,
                    "reattach after control channel loss failed"
                ),
            }
        }
        None
    }

    /// Reattaches share the respawn budget, so a channel that keeps closing
    /// ends in the ordinary park.
    async fn admit_reattach(&self, lease: &Lease) -> bool {
        let mut workers = self.workers.lock().await;
        let Some(handle) = workers
            .get_mut(&self.session_id)
            .filter(|handle| handle.lease == *lease)
        else {
            return false;
        };
        let now = Instant::now();
        handle
            .restart_history
            .retain(|t| *t >= now - RESTART_WINDOW);
        if handle.restart_history.len() as u32 >= MAX_RESPAWNS_IN_WINDOW {
            return false;
        }
        handle.restart_history.push(now);
        true
    }

    /// Installs a reattached client only if the lease and runner identity are
    /// still current and no stop was requested; otherwise releases it.
    async fn install_reattached(
        &self,
        lease: &Lease,
        identity: RunnerIdentity,
        lost: &Arc<AcpClient>,
        mut client: AcpClient,
        loss: ControlLoss,
    ) -> Option<mpsc::Receiver<Event>> {
        let session_id = &self.session_id;
        let inbound = client.take_inbound()?;
        let client = Arc::new(client);
        lease.retirement().replace_client(lost, &client);
        let installed = {
            let mut workers = self.workers.lock().await;
            let current = !worker_registry::stop_requested(session_id)
                && lock_recover(&self.lifecycle)
                    .running(session_id)
                    .is_some_and(|(current, running)| {
                        current == *lease && running == Some(identity)
                    });
            match workers
                .get_mut(session_id)
                .filter(|handle| current && handle.lease == *lease)
            {
                Some(handle) => {
                    handle.client = Arc::clone(&client);
                    true
                }
                None => false,
            }
        };
        if !installed {
            // Whatever won owns the runner; release only this channel.
            let _ = client.shutdown().await;
            return None;
        }
        // The runner settled the lost channel's reverse calls, and the lost
        // connection's tailers ended with it.
        super::publish::cancel_orphaned_requests_on(&*self.sink, &self.next_seqs, session_id);
        let resumable = super::publish::collect_resumable_background_agent_launches(
            &*self.sink,
            &self.next_seqs,
            session_id,
        );
        let open_tasks = self.sink.unresolved_background_task_launches(session_id);
        if !resumable.is_empty() {
            let _ = client.resume_background_tailing(resumable).await;
        }
        if !open_tasks.is_empty() {
            let _ = client.resume_task_tailing(open_tasks).await;
        }
        info!(
            target: "acp.supervisor",
            session = %session_id,
            pid = identity.pid,
            turn_in_flight = loss.turn_in_flight,
            "reattached to the same runner after its control channel closed"
        );
        Some(inbound)
    }

    /// Decide whether the closed worker respawns, publishing why when it does not.
    async fn approve_respawn(&self, lease: &Lease) -> Option<SpawnConfig> {
        let session_id = &self.session_id;
        match restart_decision(&self.workers, session_id).await {
            RestartDecision::Respawn(config) => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    command = %config.spec.command,
                    stored_id = ?config.stored_acp_session_id,
                    "respawn approved; sleeping {}ms before restart",
                    RESPAWN_BACKOFF.as_millis()
                );
                return Some(*config);
            }
            RestartDecision::BudgetBurned => {
                warn!(
                    target: "acp.supervisor",
                    session = %session_id,
                    max_respawns = MAX_RESPAWNS_IN_WINDOW,
                    window_secs = RESTART_WINDOW.as_secs(),
                    "restart budget burned; parking session"
                );
                self.publish(Event::AgentStartupError {
                    message: format!(
                        "ACP agent crashed more than {} times in {}s; \
                             not respawning. Use the web dashboard to retry.",
                        MAX_RESPAWNS_IN_WINDOW,
                        RESTART_WINDOW.as_secs()
                    ),
                });
            }
            RestartDecision::LeaveToReconciler => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "attached worker connection died; leaving the fresh spawn to the reconciler"
                );
                lock_recover(&self.startup_failures).insert(session_id.clone());
            }
            RestartDecision::Gone => return None,
            RestartDecision::UserStopped => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "worker stop requested; settling its handle without respawn"
                );
                self.drop_handle(lease, Some("user_stopped")).await;
                return None;
            }
        }
        self.drop_handle(lease, None).await;
        None
    }

    /// Relaunch the worker under a fresh respawn epoch; returns the new lease
    /// and event stream once it is installed.
    async fn respawn(
        &self,
        lease: &Lease,
        mut config: SpawnConfig,
    ) -> Option<(Lease, mpsc::Receiver<Event>)> {
        let session_id = &self.session_id;
        let original_prime = lease.prime();
        // Retag the existing handle while both admission gates are held.
        // This is still the same drain/client; launch_epoch remains unchanged.
        let begun = {
            let mut workers = self.workers.lock().await;
            if workers
                .get(session_id)
                .is_none_or(|handle| handle.lease != *lease)
            {
                return None;
            }
            let begun = lock_recover(&self.lifecycle).begin_respawn(lease);
            if let Ok((next, _)) = &begun {
                workers
                    .get_mut(session_id)
                    .expect("matching handle checked above")
                    .lease = next.clone();
            }
            begun
        };
        let Ok((respawn_lease, mut previous)) = begun else {
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                "respawn skipped; the session's lease moved on"
            );
            return None;
        };
        let reservation = self.reservation_for_lease(respawn_lease.clone());
        config.generation = respawn_lease.epoch();
        config.launch_lease = Some(respawn_lease.clone());

        tokio::select! {
            biased;
            _ = tokio::time::sleep(RESPAWN_BACKOFF) => {}
            _ = reservation.cancellation().cancelled() => {
                if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease) {
                    self.finish_cancelled(&respawn_lease, previous).await;
                }
                return None;
            }
        }
        if worker_registry::stop_requested(session_id) {
            lock_recover(&self.lifecycle).begin_stop(session_id, "user_stopped", false);
        }
        let cancelled = lock_recover(&self.lifecycle).cancel_requested(&respawn_lease);
        if cancelled.is_some() {
            if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease) {
                self.finish_cancelled(&respawn_lease, previous).await;
            }
            return None;
        }

        if original_prime != crate::acp::runner_lifecycle::PrimeOwnership::NotPrime {
            if previous.is_some() {
                respawn_lease.owe_previous_retirement();
            }
            let mut settlement = match previous {
                Some(identity) if identity.birth_fingerprint.is_none() => {
                    crate::acp::runner_lifecycle::Settlement::Unproven(Some(identity))
                }
                _ => tear_down_runner(&*self.process_control, session_id, previous).await,
            };
            if settlement == crate::acp::runner_lifecycle::Settlement::Proven
                && !matches!(
                    tokio::task::spawn_blocking(move || original_prime.retire()).await,
                    Ok(Ok(()))
                )
            {
                settlement = crate::acp::runner_lifecycle::Settlement::Unproven(previous);
            }
            if !matches!(settlement, crate::acp::runner_lifecycle::Settlement::Proven) {
                if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease) {
                    if !self.retain_retiring_handle(&respawn_lease).await {
                        return None;
                    }
                    respawn_lease.owe_stop_completion(None);
                    self.publish(Event::AgentStartupError {
                        message:
                            "Prime native termination is unproven; replacement remains blocked"
                                .into(),
                    });
                    self.settle_retirement(&respawn_lease, settlement).await;
                }
                return None;
            }
            previous = None;
        }
        respawn_lease.record_prime(crate::acp::runner_lifecycle::PrimeOwnership::NotPrime);
        let refreshed = await_resume_to_completion(
            reservation.cancellation(),
            Self::refresh_launch_env(&self.session_id, &mut config),
        )
        .await
        .is_ok();
        self.refuse_if_dismissed(&config).await;
        let cancelled = lock_recover(&self.lifecycle).cancel_requested(&respawn_lease);
        if !refreshed || cancelled.is_some() {
            if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease) {
                self.finish_cancelled(&respawn_lease, previous).await;
            }
            return None;
        }
        if let Some((wrapper, base)) = &config.wrapper_substitution {
            log_wrapper_substitution(session_id, &config.tool, wrapper, base);
        }
        // Like spawn_inner, keep the cooperative handshake result so a
        // concurrent permanent stop can retire its adapter-side session.
        let launched = (self.launcher)(config.clone(), AcpSessionId(session_id.clone())).await;
        let mut client = match launched {
            Ok(client) => client,
            Err(e) => {
                self.refuse_if_dismissed(&config).await;
                self.fail_launch(&respawn_lease, previous, &config, e).await;
                return None;
            }
        };
        let Some(inbound) = client.take_inbound() else {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                "respawned client missing inbound receiver; parking",
            );
            let client = Arc::new(client);
            respawn_lease.retirement().record_client(&client, false);
            self.refuse_if_dismissed(&config).await;
            self.fail_launch(
                &respawn_lease,
                previous,
                &config,
                AcpError::Spawn("respawned ACP client had no inbound channel".into()),
            )
            .await;
            return None;
        };
        let client = Arc::new(client);
        respawn_lease.retirement().record_client(&client, false);
        if !respawn_lease.attest_runner().await {
            self.refuse_if_dismissed(&config).await;
            self.fail_launch(
                &respawn_lease,
                previous,
                &config,
                AcpError::Spawn(
                    "original child birth attestation unavailable; retirement remains owned".into(),
                ),
            )
            .await;
            return None;
        }
        let identity = client.runner_pid().map(|pid| {
            respawn_lease
                .spawned_runner()
                .or_else(|| {
                    worker_registry::load(session_id)
                        .ok()
                        .flatten()
                        .filter(|record| {
                            record.pid == pid && record.generation == respawn_lease.epoch()
                        })
                        .map(|record| RunnerIdentity::from_record(&record))
                })
                .unwrap_or_else(|| RunnerIdentity::unattested(pid, respawn_lease.epoch()))
        });

        self.refuse_if_dismissed(&config).await;
        let refused = {
            let mut guard = self.workers.lock().await;
            if worker_registry::stop_requested(session_id) {
                lock_recover(&self.lifecycle).begin_stop(session_id, "user_stopped", false);
            }
            let mut install = lock_recover(&self.lifecycle).install(&respawn_lease, identity);
            if install.is_ok() && worker_registry::stop_requested(session_id) {
                lock_recover(&self.lifecycle).begin_stop(session_id, "user_stopped", false);
                install = Err(InstallError::Cancelled {
                    reason: "user_stopped".into(),
                });
            }
            match install {
                Ok(()) => match guard.get_mut(session_id) {
                    Some(handle) => {
                        handle.client = Arc::clone(&client);
                        handle.lease = respawn_lease.clone();
                        handle.native_session_id = None;
                        None
                    }
                    None => Some(InstallError::Stale),
                },
                Err(refusal) => Some(refusal),
            }
        };
        match refused {
            None => {}
            Some(InstallError::Cancelled { .. }) => {
                super::teardown::retire_client(&self.lifecycle, &respawn_lease, &client, identity)
                    .await;
                self.finish_cancelled(&respawn_lease, previous).await;
                return None;
            }
            Some(InstallError::Stale) => {
                warn!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "respawn completed under a stale lease; tearing the runner down"
                );
                super::teardown::retire_client(&self.lifecycle, &respawn_lease, &client, identity)
                    .await;
                let converted = lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease);
                if converted {
                    lock_recover(&self.lifecycle).owe_sweep(&respawn_lease);
                    if !self.retain_retiring_handle(&respawn_lease).await {
                        return None;
                    }
                }
                let settlement =
                    super::teardown::tear_down_lease(&*self.process_control, &respawn_lease, false)
                        .await;
                self.settle_retirement(&respawn_lease, settlement).await;
                return None;
            }
        }
        drop(reservation);

        // The respawned client starts with an empty `pending_responders` and
        // no tailer is respawned on replay, so requests and background
        // sub-agents still unresolved in the log are orphaned by the crashed
        // worker this replaces.
        super::publish::cancel_orphaned_requests_on(&*self.sink, &self.next_seqs, session_id);
        super::publish::detach_orphaned_background_agents_on(
            &*self.sink,
            &self.next_seqs,
            session_id,
            super::publish::WORKER_REPLACED_DETACH_WARNING,
        );
        info!(
            target: "acp.supervisor",
            session = %session_id,
            "structured view worker respawned"
        );
        lock_recover(&self.incompatible_binaries).remove(session_id);
        lock_recover(&self.respawned_in_place).insert(session_id.clone());
        Some((respawn_lease, inbound))
    }

    /// Re-resolve what may have changed since the first launch: model pins,
    /// host hook env, and MCP servers.
    async fn refresh_launch_env(session_id: &str, config: &mut SpawnConfig) {
        let agent = config.agent_key.clone();
        let profile = config.source_profile.clone().unwrap_or_default();
        let config_path = config.config_path.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                &profile,
                &config_path,
            )
        })
        .await;
        match &resolved {
            Ok(cfg) => refresh_spawn_model_effort(config, cfg.acp.acp_defaults_for(&agent)),
            Err(e) => warn!(
                target: "acp.supervisor",
                session = %session_id,
                error = %e,
                "model re-resolution on respawn failed; keeping the cached model"
            ),
        }

        let (target, native) = config.wrapper_substitution.as_ref().map_or(
            (config.agent_key.as_str(), config.agent_key.as_str()),
            |(wrapper, base)| (wrapper.as_str(), base.as_str()),
        );
        let mut claude_config_dir = None;
        if config.sandbox_info.is_none() {
            let mut host_environment = config.base_host_environment.clone();
            let minted = before_session_env(
                session_id,
                &config.tool,
                config.source_profile.clone().unwrap_or_default(),
                config.cwd.clone(),
            )
            .await;
            match minted {
                Ok(Ok(pairs)) => overlay_env(&mut host_environment, pairs),
                Ok(Err(e)) => {
                    host_environment = config.host_environment.clone();
                    warn!(
                        target: "acp.supervisor",
                        session = %session_id,
                        error = %e,
                        "before_session hook failed on respawn; reusing the last known environment"
                    )
                }
                Err(e) => {
                    host_environment = config.host_environment.clone();
                    warn!(
                        target: "acp.supervisor",
                        session = %session_id,
                        error = %e,
                        "before_session hook task failed on respawn; reusing the last known environment"
                    )
                }
            }
            claude_config_dir =
                apply_claude_store_pin(&mut host_environment, config.claude_store_pin.as_ref())
                    .or_else(|| {
                        let cfg = resolved.as_ref().ok()?;
                        apply_declared_config_dir(
                            &mut host_environment,
                            cfg,
                            &config.tool,
                            target,
                            native,
                        )
                    });
            config.host_environment = host_environment;
        }

        config.mcp_servers = resolve_mcp_servers(
            discovery_key(config.sandbox_info.is_some(), &config.agent_key, native),
            session_id,
            config.source_profile.clone(),
            McpPaths {
                cwd: config.cwd.clone(),
                config_path: config.config_path.clone(),
            },
            config.host_environment.clone(),
            claude_config_dir,
            config
                .sandbox_info
                .as_ref()
                .is_some_and(|info| info.enabled),
            "MCP re-resolution on respawn failed",
        )
        .await;
    }

    /// Route an archived, trashed or purged row through the stop path, so the cancellation retires
    /// the replacement and the runner it replaces.
    async fn refuse_if_dismissed(&self, config: &SpawnConfig) {
        let refusal =
            match admit_stored_row(config.source_profile.as_deref(), &self.session_id).await {
                Ok(()) => return,
                Err(SupervisorError::Blocked(blocked)) => blocked.code(),
                Err(SupervisorError::SessionGone(_)) => "session_gone",
                Err(error) => {
                    warn!(
                        target: "acp.supervisor",
                        session = %self.session_id,
                        error = %error,
                        "durable admission check failed on respawn; continuing without it"
                    );
                    return;
                }
            };
        lock_recover(&self.lifecycle).begin_stop(&self.session_id, refusal, false);
    }

    /// Report a failed relaunch and retire whatever runner this epoch left.
    async fn fail_launch(
        &self,
        respawn_lease: &Lease,
        previous: Option<RunnerIdentity>,
        config: &SpawnConfig,
        e: AcpError,
    ) {
        let session_id = &self.session_id;
        let launched = respawn_lease.spawned_runner().or_else(|| {
            worker_registry::load(session_id)
                .ok()
                .flatten()
                .filter(|record| record.generation == respawn_lease.epoch())
                .map(|r| RunnerIdentity::from_record(&r))
        });
        if respawn_lease.spawned_runner().is_none() {
            if let Some(launched) = launched {
                respawn_lease.record_runner(launched);
            }
        }
        if !self.retain_retiring_handle(respawn_lease).await {
            return;
        }
        // One lock, after the await: a stop recorded while this waited for
        // `workers` must not be lost to a conversion that drops it.
        let (cancelled, converted) = {
            let mut table = lock_recover(&self.lifecycle);
            let cancelled = table.cancel_requested(respawn_lease);
            let convert = launched.is_some() || cancelled.is_some();
            (
                cancelled,
                convert && table.convert_to_stopping(respawn_lease),
            )
        };
        if cancelled.is_some() {
            info!(
                target: "acp.supervisor",
                session = %session_id,
                "respawn launch failed under a pending stop: {e}"
            );
        } else {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                "respawn failed: {e}"
            );
            if matches!(e, AcpError::IncompatibleAgent(_)) {
                lock_recover(&self.incompatible_binaries)
                    .insert(session_id.clone(), config.spec.command.clone());
            }
            if !publish_rejection(&e, |event| self.publish(event)) {
                self.publish(Event::AgentStartupError {
                    message: format!("ACP agent respawn failed: {e}"),
                });
            }
        }
        if converted {
            // Under a pending stop the replaced runner is retired as well.
            respawn_lease.owe_stop_completion(None);
            if previous.is_some() && cancelled.is_some() {
                respawn_lease.owe_previous_retirement();
            }
            super::teardown::retire_registered_clients(&self.lifecycle, respawn_lease).await;
            let settlement =
                super::teardown::tear_down_lease(&*self.process_control, respawn_lease, false)
                    .await;
            self.settle_retirement(respawn_lease, settlement).await;
        }
    }

    /// Honor a stop that raced the respawn: retire the replacement and the runner it replaced.
    async fn finish_cancelled(&self, respawn_lease: &Lease, previous: Option<RunnerIdentity>) {
        if previous.is_some() {
            respawn_lease.owe_previous_retirement();
        }
        if !self.retain_retiring_handle(respawn_lease).await {
            return;
        }
        super::teardown::retire_registered_clients(&self.lifecycle, respawn_lease).await;
        let settlement =
            super::teardown::tear_down_lease(&*self.process_control, respawn_lease, false).await;
        self.settle_retirement(respawn_lease, settlement).await;
    }
}

async fn restart_decision(workers: &Workers, session_id: &str) -> RestartDecision {
    let mut guard = workers.lock().await;
    let Some(handle) = guard.get_mut(session_id) else {
        debug!(
            target: "acp.supervisor",
            session = %session_id,
            "restart_decision: worker entry gone (shutdown / delete)"
        );
        return RestartDecision::Gone;
    };
    let runner_managed = matches!(
        handle.kind,
        WorkerKind::Runner { .. } | WorkerKind::Attached
    );
    if runner_managed
        && (worker_registry::stop_requested(session_id)
            || matches!(worker_registry::load(session_id), Ok(None)))
    {
        debug!(
            target: "acp.supervisor",
            session = %session_id,
            "restart_decision: registry entry gone, treating as user-initiated stop"
        );
        return RestartDecision::UserStopped;
    }
    let now = Instant::now();
    let pre_count = handle.restart_history.len();
    handle
        .restart_history
        .retain(|t| *t >= now - RESTART_WINDOW);
    let pruned = pre_count - handle.restart_history.len();
    handle.restart_history.push(now);
    let count = handle.restart_history.len() as u32;
    debug!(
        target: "acp.supervisor",
        session = %session_id,
        respawns_in_window = count,
        max_in_window = MAX_RESPAWNS_IN_WINDOW,
        window_secs = RESTART_WINDOW.as_secs(),
        pruned_old_entries = pruned,
        "restart_decision: tallied recent crashes"
    );
    match &handle.kind {
        _ if count > MAX_RESPAWNS_IN_WINDOW => RestartDecision::BudgetBurned,
        WorkerKind::Runner { spawn_config } => RestartDecision::Respawn(spawn_config.clone()),
        // Attached: the previous daemon owned the runner and there is no spawn
        // config here, so hand the session to the reconciler rather than park
        // an adopted worker behind a banner nobody may be watching.
        WorkerKind::Attached => RestartDecision::LeaveToReconciler,
        // In-proc test fixture with no subprocess to respawn.
        #[cfg(test)]
        WorkerKind::Stdio => RestartDecision::BudgetBurned,
    }
}

/// Fence only the runner belonging to the ended connection.
async fn kill_wedged_runner(
    control: &dyn ProcessControl,
    session_id: &str,
    identity: Option<RunnerIdentity>,
) {
    let Some(identity) = identity else {
        return;
    };
    control.terminate_group(identity);
    wait_for_exit(control, identity, Duration::from_secs(3)).await;
    if control.is_alive(identity) {
        control.kill_group(identity);
        wait_for_exit(control, identity, Duration::from_millis(200)).await;
    }
    if control.is_alive(identity) {
        return;
    }
    if worker_registry::load(session_id)
        .ok()
        .flatten()
        .is_some_and(|record| identity.owns_record(&record))
    {
        if let Ok(socket_path) = worker_registry::socket_path_for(session_id) {
            let _ = std::fs::remove_file(socket_path);
        }
    }
    control.forget(identity);
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::daemon::AcpWorkerState;

    #[tokio::test]
    #[serial_test::serial]
    async fn a_durable_cli_stop_before_disconnect_prevents_automatic_respawn() {
        let (_home, _tmp) = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242);
        let sup = Supervisor::new(VecSink::new())
            .with_process_control(control.clone())
            .with_launcher(Arc::new(|_, _| {
                Box::pin(async { panic!("a stopped worker must not respawn") })
            }));
        let id = "cli-stop-disconnect";
        save_attested_test_record(id, 4242, 0);
        let record = worker_registry::load(id).unwrap().unwrap();
        let lease = sup
            .test_install_runner(
                id,
                runner_config(worker_registry::socket_path_for(id).unwrap()),
                Some(RunnerIdentity::from_record(&record)),
            )
            .await;
        lease.record_scope(record.scope.clone());
        let intent = worker_registry::request_stop(&record).unwrap();
        let (tx, rx) = mpsc::channel(4);
        let drain = sup.start_drain_task(id.into(), lease, rx, None);
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(sup.worker_state(id).await, AcpWorkerState::Absent);
        assert!(worker_registry::load(id).unwrap().is_none());
        assert_eq!(worker_registry::stop_intent(id).unwrap(), Some(intent));
        assert!(sup
            .begin_resume(
                id,
                crate::acp::runner_lifecycle::ResumeKind::Attach,
                worker_registry::ResumeAuthority::Automatic
            )
            .await
            .is_err());
        assert!(control.signals().contains(&(4242, "TERM")));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_ownership_without_a_cleanup_route_blocks_replacement_and_release() {
        for attached in [false, true] {
            let (_home, _tmp) = isolate_home();
            let control =
                Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
            control.alive(4242);
            let sup = Supervisor::new(VecSink::new())
                .with_process_control(control)
                .with_launcher(Arc::new(|_, _| {
                    Box::pin(async {
                        panic!("replacement launched before native ownership was settled")
                    })
                }));
            let id = "prime-unproven";
            save_record(id, 4242, 0);
            let mut record = worker_registry::load(id).unwrap().unwrap();
            record.agent_key = "prime-agent".into();
            worker_registry::save(&record).unwrap();
            let identity = Some(RunnerIdentity::unattested(4242, 0));
            let lease = if attached {
                let (client, _tx) =
                    crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId(id.into()));
                sup.test_install_handle(id, client, WorkerKind::Attached, identity)
                    .await
            } else {
                let mut config = runner_config(worker_registry::socket_path_for(id).unwrap());
                config.agent_key = "prime-agent".into();
                sup.test_install_runner(id, config, identity).await
            };
            let (tx, rx) = mpsc::channel(4);
            let drain = sup.start_drain_task(id.into(), lease, rx, None);
            tx.send(Event::AcpSessionAssigned {
                acp_session_id: "native-context".into(),
            })
            .await
            .unwrap();
            drop(tx);
            tokio::time::timeout(Duration::from_secs(10), drain)
                .await
                .unwrap()
                .unwrap();
            let retained = worker_registry::load(id).unwrap().unwrap();
            assert_eq!((retained.pid, retained.generation), (4242, 0));
            let mut lifecycle = lock_recover(&sup.lifecycle);
            assert!(matches!(
                lifecycle.admit(id, crate::acp::runner_lifecycle::ResumeKind::Spawn),
                Err(crate::acp::runner_lifecycle::AdmitError::TeardownPending)
            ));
            assert_eq!(lifecycle.occupied_slots(), 1);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unattested_native_drain_retains_replacement_fencing() {
        use crate::acp::runner_lifecycle::WorkerPhase;

        for key in ["prime-agent", "audit-prime"] {
            let _home = isolate_home();
            let control =
                Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
            let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
            let lifecycle = Arc::clone(&sup.lifecycle);
            control.probe_phase(move |id| lock_recover(&lifecycle).phase(id));
            let id = "native-drop-sweep";
            let mut config = runner_config(worker_registry::socket_path_for(id).unwrap());
            config.agent_key = key.into();
            if key != "prime-agent" {
                config.wrapper_substitution = Some((key.into(), "prime-agent".into()));
            }
            let lease = sup.test_install_runner(id, config, None).await;
            let (tx, rx) = mpsc::channel(4);
            let drain = sup.start_drain_task(id.into(), lease, rx, None);
            tx.send(Event::Stopped {
                reason: "rate_limited".into(),
            })
            .await
            .unwrap();
            drop(tx);
            tokio::time::timeout(Duration::from_secs(5), drain)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                control.releases(),
                [(id.into(), Some(WorkerPhase::Stopping))]
            );
            let mut lifecycle = lock_recover(&sup.lifecycle);
            assert!(matches!(
                lifecycle.admit(id, crate::acp::runner_lifecycle::ResumeKind::Spawn),
                Err(crate::acp::runner_lifecycle::AdmitError::TeardownPending)
            ));
            assert_eq!(lifecycle.occupied_slots(), 1);
        }
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn native_respawn_retains_unresolved_ownership_when_its_record_disappears() {
        let _home = isolate_home();
        let id = "native-respawn-missing-route";
        let socket = worker_registry::socket_path_for(id).unwrap();
        let mut record = worker_record(id, 4242, socket.clone());
        record.agent_key = "prime-agent".into();
        worker_registry::save(&record).unwrap();
        let (launcher, _launches) = crate::server::test_support::counting_failing_launcher();
        let sup = Supervisor::new(VecSink::new())
            .with_process_control(Arc::new(
                crate::acp::runner_lifecycle::test_support::FakeProcessControl::default(),
            ))
            .with_launcher(launcher);
        let mut config = runner_config(socket);
        config.agent_key = "prime-agent".into();
        let lease = sup.test_install_runner(id, config, None).await;
        assert_eq!(sup.worker_state(id).await, AcpWorkerState::Running);
        let (sender, inbound) = mpsc::channel(4);
        let drain = sup.start_drain_task(id.into(), lease, inbound, None);
        drop(sender);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while sup.worker_state(id).await != AcpWorkerState::Resuming {
            assert!(
                !drain.is_finished() && std::time::Instant::now() < deadline,
                "the live record must admit a respawn before its backoff"
            );
            tokio::task::yield_now().await;
        }
        // Paused time keeps the approved respawn in backoff until the route disappears.
        std::fs::remove_file(worker_registry::record_path(id).unwrap()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("respawn drain must reach its terminal ownership decision")
            .unwrap();
        assert!(sup.is_owned(id).await);
        assert!(matches!(
            sup.reserve_spawn(
                id,
                super::super::ResumeKind::Spawn,
                worker_registry::ResumeAuthority::Explicit
            )
            .await,
            Err(super::super::SupervisorError::TeardownPending(_))
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_decision_burns_the_budget_and_detects_user_stops() {
        let (_home, tmp) = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let socket = tmp.path().join("budget.sock");
        worker_registry::save(&worker_record("s-1", std::process::id(), socket.clone())).unwrap();
        sup.test_install_runner("s-1", runner_config(socket.clone()), None)
            .await;
        sup.test_install_runner("s-stop", runner_config(socket), None)
            .await;

        for i in 0..MAX_RESPAWNS_IN_WINDOW {
            assert!(
                matches!(
                    restart_decision(&sup.workers, "s-1").await,
                    RestartDecision::Respawn(_)
                ),
                "decision #{i} should be Respawn",
            );
        }
        assert!(matches!(
            restart_decision(&sup.workers, "s-1").await,
            RestartDecision::BudgetBurned
        ));
        let decision = restart_decision(&sup.workers, "s-stop").await;
        assert!(
            matches!(decision, RestartDecision::UserStopped),
            "no registry entry means a user stop, got {decision:?}"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_live_model_config_pick_reaches_the_respawn_config() {
        let (_home, tmp) = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let socket = tmp.path().join("pick.sock");
        worker_registry::save(&worker_record("s-pick", std::process::id(), socket.clone()))
            .unwrap();
        sup.test_install_runner("s-pick", runner_config(socket), None)
            .await;

        sup.refresh_cached_model_config("s-pick", "ultracode", "on")
            .await;

        let RestartDecision::Respawn(config) = restart_decision(&sup.workers, "s-pick").await
        else {
            panic!("a runner worker respawns");
        };
        assert_eq!(
            config
                .default_model_config
                .get("ultracode")
                .map(String::as_str),
            Some("on")
        );
    }

    /// An `Attached` worker (reattached to a runner a previous daemon owned)
    /// has no spawn config to respawn from. A crash after the session was
    /// established must hand the session to the reconciler rather than park it
    /// behind the crash-loop banner. With the registry entry gone the same
    /// crash is a user stop (`aoe acp stop|kill`) and must not re-arm.
    #[tokio::test]
    #[serial_test::serial]
    async fn drain_rearms_an_attached_crash_for_the_reconciler() {
        // (session, registry record saved, handed to the reconciler)
        for (id, registry_saved, expect_rearm) in [
            ("s-attach-rearm", true, true),
            ("s-attach-user-stop", false, false),
        ] {
            let (_home, tmp) = isolate_home();
            let sink = VecSink::new();
            let sup = Supervisor::new(sink.clone());
            if registry_saved {
                // A live record keeps `restart_decision` past the user-stop
                // gate, so the attached handoff is the arm exercised.
                worker_registry::save(&worker_record(
                    id,
                    std::process::id(),
                    tmp.path().join("attach.sock"),
                ))
                .unwrap();
            }
            let (client, _client_tx) = crate::acp::acp_client::AcpClient::fake_for_test(
                crate::acp::state::AcpSessionId(id.into()),
            );
            let lease = sup
                .test_install_handle(id, client, WorkerKind::Attached, None)
                .await;
            let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(16);
            let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
            inbound_tx
                .send(Event::AcpSessionAssigned {
                    acp_session_id: "acp-1".into(),
                })
                .await
                .unwrap();
            inbound_tx
                .send(Event::AgentStartupError {
                    message: "ACP connection failed: native binary failed to launch".into(),
                })
                .await
                .unwrap();
            drop(inbound_tx);
            tokio::time::timeout(Duration::from_secs(2), drain)
                .await
                .expect("drain task should exit within 2s of inbound close")
                .unwrap();

            assert!(
                !sup.workers.lock().await.contains_key(id),
                "{id}: the handle must be dropped"
            );
            assert_eq!(
                sup.take_startup_failures(),
                if expect_rearm {
                    vec![id.to_string()]
                } else {
                    Vec::<String>::new()
                },
                "{id}: only a registry-backed attached crash re-arms"
            );
            let banner_published = sink.frames.lock().unwrap().iter().any(|(_, _, ev)| {
                matches!(ev, Event::AgentStartupError { message } if message.contains("crashed more than"))
            });
            assert!(
                !banner_published,
                "{id}: the crash-loop banner must not be published for an attached worker"
            );
            assert_eq!(
                stopped_reasons(&sink, id)
                    .iter()
                    .filter(|r| *r == "user_stopped")
                    .count(),
                usize::from(!expect_rearm),
                "{id}: a registry-gone attached crash is a user stop"
            );
        }
    }

    /// A runner stopped on purpose closes its connection with an error that is
    /// not a startup failure, before or after the session is assigned. A marker
    /// naming another generation belongs to a different runner, so its error passes.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_deliberate_restart_publishes_no_startup_error() {
        // (name, session assigned first, marker generation, error is a deliberate close)
        let cases = [
            ("after assignment", true, 4, true),
            ("before assignment", false, 4, true),
            ("marker for another runner", false, 3, false),
        ];
        for (name, assigned, marker, deliberate) in cases {
            let (_home, tmp) = isolate_home();
            let id = "s-deliberate";
            let sink = VecSink::new();
            let sup = Supervisor::new(sink.clone());
            worker_registry::save(
                &worker_record(id, std::process::id(), tmp.path().join("attach.sock"))
                    .with_generation(4),
            )
            .unwrap();
            worker_registry::mark_restart_pending(id, marker);
            let (client, _client_tx) = crate::acp::acp_client::AcpClient::fake_for_test(
                crate::acp::state::AcpSessionId(id.into()),
            );
            let lease = sup
                .test_install_handle(id, client, WorkerKind::Attached, None)
                .await;
            let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(16);
            let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
            if assigned {
                inbound_tx
                    .send(Event::AcpSessionAssigned {
                        acp_session_id: "acp-1".into(),
                    })
                    .await
                    .unwrap();
            }
            inbound_tx
                .send(Event::AgentStartupError {
                    message: "ACP connection failed: agent transport closed".into(),
                })
                .await
                .unwrap();
            drop(inbound_tx);
            tokio::time::timeout(Duration::from_secs(2), drain)
                .await
                .expect("drain task should exit within 2s of inbound close")
                .unwrap();

            let published = sink
                .frames
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, _, ev)| matches!(ev, Event::AgentStartupError { .. }))
                .count();
            assert_eq!(published, usize::from(!deliberate), "{name}: published");
        }
    }

    /// A worker that goes away with no respawn behind it takes its
    /// background-agent tailers with it, so the drain's terminal arms must
    /// detach whatever the log still shows running: otherwise the panel keeps
    /// the sub-agent `Running` and `has_active_background_agent` holds the
    /// sidebar dot lit past the `Stopped` this same arm publishes (#4001).
    #[tokio::test]
    #[serial_test::serial]
    async fn a_terminal_drain_arm_detaches_the_workers_background_agents() {
        use crate::acp::state::{AcpSessionId, AcpState, AgentName, BackgroundAgentStatus};

        let id = "s-drain-detach";
        let (_home, _tmp) = isolate_home();
        let (sink, store, _rx, _store_tmp) = channel_sink();
        sink.publish(
            id,
            1,
            &Event::BackgroundAgentLaunched {
                agent_id: "sub-1".into(),
                tool_call_id: "tc-1".into(),
                description: "do a thing".into(),
                prompt: "do a thing".into(),
                model: "claude".into(),
                output_file: "/tmp/nonexistent-4029.jsonl".into(),
                started_at: chrono::Utc::now(),
                workflow: None,
            },
        );
        let sup = Supervisor::new(sink);
        sup.hydrate_seqs(store.all_session_seqs());
        let (client, _client_tx) =
            crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId(id.into()));
        // No registry record: `restart_decision` reads that as the user
        // stopping the runner externally and drops the handle without a
        // respawn, which is the arm under test.
        let lease = sup
            .test_install_handle(id, client, WorkerKind::Attached, None)
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(16);
        let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
        drop(inbound_tx);
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain task should exit within 5s of inbound close")
            .unwrap();

        assert!(
            store.unresolved_background_agent_ids(id).is_empty(),
            "the dropped worker's sub-agent must be detached in the durable log"
        );
        let mut state = AcpState::new(AcpSessionId(id.into()), AgentName("claude".into()), None);
        for (seq, event) in store.replay_from(id, 0) {
            state.apply_event(seq, event).unwrap();
        }
        assert_eq!(
            state.background_agents[0].status,
            BackgroundAgentStatus::Detached
        );
        assert!(
            !state.has_active_background_agent(),
            "the sidebar dot must not stay lit behind a stopped worker"
        );
    }

    #[tokio::test]
    async fn drain_drops_the_handle_without_respawn_on_terminal_signals() {
        // (session, events, crash message expected, handed to the reconciler)
        let rate_limited = vec![Event::Stopped {
            reason: "rate_limited".into(),
        }];
        let startup_error = Event::AgentStartupError {
            message: "ACP connection failed: native binary failed to launch".into(),
        };
        let established = Event::AcpSessionAssigned {
            acp_session_id: "acp-1".into(),
        };
        let cases = [
            ("s-rl", rate_limited, false, false),
            ("s-startup", vec![startup_error.clone()], false, true),
            ("s-crash", vec![established, startup_error], true, false),
        ];
        for (id, events, expect_crash_message, startup_failure) in cases {
            let sink = VecSink::new();
            let sup = Supervisor::new(sink.clone());
            let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(16);
            let lease = sup.test_install_stdio(id).await;
            let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
            for event in &events {
                inbound_tx.send(event.clone()).await.unwrap();
            }
            drop(inbound_tx);
            tokio::time::timeout(Duration::from_secs(2), drain)
                .await
                .expect("drain task should exit within 2s of inbound close")
                .unwrap();

            assert!(!sup.workers.lock().await.contains_key(id), "{id}");
            assert_eq!(sup.worker_state(id).await, AcpWorkerState::Absent, "{id}");
            let expected_failures: Vec<String> = if startup_failure {
                vec![id.to_string()]
            } else {
                Vec::new()
            };
            assert_eq!(sup.take_startup_failures(), expected_failures, "{id}");
            let published: Vec<Event> = sink
                .frames
                .lock()
                .unwrap()
                .iter()
                .map(|(_, _, ev)| ev.clone())
                .collect();
            assert_eq!(
                published.len(),
                events.len() + usize::from(expect_crash_message),
                "{id}: the stream's events plus at most the crash message: {published:?}"
            );
            for event in &events {
                assert!(
                    published
                        .iter()
                        .any(|p| format!("{p:?}") == format!("{event:?}")),
                    "{id}: {event:?} must reach the sink"
                );
            }
            let crash_messages = published
                .iter()
                .filter(|ev| {
                    matches!(ev, Event::AgentStartupError { message } if message.contains("crashed more than"))
                })
                .count();
            assert_eq!(crash_messages, usize::from(expect_crash_message), "{id}");
        }
    }

    /// A runner drops its control channel when a frame write takes over 2 s,
    /// so a stalled save must not stop the drain reading the next frames.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_event_save_does_not_stop_the_drain_reading_frames() {
        let (sink, store, _rx, tmp) = channel_sink();
        let sup = Supervisor::new(Arc::clone(&sink));
        let id = "s-stalled-save";
        let lease = sup.test_install_stdio(id).await;
        let blocker = rusqlite::Connection::open(tmp.path().join("acp.db")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(64);
        let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
        let frames = 256;
        let sent = tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..frames {
                inbound_tx.send(Event::ThinkingStarted).await.unwrap();
            }
        })
        .await;
        blocker.execute_batch("COMMIT").unwrap();
        sent.expect("the drain must keep reading frames while a save is stalled");

        // Closing the stream lets the drain take every frame before the flush.
        drop(inbound_tx);
        tokio::time::timeout(Duration::from_secs(10), drain)
            .await
            .expect("drain must end once its stream closes")
            .unwrap();
        sink.flush();
        let stored: Vec<u64> = store
            .replay_from(id, 0)
            .into_iter()
            .filter(|(_, event)| matches!(event, Event::ThinkingStarted))
            .map(|(seq, _)| seq)
            .collect();
        assert_eq!(
            stored,
            (1..=frames).collect::<Vec<u64>>(),
            "every frame is stored once, in order"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_context_ack_does_not_publish_later_native_assignments() {
        let (_home, _temp) = isolate_home();
        let id = "s-reset-ack-failure";
        let sink = VecSink::new();
        let supervisor = Supervisor::new(sink.clone());
        let lease = supervisor.test_install_stdio(id).await;
        let (sender, inbound) = mpsc::channel(4);
        let drain = supervisor.start_drain_task(
            id.into(),
            lease,
            inbound,
            Some(PendingContextReset {
                profile: "default".into(),
                reason: "native content changed".into(),
                transactions: vec!["missing-slot".into()],
            }),
        );
        for sid in ["first", "second"] {
            sender
                .send(Event::AcpSessionAssigned {
                    acp_session_id: sid.into(),
                })
                .await
                .unwrap();
        }
        drop(sender);
        tokio::time::timeout(Duration::from_secs(2), drain)
            .await
            .unwrap()
            .unwrap();
        {
            let events = sink.frames.lock().unwrap();
            assert!(events
                .iter()
                .any(|(_, _, event)| matches!(event, Event::SessionContextReset { .. })));
            assert!(events.iter().any(|(_, _, event)| matches!(event, Event::AgentStartupError { message } if message.contains("Could not commit the isolated native context"))));
            assert!(!events
                .iter()
                .any(|(_, _, event)| matches!(event, Event::AcpSessionAssigned { .. })));
        }
        assert_eq!(supervisor.take_startup_failures(), vec![id.to_string()]);
        assert!(!supervisor.workers.lock().await.contains_key(id));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_during_respawn_retires_the_replacement() {
        let _home = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let gate = Gate::default();
        let sink = VecSink::new();
        let sup = Arc::new(
            Supervisor::new(sink.clone())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4343)),
        );
        let lifecycle = Arc::clone(&sup.lifecycle);
        control.probe_phase(move |id| lock_recover(&lifecycle).phase(id));
        save_attested_test_record("s-resp", 4242, 0);
        let socket = worker_registry::socket_path_for("s-resp").unwrap();
        let lease = sup
            .test_install_runner(
                "s-resp",
                runner_config(socket),
                Some(attested_test_identity(4242, 0)),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-resp".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("the replacement freezes proof before its stop gate");
        assert_eq!(sup.worker_state("s-resp").await, AcpWorkerState::Resuming);
        sup.shutdown_idle("s-resp", None).await.expect("cancel");
        gate.open.notify_one();

        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain task must finish")
            .unwrap();
        let signals = control.signals();
        assert!(
            signals.contains(&(4343, "TERM")) && signals.contains(&(4242, "TERM")),
            "both the replacement and the runner it replaced are retired: {signals:?}"
        );
        assert_eq!(
            control.releases(),
            [(
                "s-resp".to_string(),
                Some(crate::acp::runner_lifecycle::WorkerPhase::Stopping)
            )],
            "the scope is released once, before the settle admits a new start"
        );
        assert!(!sup.workers.lock().await.contains_key("s-resp"));
        assert_eq!(sup.worker_state("s-resp").await, AcpWorkerState::Absent);
        assert_eq!(
            stopped_reasons(&sink, "s-resp"),
            vec!["idle_auto_stop".to_string()],
            "the stop reason the shutdown asked for is what the UI sees"
        );
        assert!(
            worker_registry::load("s-resp").unwrap().is_none(),
            "no record survives for either runner"
        );
        assert!(
            sup.take_respawned_in_place().is_empty(),
            "a retired replacement never reached the agent"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn permanent_delete_during_respawn_uses_the_replacement_client() {
        let _home = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let gate = Gate::default();
        let (launcher, deleted) = recording_gated_launcher(
            &gate,
            4343,
            "s-resp-delete",
            "native-replacement",
            Some(attested_test_birth(4343, 1)),
        );
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control)
                .with_launcher(launcher),
        );
        save_attested_test_record("s-resp-delete", 4242, 0);
        let socket = worker_registry::socket_path_for("s-resp-delete").unwrap();
        let lease = sup
            .test_install_runner(
                "s-resp-delete",
                runner_config(socket),
                Some(attested_test_identity(4242, 0)),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-resp-delete".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("replacement publishes its adapter session before the gate");
        assert_eq!(
            sup.worker_state("s-resp-delete").await,
            AcpWorkerState::Resuming
        );
        let record = worker_registry::load_strict("s-resp-delete")
            .unwrap()
            .expect("replacement registry record is published");
        assert_eq!(
            record.stored_acp_session_id.as_deref(),
            Some("native-replacement")
        );
        let respawning_lease = sup
            .capture_retirement("s-resp-delete")
            .unwrap()
            .lease
            .expect("respawn owns a cancellation lease");
        let cancellation = respawning_lease.cancellation().clone();
        let cancelled = tokio::spawn(async move { cancellation.cancelled().await });
        let deletion = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.shutdown_and_delete_and_wait("s-resp-delete", Duration::from_secs(5))
                    .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), cancelled)
            .await
            .expect("permanent delete cancels the respawn")
            .expect("cancellation observer does not panic");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !lock_recover(&sup.lifecycle).adapter_delete_pending(&respawning_lease) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "permanent delete did not register adapter deletion debt"
            );
            tokio::task::yield_now().await;
        }
        assert!(
            !deleted.load(std::sync::atomic::Ordering::SeqCst),
            "replacement has not returned its client before the gate opens"
        );
        gate.open.notify_one();

        tokio::time::timeout(Duration::from_secs(5), deletion)
            .await
            .expect("permanent delete waits for respawn retirement")
            .expect("delete task does not panic")
            .expect("delete settles");
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain task settles after deletion")
            .unwrap();
        assert!(
            deleted.load(std::sync::atomic::Ordering::SeqCst),
            "session/delete was sent through the replacement client"
        );
        assert!(worker_registry::load_strict("s-resp-delete")
            .unwrap()
            .is_none());
        assert_eq!(
            sup.worker_state("s-resp-delete").await,
            AcpWorkerState::Absent
        );
    }

    /// A stop recorded while a failed respawn waits for `workers` still
    /// retires both runners and releases the scope before the settle.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_stop_while_a_failed_respawn_waits_for_workers_releases_the_scope() {
        use crate::acp::runner_lifecycle::{StopDecision, WorkerPhase};

        let _home = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let gate = Gate::default();
        let failing = Arc::new(tokio::sync::Notify::new());
        let launcher: Launcher = {
            let (entered, open) = (Arc::clone(&gate.entered), Arc::clone(&gate.open));
            let failing = Arc::clone(&failing);
            Arc::new(move |config: SpawnConfig, id: AcpSessionId| {
                let (entered, open) = (Arc::clone(&entered), Arc::clone(&open));
                let failing = Arc::clone(&failing);
                Box::pin(async move {
                    let lease = config
                        .launch_lease
                        .as_ref()
                        .expect("supervisor retains its producer lease");
                    lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                    assert!(lease.record_runner(attested_test_identity(4343, config.generation)));
                    entered.notify_one();
                    open.notified().await;
                    save_attested_test_record(&id.0, 4343, config.generation);
                    failing.notify_one();
                    Err(AcpError::Spawn("dial deadline".into()))
                })
            })
        };
        let sink = VecSink::new();
        let sup = Arc::new(
            Supervisor::new(sink.clone())
                .with_process_control(control.clone())
                .with_launcher(launcher),
        );
        let lifecycle = Arc::clone(&sup.lifecycle);
        control.probe_phase(move |id| lock_recover(&lifecycle).phase(id));
        save_attested_test_record("s-respfail", 4242, 0);
        let socket = worker_registry::socket_path_for("s-respfail").unwrap();
        let lease = sup
            .test_install_runner(
                "s-respfail",
                runner_config(socket),
                Some(attested_test_identity(4242, 0)),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-respfail".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("the replacement records its proof before parking");
        let workers = sup.workers.lock().await;
        gate.open.notify_one();
        // The drain has no await between the failed launch and `workers`,
        // so on this single-threaded runtime it is parked on the held lock.
        tokio::time::timeout(Duration::from_secs(5), failing.notified())
            .await
            .expect("the failed replacement reaches the held workers gate");
        // What `shutdown` does under the `workers` lock it holds.
        assert!(matches!(
            lock_recover(&sup.lifecycle).begin_stop("s-respfail", "idle_auto_stop", false),
            StopDecision::CancelRequested
        ));
        drop(workers);

        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain task must finish")
            .unwrap();
        assert_eq!(
            control.releases(),
            [("s-respfail".to_string(), Some(WorkerPhase::Stopping))],
            "the scope is released once, before the settle admits a new start"
        );
        let signals = control.signals();
        assert!(
            signals.contains(&(4343, "TERM")) && signals.contains(&(4242, "TERM")),
            "both the failed replacement and the runner it replaced are retired: {signals:?}"
        );
        assert_eq!(
            stopped_reasons(&sink, "s-respfail"),
            ["idle_auto_stop"],
            "the stop owns the outcome, not the launch error"
        );
        assert_eq!(sup.worker_state("s-respfail").await, AcpWorkerState::Absent);
    }

    /// A stop arriving after a failed respawn began its own teardown gets
    /// `AlreadyStopping`; the teardown still releases the scope before the settle.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn a_stop_during_a_failed_respawn_teardown_releases_the_scope() {
        use crate::acp::runner_lifecycle::WorkerPhase;

        let _home = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).immortal(4343);
        let launcher: Launcher = Arc::new(move |config: SpawnConfig, id: AcpSessionId| {
            Box::pin(async move {
                let lease = config
                    .launch_lease
                    .as_ref()
                    .expect("supervisor retains its producer lease");
                lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                assert!(lease.record_runner(attested_test_identity(4343, config.generation)));
                save_attested_test_record(&id.0, 4343, config.generation);
                Err(AcpError::Spawn("dial deadline".into()))
            })
        });
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(launcher),
        );
        let lifecycle = Arc::clone(&sup.lifecycle);
        control.probe_phase(move |id| lock_recover(&lifecycle).phase(id));
        save_attested_test_record("s-late-respawn", 4242, 0);
        let socket = worker_registry::socket_path_for("s-late-respawn").unwrap();
        let lease = sup
            .test_install_runner(
                "s-late-respawn",
                runner_config(socket),
                Some(attested_test_identity(4242, 0)),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-late-respawn".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        // Paused time only advances once this task idles, so the teardown
        // waits out its grace until the replacement exits below.
        tokio::time::timeout(Duration::from_secs(5), control.terminated())
            .await
            .expect("the failed replacement enters captured TERM teardown");
        assert_eq!(control.signals(), [(4343, "TERM")]);
        assert_eq!(
            sup.worker_state("s-late-respawn").await,
            AcpWorkerState::Stopping
        );
        sup.shutdown("s-late-respawn")
            .await
            .expect("already stopping");
        assert!(control.releases().is_empty(), "{:?}", control.releases());
        assert!(
            control.is_alive(attested_test_identity(4242, 0)),
            "the late stop must still retire its captured predecessor"
        );
        control.exit(4343);

        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain task must finish")
            .unwrap();
        assert_eq!(control.signals(), [(4343, "TERM"), (4242, "TERM")]);
        assert_eq!(
            control.releases(),
            [("s-late-respawn".to_string(), Some(WorkerPhase::Stopping))]
        );
        assert_eq!(
            sup.worker_state("s-late-respawn").await,
            AcpWorkerState::Absent
        );
    }

    /// A failed respawn with no stop retires only its replacement and keeps
    /// the scope: the session's glob also matches the retained previous runner.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_respawn_without_a_stop_keeps_the_scope() {
        let _home = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let launcher: Launcher = Arc::new(move |config: SpawnConfig, id: AcpSessionId| {
            Box::pin(async move {
                let lease = config
                    .launch_lease
                    .as_ref()
                    .expect("supervisor retains its producer lease");
                lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                assert!(lease.record_runner(attested_test_identity(4343, config.generation)));
                save_attested_test_record(&id.0, 4343, config.generation);
                Err(AcpError::Spawn("dial deadline".into()))
            })
        });
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(launcher),
        );
        save_attested_test_record("s-respawn-kept", 4242, 0);
        let socket = worker_registry::socket_path_for("s-respawn-kept").unwrap();
        let lease = sup
            .test_install_runner(
                "s-respawn-kept",
                runner_config(socket),
                Some(attested_test_identity(4242, 0)),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-respawn-kept".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain task must finish")
            .unwrap();
        assert_eq!(control.signals(), [(4343, "TERM")]);
        assert!(
            control.is_alive(attested_test_identity(4242, 0)),
            "the previous runner is retained"
        );
        assert!(control.releases().is_empty(), "{:?}", control.releases());
        assert_eq!(
            sup.worker_state("s-respawn-kept").await,
            AcpWorkerState::Absent
        );
    }

    fn seed_row(id: &str) -> (crate::session::Storage, String) {
        let mut inst = crate::session::Instance::new(id, "/tmp");
        inst.id = id.into();
        let storage = crate::session::Storage::new_unwatched(&inst.source_profile).unwrap();
        let profile = inst.source_profile.clone();
        storage
            .update(|rows, _| {
                *rows = vec![inst];
                Ok(())
            })
            .unwrap();
        (storage, profile)
    }

    /// A crash respawn after an archive or a purge launches no replacement.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_respawn_for_a_dismissed_row_launches_nothing() {
        type Dismiss = fn(&mut Vec<crate::session::Instance>);
        let cases: [(&str, &str, Dismiss); 2] = [
            ("s-archived", "session_archived", |rows| rows[0].archive()),
            ("s-purged", "session_gone", |rows| rows.clear()),
        ];
        for (id, reason, dismiss) in cases {
            let _home = isolate_home();
            let control =
                Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
            control.alive(4242);
            let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let launcher: Launcher = {
                let launches = Arc::clone(&launches);
                Arc::new(move |_: SpawnConfig, _: AcpSessionId| {
                    launches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Box::pin(async { Err(AcpError::Spawn("must not launch".into())) })
                })
            };
            let sink = VecSink::new();
            let sup = Arc::new(
                Supervisor::new(sink.clone())
                    .with_process_control(control.clone())
                    .with_launcher(launcher),
            );
            let (storage, profile) = seed_row(id);
            storage
                .update(|rows, _| {
                    dismiss(rows);
                    Ok(())
                })
                .unwrap();
            save_attested_test_record(id, 4242, 0);
            let socket = worker_registry::socket_path_for(id).unwrap();
            let mut config = runner_config(socket);
            config.source_profile = Some(profile);
            let lease = sup
                .test_install_runner(id, config, Some(attested_test_identity(4242, 0)))
                .await;
            let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
            let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
            drop(inbound_tx);

            tokio::time::timeout(Duration::from_secs(5), drain)
                .await
                .expect("drain task must finish")
                .unwrap();
            assert_eq!(
                launches.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{id}"
            );
            assert_eq!(stopped_reasons(&sink, id), [reason.to_string()], "{id}");
            assert_eq!(sup.worker_state(id).await, AcpWorkerState::Absent, "{id}");
        }
    }

    /// An archive or trash that lands while the `before_session` hook runs reaches no launcher.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_respawn_dismissed_during_the_hook_launches_nothing() {
        type Dismiss = fn(&mut Vec<crate::session::Instance>);
        let cases: [(&str, &str, Dismiss); 2] = [
            ("s-hook-archived", "session_archived", |rows| {
                rows[0].archive()
            }),
            ("s-hook-trashed", "session_trashed", |rows| rows[0].trash()),
        ];
        for (id, reason, dismiss) in cases {
            let (_home, temp) = isolate_home();
            let entered = temp.path().join("hook-entered");
            let release = temp.path().join("hook-release");
            let app_dir = crate::session::get_app_dir().unwrap();
            std::fs::create_dir_all(&app_dir).unwrap();
            std::fs::write(
                app_dir.join("config.toml"),
                format!(
                    "[host_hooks]\nbefore_session = \"touch {entered}; while [ ! -e {release} ]; do sleep 0.02; done\"\n",
                    entered = entered.display(),
                    release = release.display(),
                ),
            )
            .unwrap();
            let control =
                Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
            control.alive(4242);
            let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let launcher: Launcher = {
                let launches = Arc::clone(&launches);
                Arc::new(move |_: SpawnConfig, _: AcpSessionId| {
                    launches.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Box::pin(async { Err(AcpError::Spawn("must not launch".into())) })
                })
            };
            let sink = VecSink::new();
            let sup = Arc::new(
                Supervisor::new(sink.clone())
                    .with_process_control(control.clone())
                    .with_launcher(launcher),
            );
            let (storage, profile) = seed_row(id);
            save_attested_test_record(id, 4242, 0);
            let socket = worker_registry::socket_path_for(id).unwrap();
            let mut config = runner_config(socket);
            config.source_profile = Some(profile);
            let lease = sup
                .test_install_runner(id, config, Some(attested_test_identity(4242, 0)))
                .await;
            let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
            let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
            drop(inbound_tx);

            tokio::time::timeout(Duration::from_secs(5), async {
                while !entered.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("hook must start");
            storage
                .update(|rows, _| {
                    dismiss(rows);
                    Ok(())
                })
                .unwrap();
            std::fs::write(&release, "").unwrap();

            tokio::time::timeout(Duration::from_secs(5), drain)
                .await
                .expect("drain task must finish")
                .unwrap();
            assert_eq!(
                launches.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "{id}"
            );
            assert_eq!(stopped_reasons(&sink, id), [reason.to_string()], "{id}");
            assert_eq!(sup.worker_state(id).await, AcpWorkerState::Absent, "{id}");
            assert!(storage.load().unwrap()[0].ensure_startable().is_err());
        }
    }

    /// A trash that lands during the respawn handshake retires the replacement uninstalled.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_respawn_trashed_during_the_handshake_retires_the_replacement() {
        let _home = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let gate = Gate::default();
        let sink = VecSink::new();
        let sup = Arc::new(
            Supervisor::new(sink.clone())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4343)),
        );
        let (storage, profile) = seed_row("s-trashed");
        save_attested_test_record("s-trashed", 4242, 0);
        let socket = worker_registry::socket_path_for("s-trashed").unwrap();
        let mut config = runner_config(socket);
        config.source_profile = Some(profile);
        let lease = sup
            .test_install_runner("s-trashed", config, Some(attested_test_identity(4242, 0)))
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-trashed".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        gate.entered.notified().await;
        storage
            .update(|rows, _| {
                rows[0].trash();
                Ok(())
            })
            .unwrap();
        gate.open.notify_one();
        control.terminated().await;
        control.exit(4343);

        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain task must finish")
            .unwrap();
        assert!(control.signals().iter().any(|(pid, _)| *pid == 4343));
        assert_eq!(
            stopped_reasons(&sink, "s-trashed"),
            ["session_trashed".to_string()]
        );
        assert_eq!(sup.worker_state("s-trashed").await, AcpWorkerState::Absent);
        assert!(storage.load().unwrap()[0].ensure_startable().is_err());
    }

    /// A trash that lands before the launcher fails reports the dismissal and retires the
    /// predecessor, not a launch error.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_respawn_after_a_trash_reports_the_dismissal() {
        let _home = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242);
        let gate = Gate::default();
        let launcher: Launcher = {
            let entered = Arc::clone(&gate.entered);
            let open = Arc::clone(&gate.open);
            Arc::new(move |_: SpawnConfig, _: AcpSessionId| {
                let entered = Arc::clone(&entered);
                let open = Arc::clone(&open);
                Box::pin(async move {
                    entered.notify_one();
                    open.notified().await;
                    Err(AcpError::Spawn("dial deadline".into()))
                })
            })
        };
        let sink = VecSink::new();
        let sup = Arc::new(
            Supervisor::new(sink.clone())
                .with_process_control(control.clone())
                .with_launcher(launcher),
        );
        let (storage, profile) = seed_row("s-trash-failed");
        save_attested_test_record("s-trash-failed", 4242, 0);
        let socket = worker_registry::socket_path_for("s-trash-failed").unwrap();
        let mut config = runner_config(socket);
        config.source_profile = Some(profile);
        let lease = sup
            .test_install_runner(
                "s-trash-failed",
                config,
                Some(attested_test_identity(4242, 0)),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-trash-failed".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        gate.entered.notified().await;
        storage
            .update(|rows, _| {
                rows[0].trash();
                Ok(())
            })
            .unwrap();
        gate.open.notify_one();
        control.terminated().await;
        control.exit(4242);

        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain task must finish")
            .unwrap();
        assert_eq!(
            stopped_reasons(&sink, "s-trash-failed"),
            ["session_trashed".to_string()]
        );
        assert_eq!(
            sup.worker_state("s-trash-failed").await,
            AcpWorkerState::Absent
        );
    }

    /// The reconciler reads this flag to remind the agent its `Monitor` died.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_installed_crash_respawn_is_flagged_for_the_reconciler() {
        let _home = isolate_home();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4343)),
        );
        save_record("s-crash", 4242, 0);
        let socket = worker_registry::socket_path_for("s-crash").unwrap();
        let lease = sup
            .test_install_runner(
                "s-crash",
                runner_config(socket),
                Some(RunnerIdentity::unattested(4242, 0)),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let _drain = sup.start_drain_task("s-crash".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        gate.entered.notified().await;
        assert!(sup.take_respawned_in_place().is_empty());
        gate.open.notify_one();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut flagged = Vec::new();
        while flagged.is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "respawn was not flagged"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
            flagged = sup.take_respawned_in_place();
        }
        assert_eq!(flagged, vec!["s-crash".to_string()]);
        sup.shutdown_idle("s-crash", None).await.expect("shutdown");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn respawn_drops_a_withdrawn_hook_route_and_realigns_native_mcp() {
        use agent_client_protocol::v1::schema::McpServer;

        let (_home, temp) = isolate_home();
        let hook_store = temp.path().join("hook-store");
        std::fs::create_dir_all(&hook_store).unwrap();
        std::fs::write(
            hook_store.join(".claude.json"),
            r#"{ "mcpServers": { "stale": { "command": "stale" } } }"#,
        )
        .unwrap();
        std::fs::write(
            temp.path().join(".claude.json"),
            r#"{ "mcpServers": { "home": { "command": "home" } } }"#,
        )
        .unwrap();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        let hook_flag = temp.path().join("provide-route");
        std::fs::write(
            app_dir.join("config.toml"),
            format!(
                "[host_hooks]\nbefore_session = \"if [ -e {flag} ]; then printf 'CLAUDE_CONFIG_DIR={store}\\n'; fi\"\n",
                flag = hook_flag.display(),
                store = hook_store.display(),
            ),
        )
        .unwrap();

        let mut config = runner_config(worker_registry::socket_path_for("s-withdraw").unwrap());
        config.base_host_environment = vec![("HOME".into(), temp.path().display().to_string())];
        config.host_environment = vec![
            ("HOME".into(), temp.path().display().to_string()),
            ("CLAUDE_CONFIG_DIR".into(), hook_store.display().to_string()),
        ];
        config.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: temp.path().join(".claude"),
            exported_default_store: Some(false),
        });

        Drain::<VecSink>::refresh_launch_env("s-withdraw", &mut config).await;
        assert!(!config
            .host_environment
            .iter()
            .any(|(key, _)| key == "CLAUDE_CONFIG_DIR"));
        let names: Vec<_> = config
            .mcp_servers
            .iter()
            .map(|server| match server {
                McpServer::Stdio(server) => server.name.as_str(),
                McpServer::Http(server) => server.name.as_str(),
                McpServer::Sse(server) => server.name.as_str(),
                _ => "unknown",
            })
            .collect();
        assert_eq!(names, ["home"]);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn respawn_forwards_a_workspace_launch_repo_project_mcp_servers() {
        let (_home, temp) = isolate_home();
        let _env = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let repo = temp.path().join("repo");
        let workspace = temp.path().join("workspace");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            repo.join(".mcp.json"),
            r#"{ "mcpServers": { "project-only": { "command": "pl" } } }"#,
        )
        .unwrap();
        let servers = crate::session::mcp::project_mcp::load_project_mcp_servers(&repo).unwrap();
        let hash = crate::session::mcp::project_mcp::fingerprint(&servers);
        crate::session::config::repo_config::trust_repo(&repo, None, Some(&hash)).unwrap();

        let mut config = runner_config(worker_registry::socket_path_for("s-ws-mcp").unwrap());
        config.cwd = workspace;
        config.config_path = repo;
        config.base_host_environment = vec![("HOME".into(), temp.path().display().to_string())];
        config.host_environment = config.base_host_environment.clone();

        Drain::<VecSink>::refresh_launch_env("s-ws-mcp", &mut config).await;

        let forwarded = serde_json::to_value(&config.mcp_servers).unwrap();
        assert!(
            forwarded
                .as_array()
                .unwrap()
                .iter()
                .any(|server| server["name"] == "project-only"),
            "{forwarded}"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn respawn_discovers_declared_codex_wrapper_store_on_host_only() {
        let (_home, _temp, account) = codex_wrapper_fixture();
        // A sandbox applies no override, so it must not read the base store.
        for (sandboxed, expected) in [(false, vec!["account"]), (true, vec![])] {
            let mut config =
                runner_config(worker_registry::socket_path_for("s-codex-wrapper").unwrap());
            config.agent_key = "work-codex".into();
            config.tool = "work-codex".into();
            config.wrapper_substitution = Some(("work-codex".into(), "codex".into()));
            config.sandbox_info = sandboxed.then(sandbox_info);

            Drain::<VecSink>::refresh_launch_env("s-codex-wrapper", &mut config).await;

            let names: Vec<_> = config
                .mcp_servers
                .iter()
                .map(|server| match server {
                    agent_client_protocol::v1::schema::McpServer::Stdio(server) => {
                        server.name.as_str()
                    }
                    _ => "unknown",
                })
                .collect();
            assert_eq!(names, expected, "sandboxed={sandboxed}");
            assert_eq!(
                config
                    .host_environment
                    .iter()
                    .find(|(key, _)| key == "CODEX_HOME")
                    .map(|(_, value)| value.as_str()),
                (!sandboxed)
                    .then(|| account.join(".codex-work"))
                    .as_deref()
                    .and_then(|p| p.to_str()),
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn respawn_reapplies_selected_claude_store_before_native_mcp_discovery() {
        use agent_client_protocol::v1::schema::McpServer;

        let (_home, temp) = isolate_home();
        let declared = temp.path().join("declared");
        let selected = temp.path().join("selected");
        std::fs::create_dir_all(&declared).unwrap();
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::write(
            declared.join(".claude.json"),
            r#"{ "mcpServers": { "declared": { "command": "declared" } } }"#,
        )
        .unwrap();
        std::fs::write(
            selected.join(".claude.json"),
            r#"{ "mcpServers": { "selected": { "command": "selected" } } }"#,
        )
        .unwrap();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            format!(
                "[host_hooks]\nbefore_session = \"printf 'CLAUDE_CONFIG_DIR={}\\nHOOK_VALUE=kept\\n'\"\n\
                 [session.agent_config_dir]\nclaude = \"{}\"\n",
                temp.path().join("hook").display(),
                declared.display()
            ),
        )
        .unwrap();

        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let (config_tx, mut config_rx) = mpsc::unbounded_channel();
        let held_senders: Arc<std::sync::Mutex<Vec<mpsc::Sender<Event>>>> = Default::default();
        let launcher_senders = Arc::clone(&held_senders);
        let launcher: Launcher = Arc::new(move |config, session_id| {
            let config_tx = config_tx.clone();
            let senders = Arc::clone(&launcher_senders);
            Box::pin(async move {
                save_record(&session_id.0, 4343, config.generation);
                config_tx.send(config).unwrap();
                let (client, tx) = crate::acp::acp_client::AcpClient::fake_for_test(session_id);
                senders.lock().unwrap().push(tx);
                Ok(client.with_runner_pid(4343))
            })
        });
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control)
                .with_launcher(launcher),
        );
        save_record("s-store", 4242, 0);
        let socket = worker_registry::socket_path_for("s-store").unwrap();
        let mut config = runner_config(socket);
        config.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: selected.clone(),
            exported_default_store: None,
        });
        config.host_environment = vec![("CLAUDE_CONFIG_DIR".into(), "stale".into())];
        config.base_host_environment = vec![("HOME".into(), temp.path().display().to_string())];
        let lease = sup
            .test_install_runner("s-store", config, Some(RunnerIdentity::unattested(4242, 0)))
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-store".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        let launched = tokio::time::timeout(Duration::from_secs(5), config_rx.recv())
            .await
            .expect("respawn should launch")
            .expect("launcher should capture config");
        let names: Vec<_> = launched
            .mcp_servers
            .iter()
            .map(|server| match server {
                McpServer::Stdio(server) => server.name.as_str(),
                McpServer::Http(server) => server.name.as_str(),
                McpServer::Sse(server) => server.name.as_str(),
                _ => "unknown",
            })
            .collect();
        assert_eq!(names, ["selected"]);
        assert_eq!(
            launched
                .host_environment
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| value.as_str()),
            selected.to_str()
        );
        assert!(launched
            .host_environment
            .contains(&("HOOK_VALUE".into(), "kept".into())));

        sup.shutdown_idle("s-store", None).await.expect("shutdown");
        held_senders.lock().unwrap().clear();
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain should stop")
            .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn respawn_reapplies_declared_config_dir_over_a_hook_value() {
        let (_home, temp) = isolate_home();
        let declared = temp.path().join("declared");
        std::fs::create_dir_all(&declared).unwrap();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            format!(
                "[host_hooks]\nbefore_session = \"printf 'CLAUDE_CONFIG_DIR={}\\n'\"\n\
                 [session.agent_config_dir]\nclaude = \"{}\"\n",
                temp.path().join("hook").display(),
                declared.display()
            ),
        )
        .unwrap();

        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let (config_tx, mut config_rx) = mpsc::unbounded_channel();
        let held_senders: Arc<std::sync::Mutex<Vec<mpsc::Sender<Event>>>> = Default::default();
        let launcher_senders = Arc::clone(&held_senders);
        let launcher: Launcher = Arc::new(move |config, session_id| {
            let config_tx = config_tx.clone();
            let senders = Arc::clone(&launcher_senders);
            Box::pin(async move {
                save_record(&session_id.0, 4343, config.generation);
                config_tx.send(config).unwrap();
                let (client, tx) = crate::acp::acp_client::AcpClient::fake_for_test(session_id);
                senders.lock().unwrap().push(tx);
                Ok(client.with_runner_pid(4343))
            })
        });
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control)
                .with_launcher(launcher),
        );
        save_record("s-declared", 4242, 0);
        let socket = worker_registry::socket_path_for("s-declared").unwrap();
        let mut config = runner_config(socket);
        config.agent_key = "claude".into();
        config.tool = "claude".into();
        config.base_host_environment = vec![("HOME".into(), temp.path().display().to_string())];
        let lease = sup
            .test_install_runner(
                "s-declared",
                config,
                Some(RunnerIdentity::unattested(4242, 0)),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-declared".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        let launched = tokio::time::timeout(Duration::from_secs(5), config_rx.recv())
            .await
            .expect("respawn should launch")
            .expect("launcher should capture config");
        assert_eq!(
            launched
                .host_environment
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| value.as_str()),
            declared.to_str()
        );

        sup.shutdown_idle("s-declared", None)
            .await
            .expect("shutdown");
        held_senders.lock().unwrap().clear();
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain should stop")
            .unwrap();
    }

    /// Losing only the control channel of a live runner mid-turn reattaches
    /// that runner and lets its turn finish, through a stalled attempt, a
    /// resume replaying more updates than the event channel holds, a second
    /// loss and a silence past the resume-idle grace. A dead runner, an
    /// undecodable frame or a stop keeps the replacement or stop path.
    #[tokio::test]
    #[serial_test::serial]
    async fn control_channel_loss_reattaches_only_a_verified_live_runner() {
        for case in [
            ControlLossCase::LiveRunner,
            ControlLossCase::DeadRunner,
            ControlLossCase::Undecodable,
            ControlLossCase::Stopped,
        ] {
            control_channel_loss_case(case).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn live_recovery_resume_refusal_exhausts_attempts_and_reports_the_turn() {
        control_channel_loss_case(ControlLossCase::RefusedResume).await;
    }

    /// A completion claimed and queued behind a full event channel when the
    /// control channel closes still ends the turn once if recovery is
    /// unavailable: no `runner_control_lost` follows its `prompt_complete`.
    #[tokio::test]
    #[serial_test::serial]
    async fn unrecoverable_loss_after_a_queued_completion_publishes_one_terminal() {
        use crate::acp::control_protocol::{self, ControlBody, PromptOutcome};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncWriteExt as _;
        use tokio::sync::oneshot;

        struct Runner(std::process::Child);
        impl Drop for Runner {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let (_home, tmp) = isolate_home();
        let id = "s-queued-completion-loss";
        let native = "native-queued-completion";
        let socket = tmp.path().join("runner.sock");
        // A live, unstopped record, so only the missing identity rules out recovery.
        let runner = Runner(
            std::process::Command::new("sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        worker_registry::save(
            &worker_registry::WorkerRecord::new(
                id.into(),
                runner.0.id(),
                socket.clone(),
                "test-agent-acp".into(),
                "test-agent".into(),
                tmp.path().into(),
                None,
                vec![],
                vec![],
                Some(native.into()),
                None,
            )
            .with_generation(7),
        )
        .unwrap();
        let listener =
            tokio::net::UnixListener::bind(crate::process::worker::control_socket_sibling(&socket))
                .unwrap();
        let (started_tx, started_rx) = oneshot::channel();
        let (fill_tx, fill_rx) = oneshot::channel::<usize>();
        let (sever_tx, sever_rx) = oneshot::channel::<()>();
        let peer = tokio::spawn(async move {
            let write = |body| control_protocol::encode_frame(&body).unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(&write(ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: id.into(),
                }))
                .await
                .unwrap();
            while let Ok(Some(frame)) = control_protocol::read_frame(&mut stream).await {
                let reply = match frame {
                    ControlBody::Initialize { .. } => ControlBody::Initialized {
                        result: serde_json::json!({"protocolVersion": 1, "agentCapabilities": {}}),
                    },
                    ControlBody::ResumeSession => ControlBody::SessionReady {
                        acp_session_id: native.into(),
                        result: serde_json::json!({}),
                    },
                    ControlBody::Prompt { .. } => ControlBody::PromptStarted { prompt_req_id: 77 },
                    _ => continue,
                };
                let prompted = matches!(reply, ControlBody::PromptStarted { .. });
                stream.write_all(&write(reply)).await.unwrap();
                if prompted {
                    break;
                }
            }
            started_tx.send(()).unwrap();
            for n in 0..fill_rx.await.unwrap() {
                let chunk = ControlBody::Notify {
                    method: "session/update".into(),
                    params: serde_json::json!({"sessionId": native, "update": {
                        "sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": format!("queued-{n}")}
                    }}),
                };
                stream.write_all(&write(chunk)).await.unwrap();
            }
            let completed = ControlBody::PromptCompleted {
                prompt_req_id: 77,
                outcome: PromptOutcome::Completed {
                    stop_reason: Some("end_turn".into()),
                },
            };
            stream.write_all(&write(completed)).await.unwrap();
            sever_rx.await.unwrap();
        });

        let mut client = AcpClient::attach(
            socket.clone(),
            tmp.path().into(),
            vec![],
            native.into(),
            false,
            AcpSessionId(id.into()),
            None,
            "test-agent".into(),
            None,
            Some(tokio::time::Instant::now() + Duration::from_secs(5)),
            None,
            None,
        )
        .await
        .unwrap();
        let mut inbound = client.take_inbound().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(inbound.recv().await, Some(Event::AcpSessionAssigned { .. })) {}
        })
        .await
        .unwrap();
        client.test_flush_commands().await;
        client.send_prompt("run", &[]).await.unwrap();
        started_rx.await.unwrap();
        fill_tx.send(inbound.max_capacity()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !client.test_terminal_claimed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the completion claims its terminal behind the full channel");
        assert_eq!(inbound.len(), inbound.max_capacity());
        sever_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while client.control_loss().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the reader records the loss");
        assert!(
            client.control_loss().unwrap().turn_in_flight,
            "EOF snapshots the claimed turn as active"
        );

        let sink = VecSink::new();
        let launches = Arc::new(AtomicUsize::new(0));
        let sup = Supervisor::new(sink.clone()).with_launcher(Arc::new({
            let launches = Arc::clone(&launches);
            move |_, _| {
                launches.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { Err(AcpError::Spawn("replacement refused".into())) })
            }
        }));
        let kind = WorkerKind::Runner {
            spawn_config: Box::new(runner_config(socket.clone())),
        };
        // No recorded runner identity: same-runner recovery is unavailable.
        let lease = sup.test_install_handle(id, client, kind, None).await;
        let drain = sup.start_drain_task(id.into(), lease, inbound, None);
        tokio::time::timeout(Duration::from_secs(10), drain)
            .await
            .expect("drain settles after the fallback")
            .unwrap();
        assert_eq!(stopped_reasons(&sink, id), ["prompt_complete"]);
        assert!(sink
            .frames
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, event)| matches!(event, Event::AgentStartupError { .. })));
        assert_eq!(launches.load(Ordering::SeqCst), 1, "fallback replacement");
        peer.abort();
        drop(runner);
    }

    fn recovery_test_drain(supervisor: &Supervisor<VecSink>, id: &str) -> Drain<VecSink> {
        Drain {
            session_id: id.into(),
            sink: Arc::clone(&supervisor.sink),
            workers: Arc::clone(&supervisor.workers),
            next_seqs: Arc::clone(&supervisor.next_seqs),
            incompatible_binaries: Arc::clone(&supervisor.incompatible_binaries),
            lifecycle: Arc::clone(&supervisor.lifecycle),
            process_control: Arc::clone(&supervisor.process_control),
            launcher: Arc::clone(&supervisor.launcher),
            notify: Arc::clone(&supervisor.worker_notify),
            startup_failures: Arc::clone(&supervisor.startup_failures),
            pending_context_resets: Arc::clone(&supervisor.pending_context_resets),
            context_reset: None,
            respawned_in_place: Arc::clone(&supervisor.respawned_in_place),
            recovery: None,
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn live_recovery_force_stop_does_not_shut_down_the_next_lease() {
        let (_home, _tmp) = isolate_home();
        let id = "s-recovery-force";
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        control.alive(4242).alive(4343);
        let (launched_tx, mut launched_rx) = mpsc::channel(1);
        let supervisor = Supervisor::new(VecSink::new())
            .with_process_control(control)
            .with_launcher(Arc::new(move |config, session_id| {
                let launched_tx = launched_tx.clone();
                Box::pin(async move {
                    let lease = config.launch_lease.as_ref().unwrap();
                    lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                    assert!(lease.record_runner(attested_test_identity(4343, config.generation)));
                    save_attested_test_record(&session_id.0, 4343, config.generation);
                    let (client, events, commands) =
                        AcpClient::fake_for_test_cmd_recording(session_id);
                    launched_tx.send((events, commands)).await.unwrap();
                    Ok(client.with_runner_pid(4343))
                })
            }));
        save_attested_test_record(id, 4242, 0);
        let lease = supervisor
            .test_install_runner(
                id,
                runner_config(worker_registry::socket_path_for(id).unwrap()),
                Some(attested_test_identity(4242, 0)),
            )
            .await;
        supervisor
            .workers
            .lock()
            .await
            .get_mut(id)
            .unwrap()
            .restart_history
            .push(Instant::now());
        let mut driver = recovery_test_drain(&supervisor, id);
        // Seed recovery before the assignment and force-stop events are pumped.
        driver.recovery = Some(Recovery {
            loss: ControlLoss {
                io_error: None,
                turn_in_flight: true,
            },
            attempts: 1,
        });
        let (events, inbound) = mpsc::channel(4);
        events
            .send(Event::AcpSessionAssigned {
                acp_session_id: "preserved-native".into(),
            })
            .await
            .unwrap();
        events
            .send(Event::Stopped {
                reason: "user_forced".into(),
            })
            .await
            .unwrap();
        drop(events);
        let old_epoch = lease.epoch();
        let task = tokio::spawn(driver.run(lease, inbound));
        let (events, commands) = tokio::time::timeout(Duration::from_secs(5), launched_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while supervisor.workers.lock().await[id].lease.epoch() == old_epoch {
            assert!(Instant::now() < deadline, "the replacement did not install");
            tokio::task::yield_now().await;
        }
        events
            .send(Event::AuthStatusUpdated { status: None })
            .await
            .unwrap();
        while !supervisor
            .sink
            .frames
            .lock()
            .unwrap()
            .iter()
            .any(|(_, _, event)| matches!(event, Event::AuthStatusUpdated { .. }))
        {
            assert!(
                Instant::now() < deadline,
                "the new connection did not start pumping"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Hold session assignment while observing the fresh connection across the old deadline.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !commands.lock().unwrap().contains(&"shutdown"),
            "an expired recovery must not close a later lease"
        );
        events
            .send(Event::AcpSessionAssigned {
                acp_session_id: "replacement-native".into(),
            })
            .await
            .unwrap();
        while supervisor.workers.lock().await[id]
            .native_session_id
            .as_deref()
            != Some("replacement-native")
        {
            assert!(
                Instant::now() < deadline,
                "the replacement never established"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(
            supervisor.workers.lock().await[id].restart_history.len(),
            2,
            "one recovery and one respawn consume the shared budget"
        );
        assert_eq!(stopped_reasons(&supervisor.sink, id), ["user_forced"]);
        events
            .send(Event::Stopped {
                reason: "rate_limited".into(),
            })
            .await
            .unwrap();
        drop(events);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn live_recovery_confirmed_replay_survives_an_elapsed_deadline() {
        use crate::acp::control_protocol::{self, ControlBody};
        use tokio::io::AsyncWriteExt as _;
        let (_home, temp) = isolate_home();
        // A confirmed resume finishes publishing after its attempt deadline.
        let id = "s-deadline-replay";
        let socket = temp.path().join("runner.sock");
        let listener =
            tokio::net::UnixListener::bind(crate::process::worker::control_socket_sibling(&socket))
                .unwrap();
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let mut peer = tokio::task::JoinSet::new();
        peer.spawn(async move {
            let mut close_rx = Some(close_rx);
            for connection in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let write = |body| control_protocol::encode_frame(&body).unwrap();
                stream.write_all(&write(ControlBody::Hello {
                    control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                    session_id: id.into(),
                })).await.unwrap();
                while let Ok(Some(frame)) = control_protocol::read_frame(&mut stream).await {
                    match frame {
                        ControlBody::Attach { .. } => {}
                        ControlBody::Initialize { .. } => {
                            stream.write_all(&write(ControlBody::Initialized {
                                result: serde_json::json!({"protocolVersion": 1, "agentCapabilities": {}}),
                            })).await.unwrap();
                        }
                        ControlBody::ResumeSession => {
                            stream.write_all(&write(ControlBody::SessionReady {
                                acp_session_id: "native-deadline".into(),
                                result: serde_json::json!({}),
                            })).await.unwrap();
                            if connection == 1 {
                                for n in 0..100 {
                                    stream.write_all(&write(ControlBody::Notify {
                                        method: "session/update".into(),
                                        params: serde_json::json!({
                                            "sessionId": "native-deadline",
                                            "update": {"sessionUpdate": "agent_message_chunk",
                                                "content": {"type": "text", "text": format!("chunk-{n}")}}
                                        }),
                                    })).await.unwrap();
                                }
                                let _ = close_rx.take().unwrap().await;
                                return;
                            }
                        }
                        other => panic!("unexpected frame {other:?}"),
                    }
                }
            }
        });
        let mut first = AcpClient::attach(
            socket,
            temp.path().into(),
            vec![],
            "native-deadline".into(),
            false,
            AcpSessionId(id.into()),
            None,
            "test-agent".into(),
            None,
            Some(tokio::time::Instant::now() + Duration::from_secs(5)),
            None,
            None,
        )
        .await
        .unwrap();
        drop(first.take_inbound());
        let supervisor = Supervisor::new(VecSink::new());
        let lease = supervisor
            .test_install_handle(id, first, WorkerKind::Attached, None)
            .await;
        let first = Arc::clone(&supervisor.workers.lock().await[id].client);
        first.shutdown().await.unwrap();
        let deadline = tokio::time::Instant::now() + CONTROL_REATTACH_DEADLINE;
        let mut next = first
            .reattach("native-deadline".into(), true, deadline, lease.clone())
            .await
            .unwrap();
        let mut inbound = next.take_inbound().unwrap();
        let next = Arc::new(next);
        supervisor.workers.lock().await.get_mut(id).unwrap().client = Arc::clone(&next);
        let within = Instant::now() + Duration::from_secs(5);
        while inbound.len() < 64 {
            assert!(
                Instant::now() < within,
                "the confirmed replay did not fill the channel"
            );
            tokio::task::yield_now().await;
        }
        tokio::time::pause();
        tokio::time::advance(CONTROL_REATTACH_DEADLINE + Duration::from_millis(1)).await;
        tokio::time::resume();
        let mut driver = recovery_test_drain(&supervisor, id);
        let pump = tokio::spawn(async move { driver.pump(&mut inbound, lease.epoch()).await });
        let chunks = || {
            supervisor
                .sink
                .frames
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, _, event)| match event {
                    Event::AgentMessageChunk { text, .. } => Some(text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let within = Instant::now() + Duration::from_secs(5);
        while chunks().len() < 100 && !pump.is_finished() {
            assert!(Instant::now() < within, "the confirmed replay stalled");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let observed = chunks();
        let _ = close_tx.send(());
        next.shutdown().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), pump)
            .await
            .unwrap()
            .unwrap();
        peer.abort_all();
        assert_eq!(
            observed,
            (0..100).map(|n| format!("chunk-{n}")).collect::<Vec<_>>(),
            "an elapsed resume deadline discarded already-confirmed output"
        );
    }

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum ControlLossCase {
        LiveRunner,
        DeadRunner,
        Undecodable,
        Stopped,
        RefusedResume,
    }

    async fn control_channel_loss_case(case: ControlLossCase) {
        use crate::acp::control_protocol::{self, ControlBody, PromptOutcome};
        use std::os::unix::process::CommandExt as _;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::io::AsyncWriteExt as _;
        use tokio::sync::oneshot;

        struct Runner(std::process::Child);
        impl Drop for Runner {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let _grace = crate::session::test_support::EnvGuard::set(&[
            ("AOE_RESUME_IDLE_GRACE_MS", "100"),
            ("AOE_RESUME_IDLE_CHECK_INTERVAL_MS", "10"),
        ]);
        let (_home, tmp) = isolate_home();
        let id = "s-control-loss";
        let native = "native-control-loss";
        let socket = tmp.path().join("runner.sock");
        let listener =
            tokio::net::UnixListener::bind(crate::process::worker::control_socket_sibling(&socket))
                .unwrap();
        let mut runner = Runner(
            std::process::Command::new("sleep")
                .arg("60")
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let record = worker_registry::WorkerRecord::new(
            id.into(),
            runner.0.id(),
            socket.clone(),
            "test-agent-acp".into(),
            "test-agent".into(),
            tmp.path().into(),
            None,
            vec![],
            vec![],
            Some(native.into()),
            None,
        )
        .with_generation(7);
        let identity = RunnerIdentity::from_record(&record);
        assert!(identity.birth_fingerprint.is_some());
        worker_registry::save(&record).unwrap();

        // Connection 0 carries the prompt and is severed mid-turn. On a
        // live runner, 1 withholds SessionReady, 2 resumes with a burst of
        // updates and is severed again, and 3 resumes, stays silent, then
        // completes the turn.
        const BURST: usize = 100;
        let update = move |text: String| ControlBody::Notify {
            method: "session/update".into(),
            params: serde_json::json!({
                "sessionId": native,
                "update": {
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text}
                }
            }),
        };
        let accepts = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = oneshot::channel();
        let (sever_tx, sever_rx) = oneshot::channel::<()>();
        let (withheld_tx, withheld_rx) = oneshot::channel();
        let (sever_again_tx, sever_again_rx) = oneshot::channel::<()>();
        let (resumed_tx, resumed_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel::<()>();
        let peer = tokio::spawn({
            let accepts = Arc::clone(&accepts);
            async move {
                let mut started_tx = Some(started_tx);
                let mut sever_rx = Some(sever_rx);
                let mut withheld_tx = Some(withheld_tx);
                let mut sever_again_rx = Some(sever_again_rx);
                let mut resumed_tx = Some(resumed_tx);
                let mut release_rx = Some(release_rx);
                let write = |body| control_protocol::encode_frame(&body).unwrap();
                let ready = || ControlBody::SessionReady {
                    acp_session_id: native.into(),
                    result: serde_json::json!({}),
                };
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let connection = accepts.fetch_add(1, Ordering::SeqCst);
                    stream
                        .write_all(&write(ControlBody::Hello {
                            control_protocol_version: control_protocol::CONTROL_PROTOCOL_VERSION,
                            session_id: id.into(),
                        }))
                        .await
                        .unwrap();
                    while let Ok(Some(frame)) = control_protocol::read_frame(&mut stream).await {
                        match (frame, connection) {
                            (ControlBody::Attach { .. }, _) => {}
                            (ControlBody::Initialize { .. }, _) => {
                                let init = ControlBody::Initialized {
                                    result: serde_json::json!({
                                        "protocolVersion": 1, "agentCapabilities": {}
                                    }),
                                };
                                stream.write_all(&write(init)).await.unwrap();
                            }
                            (ControlBody::ResumeSession, 0) => {
                                stream.write_all(&write(ready())).await.unwrap();
                            }
                            (ControlBody::Prompt { .. }, 0) => {
                                let started = ControlBody::PromptStarted { prompt_req_id: 77 };
                                stream.write_all(&write(started)).await.unwrap();
                                started_tx.take().unwrap().send(()).unwrap();
                                sever_rx.take().unwrap().await.unwrap();
                                if case == ControlLossCase::Undecodable {
                                    stream.write_all(&[0, 0, 0, 1, b'{']).await.unwrap();
                                    while let Ok(Some(_)) =
                                        control_protocol::read_frame(&mut stream).await
                                    {
                                    }
                                }
                                break;
                            }
                            (ControlBody::ResumeSession, connection)
                                if case == ControlLossCase::RefusedResume && connection > 0 =>
                            {
                                let refused = ControlBody::HandshakeFailed {
                                    error: serde_json::json!({
                                        "code": -32603, "message": "resume refused"
                                    }),
                                };
                                stream.write_all(&write(refused)).await.unwrap();
                                while let Ok(Some(_)) =
                                    control_protocol::read_frame(&mut stream).await
                                {
                                }
                                break;
                            }
                            (ControlBody::ResumeSession, 1) => {
                                while let Ok(Some(_)) =
                                    control_protocol::read_frame(&mut stream).await
                                {
                                }
                                withheld_tx.take().unwrap().send(()).unwrap();
                                break;
                            }
                            (ControlBody::ResumeSession, 2) => {
                                for n in 0..BURST {
                                    let burst = update(format!("progress {n}"));
                                    stream.write_all(&write(burst)).await.unwrap();
                                }
                                // Ordered dispatch denies this foreign-session call
                                // after all updates reach pending admission.
                                let barrier = ControlBody::ServerCall {
                                    call_id: 1,
                                    method: "fs/read_text_file".into(),
                                    params: serde_json::json!({
                                        "sessionId": "unknown", "path": "/unused"
                                    }),
                                };
                                stream.write_all(&write(barrier)).await.unwrap();
                            }
                            (ControlBody::ServerError { call_id: 1, error }, 2) => {
                                assert_eq!(error.code, -32602);
                                stream.write_all(&write(ready())).await.unwrap();
                                sever_again_rx.take().unwrap().await.unwrap();
                                break;
                            }
                            (ControlBody::ResumeSession, 3) => {
                                stream.write_all(&write(ready())).await.unwrap();
                                resumed_tx.take().unwrap().send(()).unwrap();
                                release_rx.take().unwrap().await.unwrap();
                                let chunk = update("original turn completed".into());
                                let completed = ControlBody::PromptCompleted {
                                    prompt_req_id: 77,
                                    outcome: PromptOutcome::Completed {
                                        stop_reason: Some("end_turn".into()),
                                    },
                                };
                                stream.write_all(&write(chunk)).await.unwrap();
                                stream.write_all(&write(completed)).await.unwrap();
                            }
                            (frame, connection) => {
                                panic!("unexpected frame on connection {connection}: {frame:?}")
                            }
                        }
                    }
                }
            }
        });

        let sink = VecSink::new();
        let control = Arc::new(ObservedProcessControl::default());
        let launches = Arc::new(AtomicUsize::new(0));
        // Restart budget charged by the time the replacement launches.
        let charged = Arc::new(AtomicUsize::new(0));
        let sup = Supervisor::new(sink.clone()).with_process_control(control.clone());
        let workers = Arc::clone(&sup.workers);
        let sup = sup.with_launcher(Arc::new({
            let launches = Arc::clone(&launches);
            let charged = Arc::clone(&charged);
            move |_, _| {
                launches.fetch_add(1, Ordering::SeqCst);
                let workers = Arc::clone(&workers);
                let charged = Arc::clone(&charged);
                Box::pin(async move {
                    let history = workers.lock().await[id].restart_history.len();
                    charged.store(history, Ordering::SeqCst);
                    Err(AcpError::Spawn("replacement refused".into()))
                })
            }
        }));
        let mut client = AcpClient::attach(
            socket.clone(),
            tmp.path().into(),
            vec![],
            native.into(),
            false,
            AcpSessionId(id.into()),
            None,
            "test-agent".into(),
            None,
            Some(tokio::time::Instant::now() + Duration::from_secs(5)),
            None,
            None,
        )
        .await
        .unwrap();
        let inbound = client.take_inbound().unwrap();
        let kind = WorkerKind::Runner {
            spawn_config: Box::new(runner_config(socket.clone())),
        };
        let lease = sup
            .test_install_handle(id, client, kind, Some(identity))
            .await;
        let first = Arc::clone(&sup.workers.lock().await[id].client);
        // Launches register their client for retirement, as here.
        lease.retirement().record_client(&first, false);
        let drain = sup.start_drain_task(id.into(), lease, inbound, None);
        first.send_prompt("run the tests", &[]).await.unwrap();
        started_rx.await.unwrap();
        match case {
            ControlLossCase::DeadRunner => {
                runner.0.kill().unwrap();
                runner.0.wait().unwrap();
            }
            ControlLossCase::Stopped => {
                worker_registry::request_stop(&record).unwrap();
            }
            ControlLossCase::LiveRunner
            | ControlLossCase::Undecodable
            | ControlLossCase::RefusedResume => {}
        }
        sever_tx.send(()).unwrap();

        let events = || {
            sink.frames
                .lock()
                .unwrap()
                .iter()
                .map(|(_, _, event)| event.clone())
                .collect::<Vec<_>>()
        };
        let progress = || {
            format!(
                "replacement launches={}, stops={:?}, control connections={}",
                launches.load(Ordering::SeqCst),
                stopped_reasons(&sink, id),
                accepts.load(Ordering::SeqCst)
            )
        };
        if case == ControlLossCase::LiveRunner {
            let within = Duration::from_secs(10);
            tokio::time::timeout(within, withheld_rx)
                .await
                .unwrap_or_else(|_| panic!("no bounded reattach attempt: {}", progress()))
                .unwrap();
            let chunks = || {
                events()
                    .into_iter()
                    .filter_map(|event| match event {
                        Event::AgentMessageChunk { text, .. } => Some(text),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            };
            let deadline = std::time::Instant::now() + within;
            while !chunks().contains(&format!("progress {}", BURST - 1)) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the resumed replay was not delivered: {} chunks, {}",
                    chunks().len(),
                    progress()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(
                chunks(),
                (0..BURST)
                    .map(|n| format!("progress {n}"))
                    .collect::<Vec<_>>()
            );
            sever_again_tx.send(()).unwrap();
            tokio::time::timeout(within, resumed_rx)
                .await
                .unwrap_or_else(|_| panic!("no second reattach: {}", progress()))
                .unwrap();
            let deadline = std::time::Instant::now() + within;
            while events()
                .iter()
                .filter(|event| matches!(event, Event::AcpSessionAssigned { .. }))
                .count()
                < 3
            {
                assert!(std::time::Instant::now() < deadline, "{}", progress());
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Observe silence beyond the idle grace after resume is committed.
            tokio::time::sleep(Duration::from_millis(400)).await;
            release_tx.send(()).unwrap();
            let deadline = std::time::Instant::now() + within;
            // Only the reattached runner's real completion carries this chunk.
            while !events().iter().any(|event| {
                matches!(event,
                Event::AgentMessageChunk { text, .. } if text == "original turn completed")
            }) || stopped_reasons(&sink, id).is_empty()
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the original turn never completed: {}",
                    progress()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert_eq!(launches.load(Ordering::SeqCst), 0, "no replacement");
            assert_eq!(accepts.load(Ordering::SeqCst), 4);
            assert!(control.signals().is_empty(), "{:?}", control.signals());
            assert_eq!(stopped_reasons(&sink, id), ["prompt_complete"]);
            assert!(!events()
                .iter()
                .any(|event| matches!(event, Event::AgentStartupError { .. })));
            assert_eq!(sup.running_identity(id), Some(identity));
            sup.detach_all().await;
        } else if case == ControlLossCase::RefusedResume {
            tokio::time::timeout(Duration::from_secs(15), drain)
                .await
                .expect("refused resumes must exhaust their bounded recovery")
                .unwrap();
            assert_eq!(
                accepts.load(Ordering::SeqCst),
                4,
                "initial attach and three attempts"
            );
            assert_eq!(
                launches.load(Ordering::SeqCst),
                1,
                "fallback after recovery exhaustion"
            );
            assert_eq!(stopped_reasons(&sink, id), ["runner_control_lost"]);
            assert!(
                sup.take_startup_failures().is_empty(),
                "recovery owns the retry"
            );
        } else {
            tokio::time::timeout(Duration::from_secs(10), drain)
                .await
                .unwrap_or_else(|_| panic!("{case:?}: drain did not settle"))
                .unwrap();
            assert_eq!(accepts.load(Ordering::SeqCst), 1, "{case:?}: no reattach");
            assert_eq!(
                launches.load(Ordering::SeqCst),
                usize::from(case != ControlLossCase::Stopped),
                "{case:?}"
            );
            let stops = stopped_reasons(&sink, id);
            assert_eq!(
                stops.contains(&"runner_control_lost".to_string()),
                case == ControlLossCase::DeadRunner,
                "{case:?}: {stops:?}"
            );
            if case == ControlLossCase::DeadRunner {
                assert_eq!(
                    charged.load(Ordering::SeqCst),
                    1,
                    "a dead runner is charged only its respawn"
                );
            }
        }
        peer.abort();
    }
}
