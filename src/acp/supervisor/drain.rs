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
    admit_stored_row, apply_claude_store_pin, apply_declared_config_dir, before_session_env,
    overlay_env, publish_rejection, refresh_spawn_model_effort, resolve_mcp_servers,
};
use super::teardown::{settle_sweeping_if_owed, tear_down_runner, wait_for_exit};
use super::{
    lock_recover, next_seq, BroadcastSink, Launcher, PendingContextReset, ResumeReservation,
    SeqMap, SharedSet, StartupFailure, Supervisor, SupervisorError, WorkerKind, Workers,
    MAX_RESPAWNS_IN_WINDOW, RESPAWN_BACKOFF, RESTART_WINDOW,
};
use crate::acp::acp_client::{AcpError, SpawnConfig};
use crate::acp::runner_lifecycle::{
    InstallError, Lease, LifecycleTable, ProcessControl, RunnerIdentity, WorkerPhase,
};
use crate::acp::state::{AcpSessionId, Event};
use crate::process::worker_registry;

#[cfg(test)]
static DEFERRED_REGISTRY_OBSERVED: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static RESPAWN_BACKOFF_OBSERVED: std::sync::Mutex<
    Option<(String, tokio::sync::oneshot::Sender<()>)>,
> = std::sync::Mutex::new(None);

impl<S: BroadcastSink> Supervisor<S> {
    pub(super) fn start_drain_task(
        &self,
        session_id: String,
        lease: Lease,
        inbound: mpsc::Receiver<Event>,
        context_reset: Option<PendingContextReset>,
    ) -> JoinHandle<()> {
        self.spawn_drain(session_id, lease, inbound, context_reset, false, None)
    }

    pub(super) fn start_retiring_drain(
        &self,
        session_id: String,
        lease: Lease,
        inbound: mpsc::Receiver<Event>,
        context_reset: Option<PendingContextReset>,
    ) {
        let (ready, registered) = tokio::sync::oneshot::channel();
        let task = self.spawn_drain(
            session_id,
            lease.clone(),
            inbound,
            context_reset,
            true,
            Some(registered),
        );
        lease.retirement().record_construction_drain(task);
        let _ = ready.send(());
    }

    fn spawn_drain(
        &self,
        session_id: String,
        lease: Lease,
        inbound: mpsc::Receiver<Event>,
        context_reset: Option<PendingContextReset>,
        identity_only: bool,
        registered: Option<tokio::sync::oneshot::Receiver<()>>,
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
            context_reset: if identity_only { None } else { context_reset },
            identity_only,
            respawned_in_place: Arc::clone(&self.respawned_in_place),
        };
        crate::task_util::spawn_supervised(
            "supervisor.drain",
            crate::task_util::PanicPolicy::Log,
            async move {
                if let Some(registered) = registered {
                    if registered.await.is_err() {
                        return;
                    }
                }
                drain.run(lease, inbound).await;
            },
        )
    }
}

struct Drain<S> {
    session_id: String,
    identity_only: bool,
    sink: Arc<S>,
    workers: Workers,
    next_seqs: Arc<SeqMap>,
    incompatible_binaries: Arc<std::sync::Mutex<HashMap<String, String>>>,
    lifecycle: Arc<std::sync::Mutex<LifecycleTable>>,
    process_control: Arc<dyn ProcessControl>,
    launcher: Launcher,
    notify: Arc<tokio::sync::Notify>,
    startup_failures: Arc<std::sync::Mutex<HashMap<String, StartupFailure>>>,
    pending_context_resets: SharedSet,
    context_reset: Option<PendingContextReset>,
    respawned_in_place: SharedSet,
}

/// What a worker's event stream said before it closed.
#[derive(Default)]
struct StreamEnd {
    agent_unresponsive: bool,
    startup_rate_limited: bool,
    startup_failed: bool,
    fork_rejection: Option<Arc<super::fork_rejection::QualifiedForkRejection>>,
    superseded: bool,
}

enum OwnedPublication {
    Published,
    Discarded,
    Superseded,
}

struct PublicationWait {
    fence: Arc<crate::session::NativeInputFence>,
    custody: Option<Arc<super::teardown::RetiringIdentityScope>>,
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
    /// A typed, physically attested stop claim owns this runner.
    UserStopped,
    /// Unknown registry state closes admission without granting retirement.
    Fenced,
}

impl<S: BroadcastSink> Drain<S> {
    fn publish(&self, event: Event) {
        let seq = next_seq(&self.next_seqs, &self.session_id);
        self.sink.publish(&self.session_id, seq, &event);
    }

    async fn run(mut self, mut lease: Lease, mut inbound: mpsc::Receiver<Event>) {
        loop {
            let end = self.pump(&mut inbound, &lease).await;
            if self.identity_only
                || lease.retirement().binding_checkpoint().is_some()
                || end.superseded
            {
                return;
            }
            if let Some(receipt) = &end.fork_rejection {
                let cancellation = lease.cancellation();
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => return,
                    _ = receipt.wait() => {}
                }
                if !receipt.committed() {
                    self.drop_handle(&lease).await;
                    return;
                }
            }
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
            if end.startup_rate_limited && end.fork_rejection.is_none() {
                info!(
                    target: "acp.supervisor",
                    session = %self.session_id,
                    "startup rate-limited; leaving continuation to the quota governor"
                );
                self.drop_handle(&lease).await;
                return;
            }
            if end.startup_failed || end.fork_rejection.is_some() {
                info!(
                    target: "acp.supervisor",
                    session = %self.session_id,
                    "startup failed before a session was established; leaving the retry to the reconciler"
                );
                {
                    let workers = self.workers.lock().await;
                    let lifecycle = lock_recover(&self.lifecycle);
                    if lifecycle.current_lease(&self.session_id).as_ref() != Some(&lease)
                        || !workers
                            .get(&self.session_id)
                            .is_some_and(|handle| handle.lease == lease)
                    {
                        return;
                    }
                    let failure = end
                        .fork_rejection
                        .map_or(StartupFailure::Retry, StartupFailure::ForkRejected);
                    lock_recover(&self.startup_failures).insert(self.session_id.clone(), failure);
                }
                self.drop_handle(&lease).await;
                return;
            }
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

    /// Fence publication with the installed worker or exact retained retirement custody.
    async fn publish_owned(&self, lease: &Lease, event: &mut Event) -> OwnedPublication {
        loop {
            match self.try_publish_owned(lease, event).await {
                Ok(result) => return result,
                Err(wait) => {
                    // No worker/lifecycle/cache guard survives this wait. The
                    // unchanged event is retried under every current fence.
                    let settled = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                        drop(wait.fence.lock_observation_matching(|row| {
                            wait.custody
                                .as_ref()
                                .is_none_or(|custody| custody.matches(row))
                        })?);
                        Ok(())
                    })
                    .await;
                    if settled.is_err() {
                        return OwnedPublication::Superseded;
                    }
                }
            }
        }
    }

    async fn try_publish_owned(
        &self,
        lease: &Lease,
        event: &mut Event,
    ) -> Result<OwnedPublication, PublicationWait> {
        let mut workers = self.workers.lock().await;
        let lifecycle = lock_recover(&self.lifecycle);
        if lifecycle.current_lease(&self.session_id).as_ref() != Some(lease) {
            return Ok(OwnedPublication::Superseded);
        }
        let handle = match workers.get_mut(&self.session_id) {
            Some(handle) if handle.lease == *lease => Some(handle),
            Some(_) => return Ok(OwnedPublication::Superseded),
            None => {
                if lifecycle.phase(&self.session_id) != WorkerPhase::Stopping
                    || !lease.retirement().has_retained_drain()
                {
                    return Ok(OwnedPublication::Superseded);
                }
                None
            }
        };
        let native_owner = handle
            .as_ref()
            .and_then(|handle| handle.client.connected_runner())
            .or_else(|| lease.spawned_runner());
        let retiring_identity = lease.retirement().identity_scope();
        let binding_checkpoint = lease.retirement().binding_checkpoint();
        if let (Some(change), Some(owner)) = (
            crate::acp::state::AcpSessionChange::from_event(event),
            native_owner,
        ) {
            if owner.birth_fingerprint.is_some() {
                if let Ok(Some(original)) = lease.native_scope() {
                    let store = handle
                        .as_ref()
                        .and_then(|handle| handle.client.native_store.clone())
                        .or_else(|| lease.retirement().binding_store(owner));
                    lease
                        .retirement()
                        .record_binding(super::QualifiedBindingReceipt {
                            source_context: original.context,
                            producer: lease.epoch(),
                            owner,
                            change,
                            store,
                        });
                }
            }
        }
        let native_scope = if let Some(custody) = &retiring_identity {
            if native_owner != Some(custody.owner) {
                return Ok(OwnedPublication::Superseded);
            }
            Some(custody.scope.clone())
        } else if let Some(checkpoint) = &binding_checkpoint {
            Some(checkpoint.destination.clone())
        } else {
            match lease.native_scope() {
                Ok(scope) => scope,
                Err(()) => return Ok(OwnedPublication::Superseded),
            }
        };
        let _native_lifecycle = match native_scope.as_ref() {
            Some(scope) => {
                match scope.fence.try_lock_observation_matching(|row| {
                    retiring_identity
                        .as_ref()
                        .is_none_or(|custody| custody.matches(row))
                        && binding_checkpoint.as_ref().is_none_or(|checkpoint| {
                            checkpoint
                                .destination
                                .choice
                                .as_ref()
                                .is_some_and(|choice| choice.matches(row))
                        })
                }) {
                    Ok(Some(guard)) => Some(guard),
                    Ok(None) => {
                        return Err(PublicationWait {
                            fence: Arc::clone(&scope.fence),
                            custody: retiring_identity,
                        })
                    }
                    Err(_) => return Ok(OwnedPublication::Superseded),
                }
            }
            None => None,
        };
        if self.identity_only || retiring_identity.is_some() || binding_checkpoint.is_some() {
            match event {
                // Custody preserves only the correlated binding, never channel
                // readiness, turn/status edges or actionable native controls.
                Event::AcpSessionAssigned {
                    connection_ready, ..
                } => *connection_ready = false,
                _ => return Ok(OwnedPublication::Discarded),
            }
        }
        if let Some(handle) =
            handle.filter(|_| retiring_identity.is_none() && binding_checkpoint.is_none())
        {
            match event {
                Event::AcpSessionAssigned { acp_session_id, .. } => {
                    handle.native_session_id = Some(acp_session_id.clone());
                    if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                        spawn_config.stored_acp_session_id = Some(acp_session_id.clone());
                        spawn_config.seed_history_replay = false;
                    }
                    if self.clear_pending_context_reset() {
                        self.notify.notify_waiters();
                    }
                }
                Event::SessionContextReset { .. } => {
                    handle.native_session_id = None;
                    if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                        spawn_config.stored_acp_session_id = None;
                        spawn_config.fork_from = None;
                    }
                }
                _ => {}
            }
        }
        if let Event::PromptRuntimeError {
            evidence: Some(evidence),
            ..
        } = event
        {
            evidence.generation = Some(crate::session::ErrorEvidenceGeneration::AcpWorker(
                lease.epoch(),
            ));
        }
        let native_subject = native_scope.as_ref().map(|scope| {
            crate::acp::state::NativeEventSubject::channel(scope.context, native_owner, event)
        });
        if let Some(subject) = native_subject {
            event.set_native_subject(subject);
        }
        self.sink.publish_from_worker(
            &self.session_id,
            next_seq(&self.next_seqs, &self.session_id),
            event,
            lease.epoch(),
            native_subject,
        );
        Ok(OwnedPublication::Published)
    }

    async fn pump(&mut self, inbound: &mut mpsc::Receiver<Event>, lease: &Lease) -> StreamEnd {
        let mut end = StreamEnd::default();
        let mut established = false;
        while let Some(mut event) = inbound.recv().await {
            if let Event::SessionContextReset {
                fork_rejection: Some(proof),
                ..
            } = &event
            {
                let receipt = {
                    let workers = self.workers.lock().await;
                    let lifecycle = lock_recover(&self.lifecycle);
                    let original_custody = workers
                        .get(&self.session_id)
                        .filter(|handle| handle.lease == *lease)
                        .is_some_and(|handle| {
                            handle.client.connected_runner() == Some(proof.owner)
                        })
                        || lease.retirement().retains_original_client(proof.owner)
                        || lease.retirement().fork_rejection().is_some();
                    // Existing custody accepts only the very same proof Arc.
                    if original_custody
                        && lifecycle.current_lease(&self.session_id).as_ref() == Some(lease)
                    {
                        lease.retirement().receive_fork_rejection(lease, proof)
                    } else {
                        None
                    }
                };
                if let Some(receipt) = receipt {
                    #[cfg(test)]
                    if let Some(observed) = receipt.drain_observer.lock().unwrap().take() {
                        let _ = observed.send(());
                    }
                    end.fork_rejection = Some(receipt.clone());
                    let lifecycle = self.lifecycle.clone();
                    let original = lease.clone();
                    crate::task_util::spawn_supervised(
                        "acp.fork_rejection_checkpoint",
                        crate::task_util::PanicPolicy::Log,
                        async move {
                            super::fork_rejection::checkpoint(&lifecycle, &original, &receipt).await
                        },
                    );
                }
            }
            let retiring = self.identity_only || lease.retirement().binding_checkpoint().is_some();
            if retiring {
                self.context_reset = None;
                if !matches!(
                    event,
                    Event::AcpSessionAssigned { .. }
                        | Event::SessionContextReset { .. }
                        | Event::SessionCleared
                ) {
                    continue;
                }
            }
            if let Event::AcpSessionAssigned { acp_session_id, .. } = &event {
                if let Some(pending) = self.context_reset.take() {
                    let mut reset = Event::SessionContextReset {
                        reason: pending.reason,
                        fork_rejection: None,
                    };
                    match self.publish_owned(lease, &mut reset).await {
                        OwnedPublication::Published => {}
                        OwnedPublication::Discarded => continue,
                        OwnedPublication::Superseded => {
                            end.superseded = true;
                            return end;
                        }
                    }
                    let id = self.session_id.clone();
                    let assigned = acp_session_id.clone();
                    let generation = lease.epoch();
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
                        let mut failure = Event::AgentStartupError {
                            message: format!(
                                "Could not commit the isolated native context: {error}"
                            ),
                            native_subject: None,
                        };
                        match self.publish_owned(lease, &mut failure).await {
                            OwnedPublication::Published => {}
                            OwnedPublication::Discarded => continue,
                            OwnedPublication::Superseded => {
                                end.superseded = true;
                                return end;
                            }
                        }
                        end.startup_failed = true;
                        let client = self
                            .workers
                            .lock()
                            .await
                            .get(&self.session_id)
                            .filter(|handle| handle.lease == *lease)
                            .map(|handle| Arc::clone(&handle.client));
                        if let Some(client) = client {
                            let _ = client.shutdown().await;
                        }
                        return end;
                    }
                }
            }
            match self.publish_owned(lease, &mut event).await {
                OwnedPublication::Published => {}
                OwnedPublication::Discarded => continue,
                OwnedPublication::Superseded if retiring => continue,
                OwnedPublication::Superseded => {
                    end.superseded = true;
                    return end;
                }
            }
            if retiring || lease.retirement().identity_scope().is_some() {
                continue;
            }
            match &event {
                Event::Stopped { reason } => match reason.as_str() {
                    "agent_unresponsive" | "prompt_orphaned" | "user_forced" => {
                        end.agent_unresponsive = true;
                    }
                    "rate_limited" if !established => end.startup_rate_limited = true,
                    _ => {}
                },
                Event::AgentStartupError { .. } if !established => end.startup_failed = true,
                Event::AcpSessionAssigned { .. } => {
                    established = true;
                    end.startup_rate_limited = false;
                }
                _ => {}
            }
        }
        end
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
    async fn drop_handle(&self, lease: &Lease) {
        let retirement = loop {
            match self.try_drop_handle(lease).await {
                Ok(retirement) => break retirement,
                Err(()) if self.wait_for_registry(lease).await => {}
                Err(()) => return,
            }
        };
        if let Some(lease) = retirement {
            super::teardown::retire_registered_clients(&self.lifecycle, &lease).await;
            let settlement =
                super::teardown::tear_down_lease(&*self.process_control, &lease, false).await;
            self.settle_retirement(&lease, settlement).await;
        }
        if self.clear_pending_context_reset() {
            self.notify.notify_waiters();
        }
    }

    async fn wait_for_registry(&self, lease: &Lease) -> bool {
        let cancellation = lease.cancellation();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => false,
            _ = tokio::time::sleep(Duration::from_millis(50)) =>
                lock_recover(&self.lifecycle).current_lease(&self.session_id).as_ref() == Some(lease),
        }
    }

    async fn try_drop_handle(&self, lease: &Lease) -> Result<Option<Lease>, ()> {
        let mut workers = self.workers.lock().await;
        let mut lifecycle = lock_recover(&self.lifecycle);
        let Some((current, identity)) = lifecycle.running(&self.session_id) else {
            return Ok(None);
        };
        if &current != lease
            || workers
                .get(&self.session_id)
                .is_some_and(|handle| handle.lease != *lease)
        {
            return Ok(None);
        }
        let Ok(Some(registry)) = worker_registry::try_registry_lock(&self.session_id) else {
            #[cfg(test)]
            if let Some(entered) = DEFERRED_REGISTRY_OBSERVED.lock().unwrap().take() {
                let _ = entered.send(());
            }
            return Err(());
        };
        let owner_ended = identity.is_some_and(|owner| {
            owner.birth_fingerprint.is_some() && !self.process_control.is_alive(owner)
        });
        let requested = match super::launch::claimed_stop_for_lease(&registry, lease, identity) {
            Ok(requested) => requested.is_some(),
            Err(_) if owner_ended => false,
            Err(_) => {
                #[cfg(test)]
                if let Some(entered) = DEFERRED_REGISTRY_OBSERVED.lock().unwrap().take() {
                    let _ = entered.send(());
                }
                return Err(());
            }
        };
        let record = match registry.record() {
            Ok(record) => record,
            Err(_) if owner_ended => None,
            Err(_) => return Err(()),
        };
        let needs_retirement = requested || owner_ended
            || record.as_ref().is_some_and(|record|
                record.agent_key == "prime-agent" || record.prime_runtime.is_some())
            || workers.get(&self.session_id).is_some_and(|handle| matches!(
                &handle.kind, WorkerKind::Runner { spawn_config } if spawn_config.uses_native_prime()));
        if !needs_retirement {
            drop(lifecycle);
            workers.remove(&self.session_id);
            self.detach_stopped_worker_work();
            lock_recover(&self.lifecycle).release_running(lease);
            return Ok(None);
        }
        let decision = if requested {
            lifecycle.begin_stop(&self.session_id, "user_stopped", false)
        } else {
            lifecycle.begin_connection_retirement(lease)
        };
        let crate::acp::runner_lifecycle::StopDecision::TearDown { lease, identity } = decision
        else {
            return Ok(None);
        };
        if let Some(handle) = workers.remove(&self.session_id) {
            super::teardown::capture_adapter_route_from_record(
                &lease,
                &handle.client,
                identity,
                record.as_ref(),
            );
            lease.retirement().record_worker(handle);
        }
        Ok(Some(lease))
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

    /// Decide whether the closed worker respawns, publishing why when it does not.
    async fn approve_respawn(&self, lease: &Lease) -> Option<SpawnConfig> {
        let session_id = &self.session_id;
        match restart_decision(&self.workers, &self.lifecycle, session_id, lease).await {
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
                let mut error = Event::AgentStartupError {
                    message: format!(
                        "ACP agent crashed more than {} times in {}s; \
                         not respawning. Use the web dashboard to retry.",
                        MAX_RESPAWNS_IN_WINDOW,
                        RESTART_WINDOW.as_secs()
                    ),
                    native_subject: None,
                };
                if !matches!(
                    self.publish_owned(lease, &mut error).await,
                    OwnedPublication::Published
                ) {
                    return None;
                }
            }
            RestartDecision::LeaveToReconciler => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "attached worker connection died; leaving the fresh spawn to the reconciler"
                );
                let workers = self.workers.lock().await;
                let lifecycle = lock_recover(&self.lifecycle);
                if lifecycle.current_lease(session_id).as_ref() != Some(lease)
                    || !workers
                        .get(session_id)
                        .is_some_and(|handle| handle.lease == *lease)
                {
                    return None;
                }
                lock_recover(&self.startup_failures)
                    .insert(session_id.clone(), StartupFailure::Retry);
            }
            RestartDecision::Gone => return None,
            RestartDecision::Fenced => {
                // Kernel-qualified exit can retire its own custody, not grant
                // the unreadable or missing file authority over a live runner.
                self.drop_handle(lease).await;
                return None;
            }
            RestartDecision::UserStopped => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "worker stop requested; settling its handle without respawn"
                );
                self.drop_handle(lease).await;
                return None;
            }
        }
        self.drop_handle(lease).await;
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
        #[cfg(test)]
        {
            let mut observed = RESPAWN_BACKOFF_OBSERVED.lock().unwrap();
            if observed.as_ref().is_some_and(|(id, _)| id == session_id) {
                let (_, entered) = observed.take().unwrap();
                let _ = entered.send(());
            }
        }
        tokio::time::sleep(RESPAWN_BACKOFF).await;
        // Retag the existing handle while both admission gates are held.
        // This is still the same drain/client; launch_epoch remains unchanged.
        let mut retry = false;
        let begun = loop {
            if retry && !self.wait_for_registry(lease).await {
                return None;
            }
            retry = true;
            let mut refused = false;
            let attempt = {
                let mut workers = self.workers.lock().await;
                if workers
                    .get(session_id)
                    .is_none_or(|handle| handle.lease != *lease)
                {
                    return None;
                }
                let mut lifecycle = lock_recover(&self.lifecycle);
                match worker_registry::try_registry_lock(session_id) {
                    Ok(Some(registry)) => {
                        if registry
                            .verify_resume(worker_registry::ResumeAuthority::Automatic, &None)
                            .is_err()
                        {
                            refused = true;
                            None
                        } else {
                            let begun = lifecycle.begin_respawn(lease);
                            if let Ok((next, _)) = &begun {
                                workers.get_mut(session_id).expect("matching handle").lease =
                                    next.clone();
                            }
                            Some(begun)
                        }
                    }
                    _ => {
                        #[cfg(test)]
                        if let Some(entered) = DEFERRED_REGISTRY_OBSERVED.lock().unwrap().take() {
                            let _ = entered.send(());
                        }
                        None
                    }
                }
            };
            if let Some(begun) = attempt {
                break begun;
            }
            if refused {
                self.drop_handle(lease).await;
                return None;
            }
        };
        let Ok((respawn_lease, mut previous)) = begun else {
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                "respawn skipped; the session's lease moved on"
            );
            return None;
        };
        let mut reservation = ResumeReservation {
            lease: respawn_lease.clone(),
            expected_lifecycle_generation: None,
            attach_record: None,
            preserve_owner: false,
            registry_deadline: tokio::time::Instant::now() + Duration::from_secs(3),
            resume_stop: None,
            resume_authority: worker_registry::ResumeAuthority::Automatic,
            lifecycle: Arc::clone(&self.lifecycle),
            notify: Arc::clone(&self.notify),
            deferred: None,
            deferred_claim: false,
            deferred_gate: None,
        };
        config.generation = respawn_lease.epoch();
        config.launch_lease = Some(respawn_lease.clone());
        if let Err(error) =
            super::fork_rejection::prepare_fork_seed(&respawn_lease, config.fork_from.as_deref())
                .await
        {
            self.fail_launch(
                &respawn_lease,
                previous,
                &config,
                AcpError::Spawn(error.to_string()),
            )
            .await;
            return None;
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
                    {
                        let _workers = self.workers.lock().await;
                        let lifecycle = lock_recover(&self.lifecycle);
                        if lifecycle.current_lease(session_id).as_ref() == Some(&respawn_lease) {
                            self.publish(Event::AgentStartupError { message: "Prime native termination is unproven; replacement remains blocked".into(), native_subject: None });
                        }
                    }
                    self.settle_retirement(&respawn_lease, settlement).await;
                }
                return None;
            }
            previous = None;
        }
        respawn_lease.record_prime(crate::acp::runner_lifecycle::PrimeOwnership::NotPrime);
        Self::refresh_launch_env(&self.session_id, &mut config).await;
        self.refuse_if_dismissed(&config).await;
        let cancelled = lock_recover(&self.lifecycle).cancel_requested(&respawn_lease);
        if cancelled.is_some() {
            if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease) {
                self.finish_cancelled(&respawn_lease, previous).await;
            }
            return None;
        }
        if let Some((wrapper, base)) = &config.wrapper_substitution {
            log_wrapper_substitution(session_id, &config.tool, wrapper, base);
        }
        let launched = (self.launcher)(config.clone(), AcpSessionId(session_id.clone())).await;
        let mut client = match launched {
            Ok(client) => client,
            Err(error @ (AcpError::RegistryBusy | AcpError::RetainedRuntimeUnavailable(_))) => {
                if respawn_lease.spawned_runner().is_some() {
                    self.retain_retiring_handle(&respawn_lease).await;
                    respawn_lease
                        .retirement()
                        .finish_registration(super::ConstructionFinish::new(
                            WorkerKind::Runner {
                                spawn_config: Box::new(config),
                            },
                            self.context_reset.clone(),
                            None,
                            true,
                        ));
                }
                reservation.preserve_refused_owner();
                debug!(target: "acp.supervisor", session = %session_id, %error,
                    "runtime admission deferred; retaining its owned construction");
                return None;
            }
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
            let mut lifecycle = lock_recover(&self.lifecycle);
            let Ok(Some(registry)) = worker_registry::try_registry_lock(session_id) else {
                self.defer_respawn_install(
                    &mut guard,
                    &mut reservation,
                    client,
                    inbound,
                    config,
                    identity,
                );
                return None;
            };
            let requested =
                match super::launch::claimed_stop_for_lease(&registry, &respawn_lease, identity) {
                    Ok(requested) => requested.is_some(),
                    Err(_) => {
                        self.defer_respawn_install(
                            &mut guard,
                            &mut reservation,
                            client,
                            inbound,
                            config,
                            identity,
                        );
                        return None;
                    }
                };
            let install = if requested {
                lifecycle.begin_stop(session_id, "user_stopped", false);
                Err(InstallError::Cancelled {
                    reason: "user_stopped".into(),
                })
            } else {
                lifecycle.install(&respawn_lease, identity)
            };
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

    fn defer_respawn_install(
        &self,
        workers: &mut HashMap<String, super::WorkerHandle>,
        reservation: &mut ResumeReservation,
        client: Arc<crate::acp::acp_client::AcpClient>,
        inbound: mpsc::Receiver<Event>,
        config: SpawnConfig,
        identity: Option<RunnerIdentity>,
    ) {
        let lease = reservation.lease();
        if let Some(handle) = workers.remove(&self.session_id) {
            assert_eq!(
                &handle.lease, lease,
                "only the original respawn observation is transferred"
            );
            lease.retirement().record_worker(handle);
        }
        reservation.deferred = Some(super::DeferredConstruction::Installation(Box::new(
            super::ConstructionInstallation {
                client,
                inbound,
                identity,
                finish: super::ConstructionFinish::new(
                    WorkerKind::Runner {
                        spawn_config: Box::new(config),
                    },
                    self.context_reset.clone(),
                    None,
                    true,
                ),
            },
        )));
    }

    /// Re-resolve what may have changed since the first launch: model pins,
    /// host hook env, and MCP servers.
    async fn refresh_launch_env(session_id: &str, config: &mut SpawnConfig) {
        let agent = config.agent_key.clone();
        let profile = config.source_profile.clone().unwrap_or_default();
        let cwd = config.cwd.clone();
        let resolved = tokio::task::spawn_blocking(move || {
            crate::session::config::repo_config::resolve_config_with_repo_or_warn(&profile, &cwd)
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
                        let (target, native) = config.wrapper_substitution.as_ref().map_or(
                            (config.agent_key.as_str(), config.agent_key.as_str()),
                            |(wrapper, base)| (wrapper.as_str(), base.as_str()),
                        );
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
            &config.agent_key,
            session_id,
            config.source_profile.clone(),
            config.cwd.clone(),
            config.host_environment.clone(),
            claude_config_dir,
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
            let _workers = self.workers.lock().await;
            let lifecycle = lock_recover(&self.lifecycle);
            if lifecycle.current_lease(session_id).as_ref() == Some(respawn_lease) {
                if matches!(e, AcpError::IncompatibleAgent(_)) {
                    lock_recover(&self.incompatible_binaries)
                        .insert(session_id.clone(), config.spec.command.clone());
                }
                if !publish_rejection(&e, |event| self.publish(event)) {
                    self.publish(Event::AgentStartupError {
                        message: format!("ACP agent respawn failed: {e}"),
                        native_subject: None,
                    });
                }
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

async fn restart_decision(
    workers: &Workers,
    lifecycle: &std::sync::Mutex<LifecycleTable>,
    session_id: &str,
    lease: &Lease,
) -> RestartDecision {
    let mut guard = workers.lock().await;
    let Some(handle) = guard.get_mut(session_id) else {
        debug!(
            target: "acp.supervisor",
            session = %session_id,
            "restart_decision: worker entry gone (shutdown / delete)"
        );
        return RestartDecision::Gone;
    };
    let lifecycle = lock_recover(lifecycle);
    if handle.lease != *lease || lifecycle.current_lease(session_id).as_ref() != Some(lease) {
        return RestartDecision::Gone;
    }
    let runner_managed = matches!(
        handle.kind,
        WorkerKind::Runner { .. } | WorkerKind::Attached
    );
    if runner_managed {
        let Ok(Some(registry)) = worker_registry::try_registry_lock(session_id) else {
            return RestartDecision::Fenced;
        };
        let identity = lifecycle
            .running(session_id)
            .and_then(|(_, identity)| identity);
        match super::launch::claimed_stop_for_lease(&registry, lease, identity) {
            Ok(Some(_)) => return RestartDecision::UserStopped,
            Err(_) => return RestartDecision::Fenced,
            Ok(None) => {}
        }
        if !matches!(registry.record(), Ok(Some(_))) {
            return RestartDecision::Fenced;
        }
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
    async fn registry_contention_at_channel_close_eventually_settles_the_dead_original() {
        let (_home, tmp) = isolate_home();
        let id = "drain-observation-recovery";
        let mut runner = HeldAttachRunner::ready(id, tmp.path(), 17, None, None).await;
        runner.record.scope = worker_registry::ScopeOwnership::Unscoped;
        worker_registry::save(&runner.record).unwrap();
        let control = Arc::new(ObservedProcessControl::default());
        let (sink, store, _frames, _database) = channel_sink();
        sink.publish(
            id,
            1,
            &Event::AgentMessageChunk {
                text: "history before registry refusal".into(),
                message_id: None,
            },
        );
        let sup = Supervisor::new(sink).with_process_control(control.clone());
        sup.hydrate_seqs([(id.to_owned(), 1)]);
        sup.attach(id.into(), tmp.path().into(), vec![], false, None)
            .await
            .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), runner.resumed_session())
                .await
                .unwrap(),
            runner.record.stored_acp_session_id.clone().unwrap()
        );
        let ended = {
            let workers = sup.workers.lock().await;
            let handle = workers.get(id).unwrap();
            handle.drain_task.abort_handle()
        };
        let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
        *DEFERRED_REGISTRY_OBSERVED.lock().unwrap() = Some(observed_tx);
        let registry = worker_registry::try_registry_lock(id).unwrap().unwrap();
        runner.exit_peer();
        tokio::time::timeout(Duration::from_secs(5), observed_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(
            control.signals().is_empty(),
            "a refused observation cannot signal the captured actor"
        );
        drop(registry);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !ended.is_finished() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the original decision driver must progress after exclusion is released");
        assert!(
            runner_record(id).await.is_none(),
            "the dead original's registry record must be settled"
        );
        let reservation = sup
            .reserve_spawn(
                id,
                super::super::ResumeKind::Spawn,
                worker_registry::ResumeAuthority::Explicit,
            )
            .await;
        assert!(
            reservation.is_ok(),
            "the qualified dead original must release its occupied slot: {reservation:?}"
        );
        drop(reservation);
        assert!(store
            .replay_from(id, 0)
            .iter()
            .any(|(_, event)| matches!(event,
            Event::AgentMessageChunk { text, .. } if text == "history before registry refusal")));
        assert!(!store.replay_from(id, 0).iter().any(|(_, event)| matches!(
            event,
            Event::SessionContextReset { .. } | Event::AgentStartupError { .. }
        )));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_flock_contention_releases_global_guards_and_rechecks_before_publication() {
        let (_home, _temp) = isolate_home();
        for (supersede, profile_writer) in [(false, false), (true, false), (false, true)] {
            let storage = crate::session::Storage::new_unwatched("default").unwrap();
            let mut row = crate::session::Instance::new("native contention", "/tmp");
            row.source_profile = "default".into();
            row.view = crate::session::View::Structured;
            storage
                .update(|rows, _| {
                    rows.push(row.clone());
                    Ok(())
                })
                .unwrap();
            // Seed first, then capture an actual physically bound profile.
            let storage = Arc::new(crate::session::Storage::open_reference("default").unwrap());
            let (sink, store, mut frames, _db) = channel_sink();
            let supervisor = Supervisor::new(sink.clone());
            let lease = supervisor.test_install_stdio(&row.id).await;
            lease.set_admission(Some(super::super::LaunchAdmission {
                store: storage.clone(),
                generation: row.lifecycle_generation,
                namespace: None,
                native_scope: Some(
                    crate::acp::state::NativeObservationScope::capture(&row, &storage).unwrap(),
                ),
            }));
            let drain = Drain {
                session_id: row.id.clone(),
                identity_only: false,
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
            };
            let native = if profile_writer {
                crate::session::acquire_storage_flock(
                    storage.sessions_path().parent().unwrap(),
                    crate::session::STORAGE_LOCK_FILENAME,
                )
                .unwrap()
            } else {
                storage.acquire_instance_lifecycle_lock(&row.id).unwrap()
            };
            let mut event = Event::PromptRuntimeError {
                message: "retain this completed outcome exactly once".into(),
                evidence: None,
                native_subject: None,
            };
            let publishing = drain.publish_owned(&lease, &mut event);
            tokio::pin!(publishing);
            assert!(futures_util::poll!(&mut publishing).is_pending());
            assert!(supervisor.workers.try_lock().is_ok());
            assert!(supervisor.lifecycle.try_lock().is_ok());
            assert!(
                supervisor.capture_retirement(&row.id).is_ok(),
                "native-flock owners must still be able to capture lifecycle custody"
            );
            assert!(!supervisor.next_seqs.lock().unwrap().contains_key(&row.id));
            assert!(store.replay_from(&row.id, 0).is_empty());
            if supersede {
                storage
                    .update(|rows, _| {
                        rows.iter_mut()
                            .find(|current| current.id == row.id)
                            .unwrap()
                            .lifecycle_generation += 1;
                        Ok(())
                    })
                    .unwrap();
            }
            drop(native);
            let result = tokio::time::timeout(Duration::from_secs(5), publishing)
                .await
                .unwrap();
            if supersede {
                assert!(matches!(result, OwnedPublication::Superseded));
                assert!(!supervisor.next_seqs.lock().unwrap().contains_key(&row.id));
                assert!(store.replay_from(&row.id, 0).is_empty());
                assert!(matches!(
                    frames.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ));
            } else {
                assert!(matches!(result, OwnedPublication::Published));
                let frame = frames.try_recv().unwrap();
                assert_eq!(frame.seq, 1);
                assert!(
                    matches!(frame.event.as_ref(), Event::PromptRuntimeError { message, .. }
                    if message == "retain this completed outcome exactly once")
                );
                assert_eq!(store.replay_from(&row.id, 0).len(), 1);
                assert!(matches!(
                    frames.try_recv(),
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                ));
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn an_absent_worker_without_retained_custody_cannot_publish() {
        use crate::acp::state::{AcpState, AgentName};
        let (_home, _temp) = isolate_home();
        for stopping in [false, true] {
            let id = if stopping {
                "s-uncaptured-stopping"
            } else {
                "s-uninstalled-running"
            };
            let (sink, store, mut frames, _db) = channel_sink();
            sink.control_cache.get_or_hydrate(id, || {
                (
                    AcpState::new(AcpSessionId(id.into()), AgentName("gemini".into()), None),
                    0,
                )
            });
            let supervisor = Supervisor::new(sink.clone());
            let lease = supervisor.test_install_stdio(id).await;
            let removed = {
                let mut workers = supervisor.workers.lock().await;
                let removed = workers.remove(id).unwrap();
                if stopping {
                    assert!(matches!(
                        lock_recover(&supervisor.lifecycle).begin_stop(id, "user_stopped", false),
                        crate::acp::runner_lifecycle::StopDecision::TearDown { .. }
                    ));
                }
                removed
            };
            removed.drain_task.abort();
            drop(removed);
            assert_eq!(
                lock_recover(&supervisor.lifecycle)
                    .current_lease(id)
                    .as_ref(),
                Some(&lease)
            );
            assert!(!lease.retirement().has_retained_drain());
            let (events, inbound) = mpsc::channel(1);
            events
                .send(Event::AcpSessionAssigned {
                    acp_session_id: "uncaptured-native-sid".into(),
                    connection_ready: false,
                    native_subject: None,
                })
                .await
                .unwrap();
            drop(events);
            let drain = supervisor.start_drain_task(id.into(), lease, inbound, None);
            tokio::time::timeout(Duration::from_secs(5), drain)
                .await
                .unwrap()
                .unwrap();
            assert!(store.replay_from(id, 0).is_empty());
            assert!(!supervisor.next_seqs.lock().unwrap().contains_key(id));
            assert!(matches!(
                frames.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            ));
            let state = sink
                .control_cache
                .get_or_hydrate(id, || panic!("warm state must survive"));
            assert_eq!(state.last_seq, 0);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn superseded_worker_errors_never_allocate_persist_or_fold() {
        use crate::acp::state::{AcpSessionId, AcpState, AgentName};
        use crate::session::{
            ErrorEvidenceGeneration, ErrorEvidenceSource, ErrorOperation, OperationErrorEvidence,
            Status,
        };
        let (_home, _temp) = isolate_home();
        let id = "s-evidence-fence";
        let (sink, store, mut frames, _db) = channel_sink();
        sink.control_cache.get_or_hydrate(id, || {
            (
                AcpState::new(AcpSessionId(id.into()), AgentName("gemini".into()), None),
                0,
            )
        });
        let supervisor = Supervisor::new(sink.clone());
        let old = supervisor.test_install_stdio(id).await;
        let error = || Event::PromptRuntimeError {
            message: "operation failed".into(),
            evidence: Some(OperationErrorEvidence::new(
                ErrorEvidenceSource::LocalPromptResult,
                ErrorOperation::Prompt,
            )),
            native_subject: None,
        };
        let (queued, inbound) = mpsc::channel(4);
        queued.send(error()).await.unwrap();
        queued
            .send(Event::Stopped {
                reason: "prompt_failed".into(),
            })
            .await
            .unwrap();
        supervisor.test_remove_worker(id).await;
        let replacement = supervisor.test_install_stdio(id).await;
        assert_ne!(old, replacement);
        let old_drain = supervisor.start_drain_task(id.into(), old.clone(), inbound, None);
        tokio::time::timeout(Duration::from_secs(5), old_drain)
            .await
            .unwrap()
            .unwrap();
        let (closed, inbound) = mpsc::channel(1);
        drop(closed);
        let empty_old_drain = supervisor.start_drain_task(id.into(), old, inbound, None);
        tokio::time::timeout(Duration::from_secs(5), empty_old_drain)
            .await
            .unwrap()
            .unwrap();
        assert!(supervisor
            .workers
            .lock()
            .await
            .get(id)
            .unwrap()
            .restart_history
            .is_empty());
        assert!(store.replay_from(id, 0).is_empty());
        assert!(!supervisor.next_seqs.lock().unwrap().contains_key(id));
        assert!(matches!(
            frames.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        let state = sink
            .control_cache
            .get_or_hydrate(id, || panic!("the warm fold must survive"));
        assert_eq!(state.last_seq, 0);
        assert_eq!(state.operation_error, None);
        assert_eq!(supervisor.worker_state(id).await, AcpWorkerState::Running);
        assert!(supervisor.take_startup_failures().is_empty());

        let (events, inbound) = mpsc::channel(4);
        let drain = supervisor.start_drain_task(id.into(), replacement.clone(), inbound, None);
        events
            .send(crate::acp::state::test_support::prompt("explicit request"))
            .await
            .unwrap();
        events.send(error()).await.unwrap();
        events
            .send(Event::Stopped {
                reason: "prompt_failed".into(),
            })
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let frame = frames.recv().await.unwrap();
                if matches!(&*frame.event, Event::Stopped { reason } if reason == "prompt_failed") {
                    assert_eq!(frame.seq, 3, "stale evidence consumed no sequence");
                    break;
                }
            }
        })
        .await
        .unwrap();
        let state = sink
            .control_cache
            .get_or_hydrate(id, || panic!("fold remains warm"));
        assert_eq!(state.sidebar_status(), Status::Error);
        assert!(!state.turn_active);
        assert_eq!(
            state.operation_error.unwrap().generation,
            Some(ErrorEvidenceGeneration::AcpWorker(replacement.epoch()))
        );
        assert_eq!(store.replay_from(id, 0).len(), 3);
        assert_eq!(supervisor.worker_state(id).await, AcpWorkerState::Running);
        assert!(supervisor
            .workers
            .lock()
            .await
            .get(id)
            .unwrap()
            .restart_history
            .is_empty());
        drain.abort();
        let _ = drain.await;
    }

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
                connection_ready: false,
                native_subject: None,
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
        record.runner_identity = Some(attested_test_birth(4242, record.generation));
        record.scope = worker_registry::ScopeOwnership::Unscoped;
        worker_registry::save(&record).unwrap();
        let (launcher, launches) = crate::server::test_support::counting_failing_launcher();
        let control =
            Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
        let sup = Supervisor::new(VecSink::new())
            .with_process_control(control.clone())
            .with_launcher(launcher);
        let mut config = runner_config(socket);
        config.agent_key = "prime-agent".into();
        let identity = RunnerIdentity::from_record(&record);
        let lease = sup.test_install_runner(id, config, Some(identity)).await;
        assert_eq!(sup.worker_state(id).await, AcpWorkerState::Running);
        let (sender, inbound) = mpsc::channel(4);
        let (backoff, entered) = tokio::sync::oneshot::channel();
        *RESPAWN_BACKOFF_OBSERVED.lock().unwrap() = Some((id.into(), backoff));
        let drain = sup.start_drain_task(id.into(), lease, inbound, None);
        drop(sender);
        tokio::time::timeout(Duration::from_secs(5), entered)
            .await
            .expect("the qualified original reaches the actual respawn backoff")
            .unwrap();
        // The route disappears after approval, before the post-backoff recheck.
        std::fs::remove_file(worker_registry::record_path(id).unwrap()).unwrap();
        tokio::time::advance(RESPAWN_BACKOFF).await;
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("respawn drain must reach its terminal ownership decision")
            .unwrap();
        assert!(sup.is_owned(id).await);
        let retained = lock_recover(&sup.lifecycle).current_lease(id);
        let signals = control.signals();
        let releases = control.releases();
        let refused = sup
            .reserve_spawn(
                id,
                super::super::ResumeKind::Spawn,
                worker_registry::ResumeAuthority::Explicit,
            )
            .await;
        assert!(
            refused.is_err(),
            "unresolved ownership must refuse replacement"
        );
        assert_eq!(lock_recover(&sup.lifecycle).current_lease(id), retained);
        assert_eq!(control.signals(), signals);
        assert_eq!(control.releases(), releases);
        assert_eq!(launches.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_decision_burns_the_budget_and_fences_missing_ownership() {
        let (_home, tmp) = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let socket = tmp.path().join("budget.sock");
        worker_registry::save(&worker_record("s-1", std::process::id(), socket.clone())).unwrap();
        let first_lease = sup
            .test_install_runner("s-1", runner_config(socket.clone()), None)
            .await;
        let stopped_lease = sup
            .test_install_runner("s-stop", runner_config(socket), None)
            .await;

        for i in 0..MAX_RESPAWNS_IN_WINDOW {
            assert!(
                matches!(
                    restart_decision(&sup.workers, &sup.lifecycle, "s-1", &first_lease).await,
                    RestartDecision::Respawn(_)
                ),
                "decision #{i} should be Respawn",
            );
        }
        assert!(matches!(
            restart_decision(&sup.workers, &sup.lifecycle, "s-1", &first_lease).await,
            RestartDecision::BudgetBurned
        ));
        let decision =
            restart_decision(&sup.workers, &sup.lifecycle, "s-stop", &stopped_lease).await;
        assert!(
            matches!(decision, RestartDecision::Fenced),
            "missing ownership is not a user-stop receipt: {decision:?}"
        );
    }

    /// An `Attached` worker (reattached to a runner a previous daemon owned)
    /// has no spawn config to respawn from. A crash after the session was
    /// established must hand the session to the reconciler rather than park it
    /// behind the crash-loop banner. A missing record permits the existing
    /// non-destructive park, but is not a user-stop receipt.
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
                // Only the readable original record permits management reattachment.
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
                    connection_ready: false,
                    native_subject: None,
                })
                .await
                .unwrap();
            inbound_tx
                .send(Event::AgentStartupError {
                    message: "ACP connection failed: native binary failed to launch".into(),
                    native_subject: None,
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
                "{id}: the closed management handle is parked without a destructive claim"
            );
            assert_eq!(
                sup.take_startup_failures(),
                if expect_rearm {
                    vec![(id.to_string(), super::super::StartupFailure::Retry)]
                } else {
                    Vec::new()
                },
                "{id}: only a registry-backed attached crash re-arms"
            );
            let banner_published = sink.frames.lock().unwrap().iter().any(|(_, _, ev)| {
                matches!(ev, Event::AgentStartupError { message, .. } if message.contains("crashed more than"))
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
                0,
                "{id}: no typed user-stop receipt was supplied"
            );
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
            native_subject: None,
        };
        let established = Event::AcpSessionAssigned {
            acp_session_id: "acp-1".into(),
            connection_ready: false,
            native_subject: None,
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
            let expected_failures = if startup_failure {
                vec![(id.to_string(), super::super::StartupFailure::Retry)]
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
                    matches!(ev, Event::AgentStartupError { message, .. } if message.contains("crashed more than"))
                })
                .count();
            assert_eq!(crash_messages, usize::from(expect_crash_message), "{id}");
        }
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
                    connection_ready: false,
                    native_subject: None,
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
            assert!(events.iter().any(|(_, _, event)| matches!(event, Event::AgentStartupError { message, .. } if message.contains("Could not commit the isolated native context"))));
            assert!(!events
                .iter()
                .any(|(_, _, event)| matches!(event, Event::AcpSessionAssigned { .. })));
        }
        assert_eq!(
            supervisor.take_startup_failures(),
            vec![(id.to_string(), super::super::StartupFailure::Retry)]
        );
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
        sup.shutdown("s-resp").await.expect("cancel");
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
            vec!["user_stopped".to_string()],
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
        sup.shutdown("s-crash").await.expect("shutdown");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn respawn_drops_a_withdrawn_hook_route_and_realigns_native_mcp() {
        use agent_client_protocol::schema::v1::McpServer;

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
    async fn respawn_reapplies_selected_claude_store_before_native_mcp_discovery() {
        use agent_client_protocol::schema::v1::McpServer;

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

        sup.shutdown("s-store").await.expect("shutdown");
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

        sup.shutdown("s-declared").await.expect("shutdown");
        held_senders.lock().unwrap().clear();
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain should stop")
            .unwrap();
    }
}
