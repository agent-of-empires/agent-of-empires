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
    await_resume_to_completion, before_session_env, overlay_env, publish_rejection,
    refresh_spawn_model_effort, resolve_mcp_servers,
};
use super::teardown::{settle_sweeping_if_owed, tear_down_runner, wait_for_exit};
use super::{
    lock_recover, with_publication_seq, BroadcastSink, Launcher, PendingContextReset,
    ResumeReservation, SeqMap, SharedSet, Supervisor, SupervisorError, WorkerKind, Workers,
    MAX_RESPAWNS_IN_WINDOW, RESPAWN_BACKOFF, RESTART_WINDOW,
};
use crate::acp::acp_client::{AcpError, SpawnConfig};
use crate::acp::event_sender::{AcpEventData, EventScope, SdkEvent};
use crate::acp::runner_lifecycle::{
    InstallError, Lease, LifecycleTable, ProcessControl, RunnerIdentity,
};
use crate::acp::state::{AcpSessionId, Event};
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
    sink: Arc<S>,
    workers: Workers,
    next_seqs: Arc<SeqMap>,
    incompatible_binaries: Arc<std::sync::Mutex<HashMap<String, String>>>,
    lifecycle: Arc<std::sync::Mutex<LifecycleTable>>,
    process_control: Arc<dyn ProcessControl>,
    launcher: Launcher,
    notify: Arc<tokio::sync::Notify>,
    startup_requeues: SharedSet,
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
        with_publication_seq(&self.next_seqs, &self.session_id, |seq| {
            self.sink
                .publish(&self.session_id, seq, &AcpEventData::from_host(event));
        });
    }

    async fn run(mut self, mut lease: Lease, mut inbound: mpsc::Receiver<SdkEvent>) {
        loop {
            let end = self.pump(&mut inbound, &lease).await;
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
                return;
            }
            if end.startup_failed {
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
            let Some(config) = self.approve_respawn(&lease).await else {
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
    ) -> OwnedPublication {
        loop {
            match self
                .try_publish_owned(
                    lease,
                    data,
                    replay
                        .as_mut()
                        .map(|(ordinal, replay)| (*ordinal, &mut **replay)),
                )
                .await
            {
                Ok(result) => {
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
    ) -> Result<OwnedPublication, PublicationWait> {
        let mut workers = self.workers.lock().await;
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
            && matches!(
                data.metadata.scope,
                EventScope::SessionCurrent | EventScope::OperationCurrent
            ) {
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
        if current {
            if let Some(handle) = workers.get_mut(&self.session_id) {
                if let Event::AcpSessionAssigned { acp_session_id, .. } = data.event.as_ref() {
                    let certified = handle
                        .client
                        .establishment_projection()
                        .and_then(|projection| projection.applied())
                        .is_some_and(|postimage| {
                            postimage.choice.acp_session_id.as_deref()
                                == Some(acp_session_id.as_str())
                        });
                    if certified {
                        handle.native_session_id = Some(acp_session_id.clone());
                        if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                            spawn_config.stored_acp_session_id = Some(acp_session_id.clone());
                            spawn_config.seed_history_replay = false;
                            spawn_config.fork_from = None;
                        }
                    }
                }
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
        let persisted =
            with_publication_seq(&self.next_seqs, &self.session_id, |seq| match replay {
                Some(replay) => self.sink.publish_replay_from_worker(
                    &self.session_id,
                    seq,
                    data,
                    lease.epoch(),
                    native_subject,
                    replay,
                ),
                None => self.sink.publish_from_worker(
                    &self.session_id,
                    seq,
                    data,
                    lease.epoch(),
                    native_subject,
                ),
            });
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
                Ok(()) => return,
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
        with_publication_seq(&self.next_seqs, &self.session_id, |seq| {
            self.sink.publish_from_worker(
                &self.session_id,
                seq,
                &data,
                lease.epoch(),
                Some(subject),
            );
        });
        Ok(())
    }

    async fn resolve_establishment(
        &self,
        lease: &Lease,
        delivery: &Arc<crate::acp::event_sender::EstablishmentDelivery>,
    ) -> crate::acp::event_sender::EstablishmentResolution {
        use crate::acp::establishment::EstablishmentRefusal;
        use crate::acp::event_sender::EstablishmentResolution;
        let Some(projection) = lease.retirement().establishment_projection() else {
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
            self.publish_owned(lease, &mut notice, None).await,
            OwnedPublication::Published { .. }
        )
    }

    async fn pump(&mut self, inbound: &mut mpsc::Receiver<SdkEvent>, lease: &Lease) -> StreamEnd {
        use crate::acp::event_sender::EstablishmentResolution;
        let mut end = StreamEnd::default();
        let mut established = false;
        while let Some(incoming) = {
            #[cfg(test)]
            let incoming = match self.receive_owner.as_ref() {
                Some(owner) => owner.recv(inbound).await,
                None => inbound.recv().await,
            };
            #[cfg(not(test))]
            let incoming = inbound.recv().await;
            incoming
        } {
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
                    continue;
                }
                SdkEvent::ControlPlaneLoss(loss) => {
                    self.publish_connection_loss(lease, &loss).await;
                    continue;
                }
                SdkEvent::Data(data) => data,
            };
            let projection = lease.retirement().establishment_projection();
            let mut processed = projection
                .as_deref()
                .and_then(|state| state.received_event());
            let ordinal = processed.as_ref().map(|event| event.ordinal());
            let mut replay =
                projection.as_ref().and_then(|state| {
                    let (attempt, first) = state.original_window()?;
                    if !ordinal.is_some_and(|ordinal| state.original_range_contains(ordinal)) {
                        return None;
                    }
                    Some(state.take_replay().unwrap_or_else(|| {
                        crate::acp::event_store::ReplayCommit::new(attempt, first)
                    }))
                });
            let publication = self
                .publish_owned(lease, &mut data, ordinal.zip(replay.as_mut()))
                .await;
            if let Some(journal_publication) = data.journal_publication.take() {
                let _ = journal_publication.send(matches!(
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
                | OwnedPublication::Failed => continue,
                OwnedPublication::Superseded => {
                    end.superseded = true;
                    continue;
                }
            }
            match data.event.as_ref() {
                Event::Stopped { reason } => match reason.as_str() {
                    "agent_unresponsive" | "prompt_orphaned" | "user_forced" => {
                        end.agent_unresponsive = true
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
                let mut error = AcpEventData::from_history(Event::SessionNotice {
                    key: Some("restart-budget".into()),
                    severity: "error".into(),
                    title: "Automatic restart budget exhausted".into(),
                    description: Some(format!(
                        "The ACP connection ended more than {} times in {}s; automatic restart is paused.",
                        MAX_RESPAWNS_IN_WINDOW, RESTART_WINDOW.as_secs(),
                    )),
                });
                if !matches!(
                    self.publish_owned(lease, &mut error, None).await,
                    OwnedPublication::Published { .. }
                ) {
                    return None;
                }
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
                    return None;
                }
                lock_recover(&self.startup_requeues).insert(session_id.clone());
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
        struct RegistryObservation;
        impl Drop for RegistryObservation {
            fn drop(&mut self) {
                DEFERRED_REGISTRY_OBSERVED.lock().unwrap().take();
            }
        }
        super::super::test_support::managed::with_managed(|scope| Box::pin(async move {
                let directory = scope.tempdir();
                let transcript = directory.path().join("agent.jsonl");
                std::fs::write(&transcript, "").unwrap();
                let mut fixture = super::super::launch::tests::managed_background_original(
                    scope, "drain-observation-recovery", transcript.to_str().unwrap(), Some("pending-task"), false).await;
                fixture.client = scope.adopt(fixture.client, false).await;
                fixture.client.ready().await;
                let id = fixture.client.id.clone();
                let agent_allocation = fixture.store.unresolved_background_agent_launches(&id).unwrap()
                    .into_iter().find(|launch| launch.agent_id == "sub-1").unwrap()
                    .witness.unwrap().allocation;
                let task_allocation = fixture.store.unresolved_background_task_launches(&id).unwrap()
                    .into_iter().find(|launch| launch.task_id == "pending-task").unwrap()
                    .witness.unwrap().allocation;
                // A later legacy history row may reuse an ID without superseding its Native allocation.
                let legacy_launches: Vec<_> = fixture.store.replay_from(&id, 0).unwrap()
                    .into_iter().filter(|stored| matches!(stored.event.as_ref(),
                        Event::BackgroundAgentLaunched { .. } | Event::BackgroundTaskStarted { .. }))
                    .map(|stored| stored.event.as_ref().clone()).collect();
                let supervisor = fixture.client.app_state().acp_supervisor.clone();
                for launch in legacy_launches {
                    supervisor.publish_next(&id, launch);
                }
                assert_eq!(fixture.store.unresolved_background_agent_tracking_launches(&id).unwrap().len(), 2);
                assert_eq!(fixture.store.unresolved_background_task_launches(&id).unwrap().len(), 2);
                let sid = fixture.client.actual().establishment_projection().unwrap().receipt_snapshot().unwrap()
                    .session_id().unwrap().to_owned();
                fixture.peer.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":sid,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"history before registry refusal"}}
                }})).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !matches!(fixture.client.recv().await.expect("attached original history observer remains open"),
                        Event::AgentMessageChunk { text, .. } if text == "history before registry refusal") {}
                }).await.expect("real Native history is committed before contention");
                let prefix = fixture.store.highest_seq(&id);
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
                assert_eq!(fixture.store.highest_seq(&id), prefix, "registry contention cannot authorize a terminal publication");
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
                for (allocation, agent) in [(agent_allocation, true), (task_allocation, false)] {
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
                }
                assert_eq!(events.iter().filter(|stored|
                    stored.metadata.resource_ref.is_none() && matches!(stored.event.as_ref(),
                        Event::BackgroundAgentCompleted { agent_id, .. } if agent_id == "sub-1")).count(), 1);
                assert_eq!(events.iter().filter(|stored|
                    stored.metadata.resource_ref.is_none() && matches!(stored.event.as_ref(),
                        Event::BackgroundTaskFinished { task_id, .. } if task_id == "pending-task")).count(), 1);
                assert!(events.iter().any(|stored| matches!(stored.event.as_ref(),
                    Event::AgentMessageChunk { text, .. } if text == "history before registry refusal")));
                assert!(!events.iter().any(|stored| matches!(stored.event.as_ref(), Event::SessionContextReset { .. })));
            })).await;
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
                    }
                    let supervisor = client.app_state().acp_supervisor.clone();
                    let mut frames = client.subscribe_publications();
                    let store = scope.state.acp_event_store.clone();
                    let prefix = store.highest_seq(&client.id);
                    let storage = crate::session::Storage::open_reference("test").unwrap();
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
                    let contended_path = tokio::time::timeout(Duration::from_secs(10),
                        tokio::task::spawn_blocking(move || contended_rx.recv()))
                        .await.expect("the captured diagnostic reaches actual flock contention")
                        .unwrap().expect("the actual publication wait reports its physical blocker");
                    assert_eq!(contended_path, storage.sessions_path().parent().unwrap().join(
                        if profile_writer { crate::session::STORAGE_LOCK_FILENAME.to_owned() }
                        else { format!(".instance-lifecycle-{}.lock", client.id) }));
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
                    let terminal = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let frame = frames.recv().await.unwrap();
                            if frame.session_id == client.id && matches!(frame.event.as_ref(), Event::Stopped { reason } if reason == "prompt_failed") {
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
        let (inbound_tx, inbound_rx) = mpsc::channel::<crate::acp::event_sender::SdkEvent>(4);
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

        let socket = worker_registry::socket_path_for("s-store").unwrap();
        let mut config = runner_config(socket);
        config.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: selected.clone(),
            exported_default_store: None,
        });
        config.host_environment = vec![("CLAUDE_CONFIG_DIR".into(), "stale".into())];
        config.base_host_environment = vec![("HOME".into(), temp.path().display().to_string())];
        Drain::<VecSink>::refresh_launch_env("s-store", &mut config).await;
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
}
