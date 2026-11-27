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
    refresh_spawn_model_effort, resolve_mcp_servers, McpHostEnv, McpPaths, McpSandboxScope,
};
use super::teardown::{settle_sweeping_if_owed, tear_down_runner, wait_for_exit};
use super::{
    lock_recover, with_publication_seq, BroadcastSink, Launcher, PendingContextReset,
    ResumeReservation, SeqMap, SharedSet, Supervisor, SupervisorError, WorkerKind, Workers,
    MAX_RESPAWNS_IN_WINDOW, RESPAWN_BACKOFF, RESTART_WINDOW,
};
use crate::acp::acp_client::{AcpClient, AcpError, ControlLoss, SpawnConfig};
use crate::acp::event_sender::{AcpEventData, EventScope, SdkEvent};
use crate::acp::runner_lifecycle::{
    InstallError, Lease, LifecycleTable, PrimeOwnership, ProcessControl, RunnerIdentity,
};

/// Bounds recovery of a live runner whose control channel closed.
const CONTROL_REATTACH_ATTEMPTS: u32 = 3;
const CONTROL_REATTACH_DEADLINE: Duration = Duration::from_secs(3);
use crate::acp::state::{AcpSessionId, Event, StartupCause};
use crate::process::worker_registry;

#[cfg(test)]
static DEFERRED_REGISTRY_OBSERVED: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
static PUBLICATION_FLOCK_OBSERVER: std::sync::Mutex<
    Option<(String, std::sync::mpsc::Sender<std::path::PathBuf>)>,
> = std::sync::Mutex::new(None);

#[cfg(test)]
static RESPAWN_BACKOFF_OBSERVED: std::sync::Mutex<
    Option<(String, tokio::sync::oneshot::Sender<()>)>,
> = std::sync::Mutex::new(None);

impl<S: BroadcastSink> Supervisor<S> {
    pub(super) fn start_drain_task(
        &self,
        session_id: String,
        lease: Lease,
        inbound: mpsc::Receiver<SdkEvent>,
        context_reset: Option<PendingContextReset>,
    ) -> JoinHandle<()> {
        self.spawn_drain(session_id, lease, inbound, context_reset, false, None)
    }

    pub(super) fn start_retiring_drain(
        &self,
        session_id: String,
        lease: Lease,
        inbound: mpsc::Receiver<SdkEvent>,
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
        inbound: mpsc::Receiver<SdkEvent>,
        context_reset: Option<PendingContextReset>,
        identity_only: bool,
        registered: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> JoinHandle<()> {
        let drain = Drain {
            history_scope: lease.native_scope().ok().flatten(),
            projection: lease.retirement().establishment_projection(),
            session_id,
            sink: Arc::clone(&self.sink),
            workers: Arc::clone(&self.workers),
            next_seqs: Arc::clone(&self.next_seqs),
            incompatible_binaries: Arc::clone(&self.incompatible_binaries),
            lifecycle: Arc::clone(&self.lifecycle),
            process_control: Arc::clone(&self.process_control),
            launcher: Arc::clone(&self.launcher),
            notify: Arc::clone(&self.worker_notify),
            startup_requeues: Arc::clone(&self.startup_requeues),
            rate_limit_wake: Arc::clone(&self.rate_limit_wake),
            recovery: None,
            pending_context_resets: Arc::clone(&self.pending_context_resets),
            context_reset: if identity_only { None } else { context_reset },
            identity_only,
            respawned_in_place: Arc::clone(&self.respawned_in_place),
            #[cfg(test)]
            receive_owner: (!identity_only).then(|| lease.retirement().begin_test_receive()),
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
    history_scope: Option<crate::acp::state::NativeObservationScope>,
    projection: Option<Arc<crate::acp::event_sender::ProjectionState>>,
    sink: Arc<S>,
    workers: Workers,
    next_seqs: Arc<SeqMap>,
    incompatible_binaries: Arc<std::sync::Mutex<HashMap<String, String>>>,
    lifecycle: Arc<std::sync::Mutex<LifecycleTable>>,
    process_control: Arc<dyn ProcessControl>,
    launcher: Launcher,
    notify: Arc<tokio::sync::Notify>,
    startup_requeues: SharedSet,
    rate_limit_wake: Arc<tokio::sync::Notify>,
    recovery: Option<Recovery>,
    pending_context_resets: SharedSet,
    context_reset: Option<PendingContextReset>,
    respawned_in_place: SharedSet,
    #[cfg(test)]
    receive_owner: Option<super::teardown::TestDrainReceiveOwner>,
}

impl<S> Drain<S> {
    fn reservation_for_lease(&self, lease: Lease) -> super::ResumeReservation {
        let constructor = Some(lease.begin_original_constructor(self.notify.clone()));
        super::ResumeReservation {
            lease,
            expected_lifecycle_generation: None,
            establishment: None,
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
            constructor,
        }
    }
}

/// What a worker's event stream said before it closed.
#[derive(Default)]
struct StreamEnd {
    agent_unresponsive: bool,
    startup_rate_limited: bool,
    startup_failed: bool,
    superseded: bool,
    established: bool,
}

struct Recovery {
    loss: ControlLoss,
    attempts: u32,
}

/// Mutable stream effects have exactly one owner in ordered completion.
struct DrainCompletion<S> {
    drain: Option<Drain<S>>,
    end: StreamEnd,
    established: bool,
}

enum OwnedPublication {
    Published { current: bool },
    Discarded,
    Failed,
    Superseded,
}

enum PublicationWait {
    Native(Arc<crate::session::NativeInputFence>),
    Registry(String),
    Request {
        context: Arc<crate::acp::establishment::NativeRequestContext>,
        id: String,
    },
    Constructor {
        context: Arc<crate::acp::establishment::NativeConstructorObservation>,
        id: String,
    },
    PendingRequest {
        context: Arc<crate::acp::acp_client::pending::PendingRequestPublication>,
        id: String,
    },
}

impl PublicationWait {
    async fn settled(
        self,
        #[cfg(test)] observer: Option<std::sync::mpsc::Sender<std::path::PathBuf>>,
    ) -> bool {
        matches!(
            tokio::task::spawn_blocking(move || {
                #[cfg(test)]
                let _observer = observer.map(crate::session::observe_lock_contention_for_test);
                let (result, id) = match self {
                    Self::Native(fence) => return fence.lock_original_history().map(drop),
                    Self::Registry(id) => {
                        #[cfg(test)]
                        if let Some(entered) = DEFERRED_REGISTRY_OBSERVED.lock().unwrap().take() {
                            let _ = entered.send(());
                        }
                        return worker_registry::acquire_registry_lock(&id).map(drop);
                    }
                    Self::Request { context, id } => (context.effect_guard().map(drop), id),
                    Self::Constructor { context, id } => {
                        (context.publication_guard().map(drop), id)
                    }
                    Self::PendingRequest { context, id } => {
                        (context.publication_guard().map(drop), id)
                    }
                };
                match result {
                    Err(error) if error.is::<worker_registry::RegistryBusy>() => {
                        #[cfg(test)]
                        if let Some(entered) = DEFERRED_REGISTRY_OBSERVED.lock().unwrap().take() {
                            let _ = entered.send(());
                        }
                        worker_registry::acquire_registry_lock(&id).map(drop)
                    }
                    _ => Ok(()),
                }
            })
            .await,
            Ok(Ok(()))
        )
    }
}

#[derive(Debug)]
enum RestartDecision {
    Respawn(Box<SpawnConfig>),
    BudgetBurned,
    /// An `Attached` connection died without a spawn config. Reconsider its
    /// canonical selection through the reconciler's normal admission guards.
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
        let _receipt = self.sink.queue_persisted(
            &self.session_id,
            Arc::clone(&self.next_seqs),
            AcpEventData::from_host(event),
            None,
        );
    }

    async fn run(mut self, mut lease: Lease, mut inbound: mpsc::Receiver<SdkEvent>) {
        loop {
            let (returned, end) = self.pump(&mut inbound, &lease).await;
            self = returned;
            if self.identity_only
                || lease.retirement().binding_checkpoint().is_some()
                || end.superseded
                || (lease.cancellation().is_cancelled()
                    && lease.retirement().establishment_projection().is_some())
            {
                return;
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
            if end.startup_rate_limited {
                info!(
                    target: "acp.supervisor",
                    session = %self.session_id,
                    "startup rate-limited; leaving continuation to the quota governor"
                );
                self.drop_handle(&lease).await;
                self.rate_limit_wake.notify_one();
                return;
            }
            if end.startup_failed && self.recovery.is_none() {
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
                    lock_recover(&self.startup_requeues).insert(self.session_id.clone());
                }
                self.drop_handle(&lease).await;
                return;
            }
            if !end.agent_unresponsive {
                let (returned, next_inbound) = self.recover_control_loss(&lease, &end).await;
                self = returned;
                if let Some(next_inbound) = next_inbound {
                    inbound = next_inbound;
                    continue;
                }
            }
            self.recovery = None;
            let (returned, config) = self.approve_respawn(&lease).await;
            self = returned;
            let Some(config) = config else {
                return;
            };
            match self.respawn(&lease, config).await {
                Some((next_lease, next_inbound)) => {
                    #[cfg(test)]
                    {
                        self.receive_owner = Some(next_lease.retirement().begin_test_receive());
                    }
                    lease = next_lease;
                    inbound = next_inbound;
                    self.history_scope = lease.native_scope().ok().flatten();
                }
                None => return,
            }
        }
    }

    /// Current effects require captured authority; history retains its physical fence.
    async fn publish_owned(
        &self,
        lease: &Lease,
        data: &mut AcpEventData,
        mut replay: Option<(u64, &mut crate::acp::event_store::ReplayCommit)>,
        control_loss: Option<(&AcpClient, ControlLoss)>,
    ) -> OwnedPublication {
        loop {
            match self
                .try_publish_owned(
                    lease,
                    data,
                    replay
                        .as_mut()
                        .map(|(ordinal, replay)| (*ordinal, &mut **replay)),
                    control_loss,
                )
                .await
            {
                Ok(result) => {
                    if matches!(result, OwnedPublication::Published { current: true }) {
                        if let Event::AcpSessionAssigned { acp_session_id, .. } =
                            data.event.as_ref()
                        {
                            let mut workers = self.workers.lock().await;
                            let lifecycle = lock_recover(&self.lifecycle);
                            if lifecycle.current_lease(&self.session_id).as_ref() == Some(lease) {
                                if let Some(handle) = workers
                                    .get_mut(&self.session_id)
                                    .filter(|handle| handle.lease == *lease)
                                {
                                    let certified = handle
                                        .client
                                        .establishment_projection()
                                        .and_then(|projection| projection.applied())
                                        .is_some_and(|postimage| {
                                            postimage.choice.acp_session_id.as_deref()
                                                == Some(acp_session_id.as_str())
                                                && data.metadata.native_origin.is_some_and(
                                                    |origin| {
                                                        origin.attempt_id == postimage.attempt_id
                                                    },
                                                )
                                        });
                                    if certified {
                                        handle.native_session_id = Some(acp_session_id.clone());
                                        if let WorkerKind::Runner { spawn_config } =
                                            &mut handle.kind
                                        {
                                            spawn_config.stored_acp_session_id =
                                                Some(acp_session_id.clone());
                                            spawn_config.seed_history_replay = false;
                                            spawn_config.fork_from = None;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if !matches!(result, OwnedPublication::Published { .. }) {
                        if let Some(publication) = &data.pending_request {
                            publication.abandon_unpublished().await;
                        }
                    }
                    return result;
                }
                Err(wait) => {
                    // No publication exclusion survives the physical-fence wait.
                    #[cfg(test)]
                    let observer = {
                        let slot = PUBLICATION_FLOCK_OBSERVER.lock().unwrap();
                        if slot.as_ref().is_some_and(|(id, _)| id == &self.session_id)
                            && matches!(data.event.as_ref(), Event::PromptRuntimeError { .. })
                        {
                            slot.as_ref().map(|(_, observer)| observer.clone())
                        } else {
                            None
                        }
                    };
                    if !wait
                        .settled(
                            #[cfg(test)]
                            observer,
                        )
                        .await
                    {
                        if let Some(publication) = &data.pending_request {
                            publication.abandon_unpublished().await;
                        }
                        return OwnedPublication::Superseded;
                    }
                }
            }
        }
    }

    async fn try_publish_owned(
        &self,
        lease: &Lease,
        data: &mut AcpEventData,
        replay: Option<(u64, &mut crate::acp::event_store::ReplayCommit)>,
        control_loss: Option<(&AcpClient, ControlLoss)>,
    ) -> Result<OwnedPublication, PublicationWait> {
        let workers = self.workers.lock().await;
        let installed = lock_recover(&self.lifecycle)
            .current_lease(&self.session_id)
            .as_ref()
            == Some(lease)
            && workers
                .get(&self.session_id)
                .is_some_and(|handle| handle.lease == *lease)
            && !self.identity_only
            && lease.retirement().identity_scope().is_none()
            && lease.retirement().binding_checkpoint().is_none();
        let request = data.current_scope.clone();
        let constructor = data.constructor_scope.clone();
        let pending_request = data.pending_request.clone();
        if !installed && pending_request.is_none() && data.metadata.native_origin.is_none() {
            return Ok(OwnedPublication::Superseded);
        }
        if let Some((client, _)) = control_loss {
            let lifecycle = lock_recover(&self.lifecycle);
            if !installed
                || !lifecycle
                    .running(&self.session_id)
                    .is_some_and(|(current, _)| current == *lease)
                || lease.cancellation().is_cancelled()
                || worker_registry::stop_requested(&self.session_id)
                || !workers.get(&self.session_id).is_some_and(|handle| {
                    handle.lease == *lease && std::ptr::eq(handle.client.as_ref(), client)
                })
            {
                return Ok(OwnedPublication::Superseded);
            }
        }
        let pending_guard = if let Some(publication) = &pending_request {
            if !Arc::ptr_eq(publication.event(), &data.event)
                || publication.metadata() != data.metadata
                || (!publication.is_original() && !installed)
            {
                return Ok(OwnedPublication::Discarded);
            }
            match publication.try_publication_guard() {
                Ok(guard) => Some(guard),
                Err(error) if error.is::<worker_registry::RegistryBusy>() => {
                    return Err(PublicationWait::PendingRequest {
                        context: Arc::clone(publication),
                        id: self.session_id.clone(),
                    });
                }
                Err(_) => return Ok(OwnedPublication::Discarded),
            }
        } else {
            None
        };
        let current_guard = if pending_request.is_none()
            && installed
            && (matches!(
                data.metadata.scope,
                EventScope::SessionCurrent | EventScope::OperationCurrent
            ) || data
                .metadata
                .native_autonomous
                .as_ref()
                .is_some_and(|autonomous| autonomous.qualified))
        {
            match (request.as_ref(), constructor.as_ref()) {
                (Some(request), None) => match request.try_effect_guard() {
                    Ok(guard) => Some(guard),
                    Err(error) if error.is::<worker_registry::RegistryBusy>() => {
                        return Err(PublicationWait::Request {
                            context: Arc::clone(request),
                            id: self.session_id.clone(),
                        })
                    }
                    Err(_) => None,
                },
                (None, Some(constructor)) => match constructor.try_publication_guard() {
                    Ok(guard) => Some(guard),
                    Err(error) if error.is::<worker_registry::RegistryBusy>() => {
                        return Err(PublicationWait::Constructor {
                            context: Arc::clone(constructor),
                            id: self.session_id.clone(),
                        })
                    }
                    Err(_) => None,
                },
                _ => None,
            }
        } else {
            None
        };
        let current = current_guard.is_some()
            || pending_request
                .as_ref()
                .is_some_and(|publication| !publication.is_original());
        if !current && pending_request.is_none() {
            data.metadata.make_historical();
            data.current_scope = None;
            data.constructor_scope = None;
        }
        let _history = if current || pending_guard.is_some() {
            None
        } else if let Some(scope) = &self.history_scope {
            match scope.fence.try_lock_original_history() {
                Ok(Some(guard)) => Some(guard),
                Ok(None) => return Err(PublicationWait::Native(Arc::clone(&scope.fence))),
                Err(_) => return Ok(OwnedPublication::Superseded),
            }
        } else if data.metadata.native_origin.is_some() {
            return Ok(OwnedPublication::Discarded);
        } else {
            None
        };
        let lifecycle = lock_recover(&self.lifecycle);
        if current && lifecycle.current_lease(&self.session_id).as_ref() != Some(lease) {
            data.metadata.make_historical();
            data.current_scope = None;
            data.constructor_scope = None;
            return match &self.history_scope {
                Some(scope) => Err(PublicationWait::Native(Arc::clone(&scope.fence))),
                None => Ok(OwnedPublication::Superseded),
            };
        }
        if let Some((client, loss)) = control_loss {
            if !installed
                || !lifecycle
                    .running(&self.session_id)
                    .is_some_and(|(current, _)| current == *lease)
                || lease.cancellation().is_cancelled()
                || worker_registry::stop_requested(&self.session_id)
                || !workers.get(&self.session_id).is_some_and(|handle| {
                    handle.lease == *lease && std::ptr::eq(handle.client.as_ref(), client)
                })
            {
                return Ok(OwnedPublication::Superseded);
            }
            if matches!(data.event.as_ref(), Event::Stopped { .. }) && !client.claim_lost_turn(loss)
            {
                return Ok(OwnedPublication::Discarded);
            }
        }
        if let Event::PromptRuntimeError {
            evidence: Some(_), ..
        } = data.event.as_ref()
        {
            if let Event::PromptRuntimeError {
                evidence: Some(evidence),
                ..
            } = Arc::make_mut(&mut data.event)
            {
                evidence.generation = Some(crate::session::ErrorEvidenceGeneration::AcpWorker(
                    lease.epoch(),
                ));
            }
        }
        let native_subject = data.event.native_subject();
        let (slot, seq) =
            super::with_session_sequence(&self.next_seqs, &self.session_id, |slot, current| {
                *current = current.saturating_add(1);
                (Arc::clone(slot), *current)
            });
        drop(lifecycle);
        drop(workers);
        // The original native/request/history exclusions survive the append.
        let (stored_seq, persisted) = self.sink.publish_replay_from_worker(
            &self.session_id,
            seq,
            data,
            lease.epoch(),
            native_subject,
            replay,
        );
        if persisted && stored_seq < seq {
            let mut current = lock_recover(&slot);
            if !current.retired && current.seq == seq {
                current.seq = seq - 1;
            }
        }
        Ok(if persisted {
            OwnedPublication::Published { current }
        } else {
            OwnedPublication::Failed
        })
    }

    async fn publish_connection_loss(
        &self,
        lease: &Lease,
        loss: &crate::acp::event_sender::ControlPlaneLoss,
    ) {
        loop {
            match self.try_publish_connection_loss(lease, loss).await {
                Ok(()) => {
                    #[cfg(test)]
                    {
                        let gate = loss
                            .connection
                            .test_connection_loss_published
                            .lock()
                            .unwrap()
                            .take();
                        if let Some((entered, release)) = gate {
                            let _ = entered.send(());
                            let _ = release.await;
                        }
                    }
                    return;
                }
                Err(wait) => {
                    if !wait
                        .settled(
                            #[cfg(test)]
                            None,
                        )
                        .await
                    {
                        return;
                    }
                }
            }
        }
    }

    async fn try_publish_connection_loss(
        &self,
        lease: &Lease,
        loss: &crate::acp::event_sender::ControlPlaneLoss,
    ) -> Result<(), PublicationWait> {
        let workers = self.workers.lock().await;
        let Some(handle) = workers
            .get(&self.session_id)
            .filter(|handle| handle.lease == *lease)
        else {
            return Ok(());
        };
        if !handle
            .client
            .owns_connection_loss(&loss.connection, loss.observation)
        {
            return Ok(());
        }
        let Some(origin) = loss.origin else {
            return Ok(());
        };
        let Some(owner) = origin
            .owner
            .filter(|owner| owner.birth_fingerprint.is_some())
        else {
            return Ok(());
        };
        if handle.client.connected_runner() != Some(owner) {
            return Ok(());
        }
        let Some(scope) = &self.history_scope else {
            return Ok(());
        };
        let captured = scope
            .fence
            .try_lock_original_history_project(|row, storage| {
                anyhow::ensure!(
                    origin.same_physical_row(row, storage)?,
                    crate::acp::establishment::EstablishmentRefusal::Superseded
                );
                crate::acp::state::NativeEventContext::capture(row, storage)
            });
        let (_physical, context) = match captured {
            Ok(Some(captured)) => captured,
            Ok(None) => return Err(PublicationWait::Native(Arc::clone(&scope.fence))),
            Err(_) => return Ok(()),
        };
        let registry = match worker_registry::try_registry_lock(&self.session_id) {
            Ok(Some(registry)) => registry,
            Ok(None) => return Err(PublicationWait::Registry(self.session_id.clone())),
            Err(_) => return Ok(()),
        };
        match registry.record() {
            Ok(Some(record)) if !owner.owns_record(&record) => return Ok(()),
            Err(_) => return Ok(()),
            _ => {}
        }
        let lifecycle = lock_recover(&self.lifecycle);
        if lifecycle.current_lease(&self.session_id).as_ref() != Some(lease)
            || !handle
                .client
                .owns_connection_loss(&loss.connection, loss.observation)
        {
            return Ok(());
        }
        let observed = loss.observation;
        let subject = crate::acp::state::NativeEventSubject {
            context,
            owner: Some(owner),
            kind: crate::acp::state::NativeSubjectEvent::ChannelUnavailable(observed),
        };
        let mut data = AcpEventData::from_history(Event::AcpConnectionUnavailable {
            source: observed.source,
            cause: observed.cause,
            phase: observed.phase,
            native_subject: Some(subject),
        });
        data.metadata.scope = EventScope::SessionCurrent;
        data.metadata.availability_denial = true;
        data.metadata.native_origin = Some(origin);
        let seq = with_publication_seq(&self.next_seqs, &self.session_id, |seq| seq);
        drop(lifecycle);
        drop(workers);
        self.sink.publish_replay_from_worker(
            &self.session_id,
            seq,
            &data,
            lease.epoch(),
            Some(subject),
            None,
        );
        Ok(())
    }

    async fn resolve_establishment(
        &self,
        lease: &Lease,
        delivery: &Arc<crate::acp::event_sender::EstablishmentDelivery>,
    ) -> crate::acp::event_sender::EstablishmentResolution {
        use crate::acp::establishment::EstablishmentRefusal;
        use crate::acp::event_sender::EstablishmentResolution;
        let Some(projection) = self.projection.clone() else {
            return EstablishmentResolution::Unconfirmed;
        };
        if !projection
            .delivery()
            .as_ref()
            .is_some_and(|original| Arc::ptr_eq(original, delivery))
        {
            return EstablishmentResolution::Superseded;
        }
        let (source, destination, admission) = {
            let workers = self.workers.lock().await;
            let lifecycle = lock_recover(&self.lifecycle);
            if lifecycle.current_lease(&self.session_id).as_ref() != Some(lease) {
                return EstablishmentResolution::Superseded;
            }
            if let Some(receipt) = &delivery.receipt {
                let owned = workers
                    .get(&self.session_id)
                    .filter(|handle| handle.lease == *lease)
                    .is_some_and(|handle| handle.client.connected_runner() == Some(receipt.owner))
                    || lease.retirement().retains_original_client(receipt.owner);
                if !owned
                    || receipt.owner.birth_fingerprint.is_none()
                    || !receipt.attempt.same_source(&delivery.attempt)
                {
                    return EstablishmentResolution::Superseded;
                }
            }
            let Ok(Some(source)) = lease.native_scope() else {
                return EstablishmentResolution::Unconfirmed;
            };
            let Some(admission) = lease.admission() else {
                return EstablishmentResolution::Unconfirmed;
            };
            let destination = lease
                .retirement()
                .binding_checkpoint()
                .map(|checkpoint| checkpoint.destination.clone())
                .or_else(|| {
                    lease
                        .retirement()
                        .identity_scope()
                        .map(|custody| custody.scope.clone())
                })
                .unwrap_or_else(|| source.clone());
            (source, destination, admission)
        };
        let complete = delivery.complete;
        let sink = Arc::clone(&self.sink);
        let store = admission.store;
        let id = self.session_id.clone();
        let lifecycle = Arc::clone(&self.lifecycle);
        let original = lease.clone();
        let identity_only = self.identity_only;
        let checkpoint = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let _native = store.storage().acquire_instance_lifecycle_lock(&id)?;
            let (resolution, destination, generation) =
                if identity_only || original.cancellation().is_cancelled() {
                    projection.retirement_checkpoint_under_native_lock(
                        store.as_ref(),
                        sink.as_ref(),
                        &destination,
                    )?
                } else {
                    projection.checkpoint_under_native_lock(
                        store.as_ref(),
                        sink.as_ref(),
                        &destination,
                    )?
                };
            let current = lock_recover(&lifecycle);
            anyhow::ensure!(
                current.current_lease(&id).as_ref() == Some(&original)
                    && original.resolve_establishment_scope(&source, destination, generation),
                EstablishmentRefusal::Superseded
            );
            Ok(resolution)
        })
        .await;
        let resolution = match checkpoint {
            Ok(Ok(resolution)) => resolution,
            Ok(Err(error)) if error.is::<EstablishmentRefusal>() => {
                return if matches!(
                    error.downcast_ref::<EstablishmentRefusal>(),
                    Some(EstablishmentRefusal::Superseded)
                ) {
                    EstablishmentResolution::Superseded
                } else {
                    EstablishmentResolution::Unconfirmed
                };
            }
            error => {
                warn!(target: "acp.establishment", session = %self.session_id,
                    ?error, "original establishment checkpoint remains unconfirmed");
                return EstablishmentResolution::Unconfirmed;
            }
        };
        if !complete {
            return EstablishmentResolution::Unconfirmed;
        }
        if self.identity_only || lease.cancellation().is_cancelled() {
            EstablishmentResolution::Retiring
        } else {
            resolution
        }
    }

    async fn acknowledge_original_context_reset(
        &mut self,
        lease: &Lease,
        delivery: &crate::acp::event_sender::EstablishmentDelivery,
        postimage: &crate::acp::establishment::EstablishmentPostimage,
    ) -> bool {
        let Some(pending) = self.context_reset.clone() else {
            return true;
        };
        let Some(receipt) = delivery.receipt.as_ref() else {
            return false;
        };
        let client = {
            let workers = self.workers.lock().await;
            let Some(handle) = workers
                .get(&self.session_id)
                .filter(|handle| handle.lease == *lease)
            else {
                return false;
            };
            Arc::clone(&handle.client)
        };
        let Some(context) = client.original_request_context().map(Arc::new) else {
            return false;
        };
        if context.receipt.as_ref() != Some(receipt)
            || receipt.attempt.profile != pending.profile
            || receipt.postimage(receipt.attempt.context).ok().as_ref() != Some(postimage)
        {
            return false;
        }
        let Some(admission) = lease.admission() else {
            return false;
        };
        let store = admission.store;
        let workers = Arc::clone(&self.workers);
        let lifecycle = Arc::clone(&self.lifecycle);
        let original = lease.clone();
        let id = self.session_id.clone();
        let proof = Arc::clone(&context);
        let reason = pending.reason;
        let acknowledged =
            tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let receipt = proof
                    .receipt
                    .as_ref()
                    .ok_or(crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?;
                let assigned = receipt
                    .session_id()
                    .ok_or(crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?;
                anyhow::ensure!(
                    store.storage().profile() == pending.profile,
                    crate::acp::establishment::EstablishmentRefusal::Superseded
                );
                loop {
                    drop(workers.blocking_lock());
                    let _effect = match proof.effect_guard() {
                        Ok(guard) => guard,
                        Err(error) if error.is::<worker_registry::RegistryBusy>() => {
                            drop(worker_registry::acquire_registry_lock(&id)?);
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let Ok(mut installed) = workers.try_lock() else {
                        continue;
                    };
                    let handle = installed
                        .get_mut(&id)
                        .filter(|handle| {
                            handle.lease == original && Arc::ptr_eq(&handle.client, &client)
                        })
                        .ok_or(crate::acp::establishment::EstablishmentRefusal::Superseded)?;
                    {
                        let current = lock_recover(&lifecycle);
                        anyhow::ensure!(
                            current.current_lease(&id).as_ref() == Some(&original)
                                && !original.cancellation().is_cancelled(),
                            crate::acp::establishment::EstablishmentRefusal::Superseded
                        );
                    }
                    crate::migrations::v033_isolate_sandbox_content::acknowledge_context_reset(
                    store.storage(), &id,
                    crate::migrations::v033_isolate_sandbox_content::NativeContextView::Structured,
                    original.epoch(), &pending.transactions, Some(assigned),
                )?;
                    let current = lock_recover(&lifecycle);
                    anyhow::ensure!(
                        current.current_lease(&id).as_ref() == Some(&original),
                        crate::acp::establishment::EstablishmentRefusal::Superseded
                    );
                    handle.native_session_id = Some(assigned.to_owned());
                    if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                        spawn_config.stored_acp_session_id = Some(assigned.to_owned());
                        spawn_config.seed_history_replay = false;
                        spawn_config.fork_from = None;
                    }
                    return Ok(());
                }
            })
            .await;
        if !matches!(acknowledged, Ok(Ok(()))) {
            warn!(target: "acp.establishment", session = %self.session_id, ?acknowledged,
                "sandbox acknowledgment failed; native receipt remains committed");
            return false;
        }
        self.context_reset = None;
        self.clear_pending_context_reset();
        self.notify.notify_waiters();
        let mut notice = AcpEventData::from_history(Event::SessionContextReset { reason });
        notice.metadata.native_origin = context.origin();
        notice.metadata.scope = EventScope::SessionCurrent;
        notice.current_scope = Some(context);
        matches!(
            self.publish_owned(lease, &mut notice, None, None).await,
            OwnedPublication::Published { .. }
        )
    }

    async fn pump(
        self,
        inbound: &mut mpsc::Receiver<SdkEvent>,
        lease: &Lease,
    ) -> (Self, StreamEnd) {
        #[cfg(test)]
        let (self_owned, receive_owner) = {
            let mut drain = self;
            let receive_owner = drain.receive_owner.take();
            (drain, receive_owner)
        };
        #[cfg(not(test))]
        let self_owned = self;
        let sink = Arc::clone(&self_owned.sink);
        #[cfg(test)]
        let submission_session = self_owned.session_id.clone();
        let completion = Arc::new(tokio::sync::Mutex::new(DrainCompletion {
            drain: Some(self_owned),
            end: StreamEnd::default(),
            established: false,
        }));
        while let Some(incoming) = {
            #[cfg(test)]
            let incoming = match receive_owner.as_ref() {
                Some(owner) => owner.recv(inbound).await,
                None => inbound.recv().await,
            };
            #[cfg(not(test))]
            let incoming = inbound.recv().await;
            incoming
        } {
            #[cfg(test)]
            let submission =
                super::sink::TestSdkSubmission::from_incoming(&submission_session, &incoming);
            let completion = Arc::clone(&completion);
            let lease = lease.clone();
            let publication = super::WorkerPublication::new(async move {
                let mut completion = completion.lock().await;
                let DrainCompletion {
                    drain,
                    end,
                    established,
                } = &mut *completion;
                drain
                    .as_mut()
                    .expect("SDK item after ordered EOF")
                    .complete_incoming(incoming, &lease, end, established)
                    .await;
            });
            #[cfg(test)]
            let publication = publication.with_test_submission(submission);
            if let Some(publication) = sink.publish_from_worker(publication) {
                publication.complete().await;
            }
        }
        let (done, finished) = tokio::sync::oneshot::channel();
        let publication = super::WorkerPublication::new(async move {
            let mut completion = completion.lock().await;
            let drain = completion.drain.take().expect("ordered EOF delivered once");
            completion.end.established = completion.established;
            let end = std::mem::take(&mut completion.end);
            let _ = done.send((drain, end));
        });
        if let Some(publication) = sink.publish_from_worker(publication) {
            publication.complete().await;
        }
        let (drain, end) = finished
            .await
            .expect("ordered worker completion ended before EOF");
        #[cfg(test)]
        let drain = {
            let mut drain = drain;
            drain.receive_owner = receive_owner;
            drain
        };
        (drain, end)
    }

    async fn complete_incoming(
        &mut self,
        incoming: SdkEvent,
        lease: &Lease,
        end: &mut StreamEnd,
        established: &mut bool,
    ) {
        use crate::acp::event_sender::EstablishmentResolution;
        let mut data = match incoming {
            SdkEvent::EstablishmentClosed { delivery } => {
                let mut resolution = self.resolve_establishment(lease, &delivery).await;
                if let EstablishmentResolution::Applied(postimage) = &resolution {
                    if !self
                        .acknowledge_original_context_reset(lease, &delivery, postimage)
                        .await
                    {
                        resolution = EstablishmentResolution::Unconfirmed;
                        end.startup_failed = true;
                    }
                }
                delivery.resolve(resolution);
                return;
            }
            SdkEvent::ControlPlaneLoss(loss) => {
                self.publish_connection_loss(lease, &loss).await;
                return;
            }
            SdkEvent::Data(data) => data,
        };
        let projection = self.projection.clone();
        let mut processed = projection
            .as_deref()
            .and_then(|state| state.received_event());
        let ordinal = processed.as_ref().map(|event| event.ordinal());
        let mut replay = projection.as_ref().and_then(|state| {
            let (attempt, first) = state.original_window()?;
            if !ordinal.is_some_and(|ordinal| state.original_range_contains(ordinal)) {
                return None;
            }
            Some(
                state
                    .take_replay()
                    .unwrap_or_else(|| crate::acp::event_store::ReplayCommit::new(attempt, first)),
            )
        });
        let recovery_diagnostic = self.recovery.is_some()
            && matches!(data.event.as_ref(), Event::AgentStartupError { .. })
            && data.metadata.native_origin.is_none();
        if recovery_diagnostic {
            end.startup_failed = true;
        }
        let publication = if matches!(data.event.as_ref(), Event::AgentStartupError { .. })
            && (self.restart_intended() || recovery_diagnostic)
        {
            OwnedPublication::Discarded
        } else {
            self.publish_owned(lease, &mut data, ordinal.zip(replay.as_mut()), None)
                .await
        };
        if let Some(journal) = data.journal_publication.take() {
            let _ = journal.send(matches!(
                publication,
                OwnedPublication::Published { current: true }
            ));
        }
        if let (Some(projection), Some(mut replay)) = (&projection, replay) {
            if !matches!(publication, OwnedPublication::Published { .. }) {
                replay.reject();
                projection.lost();
            }
            projection.retain_replay(Some(replay));
        }
        if let Some(processed) = processed.as_mut() {
            processed.complete();
        }
        drop(processed);
        match publication {
            OwnedPublication::Published { current: true } => {}
            OwnedPublication::Published { current: false }
            | OwnedPublication::Discarded
            | OwnedPublication::Failed => return,
            OwnedPublication::Superseded => {
                end.superseded = true;
                return;
            }
        }
        if data.metadata.native_autonomous.is_some() {
            return;
        }
        match data.event.as_ref() {
            Event::Stopped { reason } => match reason.as_str() {
                "agent_unresponsive" | "prompt_orphaned" | "user_forced" => {
                    end.agent_unresponsive = true
                }
                "rate_limited" if !*established => end.startup_rate_limited = true,
                _ => {}
            },
            Event::AgentStartupError { .. } if !*established => end.startup_failed = true,
            Event::AcpSessionAssigned {
                connection_ready: true,
                ..
            } => {
                *established = true;
                end.startup_rate_limited = false;
                self.recovery = None;
            }
            _ => {}
        }
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
            if !self.detach_stopped_worker_work() {
                return Err(());
            }
            workers.remove(&self.session_id);
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
    fn detach_stopped_worker_work(&self) -> bool {
        super::publish::complete_stopped_worker_work(
            &*self.sink,
            &self.next_seqs,
            &self.session_id,
            Some("the worker that was tracking this sub-agent stopped; tracking stopped"),
        )
    }

    /// A runner that closed only its control channel still owns its agent and
    /// turn, so dial it again under the same lease rather than replace it.
    /// Reports the loss when the worker cannot be recovered that way.
    async fn recover_control_loss(
        mut self,
        lease: &Lease,
        end: &StreamEnd,
    ) -> (Self, Option<mpsc::Receiver<SdkEvent>>) {
        let recovery = self.recovery.take().filter(|_| !end.established);
        let observed = {
            let workers = self.workers.lock().await;
            workers
                .get(&self.session_id)
                .filter(|handle| handle.lease == *lease)
                .filter(|handle| {
                    !matches!(&handle.kind, WorkerKind::Runner { spawn_config }
                    if spawn_config.uses_native_prime())
                })
                .filter(|handle| {
                    handle.client.connection_unavailable().is_none_or(|loss| {
                        loss.source != crate::acp::state::AcpConnectionSource::Native
                    })
                })
                .map(|handle| (Arc::clone(&handle.client), handle.client.control_loss()))
        };
        let Some((client, observed)) = observed else {
            return (self, None);
        };
        let (loss, attempts) = match recovery {
            Some(recovery) if end.startup_failed || observed.is_some() => {
                (recovery.loss, recovery.attempts)
            }
            Some(recovery) => {
                self = self.report_control_loss(lease, client, recovery.loss).await;
                return (self, None);
            }
            None => match observed {
                Some(loss) => (loss, 0),
                None => return (self, None),
            },
        };
        match self
            .reattach_same_runner(lease, &client, loss, attempts)
            .await
        {
            Some((inbound, recovery)) => {
                self.recovery = Some(recovery);
                (self, Some(inbound))
            }
            None => {
                self = self.report_control_loss(lease, client, loss).await;
                (self, None)
            }
        }
    }

    async fn report_control_loss(
        self,
        lease: &Lease,
        client: Arc<AcpClient>,
        loss: ControlLoss,
    ) -> Self {
        let sink = Arc::clone(&self.sink);
        let lease = lease.clone();
        let (done, finished) = tokio::sync::oneshot::channel();
        let publication = super::WorkerPublication::new(async move {
            for event in [
                Event::Stopped {
                    reason: "runner_control_lost".into(),
                },
                Event::startup_error("ACP connection failed: runner control channel closed"),
            ] {
                let mut data = AcpEventData::from_host(event);
                if matches!(
                    self.publish_owned(&lease, &mut data, None, Some((&client, loss)))
                        .await,
                    OwnedPublication::Superseded
                ) {
                    break;
                }
            }
            let _ = done.send(self);
        });
        if let Some(publication) = sink.publish_from_worker(publication) {
            publication.complete().await;
        }
        finished
            .await
            .expect("ordered control loss publication completed")
    }

    /// Dials the runner until one attempt is installed. The client bounds the
    /// wait for resume confirmation while the drain publishes its replay.
    async fn reattach_same_runner(
        &mut self,
        lease: &Lease,
        lost: &Arc<AcpClient>,
        loss: ControlLoss,
        attempts: u32,
    ) -> Option<(mpsc::Receiver<SdkEvent>, Recovery)> {
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
            let resume_by = tokio::time::Instant::now() + CONTROL_REATTACH_DEADLINE;
            let record = loop {
                if lease.cancellation().is_cancelled()
                    || worker_registry::stop_requested(session_id)
                    || tokio::time::Instant::now() >= resume_by
                    || !lock_recover(&self.lifecycle)
                        .running(session_id)
                        .is_some_and(|(current, owner)| {
                            current == *lease && owner == Some(identity)
                        })
                {
                    return None;
                }
                match worker_registry::load_strict(session_id) {
                    Ok(Some(record))
                        if identity.owns_record(&record)
                            && worker_registry::is_record_live(&record) =>
                    {
                        break record
                    }
                    Err(error) if error.is::<worker_registry::RegistryBusy>() => {
                        tokio::task::yield_now().await;
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
                }
            };
            let stored_acp_session_id = record.stored_acp_session_id?;
            // Charge only a verified live runner, so a dead one pays just its respawn.
            if attempt == 1 && !self.admit_reattach(lease).await {
                return None;
            }
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
        &mut self,
        lease: &Lease,
        identity: RunnerIdentity,
        lost: &Arc<AcpClient>,
        mut client: AcpClient,
        loss: ControlLoss,
    ) -> Option<mpsc::Receiver<SdkEvent>> {
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
            match workers.get_mut(session_id).filter(|handle| {
                current && handle.lease == *lease && Arc::ptr_eq(&handle.client, lost)
            }) {
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
        let recovery = (|| -> anyhow::Result<_> {
            let agents = super::publish::collect_resumable_background_agent_launches(
                &*self.sink,
                &self.next_seqs,
                session_id,
            )?;
            let tasks = self.sink.unresolved_background_task_launches(session_id)?;
            Ok((agents, tasks))
        })();
        match recovery {
            Ok((agents, tasks)) => {
                if !agents.is_empty() {
                    let _ = client.resume_background_tailing(agents).await;
                }
                if !tasks.is_empty() {
                    let _ = client.resume_task_tailing(tasks).await;
                }
            }
            Err(error) => {
                warn!(target: "acp.resource", session = session_id, %error,
                    "resource tracking could not be restored");
                self.publish(Event::SessionNotice {
                    key: Some("resource-recovery".into()),
                    severity: "warning".into(),
                    title: "Background tracking could not be restored".into(),
                    description: Some(
                        "Retained history is preserved; no resource reader was started.".into(),
                    ),
                });
            }
        }
        info!(
            target: "acp.supervisor",
            session = %session_id,
            pid = identity.pid,
            turn_in_flight = loss.turn_in_flight,
            "reattached to the same runner after its control channel closed"
        );
        self.projection = client.establishment_projection();
        Some(inbound)
    }

    /// Decide whether the closed worker respawns, publishing why when it does not.
    async fn approve_respawn(self, lease: &Lease) -> (Self, Option<SpawnConfig>) {
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
                return (self, Some(*config));
            }
            RestartDecision::BudgetBurned => {
                warn!(
                    target: "acp.supervisor",
                    session = %session_id,
                    max_respawns = MAX_RESPAWNS_IN_WINDOW,
                    window_secs = RESTART_WINDOW.as_secs(),
                    "restart budget burned; parking session"
                );
                let mut error = AcpEventData::from_history(Event::SessionNotice {
                    key: Some("restart-budget".into()),
                    severity: "error".into(),
                    title: "Automatic restart budget exhausted".into(),
                    description: Some(format!(
                        "The ACP connection ended more than {} times in {}s; automatic restart is paused.",
                        MAX_RESPAWNS_IN_WINDOW, RESTART_WINDOW.as_secs(),
                    )),
                });
                let sink = Arc::clone(&self.sink);
                let original = lease.clone();
                let (done, finished) = tokio::sync::oneshot::channel();
                let publication = super::WorkerPublication::new(async move {
                    let published = matches!(
                        self.publish_owned(&original, &mut error, None, None).await,
                        OwnedPublication::Published { .. }
                    );
                    let _ = done.send((self, published));
                });
                if let Some(publication) = sink.publish_from_worker(publication) {
                    publication.complete().await;
                }
                let (drain, published) = finished
                    .await
                    .expect("ordered budget publication completed");
                if published {
                    drain.drop_handle(lease).await;
                }
                return (drain, None);
            }
            RestartDecision::LeaveToReconciler => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "attached worker connection died; requeueing canonical admission"
                );
                let workers = self.workers.lock().await;
                let lifecycle = lock_recover(&self.lifecycle);
                if lifecycle.current_lease(session_id).as_ref() != Some(lease)
                    || !workers
                        .get(session_id)
                        .is_some_and(|handle| handle.lease == *lease)
                {
                    drop(lifecycle);
                    drop(workers);
                    return (self, None);
                }
                lock_recover(&self.startup_requeues).insert(session_id.clone());
            }
            RestartDecision::Gone => return (self, None),
            RestartDecision::Fenced => {
                // Kernel-qualified exit can retire its own custody, not grant
                // the unreadable or missing file authority over a live runner.
                self.drop_handle(lease).await;
                return (self, None);
            }
            RestartDecision::UserStopped => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "worker stop requested; settling its handle without respawn"
                );
                self.drop_handle(lease).await;
                return (self, None);
            }
        }
        self.drop_handle(lease).await;
        (self, None)
    }

    /// Relaunch the worker under a fresh respawn epoch; returns the new lease
    /// and event stream once it is installed.
    async fn respawn(
        &mut self,
        lease: &Lease,
        mut config: SpawnConfig,
    ) -> Option<(Lease, mpsc::Receiver<SdkEvent>)> {
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
        let container = config
            .sandbox_info
            .as_ref()
            .filter(|sandbox| sandbox.enabled)
            .map(|sandbox| sandbox.container_name.clone());
        let container = tokio::select! {
            biased;
            _ = lease.cancellation().cancelled() => return None,
            (_, state) = async {
                tokio::join!(tokio::time::sleep(RESPAWN_BACKOFF), async {
                    match container {
                        Some(name) => probe_sandbox_container(name).await,
                        None => ContainerState::Present,
                    }
                })
            } => state,
        };
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
        let mut reservation = self.reservation_for_lease(respawn_lease.clone());
        // Fence completion through orphan publication and success bookkeeping.
        let _original_constructor = reservation.constructor.as_ref().map(|work| work.fork());
        config.generation = respawn_lease.epoch();
        config.launch_lease = Some(respawn_lease.clone());

        let cancelled = lock_recover(&self.lifecycle).cancel_requested(&respawn_lease);
        if cancelled.is_some() {
            if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease) {
                self.finish_cancelled(&respawn_lease, previous).await;
            }
            return None;
        }
        // A respawn can't reach a removed container; only an explicit start rebuilds it.
        if container == ContainerState::Gone {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                "sandbox container is gone; parking session instead of respawning"
            );
            if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease) {
                self.publish(Event::AgentStartupError {
                    message: "This Session's sandbox container is gone. Recreate it or start a new Session and container."
                        .into(),
                    cause: Some(StartupCause::ContainerMissing),
                    native_subject: None,
                });
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
                            self.publish(Event::startup_error(
                                "Prime native termination is unproven; replacement remains blocked",
                            ));
                        }
                    }
                    self.settle_retirement(&respawn_lease, settlement).await;
                }
                return None;
            }
            previous = None;
        }
        respawn_lease.record_prime(crate::acp::runner_lifecycle::PrimeOwnership::NotPrime);
        let refreshed = await_resume_to_completion(
            reservation.cancellation(),
            Self::refresh_launch_env(&self.session_id, &mut config, container.runtime_use()),
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
        self.projection = client.establishment_projection();
        Some((respawn_lease, inbound))
    }

    fn defer_respawn_install(
        &self,
        workers: &mut HashMap<String, super::WorkerHandle>,
        reservation: &mut ResumeReservation,
        client: Arc<crate::acp::acp_client::AcpClient>,
        inbound: mpsc::Receiver<SdkEvent>,
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
                constructor: reservation.constructor.as_ref().map(|work| work.fork()),
            },
        )));
    }

    /// Re-resolve what may have changed since the first launch: model pins,
    /// host hook env, and MCP servers.
    async fn refresh_launch_env(
        session_id: &str,
        config: &mut SpawnConfig,
        runtime: crate::acp::mcp_rewrite::RuntimeUse,
    ) {
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

        let resolved_mcp = resolve_mcp_servers(
            discovery_key(config.sandbox_info.is_some(), &config.agent_key, native),
            session_id,
            config.source_profile.clone(),
            McpPaths {
                cwd: config.cwd.clone(),
                config_path: config.config_path.clone(),
                sandboxed: config.sandbox_info.is_some(),
            },
            McpHostEnv {
                session_env: config.host_environment.clone(),
                native_config_override: claude_config_dir,
            },
            McpSandboxScope {
                sandbox_info: config.sandbox_info.as_ref(),
                workspace_info: config.workspace_info.as_ref(),
                runtime,
            },
            "MCP re-resolution on respawn failed",
        )
        .await;
        config.mcp_servers = resolved_mcp.servers;
        config.mcp_credentials = resolved_mcp.credentials;
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
        let rate_limited = matches!(e, AcpError::RateLimited(_));
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
                    self.publish(Event::startup_error(format!(
                        "ACP agent respawn failed: {e}"
                    )));
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
        if rate_limited {
            self.rate_limit_wake.notify_one();
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

/// What the respawn probe learned about a sandbox container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerState {
    Gone,
    Present,
    /// A runtime error, or a probe slower than the respawn backoff; the slow probe is killed.
    Unknown,
}

impl ContainerState {
    /// An unanswering runtime is not asked again while the respawn re-resolves MCP servers.
    fn runtime_use(self) -> crate::acp::mcp_rewrite::RuntimeUse {
        match self {
            Self::Unknown => crate::acp::mcp_rewrite::RuntimeUse::Skip,
            Self::Gone | Self::Present => crate::acp::mcp_rewrite::RuntimeUse::Ask,
        }
    }
}

async fn probe_sandbox_container(name: String) -> ContainerState {
    let cancel = tokio_util::sync::CancellationToken::new();
    // Also kills the probe when a cancelled respawn drops this future.
    let _kill = cancel.clone().drop_guard();
    let probe = tokio::task::spawn_blocking(move || {
        crate::containers::get_container_runtime()
            .does_container_exist_until_cancelled(&name, &cancel)
    });
    match tokio::time::timeout(RESPAWN_BACKOFF, probe).await {
        Ok(Ok(Ok(false))) => ContainerState::Gone,
        Ok(Ok(Ok(true))) => ContainerState::Present,
        _ => ContainerState::Unknown,
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

    async fn budget_publication_probe(supersede: bool) {
        #[derive(Debug)]
        enum Reached {
            Queued,
            Inline,
        }
        let (_home, _tmp) = isolate_home();
        let sink = VecSink::new();
        let supervisor = Arc::new(Supervisor::new(Arc::clone(&sink)));
        let id = "ordered-restart-budget";
        let original = supervisor.test_install_stdio(id).await;
        supervisor
            .workers
            .lock()
            .await
            .get_mut(id)
            .unwrap()
            .restart_history = vec![Instant::now(); MAX_RESPAWNS_IN_WINDOW as usize];
        let drain = Drain {
            session_id: id.into(),
            identity_only: false,
            history_scope: None,
            projection: original.retirement().establishment_projection(),
            sink: Arc::clone(&supervisor.sink),
            workers: Arc::clone(&supervisor.workers),
            next_seqs: Arc::clone(&supervisor.next_seqs),
            incompatible_binaries: Arc::clone(&supervisor.incompatible_binaries),
            lifecycle: Arc::clone(&supervisor.lifecycle),
            process_control: Arc::clone(&supervisor.process_control),
            launcher: Arc::clone(&supervisor.launcher),
            notify: Arc::clone(&supervisor.worker_notify),
            startup_requeues: Arc::clone(&supervisor.startup_requeues),
            rate_limit_wake: Arc::clone(&supervisor.rate_limit_wake),
            recovery: None,
            pending_context_resets: Arc::clone(&supervisor.pending_context_resets),
            context_reset: None,
            respawned_in_place: Arc::clone(&supervisor.respawned_in_place),
            receive_owner: None,
        };
        let mut frames = sink.completed.lock().unwrap().resubscribe();
        let (entered, entering) = tokio::sync::oneshot::channel();
        let (release_writer, writer_released) = tokio::sync::oneshot::channel();
        assert!(sink
            .durable
            .publish_from_worker(super::super::WorkerPublication::new(async move {
                let _ = entered.send(());
                writer_released
                    .await
                    .expect("release the actual ordered writer");
            }))
            .is_none());
        tokio::time::timeout(Duration::from_secs(10), entering)
            .await
            .unwrap()
            .unwrap();
        let (reached, position) = std::sync::mpsc::channel();
        let queued = reached.clone();
        *sink.worker_queued.lock().unwrap() = Some(Box::new(move || {
            queued.send(Reached::Queued).unwrap();
        }));
        let (release_budget, budget_released) = std::sync::mpsc::channel();
        let budget_released = std::sync::Mutex::new(budget_released);
        *sink.before_replay.lock().unwrap() = Some(Arc::new(move |event| {
            if matches!(event, Event::SessionNotice { key: Some(key), .. } if key == "restart-budget")
            {
                let _ = reached.send(Reached::Inline);
                budget_released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .expect("release the original budget publication");
            }
        }));
        let approval = {
            let original = original.clone();
            tokio::spawn(async move { drain.approve_respawn(&original).await.1 })
        };
        let reached = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || position.recv_timeout(Duration::from_secs(10))),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        let replacement = if supersede {
            supervisor.test_respawn_worker(id).await;
            Some(supervisor.running_lease(id).unwrap())
        } else {
            None
        };
        let host = sink.durable.queue_persisted(
            id,
            Arc::clone(&supervisor.next_seqs),
            AcpEventData::from_host(Event::SessionNotice {
                key: Some("host-after-budget".into()),
                severity: "info".into(),
                title: "later host publication".into(),
                description: None,
            }),
            None,
        );
        let mut delivered = Vec::new();
        match reached {
            Reached::Inline => {
                release_writer.send(()).unwrap();
                delivered.push(
                    tokio::time::timeout(Duration::from_secs(10), frames.recv())
                        .await
                        .unwrap()
                        .unwrap(),
                );
                release_budget.send(()).unwrap();
            }
            Reached::Queued => {
                release_budget.send(()).unwrap();
                release_writer.send(()).unwrap();
            }
        }
        assert!(tokio::time::timeout(Duration::from_secs(10), approval)
            .await
            .unwrap()
            .unwrap()
            .is_none());
        let host = tokio::task::spawn_blocking(move || host.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(host.1, "the real writer must persist the host publication");
        let flush_sink = Arc::clone(&sink);
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                flush_sink.durable.flush();
            }),
        )
        .await
        .unwrap()
        .unwrap();
        while let Ok(frame) = frames.try_recv() {
            delivered.push(frame);
        }
        let stored = sink
            .establishment_store()
            .unwrap()
            .replay_from(id, 0)
            .unwrap();
        let current = supervisor.running_lease(id);
        supervisor.test_remove_worker(id).await;
        let mut transcript = crate::acp::transcript::TranscriptModel::new();
        for frame in &delivered {
            transcript.apply_scoped_event(frame.seq, &frame.event, &frame.view_metadata());
        }
        let broadcast_seqs: Vec<_> = delivered.iter().map(|frame| frame.seq).collect();
        let durable_seqs: Vec<_> = stored.iter().map(|event| event.seq).collect();
        if let Some(replacement) = replacement {
            assert_eq!(current, Some(replacement));
            assert_eq!(
                broadcast_seqs,
                [1],
                "a superseded budget notice must allocate no sequence"
            );
            assert_eq!(durable_seqs, [1]);
            assert!(delivered.iter().all(|frame| !matches!(frame.event.as_ref(),
                Event::SessionNotice { key: Some(key), .. } if key == "restart-budget")));
        } else {
            assert!(
                current.is_none(),
                "the exhausted original must settle without respawning"
            );
            assert_eq!(
                broadcast_seqs,
                [1, 2],
                "live delivery must follow actual writer sequence order"
            );
            assert_eq!(broadcast_seqs, durable_seqs);
            assert_eq!(delivered[0].worker_generation, Some(original.epoch()));
            assert!(
                transcript
                    .rows()
                    .iter()
                    .any(|row| row.text.contains("Automatic restart budget exhausted")),
                "the actual live transcript must retain the budget advisory"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn restart_budget_advisory_and_host_publications_share_writer_order() {
        budget_publication_probe(false).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn superseded_queued_restart_budget_advisory_allocates_no_sequence() {
        budget_publication_probe(true).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn registry_contention_at_channel_close_eventually_settles_the_dead_original() {
        struct RegistryObservation;
        impl Drop for RegistryObservation {
            fn drop(&mut self) {
                DEFERRED_REGISTRY_OBSERVED.lock().unwrap().take();
            }
        }
        for open_task in [None, Some("pending-task")] {
            super::super::test_support::managed::with_managed(|scope| Box::pin(async move {
                let directory = scope.tempdir();
                let transcript = directory.path().join("agent.jsonl");
                std::fs::write(&transcript, "").unwrap();
                let mut fixture = super::super::launch::tests::managed_background_original(
                    scope, "drain-observation-recovery", transcript.to_str().unwrap(), open_task, false).await;
                fixture.client = scope.adopt(fixture.client, false).await;
                fixture.client.ready().await;
                let id = fixture.client.id.clone();
                let agent = open_task.is_none();
                let allocation = if agent {
                    fixture.store.unresolved_background_agent_launches(&id).unwrap()
                        .into_iter().find(|launch| launch.agent_id == "sub-1").unwrap()
                        .witness.unwrap().allocation
                } else {
                    fixture.store.unresolved_background_task_launches(&id).unwrap()
                        .into_iter().find(|launch| launch.task_id == "pending-task").unwrap()
                        .witness.unwrap().allocation
                };
                // A later legacy history row may reuse an ID without superseding its Native allocation.
                let legacy_launches: Vec<_> = fixture.store.replay_from(&id, 0).unwrap()
                    .into_iter().filter(|stored| matches!(stored.event.as_ref(),
                        Event::BackgroundAgentLaunched { .. } | Event::BackgroundTaskStarted { .. }))
                    .map(|stored| stored.event.as_ref().clone()).collect();
                let supervisor = fixture.client.app_state().acp_supervisor.clone();
                for launch in legacy_launches {
                    supervisor.publish_next(&id, launch);
                }
                assert_eq!(fixture.store.unresolved_background_agent_tracking_launches(&id).unwrap().len(), if agent { 2 } else { 0 });
                assert_eq!(fixture.store.unresolved_background_task_launches(&id).unwrap().len(), if agent { 0 } else { 2 });
                let sid = fixture.client.actual().establishment_projection().unwrap().receipt_snapshot().unwrap()
                    .session_id().unwrap().to_owned();
                fixture.peer.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"history before registry refusal"}}
                }})).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !matches!(fixture.client.recv().await.expect("attached original history observer remains open"),
                        Event::AgentMessageChunk { text, .. } if text == "history before registry refusal") {}
                }).await.expect("real Native history is committed before contention");
                let prefix = fixture.store.highest_seq(&id).unwrap();
                let mut publication = fixture.client.subscribe_publications();
                let identity = fixture.client.actual().connected_runner().expect("actual original runner");
                let birth = crate::process::process_birth_identity(identity.pid).unwrap()
                    .expect("original runner remains alive before channel-close test");
                assert_eq!(Some(birth.fingerprint()), identity.birth_fingerprint);
                let runner = crate::process::CapturedProcess::capture(identity.pid, &birth).unwrap()
                    .expect("capture only the original runner incarnation");
                let (deferred_tx, deferred_rx) = tokio::sync::oneshot::channel();
                *DEFERRED_REGISTRY_OBSERVED.lock().unwrap() = Some(deferred_tx);
                let _observation = RegistryObservation;
                let registry = worker_registry::try_registry_lock(&id).unwrap().unwrap();
                fixture.native.signal(libc::SIGKILL).expect("signal only the captured original Native incarnation");
                runner.signal(libc::SIGKILL).expect("close only the captured original runner incarnation");
                tokio::time::timeout(Duration::from_secs(10), deferred_rx).await
                    .expect("the actual closed-channel drain reaches registry contention").unwrap();
                assert!(tokio::time::timeout(Duration::from_millis(100), publication.recv()).await.is_err(),
                    "actual original loss cannot publish through held registry exclusion");
                assert_eq!(fixture.store.highest_seq(&id).unwrap(), prefix, "registry contention cannot authorize a terminal publication");
                drop(registry);
                tokio::time::timeout(Duration::from_secs(15), async {
                    while fixture.client.recv().await.is_some() {}
                }).await.expect("the original drain settles after registry exclusion release");
                assert!(worker_registry::load_strict(&id).unwrap().is_none());
                assert!(fixture.store.unresolved_background_agent_ids(&id).is_empty());
                assert!(fixture.store.unresolved_background_task_ids(&id).is_empty());
                let state = super::super::launch::tests::fold_background_journal(&fixture.store, &id);
                assert!(!state.has_active_background_agent());
                let events = fixture.store.replay_from(&id, 0).expect("durable replay");
                    let ends: Vec<_> = events.iter().filter(|stored| {
                        stored.metadata.resource_ref == Some(allocation)
                            && if agent {
                                matches!(stored.event.as_ref(), Event::BackgroundAgentCompleted {
                                    status: crate::acp::state::BackgroundAgentStatus::Detached, ..
                                })
                            } else {
                                matches!(stored.event.as_ref(), Event::BackgroundTaskFinished { .. })
                            }
                    }).collect();
                    assert_eq!(ends.len(), 1, "exact recorded allocation ends once");
                    assert!(ends[0].metadata.host_action);
                    assert!(ends[0].metadata.native_origin.is_none());
                    assert!(ends[0].metadata.resource_witness.is_none());
                assert_eq!(events.iter().filter(|stored|
                    stored.metadata.resource_ref.is_none() && matches!(stored.event.as_ref(),
                        Event::BackgroundAgentCompleted { agent_id, .. } if agent_id == "sub-1")).count(), usize::from(agent));
                assert_eq!(events.iter().filter(|stored|
                    stored.metadata.resource_ref.is_none() && matches!(stored.event.as_ref(),
                        Event::BackgroundTaskFinished { task_id, .. } if task_id == "pending-task")).count(), usize::from(!agent));
                assert!(events.iter().any(|stored| matches!(stored.event.as_ref(),
                    Event::AgentMessageChunk { text, .. } if text == "history before registry refusal")));
                assert!(!events.iter().any(|stored| matches!(stored.event.as_ref(), Event::SessionContextReset { .. })));
            })).await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_flock_contention_releases_global_guards_and_rechecks_before_publication() {
        struct PublicationObservation;
        impl Drop for PublicationObservation {
            fn drop(&mut self) {
                PUBLICATION_FLOCK_OBSERVER.lock().unwrap().take();
            }
        }
        super::super::test_support::managed::with_managed(|scope| Box::pin(async move {
                use crate::acp::event_sender::EventScope;
                for (supersede, profile_writer) in [(false, false), (true, false), (false, true)] {
                    let directory = scope.tempdir();
                    let test_name = std::thread::current().name().unwrap().to_owned();
                    let inspection = scope.inspect_next_runner(&test_name);
                    let (request_tx, request_rx) = tokio::sync::oneshot::channel();
                    let mut request_tx = Some(request_tx);
                    let (config, peer) = scope.native_peer(directory.path().into(), "gemini", "gemini", move |request| {
                        let result = match request["method"].as_str()? {
                            "initialize" => serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                            "session/new" => serde_json::json!({"sessionId":"contention-native"}),
                            "session/prompt" => {
                                request_tx.take().unwrap().send(request["id"].clone()).unwrap();
                                return None;
                            }
                            _ => return None,
                        };
                        Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
                    });
                    let mut client = scope.spawn(config, "native-publication-contention").await.unwrap();
                    client.ready().await;
                    inspection.query("observe-producer").await;
                    let supervisor = scope.state.acp_supervisor.clone();
                    let mut frames = scope.state.acp_events_tx.subscribe();
                    supervisor.publish_user_prompt(&client.id, "original request".into()).await;
                    client.send_prompt("original request", &[]).await.unwrap();
                    let request_id = tokio::time::timeout(Duration::from_secs(10), request_rx).await.unwrap().unwrap();
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let frame = frames.recv().await.unwrap();
                            if frame.session_id == client.id && matches!(frame.event.as_ref(), Event::ThinkingStarted) { break; }
                        }
                    }).await.expect("actual admission is durable before physical exclusion");
                    let original_row = crate::session::Storage::open_unwatched("test").unwrap()
                        .load_instances_complete().unwrap().into_iter().find(|row| row.id == client.id).unwrap();
                    if profile_writer {
                        // Adoption keeps the actual pending Native request, but its outcome is
                        // Original history. That path, unlike a current operation's row read,
                        // reloads the captured profile revision under its shared storage flock.
                        let owner = client.actual().connected_runner();
                        client = scope.adopt(client, true).await;
                        client.ready().await;
                        assert_eq!(client.actual().connected_runner(), owner);
                        assert_eq!(peer.requests().iter().filter(|method| method.as_str() == "session/prompt").count(), 1);
                        // Usability precedes the retained SDK's final publications.
                        // Do not trap one ahead of the diagnostic whose flock wait we observe.
                        tokio::time::timeout(Duration::from_secs(10), async {
                            let (mut assigned, mut capabilities) = (false, false);
                            while !assigned || !capabilities {
                                match client.next_event().await.expect("actual retained handshake publication") {
                                    Event::AcpSessionAssigned { acp_session_id, .. }
                                        if acp_session_id == "contention-native" => assigned = true,
                                    Event::PromptCapabilities { .. } => capabilities = true,
                                    Event::AgentStartupError { message, .. } => panic!("{message}"),
                                    Event::AcpConnectionUnavailable { .. } => panic!("the retained original channel stays live"),
                                    _ => {}
                                }
                            }
                        }).await.expect("the retained handshake is durable before diagnostic-only exclusion");
                    }
                    let supervisor = client.app_state().acp_supervisor.clone();
                    let mut frames = client.subscribe_publications();
                    let store = scope.state.acp_event_store.clone();
                    let prefix = store.highest_seq(&client.id).unwrap();
                    let storage = crate::session::Storage::open_reference("test").unwrap();
                    let mut preceding_lifecycle = if profile_writer {
                        Some((
                            storage.sessions_path().parent().unwrap().join(
                                format!(".instance-lifecycle-{}.lock", client.id)),
                            storage.acquire_instance_lifecycle_lock(&client.id).unwrap(),
                        ))
                    } else {
                        None
                    };
                    let native = if profile_writer {
                        crate::session::acquire_storage_flock(storage.sessions_path().parent().unwrap(), crate::session::STORAGE_LOCK_FILENAME).unwrap()
                    } else {
                        storage.acquire_instance_lifecycle_lock(&client.id).unwrap()
                    };
                    if profile_writer {
                        // An unchanged cached revision does not read the profile flock.
                        // Force a real reload without changing the captured Native source.
                        let file = std::fs::File::open(storage.sessions_path()).unwrap();
                        let modified = file.metadata().unwrap().modified().unwrap() + Duration::from_secs(1);
                        file.set_times(std::fs::FileTimes::new().set_modified(modified)).unwrap();
                    }
                    let (contended_tx, contended_rx) = std::sync::mpsc::channel();
                    assert!(PUBLICATION_FLOCK_OBSERVER.lock().unwrap()
                        .replace((client.id.clone(), contended_tx)).is_none());
                    let _observation = PublicationObservation;
                    peer.send(serde_json::json!({"jsonrpc":"2.0","id":request_id,
                        "error":{"code":-32603,"message":"retain this completed outcome exactly once"}})).await;
                    let (completion, started_window) = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let snapshot = inspection.query("producer-snapshot").await;
                            let frames: Vec<crate::acp::control_protocol::ControlBody> = snapshot["events"]
                                .as_array().unwrap().iter()
                                .filter(|event| event["stage"] == "committed")
                                .filter_map(|event| event.get("body"))
                                .map(|body| serde_json::from_value(body.clone()).unwrap())
                                .collect();
                            let completed = frames.iter().find_map(|frame| match frame {
                                crate::acp::control_protocol::ControlBody::PromptCompleted {
                                    prompt_req_id, evidence: Some(evidence), outcome,
                                } if Some(*prompt_req_id) == request_id.as_i64() => {
                                    assert!(matches!(outcome, crate::acp::control_protocol::PromptOutcome::Error { message, .. }
                                        if message == "retain this completed outcome exactly once"));
                                    Some(evidence.clone())
                                }
                                _ => None,
                            });
                            if let Some(completion) = completed {
                                let started_window = frames.iter().find_map(|frame| match frame {
                                    crate::acp::control_protocol::ControlBody::PromptStarted {
                                        prompt_req_id, window_id, ..
                                    } if Some(*prompt_req_id) == request_id.as_i64() => Some(*window_id),
                                    _ => None,
                                }).expect("actual original prompt start precedes its captured completion");
                                break (completion, started_window);
                            }
                            tokio::task::yield_now().await;
                        }
                    }).await.expect("the actual producer captures the correlated Original outcome before publication filtering");
                    assert_eq!(completion.window_id, started_window);
                    assert_eq!(completion.receipt.owner, client.actual().connected_runner().unwrap());
                    assert_eq!(completion.origin,
                        crate::acp::state::NativeEventOrigin::owned_attempt(
                            &completion.receipt.attempt, Some(completion.receipt.owner),
                        ).unwrap());
                    assert_eq!(completion.origin.context,
                        crate::acp::state::NativeEventContext::capture(&original_row, &storage).unwrap());
                    let held_path = storage.sessions_path().parent().unwrap().join(
                        if profile_writer { crate::session::STORAGE_LOCK_FILENAME.to_owned() }
                        else { format!(".instance-lifecycle-{}.lock", client.id) });
                    tokio::time::timeout(Duration::from_secs(10),
                        tokio::task::spawn_blocking(move || {
                            loop {
                                let reported_path = contended_rx.recv()?;
                                if preceding_lifecycle.as_ref()
                                    .is_some_and(|(path, _)| path == &reported_path)
                                {
                                    drop(preceding_lifecycle.take());
                                }
                                if reported_path == held_path {
                                    if profile_writer {
                                        assert!(preceding_lifecycle.is_none(),
                                            "profile contention follows the actual lifecycle wait");
                                    }
                                    break;
                                }
                            }
                            Ok::<(), std::sync::mpsc::RecvError>(())
                        }))
                        .await.expect("the captured diagnostic reaches the held flock")
                        .unwrap().expect("the actual publication wait reports its physical blocker");
                    assert!(supervisor.workers.try_lock().is_ok(),
                        "the diagnostic releases worker custody before its physical wait");
                    assert!(supervisor.lifecycle.try_lock().is_ok(),
                        "the diagnostic releases lifecycle custody before its physical wait");
                    assert!(tokio::time::timeout(Duration::from_millis(100), async {
                        loop {
                            let frame = frames.recv().await.unwrap();
                            if frame.session_id == client.id && matches!(frame.event.as_ref(),
                                Event::PromptRuntimeError { message, .. } if message == "retain this completed outcome exactly once")
                            {
                                break;
                            }
                        }
                    }).await.is_err(), "physical exclusion delays this captured diagnostic");
                    assert!(!store.replay_from(&client.id, prefix).unwrap().iter().any(|stored|
                        matches!(stored.event.as_ref(), Event::PromptRuntimeError { .. } | Event::Stopped { .. })),
                        "neither the original outcome nor its terminal fold crosses the held flock");
                    if supersede {
                        storage.update(|rows, _| {
                            rows.iter_mut().find(|row| row.id == client.id).unwrap().lifecycle_generation += 1;
                            Ok(())
                        }).unwrap();
                    }
                    drop(native);
                    let terminal_reason = if profile_writer { "prompt_failed" } else { "prompt_rejected" };
                    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let frame = frames.recv().await.unwrap();
                            if frame.session_id == client.id && matches!(frame.event.as_ref(), Event::Stopped { reason } if reason == terminal_reason) {
                                break frame;
                            }
                        }
                    }).await.expect("the original outcome is preserved after exclusion release");
                    let suffix = store.replay_from(&client.id, prefix).expect("durable suffix");
                    let errors: Vec<_> = suffix.iter().filter(|stored| matches!(stored.event.as_ref(),
                        Event::PromptRuntimeError { message, .. } if message == "retain this completed outcome exactly once")).collect();
                    assert_eq!(errors.len(), 1, "Original is neither dropped nor duplicated");
                    assert_eq!(errors[0].metadata.scope,
                        if supersede || profile_writer { EventScope::HistoryOnly } else { EventScope::OperationCurrent });
                    assert_eq!(terminal.scope,
                        if supersede || profile_writer { crate::acp::protocol::AcpEventScope::HistoryOnly } else { crate::acp::protocol::AcpEventScope::OperationCurrent });
                    assert!(supervisor.take_startup_requeues().is_empty());
                    if supersede {
                        // Remove only the adversarial test mutation before checked cleanup.
                        // No prompt or current-state effect is attempted after invalidation.
                        storage.update(|rows, _| {
                            rows.iter_mut().find(|row| row.id == client.id).unwrap().lifecycle_generation = original_row.lifecycle_generation;
                            Ok(())
                        }).unwrap();
                    }
                    client.shutdown().await.expect("checked original retirement and checkpoint cleanup");
                }
            })).await;
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

    #[tokio::test]
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
        tokio::time::pause();
        // The route disappears after approval, before the post-backoff recheck.
        std::fs::remove_file(worker_registry::record_path(id).unwrap()).unwrap();
        tokio::time::advance(RESPAWN_BACKOFF).await;
        tokio::time::resume();
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

    #[tokio::test]
    #[serial_test::serial]
    async fn a_live_model_config_pick_reaches_the_respawn_config() {
        let (_home, tmp) = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let socket = tmp.path().join("pick.sock");
        worker_registry::save(&worker_record("s-pick", std::process::id(), socket.clone()))
            .unwrap();
        let lease = sup
            .test_install_runner("s-pick", runner_config(socket), None)
            .await;

        sup.refresh_cached_model_config("s-pick", "ultracode", "on")
            .await;

        let RestartDecision::Respawn(config) =
            restart_decision(&sup.workers, &sup.lifecycle, "s-pick", &lease).await
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

    #[tokio::test]
    async fn current_quota_keeps_the_ready_native_and_accepts_a_distinct_explicit_input() {
        super::super::test_support::managed::with_managed(|scope| Box::pin(async move {
                let directory = scope.tempdir();
                let mut quota = true;
                let (config, peer) = scope.native_peer(directory.path().into(), "gemini", "gemini", move |request| {
                    let id = request.get("id")?;
                    let result = match request["method"].as_str()? {
                        "initialize" => serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                        "session/new" => serde_json::json!({"sessionId":"quota-native"}),
                        "session/prompt" if quota => {
                            quota = false;
                            return Some(serde_json::json!({"jsonrpc":"2.0","id":id,
                                "error":{"code":-32603,"message":"Rate limit exceeded","data":{"errorKind":"rate_limit"}}}));
                        }
                        "session/prompt" => serde_json::json!({"stopReason":"end_turn"}),
                        _ => return None,
                    };
                    Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}))
                });
                let mut client = scope.spawn(config, "ready-current-quota").await.unwrap();
                client.ready().await;
                let original = client.actual().connected_runner();
                for (text, reason) in [("first explicit request", "rate_limited"), ("distinct explicit request", "prompt_complete")] {
                    scope.state.acp_supervisor.publish_user_prompt(&client.id, text.into()).await;
                    client.send_prompt(text, &[]).await.unwrap();
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            match client.recv().await.expect("ready Native remains attached") {
                                Event::Stopped { reason: actual } if actual == reason => break,
                                Event::AgentStartupError { message, .. } => panic!("quota is not startup failure: {message}"),
                                _ => {}
                            }
                        }
                    }).await.expect("genuine correlated terminal");
                    assert_eq!(scope.state.acp_supervisor.worker_state(&client.id).await, AcpWorkerState::Running);
                    assert_eq!(client.actual().connected_runner(), original);
                    assert!(scope.state.acp_supervisor.take_startup_requeues().is_empty());
                }
                assert_eq!(peer.requests().iter().filter(|method| method.as_str() == "session/new").count(), 1);
                assert!(!peer.requests().iter().any(|method| method == "session/load"));
                client.shutdown().await.expect("checked original retirement and checkpoint cleanup");
            })).await;
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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
    #[tokio::test]
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
        let drain = sup.start_drain_task("s-late-respawn".into(), lease, inbound_rx, None);
        drop(inbound_tx);

        // External writer completion uses the real clock. The captured TERM and
        // immortal replacement keep teardown open until the explicit exit below.
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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

    /// A crash respawn parks a sandboxed Session whose container is known gone, and goes
    /// ahead when the runtime can't say: an error, or a probe slower than the backoff.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_crash_respawn_parks_only_when_the_sandbox_container_is_gone() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (_home, tmp) = isolate_home();
        crate::session::config::update_config(|config| {
            config.sandbox.container_runtime = crate::session::ContainerRuntimeName::Docker
        })
        .unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let root = tmp.path().display();
        // `alive` is the container; a slow probe blocks until it is killed.
        std::fs::write(
            bin.join("docker"),
            format!(
                "#!/bin/sh\ncase \"$1 $2\" in \"container inspect\")\n\
                 case \"$(cat {root}/mode)\" in\n\
                 unreachable) echo 'Cannot connect to the Docker daemon at unix:///x. Is the docker daemon running?' >&2; exit 1 ;;\n\
                 slow) echo $$ > {root}/slow.pid; while :; do sleep 0.05; done ;;\n\
                 esac\n\
                 [ -e {root}/alive ] && exit 0\n\
                 echo \"Error: No such container: $3\" >&2; exit 1 ;;\nesac\nexit 0\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(bin.join("docker"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let _path = crate::session::test_support::path_prepended(&bin);
        // (mode, container alive after the crash, parks)
        for (mode, alive, parks) in [
            ("gone", false, true),
            ("present", true, false),
            ("unreachable", false, false),
            ("slow", false, false),
        ] {
            let id = format!("s-container-{mode}");
            std::fs::write(tmp.path().join("mode"), mode).unwrap();
            // The container exists while the worker runs.
            std::fs::write(tmp.path().join("alive"), "").unwrap();
            let control =
                Arc::new(crate::acp::runner_lifecycle::test_support::FakeProcessControl::default());
            control.alive(4242).alive(4343);
            let launches = Arc::new(AtomicUsize::new(0));
            let launched = Arc::new(tokio::sync::Notify::new());
            let launcher: Launcher = {
                let (launches, launched) = (Arc::clone(&launches), Arc::clone(&launched));
                Arc::new(move |config: SpawnConfig, id: AcpSessionId| {
                    let (launches, launched) = (Arc::clone(&launches), Arc::clone(&launched));
                    Box::pin(async move {
                        let lease = config.launch_lease.as_ref().unwrap();
                        lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                        assert!(
                            lease.record_runner(attested_test_identity(4343, config.generation))
                        );
                        save_attested_test_record(&id.0, 4343, config.generation);
                        launches.fetch_add(1, Ordering::SeqCst);
                        launched.notify_one();
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
            save_attested_test_record(&id, 4242, 0);
            let socket = worker_registry::socket_path_for(&id).unwrap();
            let mut config = runner_config(socket);
            config.sandbox_info = Some(sandbox_info());
            let lease = sup
                .test_install_runner(&id, config, Some(attested_test_identity(4242, 0)))
                .await;
            let (inbound_tx, inbound_rx) = mpsc::channel(4);
            let drain = sup.start_drain_task(id.clone(), lease, inbound_rx, None);
            if !alive {
                std::fs::remove_file(tmp.path().join("alive")).unwrap();
            }
            drop(inbound_tx);
            if mode == "slow" {
                tokio::time::timeout(Duration::from_secs(5), launched.notified())
                    .await
                    .expect("a slow probe must not hold back the respawn");
                // The cap kills the blocked inspect rather than leaving it to its own timeout.
                let pid: i32 = std::fs::read_to_string(tmp.path().join("slow.pid"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                // SAFETY: signal 0 only asks whether the probe's PID still exists.
                while unsafe { libc::kill(pid, 0) } == 0 {
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the capped probe still runs"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            tokio::time::timeout(Duration::from_secs(5), drain)
                .await
                .expect("drain task must finish")
                .unwrap();

            assert_eq!(
                launches.load(Ordering::SeqCst),
                usize::from(!parks),
                "{mode}"
            );
            let typed: Vec<String> = sink
                .frames
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, _, event)| match event {
                    Event::AgentStartupError {
                        message,
                        cause: Some(StartupCause::ContainerMissing),
                        ..
                    } => Some(message.clone()),
                    _ => None,
                })
                .collect();
            let expected: &[&str] = if parks {
                &["This Session's sandbox container is gone. Recreate it or start a new Session and container."]
            } else {
                &[]
            };
            assert_eq!(typed, expected, "{mode}");
            assert_eq!(
                sup.worker_state(&id).await,
                AcpWorkerState::Absent,
                "{mode}"
            );
        }
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
            let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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
            let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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

        Drain::<VecSink>::refresh_launch_env("s-withdraw", &mut config, Default::default()).await;
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

        Drain::<VecSink>::refresh_launch_env("s-ws-mcp", &mut config, Default::default()).await;

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

            Drain::<VecSink>::refresh_launch_env(
                "s-codex-wrapper",
                &mut config,
                Default::default(),
            )
            .await;

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
    async fn respawn_runs_the_rewrite_hook_with_the_sandbox_mounts() {
        use crate::acp::mcp_rewrite::test_support::{
            outside_main_mount, workspace_info, RecordingRewriter, SlotGuard, OUTSIDE_MAIN,
        };
        use crate::session::mcp::project_mcp::{ProjectMcpServer, ProjectMcpTransport};
        use agent_client_protocol::v1::schema::McpServer;
        use std::path::Path;

        let (_home, temp) = isolate_home();
        let rewriter = RecordingRewriter::returning(vec![ProjectMcpServer {
            name: "rewritten".into(),
            transport: ProjectMcpTransport::Stdio {
                command: "gateway".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        }]);
        let _slot = SlotGuard::install(rewriter.clone());

        let mut config = runner_config(worker_registry::socket_path_for("s-hook").unwrap());
        config.cwd = temp.path().to_path_buf();
        config.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "aoe-sandbox:test".into(),
            container_name: "aoe-sandbox-hook".into(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            provider: None,
        });
        config.workspace_info = Some(workspace_info(
            temp.path(),
            &[("alpha", Path::new(OUTSIDE_MAIN))],
        ));
        Drain::<VecSink>::refresh_launch_env("s-hook", &mut config, Default::default()).await;

        let sandboxes = rewriter.sandboxes.lock().unwrap();
        let [Some(sandbox)] = sandboxes.as_slice() else {
            panic!("one sandboxed call expected: {sandboxes:?}");
        };
        assert!(
            sandbox.mounts.contains(&outside_main_mount()),
            "respawn reports the workspace mounts: {sandbox:?}"
        );
        assert!(matches!(
            config.mcp_servers.as_slice(),
            [McpServer::Stdio(server)] if server.name == "rewritten"
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn respawn_reinjects_plugin_served_servers() {
        use crate::acp::mcp_rewrite::test_support::SlotGuard;
        use crate::plugin::mcp_serve::test_support::{names, ServingRewriter};
        use crate::server::daemon_endpoint::{test_support::EndpointGuard, DaemonEndpoint};
        use crate::server::session_credential::McpSessionSecret;
        use agent_client_protocol::v1::schema::McpServer;

        let (_home, temp) = isolate_home();
        let _env = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let rewriter = ServingRewriter::new(vec![("acme.a", names(&["docs"]))], false);
        let _slot = SlotGuard::install(rewriter.clone());
        let secret = std::sync::Arc::new(
            McpSessionSecret::load_or_generate(&temp.path().join("mcp_session_secret")).unwrap(),
        );
        let _endpoint = EndpointGuard::install(DaemonEndpoint {
            host_url: Some("http://127.0.0.1:4321".into()),
            sandbox_url: None,
            sandbox_socket: None,
            secret: secret.clone(),
        });

        let mut config = runner_config(worker_registry::socket_path_for("s-plugin").unwrap());
        config.cwd = temp.path().to_path_buf();
        Drain::<VecSink>::refresh_launch_env("s-plugin", &mut config, Default::default()).await;

        let [McpServer::Http(server)] = config.mcp_servers.as_slice() else {
            panic!("one plugin entry expected: {:?}", config.mcp_servers);
        };
        assert_eq!(server.name, "acme_a__docs");
        assert_eq!(server.url, "http://127.0.0.1:4321/mcp/plugins/acme.a/docs");
        let credential = secret.plugin_server_credential_for("acme.a", "docs", "s-plugin");
        let var = crate::plugin::mcp_serve::credential_var_name(&credential);
        let bearer = format!("Bearer ${{{var}}}");
        assert!(
            server
                .headers
                .iter()
                .any(|h| h.name == "Authorization" && h.value == bearer),
            "{:?}",
            server.headers
        );
        let want = vec![(var, credential)];
        assert_eq!(
            config.mcp_credentials,
            want.into(),
            "the respawn recomputes the pair from the derivation"
        );
        assert_eq!(
            rewriter.requests.lock().unwrap()[0]["servers"][0]["plugin"],
            "acme.a"
        );
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

        let socket = worker_registry::socket_path_for("s-store").unwrap();
        let mut config = runner_config(socket);
        config.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: selected.clone(),
            exported_default_store: None,
        });
        config.host_environment = vec![("CLAUDE_CONFIG_DIR".into(), "stale".into())];
        config.base_host_environment = vec![("HOME".into(), temp.path().display().to_string())];
        Drain::<VecSink>::refresh_launch_env("s-store", &mut config, Default::default()).await;
        let launched = config;
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
        let held_senders: Arc<std::sync::Mutex<Vec<crate::acp::event_sender::EventSender>>> =
            Default::default();
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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
    #[tokio::test]
    #[serial_test::serial]
    async fn an_absent_worker_without_retained_custody_cannot_publish() {
        use super::super::test_support::managed::with_managed;
        for stopping in [false, true] {
            with_managed(|scope| {
                Box::pin(async move {
                    let live = crate::server::test_support::managed_control_session(
                        scope,
                        "absent-publication-owner",
                        "claude",
                        None,
                    )
                    .await;
                    let id = live.id().to_owned();
                    let supervisor = live.state.acp_supervisor.clone();
                    let store = live.state.acp_event_store.clone();
                    let cache = live.state.acp_control_cache.clone();
                    assert!(Arc::ptr_eq(&cache, store.control_cache()));
                    let lease = live.client.lease.clone();
                    let owner = live.client.actual().connected_runner().unwrap();
                    if stopping {
                        live.client.stop_session().await;
                    } else {
                        live.client
                            .shutdown()
                            .await
                            .expect("complete the actual captured retirement");
                    }
                    supervisor.sink.flush();
                    assert!(!supervisor.workers.lock().await.contains_key(&id));
                    assert!(lock_recover(&supervisor.lifecycle)
                        .current_lease(&id)
                        .is_none());
                    assert!(!lease.retirement().retains_original_client(owner));
                    let before = live
                        .state
                        .session_service
                        .fold_control_state(&id)
                        .await
                        .unwrap();
                    let prefix = store.highest_seq(&id).unwrap();
                    let mut frames = live.state.acp_events_tx.subscribe();
                    let drain = Drain {
                        session_id: id.clone(),
                        identity_only: false,
                        history_scope: lease.native_scope().unwrap(),
                        projection: lease.retirement().establishment_projection(),
                        sink: Arc::clone(&supervisor.sink),
                        workers: Arc::clone(&supervisor.workers),
                        next_seqs: Arc::clone(&supervisor.next_seqs),
                        incompatible_binaries: Arc::clone(&supervisor.incompatible_binaries),
                        lifecycle: Arc::clone(&supervisor.lifecycle),
                        process_control: Arc::clone(&supervisor.process_control),
                        launcher: Arc::clone(&supervisor.launcher),
                        notify: Arc::clone(&supervisor.worker_notify),
                        startup_requeues: Arc::clone(&supervisor.startup_requeues),
                        rate_limit_wake: Arc::clone(&supervisor.rate_limit_wake),
                        recovery: None,
                        pending_context_resets: Arc::clone(&supervisor.pending_context_resets),
                        context_reset: None,
                        respawned_in_place: Arc::clone(&supervisor.respawned_in_place),
                        receive_owner: None,
                    };
                    let mut data = AcpEventData::from_history(Event::AcpSessionAssigned {
                        acp_session_id: "uncaptured-native-sid".into(),
                        connection_ready: false,
                        native_subject: None,
                    });
                    assert!(matches!(
                        drain.publish_owned(&lease, &mut data, None, None).await,
                        OwnedPublication::Superseded
                    ));
                    supervisor.sink.flush();
                    assert!(store.replay_from(&id, prefix).unwrap().is_empty());
                    assert_eq!(store.highest_seq(&id).unwrap(), prefix);
                    assert_eq!(
                        super::super::try_activity_seq(&supervisor.next_seqs, &id)
                            .expect("a superseded publication cannot retain sequence exclusion"),
                        prefix
                    );
                    assert!(matches!(
                        frames.try_recv(),
                        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
                    ));
                    let after = cache
                        .get_or_hydrate(&id, || panic!("the same warm state must survive"))
                        .unwrap();
                    assert_eq!(after.last_seq, before.last_seq);
                    assert_eq!(after.operation_error, before.operation_error);
                    assert_eq!(after.session_id, before.session_id);
                })
            })
            .await;
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn superseded_worker_errors_never_allocate_persist_or_fold() {
        use super::super::test_support::managed::with_managed;
        use crate::daemon::AcpWorkerState;
        use crate::session::{
            ErrorEvidenceGeneration, ErrorEvidenceSource, ErrorOperation, OperationErrorEvidence,
            Status,
        };
        with_managed(|scope| Box::pin(async move {
                let mut live = crate::server::test_support::managed_control_session(scope, "superseded-publication-owner", "claude", None).await;
                let id = live.id().to_owned();
                let old = live.client.lease.clone();
                live.client = scope.adopt(live.client, false).await;
                live.client.ready().await;
                live.state = live.client.app_state();
                let replacement = live.client.lease.clone();
                assert_ne!(old, replacement);
                // Readiness precedes these legitimate replacement publications.
                // Settle them before capturing both the warm fold and durable prefix.
                tokio::time::timeout(Duration::from_secs(10), async {
                    let (mut assigned, mut capabilities) = (false, false);
                    while !assigned || !capabilities {
                        match live.client.next_event().await.expect("actual replacement handshake publication") {
                            Event::AcpSessionAssigned { acp_session_id, .. }
                                if acp_session_id == live.sid => assigned = true,
                            Event::PromptCapabilities { .. } => capabilities = true,
                            Event::AgentStartupError { message, .. } => panic!("{message}"),
                            Event::AcpConnectionUnavailable { .. } => panic!("the replacement original channel stays live"),
                            _ => {}
                        }
                    }
                }).await.expect("the replacement handshake precedes the no-old-worker baseline");
                let supervisor = live.state.acp_supervisor.clone();
                let store = live.state.acp_event_store.clone();
                let cache = live.state.acp_control_cache.clone();
                assert!(Arc::ptr_eq(&cache, store.control_cache()));
                let before = live.state.session_service.fold_control_state(&id).await.unwrap();
                let prefix = store.highest_seq(&id).unwrap();
                let mut frames = live.state.acp_events_tx.subscribe();
                let error = || Event::PromptRuntimeError {
                    message: "operation failed".into(),
                    evidence: Some(OperationErrorEvidence::new(ErrorEvidenceSource::LocalPromptResult, ErrorOperation::Prompt)),
                    native_subject: None,
                };
                let drain = Drain {
                        session_id: id.clone(), identity_only: false, history_scope: old.native_scope().unwrap(),
                        projection: old.retirement().establishment_projection(),
                        sink: Arc::clone(&supervisor.sink), workers: Arc::clone(&supervisor.workers),
                        next_seqs: Arc::clone(&supervisor.next_seqs), incompatible_binaries: Arc::clone(&supervisor.incompatible_binaries),
                        lifecycle: Arc::clone(&supervisor.lifecycle), process_control: Arc::clone(&supervisor.process_control),
                        launcher: Arc::clone(&supervisor.launcher), notify: Arc::clone(&supervisor.worker_notify),
                        startup_requeues: Arc::clone(&supervisor.startup_requeues),
                        rate_limit_wake: Arc::clone(&supervisor.rate_limit_wake), recovery: None,
                        pending_context_resets: Arc::clone(&supervisor.pending_context_resets), context_reset: None,
                        respawned_in_place: Arc::clone(&supervisor.respawned_in_place), receive_owner: None,
                    };
                for event in [error(), Event::Stopped { reason: "prompt_failed".into() }] {
                    let mut data = AcpEventData::from_history(event);
                    assert!(matches!(drain.publish_owned(&old, &mut data, None, None).await, OwnedPublication::Superseded));
                }
                let (closed, inbound) = mpsc::channel(1);
                drop(closed);
                let empty = supervisor.start_drain_task(id.clone(), old, inbound, None);
                tokio::time::timeout(Duration::from_secs(10), empty).await.unwrap().unwrap();
                supervisor.sink.flush();
                assert!(supervisor.workers.lock().await.get(&id).unwrap().restart_history.is_empty());
                assert!(store.replay_from(&id, prefix).unwrap().is_empty());
                assert_eq!(store.highest_seq(&id).unwrap(), prefix);
                assert_eq!(super::super::try_activity_seq(&supervisor.next_seqs, &id).expect("the unchanged sequence remains observable"), prefix);
                assert!(matches!(frames.try_recv(), Err(tokio::sync::broadcast::error::TryRecvError::Empty)));
                let unchanged = cache.get_or_hydrate(&id, || panic!("the warm fold must survive")).unwrap();
                assert_eq!(unchanged.last_seq, before.last_seq);
                assert_eq!(unchanged.operation_error, None);
                assert_eq!(supervisor.worker_state(&id).await, AcpWorkerState::Running);
                assert!(supervisor.take_startup_requeues().is_empty());
                let request = live.start_prompt("distinct actual replacement input").await;
                live.native.send(serde_json::json!({"jsonrpc":"2.0","id":request["id"],
                    "error":{"code":-32603,"message":"operation failed"}})).await;
                let terminal = tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let frame = frames.recv().await.unwrap();
                        if matches!(frame.event.as_ref(), Event::Stopped { reason } if reason == "prompt_rejected") { break frame; }
                    }
                }).await.expect("the real replacement Native request produces its own durable failure and terminal");
                assert_eq!(terminal.seq, prefix + 5, "only the replacement turn allocates frames after the prefix");
                assert_eq!(terminal.worker_generation, Some(replacement.epoch()));
                let state = cache.get_or_hydrate(&id, || panic!("the same fold remains warm")).unwrap();
                assert_eq!(state.sidebar_status(), Status::Error);
                assert!(!state.turn_active);
                assert_eq!(state.operation_error.unwrap().generation, Some(ErrorEvidenceGeneration::AcpWorker(replacement.epoch())));
                let suffix = store.replay_from(&id, prefix).unwrap();
                assert_eq!(suffix.len(), 5);
                assert!(suffix.iter().any(|stored| matches!(stored.event.as_ref(), Event::PromptRejected { text, .. } if text == "distinct actual replacement input")));
                assert!(suffix.iter().any(|stored| matches!(stored.event.as_ref(), Event::PromptRuntimeError { message, .. } if message == "operation failed")
                    && stored.metadata.native_prompt_id == request["id"].as_i64() && stored.metadata.native_origin.is_some()));
                assert_eq!(supervisor.worker_state(&id).await, AcpWorkerState::Running);
                assert!(supervisor.workers.lock().await.get(&id).unwrap().restart_history.is_empty());
            })).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[serial_test::serial]
    async fn a_stalled_event_save_does_not_stop_the_drain_reading_frames() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (sink, store, _rx, tmp) = super::super::test_support::channel_sink();
        let supervisor = Supervisor::new(Arc::clone(&sink));
        let id = "s-stalled-save";
        let lease = supervisor.test_install_stdio(id).await;
        assert!(
            lease.retirement().establishment_projection().is_none(),
            "this descriptive transport fixture cannot advance a real Native SDK frontier"
        );
        let blocker = rusqlite::Connection::open(tmp.path().join("acp.db")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let (inbound_tx, inbound_rx) = mpsc::channel(64);
        let drain = supervisor.start_drain_task(id.into(), lease, inbound_rx, None);
        let frames = 256_u64;
        let sent = tokio::time::timeout(Duration::from_secs(2), async {
            for _ in 0..frames {
                inbound_tx
                    .send(SdkEvent::Data(AcpEventData::from_history(
                        Event::ThinkingStarted,
                    )))
                    .await
                    .unwrap();
            }
        })
        .await;
        blocker.execute_batch("COMMIT").unwrap();
        sent.expect(
            "the owned drain must keep reading frames while an actual SQLite save is stalled",
        );
        drop(inbound_tx);
        tokio::time::timeout(Duration::from_secs(10), drain)
            .await
            .expect("ordered completion reaches EOF after every queued DATA frame")
            .unwrap();
        sink.flush();
        let stored: Vec<_> = store
            .replay_from(id, 0)
            .unwrap()
            .into_iter()
            .filter(|stored| matches!(stored.event.as_ref(), Event::ThinkingStarted))
            .map(|stored| {
                assert_eq!(stored.metadata.scope, EventScope::HistoryOnly);
                stored.seq
            })
            .collect();
        assert_eq!(
            stored,
            (1..=frames).collect::<Vec<_>>(),
            "every descriptive frame is stored once, in order, without Native authority"
        );
    }
}
