//! Bringing a worker up: admission, spawn, attach, and the launch environment.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, info, warn};

use super::agents::{
    apply_agent_command_override, log_wrapper_substitution, wrapper_substitution_for,
};
use super::publish::collect_resumable_background_agent_launches;
use super::{
    lock_recover, BroadcastSink, PendingContextReset, RestartRequestGuard, ResumeKind,
    ResumeReservation, SpawnRequest, Supervisor, SupervisorError, WorkerHandle, WorkerKind,
};
use crate::acp::acp_client::{AcpClient, AcpError, SpawnConfig};
use crate::acp::agent_policy::AgentPolicy;
use crate::acp::runner_lifecycle::{AdmitError, InstallError, Lease, RunnerIdentity};
use crate::acp::state::{AcpSessionId, Event};
use crate::process::worker_registry;
use crate::session::config::repo_config::resolve_config_with_repo_or_warn;
use crate::session::SandboxInfo;
use tokio_util::sync::CancellationToken;

/// Immutable intent captured before queueing. It is not native RPC permission.
pub(crate) struct LiveFreshProposal {
    workers: std::sync::Weak<Mutex<std::collections::HashMap<String, WorkerHandle>>>,
    lifecycle: std::sync::Weak<std::sync::Mutex<crate::acp::runner_lifecycle::LifecycleTable>>,
    client: std::sync::Weak<AcpClient>,
    lease: Lease,
    owner: RunnerIdentity,
    admission: super::LaunchAdmission,
    expected: crate::acp::establishment::EstablishmentExpected,
    attempt: Arc<crate::acp::establishment::EstablishmentAttempt>,
    projection: Arc<crate::acp::event_sender::ProjectionState>,
    predecessor: Arc<crate::acp::event_sender::EstablishmentDelivery>,
    postimage: Arc<crate::acp::establishment::EstablishmentPostimage>,
    receipt: Arc<crate::acp::establishment::NativeEstablishmentReceipt>,
    seal: crate::acp::event_store::EstablishmentSeal,
    journal: Arc<crate::acp::event_store::EventStore>,
}

pub(crate) struct LiveFreshArm {
    pub(crate) begun: bool,
    pub(crate) admitted: bool,
    pub(crate) confirmed: bool,
}

impl std::fmt::Debug for LiveFreshProposal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveFreshProposal")
            .finish_non_exhaustive()
    }
}

impl LiveFreshProposal {
    pub(crate) fn attempt(&self) -> Arc<crate::acp::establishment::EstablishmentAttempt> {
        Arc::clone(&self.attempt)
    }

    pub(crate) fn owner(&self) -> RunnerIdentity {
        self.owner
    }

    pub(crate) fn store(&self) -> Option<crate::session::ExecutionBinding> {
        self.receipt.store.clone()
    }

    pub(crate) fn projection(&self) -> &Arc<crate::acp::event_sender::ProjectionState> {
        &self.projection
    }

    pub(crate) fn native_scope(&self) -> anyhow::Result<crate::acp::state::NativeObservationScope> {
        self.lease
            .native_scope()
            .map_err(|()| SupervisorError::NativeStoreUnavailable)?
            .filter(|scope| {
                scope
                    .choice
                    .as_ref()
                    .and_then(|choice| choice.authority)
                    .is_some_and(|key| key.id == self.attempt.id)
            })
            .ok_or_else(|| crate::acp::establishment::EstablishmentRefusal::Superseded.into())
    }

    pub(crate) async fn current(&self) -> bool {
        let (Some(workers), Some(lifecycle), Some(client)) = (
            self.workers.upgrade(),
            self.lifecycle.upgrade(),
            self.client.upgrade(),
        ) else {
            return false;
        };
        let workers = workers.lock().await;
        let lifecycle = lock_recover(&lifecycle);
        workers.get(self.lease.session_id()).is_some_and(|handle| {
            handle.lease == self.lease && Arc::ptr_eq(&handle.client, &client)
        }) && lifecycle.current_lease(self.lease.session_id()).as_ref() == Some(&self.lease)
            && lifecycle.phase(self.lease.session_id())
                == crate::acp::runner_lifecycle::WorkerPhase::Running
            && !self.lease.cancellation().is_cancelled()
            && !self.lease.stop_completion_pending()
            && client.connected_runner() == Some(self.owner)
            && client.ensure_available().is_ok()
    }

    fn check_current(&self) -> anyhow::Result<()> {
        use crate::acp::establishment::EstablishmentRefusal;
        let workers = self
            .workers
            .upgrade()
            .ok_or(EstablishmentRefusal::Superseded)?;
        let lifecycle = self
            .lifecycle
            .upgrade()
            .ok_or(EstablishmentRefusal::Superseded)?;
        let client = self
            .client
            .upgrade()
            .ok_or(EstablishmentRefusal::Superseded)?;
        let workers = workers
            .try_lock()
            .map_err(|_| super::NativeObservationBusy)?;
        let lifecycle = lifecycle
            .try_lock()
            .map_err(|_| super::NativeObservationBusy)?;
        anyhow::ensure!(
            workers.get(self.lease.session_id()).is_some_and(|handle| {
                handle.lease == self.lease && Arc::ptr_eq(&handle.client, &client)
            }) && lifecycle.current_lease(self.lease.session_id()).as_ref() == Some(&self.lease)
                && lifecycle.phase(self.lease.session_id())
                    == crate::acp::runner_lifecycle::WorkerPhase::Running
                && !self.lease.cancellation().is_cancelled()
                && !self.lease.stop_completion_pending()
                && self.owner.birth_fingerprint.is_some()
                && client.connected_runner() == Some(self.owner)
                && client.ensure_available().is_ok(),
            EstablishmentRefusal::Superseded
        );
        Ok(())
    }

    fn check_owner(&self) -> anyhow::Result<()> {
        use crate::acp::establishment::EstablishmentRefusal;
        let registry = worker_registry::try_registry_lock(self.lease.session_id())?
            .ok_or(worker_registry::RegistryBusy)?;
        let record = registry.record()?.ok_or(crate::session::SessionGone)?;
        anyhow::ensure!(
            registry.stop()?.is_none()
                && self.owner.owns_record(&record)
                && record.source_profile.as_deref() == Some(self.attempt.profile.as_str())
                && crate::process::process_birth_identity(self.owner.pid)?
                    .map(|birth| birth.fingerprint())
                    == self.owner.birth_fingerprint,
            EstablishmentRefusal::Superseded
        );
        Ok(())
    }

    pub(crate) async fn namespace(
        &self,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>> {
        match &self.admission.namespace {
            Some(namespace) => Ok(Some(
                tokio::time::timeout_at(deadline, Arc::clone(namespace).read_owned()).await?,
            )),
            None => Ok(None),
        }
    }

    /// One native CAS. Even an unconfirmed write is observed, never re-armed.
    pub(crate) async fn arm(
        self: Arc<Self>,
        namespace: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<LiveFreshArm> {
        tokio::task::spawn_blocking(move || {
            use crate::acp::establishment::EstablishmentRefusal;
            let _namespace = namespace;
            let store = &self.admission.store;
            let _native = store
                .storage()
                .acquire_instance_lifecycle_lock(self.lease.session_id())?;
            self.check_current()?;
            self.check_owner()?;
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline
                    && self
                        .projection
                        .applied()
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &self.postimage))
                    && self
                        .projection
                        .delivery()
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &self.predecessor))
                    && self.seal.matches(&self.receipt),
                EstablishmentRefusal::Unconfirmed
            );
            // Clear/supersession can invalidate a seal while the command waits.
            let seal = self.journal.establishment_seal(&self.receipt)?;
            let mut mutation = |rows: &mut Vec<crate::session::Instance>,
                                _: &mut Vec<crate::session::Group>|
             -> anyhow::Result<()> {
                let row = rows
                    .iter_mut()
                    .find(|row| row.id == self.lease.session_id())
                    .ok_or(crate::session::SessionGone)?;
                anyhow::ensure!(
                    tokio::time::Instant::now() < deadline
                        && !self.lease.cancellation().is_cancelled()
                        && self.expected.matches(row, store.storage())?,
                    EstablishmentRefusal::Superseded
                );
                row.install_prepared_acp_fresh(
                    store.storage(),
                    &self.expected,
                    &self.attempt,
                    self.lease.producer_id(),
                )
            };
            let committed = store.commit(&mut mutation);
            let row = store
                .storage()
                .load_instances_complete()?
                .into_iter()
                .find(|row| row.id == self.lease.session_id())
                .ok_or(crate::session::SessionGone)?;
            if !self.attempt.matches_pending(&row, store.storage())? {
                committed?;
                anyhow::bail!(EstablishmentRefusal::Superseded);
            }
            let confirmed = committed.is_ok();
            if let Err(error) = committed {
                warn!(target: "acp.establishment", %error,
                    "live Fresh native write is present but unconfirmed");
            }
            let begun = self.projection.begin(
                Arc::clone(&self.attempt),
                &self.predecessor,
                &self.postimage,
                &seal,
            );
            if !begun {
                return Ok(LiveFreshArm {
                    begun: false,
                    admitted: false,
                    confirmed,
                });
            }
            let mut prepared = self.admission.clone();
            prepared.generation = self.attempt.generation;
            prepared.establishment = Some(Arc::clone(&self.attempt));
            prepared.native_scope = Some(crate::acp::state::NativeObservationScope::capture(
                &row,
                store.storage(),
            )?);
            let admitted = (|| -> anyhow::Result<bool> {
                let workers = self
                    .workers
                    .upgrade()
                    .ok_or(EstablishmentRefusal::Superseded)?;
                let lifecycle = self
                    .lifecycle
                    .upgrade()
                    .ok_or(EstablishmentRefusal::Superseded)?;
                let client = self
                    .client
                    .upgrade()
                    .ok_or(EstablishmentRefusal::Superseded)?;
                let workers = workers
                    .try_lock()
                    .map_err(|_| super::NativeObservationBusy)?;
                let lifecycle = lifecycle
                    .try_lock()
                    .map_err(|_| super::NativeObservationBusy)?;
                if !workers.get(self.lease.session_id()).is_some_and(|handle| {
                    handle.lease == self.lease && Arc::ptr_eq(&handle.client, &client)
                }) {
                    return Ok(false);
                }
                Ok(lifecycle.replace_live_fresh_admission(
                    &self.lease,
                    self.owner,
                    &self.admission,
                    prepared,
                ))
            })()
            .unwrap_or(false);
            Ok(LiveFreshArm {
                begun,
                admitted,
                confirmed,
            })
        })
        .await?
    }

    /// A pre-arm refusal may keep old usability only with fresh positive proof.
    pub(crate) async fn old_is_current(self: &Arc<Self>) -> bool {
        let original = Arc::clone(self);
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            original.check_current()?;
            original.check_owner()?;
            let store = &original.admission.store;
            let _native = store
                .storage()
                .acquire_instance_lifecycle_lock(original.lease.session_id())?;
            let row = store
                .storage()
                .load_instances_complete()?
                .into_iter()
                .find(|row| row.id == original.lease.session_id())
                .ok_or(crate::session::SessionGone)?;
            anyhow::ensure!(
                original.expected.matches(&row, store.storage())?
                    && original.postimage.matches(&row, store.storage())?
                    && original
                        .projection
                        .applied()
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &original.postimage)),
                crate::acp::establishment::EstablishmentRefusal::Superseded
            );
            original.journal.establishment_seal(&original.receipt)?;
            Ok(())
        })
        .await
        .is_ok_and(|result| result.is_ok())
    }
}

pub(super) fn registry_error(error: anyhow::Error) -> SupervisorError {
    if error.is::<worker_registry::RegistryBusy>() {
        SupervisorError::RegistryBusy
    } else {
        SupervisorError::Acp(AcpError::Spawn(error.to_string()))
    }
}

pub(super) fn claimed_stop_for_lease(
    registry: &worker_registry::RegistryGuard<'_>,
    lease: &Lease,
    identity: Option<RunnerIdentity>,
) -> Result<Option<worker_registry::WorkerRecord>, SupervisorError> {
    let Some(intent) = registry.stop().map_err(registry_error)? else {
        return Ok(None);
    };
    let record = registry
        .claim_stop(&intent)
        .map_err(registry_error)?
        .ok_or_else(|| SupervisorError::Superseded(lease.session_id().into()))?;
    let owner = RunnerIdentity::from_record(&record);
    if identity != Some(owner) || !lease.record_runner(owner) {
        return Err(SupervisorError::Superseded(lease.session_id().into()));
    }
    Ok(Some(record))
}

async fn revalidate_attach_record(
    record: &worker_registry::WorkerRecord,
    deadline: tokio::time::Instant,
) -> Result<(), SupervisorError> {
    let registry = worker_registry::registry_lock_until(&record.session_id, deadline)
        .await
        .map_err(registry_error)?;
    registry
        .revalidate_control_record(record)
        .map_err(|refusal| AcpError::RetainedRuntimeUnavailable(Box::new(refusal)).into())
}

#[cfg(test)]
type AttachInstallGate = (
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);
#[cfg(test)]
static ATTACH_INSTALL_GATE: std::sync::Mutex<Option<AttachInstallGate>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) static OWNED_INSTALL_GATE: std::sync::Mutex<
    Option<(
        tokio::sync::oneshot::Sender<Arc<AcpClient>>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
> = std::sync::Mutex::new(None);

impl<S: BroadcastSink> Supervisor<S> {
    pub(crate) async fn prepare_live_fresh(
        &self,
        session_id: &str,
        client: &Arc<AcpClient>,
        deadline: tokio::time::Instant,
    ) -> anyhow::Result<Arc<LiveFreshProposal>> {
        use crate::acp::establishment::{
            EstablishmentExpected, EstablishmentRecovery, EstablishmentRecoveryAction,
            EstablishmentRefusal,
        };
        let lease = {
            let workers = self.workers.lock().await;
            let handle = workers
                .get(session_id)
                .ok_or(EstablishmentRefusal::Superseded)?;
            anyhow::ensure!(
                Arc::ptr_eq(&handle.client, client),
                EstablishmentRefusal::Superseded
            );
            handle.lease.clone()
        };
        let admission = lease.admission().ok_or(EstablishmentRefusal::Unconfirmed)?;
        let owner = client
            .connected_runner()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let projection = client
            .establishment_projection()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let predecessor = projection
            .delivery()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let postimage = projection
            .applied()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let receipt = predecessor
            .receipt
            .clone()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let journal = self
            .sink
            .establishment_store()
            .ok_or(EstablishmentRefusal::Unconfirmed)?;
        let namespace = match &admission.namespace {
            Some(namespace) => {
                Some(tokio::time::timeout_at(deadline, Arc::clone(namespace).read_owned()).await?)
            }
            None => None,
        };
        let workers = Arc::downgrade(&self.workers);
        let lifecycle = Arc::downgrade(&self.lifecycle);
        let client = Arc::downgrade(client);
        tokio::task::spawn_blocking(move || {
            let _namespace = namespace;
            let store = &admission.store;
            let _native = store
                .storage()
                .acquire_instance_lifecycle_lock(lease.session_id())?;
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline
                    && owner.birth_fingerprint.is_some()
                    && receipt.owner == owner
                    && receipt.attempt.id == postimage.attempt_id
                    && predecessor.complete,
                EstablishmentRefusal::Unconfirmed
            );
            let mut row = store
                .storage()
                .load_instances_complete()?
                .into_iter()
                .find(|row| row.id == lease.session_id())
                .ok_or(crate::session::SessionGone)?;
            // The original postimage remains projection/seal evidence. A new
            // explicit Fresh intent freezes the certified current choice here;
            // arm must match that exact expected value, never recapture it.
            receipt.current_applied_postimage(&row, store.storage(), &owner)?;
            let expected = EstablishmentExpected::capture(&row, store.storage())?;
            let seal = journal.establishment_seal(&receipt)?;
            let initial = row.arm_acp_establishment(
                store.storage(),
                Some(&EstablishmentRecovery {
                    expected: expected.clone(),
                    action: EstablishmentRecoveryAction::Fresh,
                }),
            )?;
            let attempt =
                Arc::new(row.bind_acp_producer(store.storage(), &initial, lease.producer_id())?);
            drop(_native);
            let proposal = Arc::new(LiveFreshProposal {
                workers,
                lifecycle,
                client,
                lease,
                owner,
                admission,
                expected,
                attempt,
                projection,
                predecessor,
                postimage,
                receipt,
                seal,
                journal,
            });
            proposal.check_current()?;
            proposal.check_owner()?;
            Ok(proposal)
        })
        .await?
    }

    pub(crate) fn validate_reserved_resume(
        &self,
        reservation: &ResumeReservation,
    ) -> Result<(), SupervisorError> {
        let lifecycle = match self.lifecycle.try_lock() {
            Ok(lifecycle) => lifecycle,
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return Err(SupervisorError::RegistryBusy),
        };
        if !lifecycle.construction_is_pending(reservation.lease())
            || lifecycle
                .current_lease(reservation.lease().session_id())
                .as_ref()
                != Some(reservation.lease())
            || lifecycle.cancel_requested(reservation.lease()).is_some()
        {
            return Err(SupervisorError::SpawnCancelled(
                reservation.lease().session_id().into(),
            ));
        }
        Ok(())
    }

    async fn consume_resume_stop(
        &self,
        reservation: &mut ResumeReservation,
    ) -> Result<(), SupervisorError> {
        if reservation.resume_stop.is_none() {
            return Ok(());
        }
        let lease = reservation.lease().clone();
        let id = lease.session_id();
        let deadline = if lease.is_borrowed_attach() {
            reservation.registry_deadline
        } else {
            tokio::time::Instant::now() + std::time::Duration::from_secs(3)
        };
        loop {
            {
                let _workers = tokio::time::timeout_at(deadline, self.workers.lock())
                    .await
                    .map_err(|_| SupervisorError::RegistryBusy)?;
                let lifecycle = lock_recover(&self.lifecycle);
                if tokio::time::Instant::now() >= deadline {
                    return Err(SupervisorError::RegistryBusy);
                }
                if let Some(registry) =
                    worker_registry::try_registry_lock(id).map_err(registry_error)?
                {
                    if !lifecycle.construction_is_pending(&lease)
                        || lifecycle.cancel_requested(&lease).is_some()
                    {
                        return Err(SupervisorError::SpawnCancelled(id.into()));
                    }
                    if let Some(observed) = reservation.resume_stop.as_ref() {
                        registry
                            .admit_resume(reservation.resume_authority, observed)
                            .map_err(registry_error)?;
                    }
                    reservation.stop_committed();
                    return Ok(());
                }
            }
            drop(
                worker_registry::registry_lock_until(id, deadline)
                    .await
                    .map_err(registry_error)?,
            );
        }
    }

    /// Spawn a structured view worker for the given session.
    pub async fn spawn(
        &self,
        req: SpawnRequest,
        authority: crate::process::worker_registry::ResumeAuthority,
    ) -> Result<(), SupervisorError> {
        let kind = if req.stored_acp_session_id.is_some() {
            ResumeKind::Spawn
        } else {
            ResumeKind::SpawnFirst
        };
        match self
            .begin_resume_with_restart_claim(&req.session_id, kind, authority, None, None, None)
            .await?
        {
            Some(r) => self.spawn_inner(req, r).await,
            None => Err(SupervisorError::AlreadyRunning(req.session_id)),
        }
    }

    /// Reserve once through the same admission used by request-aware launches.
    pub(crate) async fn reserve_spawn(
        &self,
        session_id: &str,
        kind: ResumeKind,
        authority: crate::process::worker_registry::ResumeAuthority,
    ) -> Result<ResumeReservation, SupervisorError> {
        self.reserve_spawn_with_restart(session_id, kind, authority, None)
            .await
    }

    pub(crate) async fn reserve_spawn_with_restart(
        &self,
        session_id: &str,
        kind: ResumeKind,
        authority: worker_registry::ResumeAuthority,
        restart: Option<&RestartRequestGuard>,
    ) -> Result<ResumeReservation, SupervisorError> {
        match self
            .begin_resume_with_restart_claim(session_id, kind, authority, None, restart, None)
            .await?
        {
            Some(reservation) => Ok(reservation),
            None => Err(SupervisorError::AlreadyRunning(session_id.to_owned())),
        }
    }

    pub(crate) async fn begin_resume(
        &self,
        session_id: &str,
        kind: ResumeKind,
        authority: worker_registry::ResumeAuthority,
    ) -> Result<Option<ResumeReservation>, SupervisorError> {
        self.begin_resume_inner(session_id, kind, authority, None)
            .await
    }

    /// Existing build/version replacement policy may reserve a spawn only for
    /// its frozen predecessor, after the same strict control-capability gate.
    pub(crate) async fn begin_governed_replacement(
        &self,
        session_id: &str,
        kind: ResumeKind,
        predecessor: RunnerIdentity,
    ) -> Result<Option<ResumeReservation>, SupervisorError> {
        self.begin_resume_inner(
            session_id,
            kind,
            worker_registry::ResumeAuthority::Automatic,
            Some(predecessor),
        )
        .await
    }

    /// Admit a resume and reserve its capacity slot; only spawns count, since
    /// an attach takes over a runner the registry already counts.
    async fn begin_resume_inner(
        &self,
        session_id: &str,
        kind: ResumeKind,
        authority: crate::process::worker_registry::ResumeAuthority,
        predecessor: Option<RunnerIdentity>,
    ) -> Result<Option<ResumeReservation>, SupervisorError> {
        self.begin_resume_with_restart_claim(session_id, kind, authority, predecessor, None, None)
            .await
    }

    async fn begin_resume_with_restart_claim(
        &self,
        session_id: &str,
        kind: ResumeKind,
        authority: crate::process::worker_registry::ResumeAuthority,
        predecessor: Option<RunnerIdentity>,
        restart: Option<&RestartRequestGuard>,
        cancellation: Option<CancellationToken>,
    ) -> Result<Option<ResumeReservation>, SupervisorError> {
        // Declared before body resources so every early admitted return drops them first.
        let mut _original_constructor = None;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let workers = tokio::time::timeout_at(deadline, self.workers.lock())
                .await
                .map_err(|_| SupervisorError::RegistryBusy)?;
            if tokio::time::Instant::now() >= deadline {
                return Err(SupervisorError::RegistryBusy);
            }
            {
                let request_state = lock_recover(&self.spawn_requests);
                if (request_state.stop_fences.contains_key(session_id) && restart.is_none())
                    || cancellation
                        .as_ref()
                        .is_some_and(CancellationToken::is_cancelled)
                {
                    return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
                }
            }
            {
                let restart_requests = lock_recover(&self.restart_requests);
                let active_restart = restart_requests.get(session_id);
                let restart_authorized = restart.is_some_and(|claim| {
                    claim.session_id == session_id
                        && Arc::ptr_eq(&claim.requests, &self.restart_requests)
                        && active_restart.is_some_and(|(request_id, token)| {
                            *request_id == claim.request_id && !token.is_cancelled()
                        })
                });
                if (active_restart.is_some() || restart.is_some()) && !restart_authorized {
                    if restart
                        .is_some_and(|claim| claim.session_id == session_id && claim.is_cancelled())
                    {
                        return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
                    }
                    return Err(SupervisorError::RestartInProgress(session_id.to_string()));
                }
            }
            {
                let mut table = lock_recover(&self.lifecycle);
                if let Some(registry) =
                    worker_registry::try_registry_lock(session_id).map_err(registry_error)?
                {
                    let observed_stop = registry.stop().map_err(registry_error)?;
                    registry
                        .verify_resume(authority, &observed_stop)
                        .map_err(registry_error)?;
                    let retained = if workers.contains_key(session_id) {
                        None
                    } else {
                        registry.control_admission().map_err(|refusal| {
                            AcpError::RetainedRuntimeUnavailable(Box::new(refusal))
                        })?
                    };
                    if predecessor.is_some_and(|expected| {
                        retained
                            .as_ref()
                            .is_none_or(|record| !expected.owns_record(record))
                    }) {
                        return Err(AcpError::RetainedRuntimeUnavailable(Box::new(
                            worker_registry::ControlAdmissionRefusal::unconfirmed(predecessor),
                        ))
                        .into());
                    }
                    if kind != ResumeKind::Attach && predecessor.is_none() {
                        if let Some(record) = retained
                            .as_ref()
                            .filter(|record| worker_registry::retained_process_may_be_alive(record))
                        {
                            return Err(AcpError::RetainedRuntimeUnavailable(Box::new(
                                worker_registry::ControlAdmissionRefusal::from_record(
                                    record,
                                    worker_registry::ControlRefusalReason::OwnershipUnconfirmed,
                                ),
                            ))
                            .into());
                        }
                    }
                    let attach_record = if kind == ResumeKind::Attach {
                        retained.map(Box::new)
                    } else {
                        None
                    };
                    if tokio::time::Instant::now() >= deadline {
                        return Err(SupervisorError::RegistryBusy);
                    }
                    if authority == worker_registry::ResumeAuthority::Explicit {
                        table.forget_stale_cancel(session_id);
                    }
                    let lease = match table.admit(session_id, kind) {
                        Ok(lease) => lease,
                        Err(AdmitError::AlreadyPresent) => return Ok(None),
                        Err(AdmitError::TeardownPending) => {
                            return Err(SupervisorError::TeardownPending(session_id.into()))
                        }
                        Err(AdmitError::Cancelled(_)) => {
                            return Err(SupervisorError::SpawnCancelled(session_id.into()))
                        }
                    };
                    {
                        _original_constructor =
                            Some(lease.begin_original_constructor(self.worker_notify.clone()));
                    }
                    let request_state = lock_recover(&self.spawn_requests);
                    if (request_state.stop_fences.contains_key(session_id) && restart.is_none())
                        || cancellation
                            .as_ref()
                            .is_some_and(CancellationToken::is_cancelled)
                    {
                        table.abandon(&lease);
                        return Err(SupervisorError::SpawnCancelled(session_id.to_string()));
                    }
                    drop(request_state);
                    if kind != ResumeKind::Attach {
                        let registry_count = worker_registry::list()
                            .map(|records| {
                                records
                                    .into_iter()
                                    .filter(|record| {
                                        worker_registry::is_record_live(record)
                                            && table.counts_registry_record(&record.session_id)
                                    })
                                    .count()
                            })
                            .unwrap_or(0);
                        let combined = table.occupied_slots() + registry_count;
                        if combined > self.max_concurrent_workers as usize {
                            table.abandon(&lease);
                            return Err(SupervisorError::CapacityFull {
                                current: combined - 1,
                                limit: self.max_concurrent_workers,
                            });
                        }
                    }
                    if let Some(record) = &attach_record {
                        table.note_generation(session_id, record.generation);
                        if !lease.record_runner(RunnerIdentity::from_record(record)) {
                            table.abandon(&lease);
                            return Err(SupervisorError::Superseded(session_id.into()));
                        }
                        lease.record_prime(
                            crate::acp::runner_lifecycle::PrimeOwnership::from_record(record),
                        );
                        lease.record_scope(record.scope.clone());
                    }
                    return Ok(Some(ResumeReservation {
                        lease,
                        expected_lifecycle_generation: None,
                        establishment: None,
                        attach_record,
                        preserve_owner: false,
                        registry_deadline: deadline,
                        resume_stop: Some(observed_stop),
                        resume_authority: authority,
                        lifecycle: Arc::clone(&self.lifecycle),
                        notify: Arc::clone(&self.worker_notify),
                        deferred: None,
                        deferred_claim: false,
                        deferred_gate: None,
                        constructor: _original_constructor.as_ref().map(|work| work.fork()),
                    }));
                }
            }
            drop(workers);
            drop(
                worker_registry::registry_lock_until(session_id, deadline)
                    .await
                    .map_err(registry_error)?,
            );
        }
    }

    /// Arm the native target before preparation effects without claiming an RPC was issued.
    pub(crate) async fn prepare_launch_admission(
        &self,
        admission: &mut super::LaunchAdmission,
        reservation: &mut ResumeReservation,
        namespace: Option<&tokio::sync::OwnedRwLockReadGuard<()>>,
    ) -> Result<crate::session::Instance, SupervisorError> {
        let lease = reservation.lease().clone();
        if admission.establishment.is_none() {
            admission.establishment = reservation.establishment.clone();
        }
        let preparation = async {
            capture_admission_scope(admission, lease.session_id(), namespace).await?;
            prepare_establishment(
                admission,
                lease.session_id(),
                Some(lease.producer_id()),
                namespace,
            )
            .await
        };
        let row = match await_resume_to_completion(reservation.cancellation(), preparation).await {
            Ok(result) => result?,
            Err(()) => return Err(self.cancelled_spawn(&lease).await),
        };
        reservation.establishment = admission.establishment.clone();
        lease.set_admission(Some(admission.clone()));
        if !lease.bind_establishment_admission(admission) {
            return Err(SupervisorError::Superseded(lease.session_id().to_owned()));
        }
        Ok(row)
    }

    /// Spawn body, run under the reservation from `begin_resume`.
    pub(crate) async fn spawn_inner(
        &self,
        mut req: SpawnRequest,
        mut reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        #[cfg(test)]
        let _original_constructor = reservation.constructor.as_ref().map(|work| work.fork());
        if self.native_admission_required
            && req.source_profile.is_none()
            && req.launch_admission.is_none()
        {
            return Err(SupervisorError::NativeStoreUnavailable);
        }
        let lease = reservation.lease().clone();
        if req.launch_admission.is_none() {
            match await_resume_to_completion(
                reservation.cancellation(),
                capture_durable_launch(
                    req.source_profile.as_deref(),
                    &req.session_id,
                    Some(req.expected_lifecycle_generation),
                    false,
                ),
            )
            .await
            {
                Ok(Ok((admission, _))) => req.launch_admission = admission,
                Ok(Err(error)) => return Err(error),
                Err(()) => return Err(self.cancelled_spawn(&lease).await),
            }
        }
        if let Some(admission) = req.launch_admission.as_mut() {
            if admission.establishment.is_none() {
                admission.establishment = reservation.establishment.clone();
            }
            self.prepare_launch_admission(admission, &mut reservation, None)
                .await?;
            if let Some(attempt) = &admission.establishment {
                use crate::acp::establishment::EstablishmentTarget;
                match &attempt.target {
                    EstablishmentTarget::New => {
                        req.stored_acp_session_id = None;
                        req.fork_from = None;
                        req.seed_history_replay = false;
                    }
                    EstablishmentTarget::Load { session_id, import } => {
                        req.stored_acp_session_id = Some(session_id.clone());
                        req.fork_from = None;
                        req.seed_history_replay = *import;
                    }
                    EstablishmentTarget::Fork { session_id } => {
                        req.stored_acp_session_id = None;
                        req.fork_from = Some(session_id.clone());
                        req.seed_history_replay = false;
                    }
                }
            }
        }
        lease.set_admission(req.launch_admission.clone());
        match await_resume_to_completion(
            reservation.cancellation(),
            admit_durable_launch(req.launch_admission.as_ref(), &req.session_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(()) => return Err(self.cancelled_spawn(&lease).await),
        }
        self.consume_resume_stop(&mut reservation).await?;
        let session_id = req.session_id.as_str();
        let warmup_guard =
            match await_resume(reservation.cancellation(), self.warmup_guard(&req.agent)).await {
                Ok(guard) => guard,
                Err(()) => return Err(self.cancelled_spawn(&lease).await),
            };
        let (mut config, context_reset) = match await_resume_to_completion(
            reservation.cancellation(),
            self.spawn_config(&req, lease.epoch()),
        )
        .await
        {
            Ok(Ok(config)) => config,
            Ok(Err(error)) => return Err(error),
            Err(()) => {
                drop(warmup_guard);
                return Err(self.cancelled_spawn(&lease).await);
            }
        };
        if !config.uses_native_prime() {
            lease.record_prime(crate::acp::runner_lifecycle::PrimeOwnership::NotPrime);
        }
        config.launch_lease = Some(lease.clone());
        debug!(
            target: "acp.supervisor",
            session = %session_id,
            stored_id = ?config.stored_acp_session_id,
            "spawning structured view worker"
        );
        // The hooks above may re-enter aoe, so the lifecycle lock is taken only for each check.
        match await_resume_to_completion(
            reservation.cancellation(),
            admit_durable_launch(req.launch_admission.as_ref(), &req.session_id),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(()) => {
                drop(warmup_guard);
                return Err(self.cancelled_spawn(&lease).await);
            }
        }
        // Clear a partial replay from a failed import before session/load re-emits it.
        if config.seed_history_replay {
            let sink = Arc::clone(&self.sink);
            let id = req.session_id.clone();
            with_durable_launch(req.launch_admission.clone(), id.clone(), move || {
                sink.clear_session_events(&id).map_err(|error| {
                    AcpError::Spawn(format!("import replay clear failed: {error}"))
                })
            })
            .await?;
        }

        let mode = req.acp_mode_id.take().or_else(|| {
            if req.yolo_mode {
                crate::acp::agent_profiles::resolve(&req.agent)
                    .yolo_mode_id
                    .map(str::to_owned)
            } else {
                None
            }
        });
        let launched = (self.launcher)(config.clone(), AcpSessionId(session_id.to_string())).await;
        let mut client = match launched {
            Ok(c) => c,
            Err(err) => {
                // A stop that landed during the launch owns the outcome.
                let cancelled = lock_recover(&self.lifecycle)
                    .cancel_requested(&lease)
                    .is_some();
                if cancelled {
                    drop(warmup_guard);
                    return Err(self.cancelled_spawn(&lease).await);
                }
                if matches!(
                    &err,
                    AcpError::RegistryBusy | AcpError::RetainedRuntimeUnavailable(_)
                ) {
                    lease
                        .retirement()
                        .finish_registration(super::ConstructionFinish::new(
                            WorkerKind::Runner {
                                spawn_config: Box::new(config),
                            },
                            context_reset,
                            mode,
                            false,
                        ));
                    reservation.preserve_refused_owner();
                    return Err(if matches!(err, AcpError::RegistryBusy) {
                        SupervisorError::RegistryBusy
                    } else {
                        SupervisorError::Acp(err)
                    });
                }
                if matches!(err, AcpError::IncompatibleAgent(_)) {
                    self.mark_incompatible_binary(session_id, &config.spec.command);
                }
                publish_rejection(&err, |event| {
                    self.publish_next(session_id, event);
                });
                self.reap_failed_launch(&lease).await;
                return Err(SupervisorError::Acp(err));
            }
        };

        let inbound = client
            .take_inbound()
            .expect("freshly spawned AcpClient always has inbound receiver");
        let client = Arc::new(client);
        lease.retirement().record_client(&client, false);
        if !lease.attest_runner().await {
            let identity = lease.spawned_runner();
            super::teardown::retire_client(&self.lifecycle, &lease, &client, identity).await;
            self.reap_failed_launch(&lease).await;
            return Err(SupervisorError::TeardownPending(session_id.to_owned()));
        }
        let identity = client.runner_pid().map(|pid| {
            lease
                .spawned_runner()
                .or_else(|| {
                    worker_registry::load(session_id)
                        .ok()
                        .flatten()
                        .filter(|record| record.pid == pid && record.generation == lease.epoch())
                        .map(|record| RunnerIdentity::from_record(&record))
                })
                .unwrap_or_else(|| RunnerIdentity::unattested(pid, lease.epoch()))
        });
        // A peer that archived or trashed the row during the handshake wins: retire the runner.
        match admit_durable_launch(req.launch_admission.as_ref(), &req.session_id).await {
            Ok(()) => {}
            Err(refused) => {
                super::teardown::retire_client(&self.lifecycle, &lease, &client, identity).await;
                drop(client);
                drop(warmup_guard);
                self.reap_failed_launch(&lease).await;
                return Err(refused);
            }
        }

        if warmup_guard.is_some() {
            lock_recover(&self.warmed_up_agents).insert(req.agent.clone());
        }
        drop(warmup_guard);
        info!(target: "acp.supervisor", session = %session_id, "structured view worker spawned");
        lock_recover(&self.incompatible_binaries).remove(session_id);

        let kind = WorkerKind::Runner {
            spawn_config: Box::new(config),
        };
        #[cfg(test)]
        let gate = { OWNED_INSTALL_GATE.lock().unwrap().take() };
        #[cfg(test)]
        if let Some((entered, release)) = gate {
            let _ = entered.send(client.clone());
            let _ = release.await;
        }
        self.install_worker(
            session_id,
            reservation,
            client,
            inbound,
            identity,
            super::ConstructionFinish::new(kind, context_reset, mode, false),
        )
        .await?;

        Ok(())
    }

    async fn cancelled_spawn(&self, lease: &Lease) -> SupervisorError {
        let session_id = lease.session_id();
        self.reap_failed_launch(lease).await;
        SupervisorError::SpawnCancelled(session_id.to_string())
    }

    /// Serializes an agent's spawns until its first one finishes.
    async fn warmup_guard(&self, agent: &str) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        if lock_recover(&self.warmed_up_agents).contains(agent) {
            return None;
        }
        let lock = lock_recover(&self.agent_warmup_locks)
            .entry(agent.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        Some(lock.lock_owned().await)
    }

    async fn spawn_config(
        &self,
        req: &SpawnRequest,
        generation: u64,
    ) -> Result<(SpawnConfig, Option<PendingContextReset>), SupervisorError> {
        let profile = req.source_profile.clone().unwrap_or_default();
        let cwd = req.cwd.clone();
        let (resolved_cfg, policy) = tokio::task::spawn_blocking(move || {
            (
                resolve_config_with_repo_or_warn(&profile, &cwd),
                AgentPolicy::load(),
            )
        })
        .await
        .map_err(|e| {
            SupervisorError::InvalidAgentCommand(format!("config load task failed: {e}"))
        })?;
        let (mut spec, spec_from_registry) = self
            .resolve_agent_spec(&req.agent, &resolved_cfg.session, &policy)
            .await?;
        if let Some(ovr) = &req.agent_command_override {
            apply_agent_command_override(&req.agent, spec_from_registry, ovr, &mut spec)?;
        }
        let wrapper_substitution = wrapper_substitution_for(
            &*self.registry.lock().await,
            &req.tool,
            &req.agent,
            spec_from_registry,
            &resolved_cfg.session.agent_detect_as,
        );
        if let Some((wrapper, base)) = &wrapper_substitution {
            log_wrapper_substitution(&req.session_id, &req.tool, wrapper, base);
        }
        if spec.command.contains("${aoe_data_dir}") {
            if let Ok(data_dir) = crate::session::get_app_dir() {
                spec.command = spec
                    .command
                    .replace("${aoe_data_dir}", &data_dir.to_string_lossy());
            }
        }

        let acp_defaults = resolved_cfg.acp.acp_defaults_for(&req.agent);
        let (model, effort) = crate::session::config::resolve_spawn_model_effort(
            acp_defaults,
            req.model.clone(),
            req.effort.clone(),
        );

        let (base_host_environment, mut host_environment) = if req.sandbox_info.is_none() {
            host_spawn_environment(
                &resolved_cfg,
                &req.session_id,
                &req.tool,
                req.source_profile.clone().unwrap_or_default(),
                req.cwd.clone(),
            )
            .await?
        } else {
            (Vec::new(), Vec::new())
        };

        let claude_store_pin = req.claude_store_pin.clone().filter(|_| {
            req.sandbox_info.is_none() && matches!(req.agent.as_str(), "claude" | "claude-code")
        });
        let claude_config_dir =
            apply_claude_store_pin(&mut host_environment, claude_store_pin.as_ref()).or_else(
                || {
                    if req.sandbox_info.is_some() {
                        return None;
                    }
                    let (target, native) = wrapper_substitution.as_ref().map_or(
                        (req.agent.as_str(), req.agent.as_str()),
                        |(wrapper, base)| (wrapper.as_str(), base.as_str()),
                    );
                    apply_declared_config_dir(
                        &mut host_environment,
                        &resolved_cfg,
                        &req.tool,
                        target,
                        native,
                    )
                },
            );

        let mut provider_env = req.provider_env.clone();
        if let Some(model) = model.clone() {
            provider_env.push(("AOE_AGENT_MODEL".into(), model));
        }
        // Every worker runs through `aoe __acp-runner` so it survives `aoe serve --stop`.
        let socket_path = worker_registry::socket_path_for(&req.session_id).map_err(|e| {
            SupervisorError::Acp(AcpError::Spawn(format!("worker socket path: {e:#}")))
        })?;
        let mcp_servers = resolve_mcp_servers(
            &req.agent,
            &req.session_id,
            req.source_profile.clone(),
            req.cwd.clone(),
            host_environment.clone(),
            claude_config_dir,
            "MCP resolution task failed",
        )
        .await;

        let (stored_acp_session_id, fork_from, seed_history_replay, context_reset, source_profile) =
            if req.sandbox_info.as_ref().is_some_and(|info| info.enabled) {
                let native_key = wrapper_substitution
                    .as_ref()
                    .map(|(_, base)| base.as_str())
                    .unwrap_or(&req.agent);
                let native_agent =
                    crate::acp::agent_profiles::resolve(native_key).native_config_agent;
                let profile = req.source_profile.clone().unwrap_or_default();
                let id = req.session_id.clone();
                let continuation = req.sandbox_continuation;
                let context = tokio::task::spawn_blocking(move || {
                    crate::migrations::v033_isolate_sandbox_content::prepare_acp_context(
                        &profile,
                        &id,
                        native_agent,
                        generation,
                        crate::migrations::v033_isolate_sandbox_content::AcpContextUse::Launch,
                        continuation,
                    )
                })
                .await
                .map_err(|error| {
                    SupervisorError::Acp(AcpError::Spawn(format!(
                        "sandbox context handoff task: {error}"
                    )))
                })?
                .map_err(|error| {
                    SupervisorError::Acp(AcpError::Spawn(format!(
                        "sandbox context handoff: {error}"
                    )))
                })?;
                let reset_profile = context.profile.clone();
                let reset = context
                    .notice
                    .map(|(reason, transactions)| PendingContextReset {
                        profile: reset_profile,
                        reason,
                        transactions,
                    });
                (
                    context.stored_session_id,
                    context.fork_from,
                    context.seed_history_replay,
                    reset,
                    Some(context.profile),
                )
            } else {
                (
                    req.stored_acp_session_id.clone(),
                    req.fork_from.clone(),
                    req.seed_history_replay,
                    None,
                    req.source_profile.clone(),
                )
            };

        Ok((
            SpawnConfig {
                launch_lease: None,
                agent_key: req.agent.clone(),
                tool: req.tool.clone(),
                spec,
                cwd: req.cwd.clone(),
                additional_dirs: req.additional_dirs.clone(),
                provider_env,
                host_environment,
                base_host_environment,
                default_effort: effort,
                default_effort_explicit: req.effort_explicit,
                default_mode: acp_defaults.and_then(|defaults| defaults.mode()),
                default_model: model,
                socket_path: Some(socket_path),
                stored_acp_session_id,
                fork_from,
                sandbox_info: req.sandbox_info.clone(),
                source_profile,
                mcp_servers,
                seed_history_replay,
                prior_conversation: req.prior_conversation,
                artifact_dir: crate::session::artifacts::session_artifact_dir(&req.session_id).ok(),
                wrapper_substitution,
                generation,
                claude_store_pin,
                provider_routing: req
                    .provider
                    .as_deref()
                    .and_then(crate::session::environment::provider_override_env)
                    .unwrap_or_default(),
            },
            context_reset,
        ))
    }

    /// Install a launched client under the reservation's lease, or retire it
    /// when a stop or a newer epoch won the race. Returns the installed client.
    async fn install_worker(
        &self,
        session_id: &str,
        mut reservation: ResumeReservation,
        client: Arc<AcpClient>,
        inbound: mpsc::Receiver<crate::acp::event_sender::SdkEvent>,
        identity: Option<RunnerIdentity>,
        finish: super::ConstructionFinish,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        if !reservation.lease().is_borrowed_attach() {
            reservation.registry_deadline =
                tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        }
        reservation.deferred = Some(super::DeferredConstruction::Installation(Box::new(
            super::ConstructionInstallation {
                client,
                inbound,
                identity,
                finish,
                constructor: reservation.constructor.as_ref().map(|work| work.fork()),
            },
        )));
        self.finish_construction(session_id, reservation).await
    }

    async fn finish_construction(
        &self,
        session_id: &str,
        mut reservation: ResumeReservation,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        let constructor;
        use super::DeferredConstruction as Pending;
        let lease = reservation.lease().clone();
        let deadline = reservation.registry_deadline;
        if matches!(reservation.deferred, Some(Pending::PostInstall(..))) {
            return self.finish_post_install(reservation).await;
        }
        super::teardown::quiesce_retained_drain(&lease).await;
        let identity = match reservation.deferred.as_ref().expect("installation custody") {
            Pending::Installation(installation) => installation.identity,
            _ => unreachable!("connection phase completed before installation"),
        };
        let mut workers = loop {
            let workers = tokio::time::timeout_at(deadline, self.workers.lock())
                .await
                .map_err(|_| SupervisorError::RegistryBusy)?;
            if tokio::time::Instant::now() >= deadline {
                return Err(SupervisorError::RegistryBusy);
            }
            let outcome = {
                let mut lifecycle = lock_recover(&self.lifecycle);
                if lifecycle.current_lease(session_id).as_ref() != Some(&lease) {
                    Some(Err(InstallError::Stale))
                } else {
                    match worker_registry::try_registry_lock(session_id) {
                        Ok(Some(registry)) => {
                            match claimed_stop_for_lease(&registry, &lease, identity) {
                                Ok(Some(_)) => {
                                    lifecycle.begin_stop(session_id, "user_stopped", false);
                                }
                                Ok(None) => {}
                                Err(_) => {
                                    return Err(AcpError::RetainedRuntimeUnavailable(Box::new(
                                        worker_registry::ControlAdmissionRefusal::unconfirmed(
                                            identity,
                                        ),
                                    ))
                                    .into())
                                }
                            }
                            Some(lifecycle.install(&lease, identity))
                        }
                        Ok(None) => None,
                        Err(_) => {
                            return Err(AcpError::RetainedRuntimeUnavailable(Box::new(
                                worker_registry::ControlAdmissionRefusal::unconfirmed(identity),
                            ))
                            .into())
                        }
                    }
                }
            };
            match outcome {
                Some(Ok(())) => break workers,
                Some(Err(refusal)) => {
                    drop(workers);
                    reservation.yield_to_retirement();
                    self.retire_deferred_clients(&lease).await;
                    return Err(self.retire_refused_install(&lease, refusal).await);
                }
                None => {
                    drop(workers);
                    drop(
                        worker_registry::registry_lock_until(session_id, deadline)
                            .await
                            .map_err(registry_error)?,
                    );
                }
            }
        };
        let pending = reservation.deferred.take();
        let pending = {
            let mut pending = pending;
            constructor = pending.as_mut().and_then(Pending::take_constructor);
            pending
        };
        let Some(Pending::Installation(installation)) = pending else {
            unreachable!("one installed construction")
        };
        let super::ConstructionInstallation {
            client,
            inbound,
            finish,
            ..
        } = *installation;
        let super::ConstructionFinish {
            kind,
            context_reset,
            mode,
            respawn,
        } = finish;
        // Freeze the installing actor before publishing its handle to concurrent Fresh.
        let mode_projection = mode
            .as_ref()
            .and_then(|_| client.establishment_projection());
        let mode_waits_for_native = mode_projection.is_some();
        let mode_attempt = mode_projection.and_then(|projection| projection.attempt());
        self.cancel_orphaned_requests(session_id);
        let recovery: anyhow::Result<_> = if matches!(kind, WorkerKind::Attached) {
            (|| {
                Ok((
                    collect_resumable_background_agent_launches(
                        &*self.sink,
                        &self.next_seqs,
                        session_id,
                    )?,
                    self.sink.unresolved_background_task_launches(session_id)?,
                ))
            })()
        } else {
            self.detach_orphaned_background_agents(session_id);
            Ok((Vec::new(), Vec::new()))
        };
        if context_reset.is_some() {
            lock_recover(&self.pending_context_resets).insert(session_id.to_string());
        }
        let restart_history = if let Some(mut old) = workers.remove(session_id) {
            assert_eq!(
                old.lease, lease,
                "installation never replaces an unrelated owner"
            );
            let history = std::mem::take(&mut old.restart_history);
            lease.retirement().record_client(&old.client, false);
            old.drain_task.abort();
            history
        } else {
            Vec::new()
        };
        let drain_task = self.start_drain_task(
            session_id.to_string(),
            lease.clone(),
            inbound,
            context_reset,
        );
        workers.insert(
            session_id.to_string(),
            WorkerHandle {
                client: client.clone(),
                native_session_id: None,
                drain_task,
                restart_history,
                kind,
                launch_epoch: lease.epoch(),
                lease: lease.clone(),
            },
        );
        drop(workers);
        if respawn {
            lock_recover(&self.respawned_in_place).insert(session_id.to_owned());
        }
        self.worker_notify.notify_waiters();
        let recovery = match recovery {
            Ok(recovery) => Some(recovery),
            Err(error) => {
                warn!(target: "acp.resource", session = session_id, %error,
                    "resource tracking could not be restored");
                self.publish_next(
                    session_id,
                    Event::SessionNotice {
                        key: Some("resource-recovery".into()),
                        severity: "warning".into(),
                        title: "Background tracking could not be restored".into(),
                        description: Some(
                            "Retained history is preserved; no resource reader was started.".into(),
                        ),
                    },
                );
                None
            }
        };
        if mode.is_none()
            && recovery
                .as_ref()
                .is_none_or(|(agents, tasks)| agents.is_empty() && tasks.is_empty())
        {
            return Ok(client);
        }
        let label = session_id.to_owned();
        reservation.deferred = Some(Pending::PostInstall(
            Box::pin(async move {
                if let Some((resumable, open_tasks)) = recovery {
                    if !resumable.is_empty() {
                        if let Err(error) = client.resume_background_tailing(resumable).await {
                            warn!(target: "acp.resource", session = %label, %error, "agent tracking restore command failed");
                        }
                    }
                    if !open_tasks.is_empty() {
                        if let Err(error) = client.resume_task_tailing(open_tasks).await {
                            warn!(target: "acp.resource", session = %label, %error, "task tracking restore command failed");
                        }
                    }
                }
                if let Some(mode_id) = mode.as_deref() {
                    let restored = async {
                        if mode_waits_for_native {
                            client.wait_until_usable().await?;
                        }
                        client
                            .set_mode_for_establishment(mode_id, mode_attempt.as_ref(), deadline)
                            .await
                    }
                    .await;
                    if let Err(error) = restored {
                        warn!(target: "acp.supervisor", session = %label,
                            "set_mode({mode_id}) after spawn failed: {error}");
                    }
                }
                client
            }),
            constructor,
        ));
        self.finish_post_install(reservation).await
    }

    async fn finish_post_install(
        &self,
        mut reservation: ResumeReservation,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        let cancellation = reservation.lease.retirement().deferred_cancellation();
        let id = reservation.lease.session_id();
        let deadline = reservation.registry_deadline;
        if tokio::time::Instant::now() >= deadline {
            return Err(SupervisorError::RegistryBusy);
        }
        let Some(super::DeferredConstruction::PostInstall(work, ..)) =
            reservation.deferred.as_mut()
        else {
            unreachable!("owned post-install obligation")
        };
        let client = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SupervisorError::SpawnCancelled(id.to_owned())),
            result = tokio::time::timeout_at(deadline, work) =>
                result.map_err(|_| SupervisorError::RegistryBusy)?,
        };
        reservation.deferred.take();
        Ok(client)
    }

    /// Continue an already admitted actor, never a second launch or capacity charge.
    pub(crate) async fn resume_deferred(&self, session_id: &str) -> Result<bool, SupervisorError> {
        let mut _original_constructor = None;
        use super::DeferredConstruction as Pending;
        let captured = {
            let lifecycle = lock_recover(&self.lifecycle);
            let Some(lease) = lifecycle.current_lease(session_id) else {
                return Ok(false);
            };
            if lease.retirement().deferred_is_claimed() {
                return Ok(true);
            }
            if !lifecycle.construction_is_pending(&lease) && lifecycle.running(session_id).is_none()
            {
                return Ok(false);
            }
            // Retain actual original work before its deferred payload leaves storage.
            {
                _original_constructor = lease.retain_original_constructor();
            }
            lease
                .retirement()
                .take_deferred()
                .map(|pending| (lease, pending))
        };
        let Some((lease, pending)) = captured else {
            return Ok(false);
        };
        let mut reservation = ResumeReservation {
            expected_lifecycle_generation: lease.admission().map(|admission| admission.generation),
            establishment: lease
                .admission()
                .and_then(|admission| admission.establishment),
            lease: lease.clone(),
            attach_record: None,
            lifecycle: self.lifecycle.clone(),
            notify: self.worker_notify.clone(),
            preserve_owner: false,
            registry_deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(3),
            resume_stop: None,
            resume_authority: worker_registry::ResumeAuthority::Automatic,
            deferred: Some(pending),
            deferred_claim: true,
            deferred_gate: None,
            constructor: _original_constructor.as_ref().map(|work| work.fork()),
        };
        let cancellation = lease.retirement().deferred_cancellation();
        reservation.deferred_gate = Some(tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SupervisorError::SpawnCancelled(session_id.to_owned())),
            gate = lease.retirement().claim_deferred_gate() => gate,
        });
        let admission = lease.admission();
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SupervisorError::SpawnCancelled(session_id.to_owned())),
            result = admit_durable_launch(admission.as_ref(), session_id) => result?,
        }
        if let Some(Pending::Registration { connection, .. }) = reservation.deferred.as_mut() {
            let attested = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(SupervisorError::SpawnCancelled(session_id.to_owned())),
                attested = lease.attest_runner() => attested,
            };
            if !attested {
                return Err(SupervisorError::TeardownPending(session_id.to_owned()));
            }
            connection.attest_owner(
                lease
                    .spawned_runner()
                    .expect("owned connection's physical runner"),
            )?;
            let outcome = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(AcpError::Spawn("runner construction cancelled".into())),
                result = connection.advance(Some(reservation.registry_deadline)) => result,
            };
            if let Err(error) = outcome {
                if matches!(
                    &error,
                    AcpError::RegistryBusy | AcpError::RetainedRuntimeUnavailable(_)
                ) {
                    return Err(if matches!(error, AcpError::RegistryBusy) {
                        SupervisorError::RegistryBusy
                    } else {
                        error.into()
                    });
                }
                reservation.yield_to_retirement();
                self.reap_failed_launch(&lease).await;
                return Err(error.into());
            }
            let constructor;
            let pending = reservation.deferred.take();
            let pending = {
                let mut pending = pending;
                constructor = pending.as_mut().and_then(Pending::take_constructor);
                pending
            };
            let Some(Pending::Registration {
                mut connection,
                finish: Some(finish),
                ..
            }) = pending
            else {
                unreachable!("fully described original connection")
            };
            let mut client = connection.take_client();
            let inbound = client.take_inbound().expect("original SDK event receiver");
            let client = Arc::new(client);
            lease.retirement().record_client(&client, false);
            reservation.deferred = Some(Pending::Installation(Box::new(
                super::ConstructionInstallation {
                    client,
                    inbound,
                    identity: lease.spawned_runner(),
                    finish: *finish,
                    constructor,
                },
            )));
        }
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(SupervisorError::SpawnCancelled(session_id.to_owned())),
            result = admit_durable_launch(admission.as_ref(), session_id) => result?,
        }
        self.finish_construction(session_id, reservation).await?;
        Ok(true)
    }

    /// Retire a runner a failed spawn left behind under this lease's generation
    /// and release the scope it created, while the lease still holds the
    /// session. A first launch has no previous runner to keep. Under a pending
    /// stop the scope is released even when no runner of this generation exists.
    async fn reap_failed_launch(&self, lease: &Lease) {
        if lease.is_borrowed_attach() {
            return;
        }
        let session_id = lease.session_id();
        let identity = lease.spawned_runner().or_else(|| {
            if lock_recover(&self.lifecycle).pending_attach(lease) {
                // An explicit stop owns only the runner captured at admission.
                return None;
            }
            worker_registry::load(session_id)
                .ok()
                .flatten()
                .filter(|record| record.generation == lease.epoch())
                .map(|record| RunnerIdentity::from_record(&record))
        });
        let identity = match identity {
            Some(identity) => identity,
            None if lease.stop_completion_pending() => {
                // abandon parks cancellation-before-birth instead of deleting its debt.
                let abandoned = lock_recover(&self.lifecycle).abandon(lease);
                if abandoned {
                    self.settle_sweeping_if_owed(
                        lease,
                        crate::acp::runner_lifecycle::Settlement::Proven,
                    )
                    .await;
                    return;
                }

                // The producer can acquire the birth lock, record its runner,
                // and release it while abandon waits on that same lock. In
                // that interleaving cancel_before_birth returns false, so the
                // first identity snapshot was stale. Re-read ownership before
                // deciding that there is nothing left to reap.
                let Some(identity) = lease.spawned_runner() else {
                    self.worker_notify.notify_waiters();
                    return;
                };
                identity
            }
            None => return,
        };
        if lease.spawned_runner().is_none() {
            lease.record_runner(identity);
        }
        let stopping = {
            let mut lifecycle = lock_recover(&self.lifecycle);
            lifecycle.convert_to_stopping(lease)
                || (lifecycle.current_lease(session_id).as_ref() == Some(lease)
                    && lifecycle.phase(session_id)
                        == crate::acp::runner_lifecycle::WorkerPhase::Stopping)
        };
        if !stopping {
            // A newer lifecycle epoch may have displaced this construction
            // while its cancellable launcher was still returning. The stale
            // Lease still owns the runner it built, so retire that runner
            // without touching the newer epoch's scope or lifecycle entry.
            super::teardown::retire_registered_clients(&self.lifecycle, lease).await;
            let _ = super::teardown::tear_down_lease(&*self.process_control, lease, false).await;
            return;
        }
        // Scope debt must exist before any cancellable physical teardown.
        lock_recover(&self.lifecycle).owe_sweep(lease);
        self.retire_deferred_clients(lease).await;
        let settlement =
            super::teardown::tear_down_lease(&*self.process_control, lease, false).await;
        self.settle_sweeping_if_owed(lease, settlement).await;
    }

    /// Tear down a built worker whose install the lifecycle table refused.
    async fn retire_refused_install(
        &self,
        lease: &Lease,
        refusal: InstallError,
    ) -> SupervisorError {
        let session_id = lease.session_id();
        if lease.is_borrowed_attach() {
            return SupervisorError::SpawnCancelled(session_id.to_string());
        }
        match refusal {
            InstallError::Cancelled { reason } => {
                debug!(target: "acp.supervisor", session = %session_id, %reason,
                        "resume cancelled by a concurrent shutdown; retiring matching construction");
            }
            InstallError::Stale => {
                warn!(target: "acp.supervisor", session = %session_id,
                        "resume completed under a stale lease; retiring its frozen runner only");
                lock_recover(&self.lifecycle).convert_to_stopping(lease);
                lock_recover(&self.lifecycle).owe_sweep(lease);
            }
        }
        let settlement =
            super::teardown::tear_down_lease(&*self.process_control, lease, false).await;
        self.settle_sweeping_if_owed(lease, settlement).await;
        SupervisorError::SpawnCancelled(session_id.to_string())
    }

    /// Reattach to a runner left by a previous daemon by dialing its socket.
    pub async fn attach(
        &self,
        session_id: String,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        in_flight_turn: bool,
        sandbox: Option<SandboxInfo>,
    ) -> Result<(), SupervisorError> {
        match self
            .begin_resume(
                &session_id,
                ResumeKind::Attach,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await?
        {
            Some(r) => {
                self.attach_inner(session_id, cwd, additional_dirs, in_flight_turn, sandbox, r)
                    .await
            }
            None => Err(SupervisorError::AlreadyRunning(session_id)),
        }
    }

    /// Attach body, run under a reservation from `begin_resume`.
    pub(crate) async fn attach_inner(
        &self,
        session_id: String,
        cwd: PathBuf,
        additional_dirs: Vec<PathBuf>,
        in_flight_turn: bool,
        sandbox: Option<SandboxInfo>,
        mut reservation: ResumeReservation,
    ) -> Result<(), SupervisorError> {
        let _original_constructor = reservation.constructor.as_ref().map(|work| work.fork());
        let record = match reservation.attach_record.take() {
            Some(record) => record,
            _ => return Err(SupervisorError::UnknownSession(session_id)),
        };
        revalidate_attach_record(&record, reservation.registry_deadline).await?;
        let (admission, claude_home) = capture_durable_launch(
            record.source_profile.as_deref(),
            &session_id,
            reservation.expected_lifecycle_generation,
            true,
        )
        .await?;
        admit_durable_launch(admission.as_ref(), &session_id).await?;
        reservation.lease().set_admission(admission.clone());
        let identity = RunnerIdentity::from_record(&record);
        let native_store = capture_attach_store(
            admission.as_ref(),
            self.sink.establishment_store(),
            &session_id,
            identity,
        )
        .await?;

        let agent_key = if record.agent_key.is_empty() {
            record.agent_name.clone()
        } else {
            record.agent_key.clone()
        };
        let key = agent_key.clone();
        let agent_allowed = tokio::task::spawn_blocking(move || AgentPolicy::load().allows(&key))
            .await
            .map_err(|e| {
                SupervisorError::InvalidAgentCommand(format!("agent policy load task failed: {e}"))
            })?;
        revalidate_attach_record(&record, reservation.registry_deadline).await?;
        if !agent_allowed {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                agent = %agent_key,
                "detached structured view worker runs an agent that [acp] allowed_agents no longer \
                 permits; terminating it instead of reattaching"
            );
            reservation.lease().claim_runner_ownership();
            self.reap_failed_launch(reservation.lease()).await;
            return Err(SupervisorError::AgentNotAllowed(agent_key));
        }

        // A cached registry ID is routing metadata, not an establishment receipt.
        // The runner supplies its genuine current session (or native loss).
        let stored_acp_session_id = match admission
            .as_ref()
            .and_then(|admission| admission.establishment.as_deref())
        {
            Some(attempt) => match &attempt.target {
                crate::acp::establishment::EstablishmentTarget::New
                | crate::acp::establishment::EstablishmentTarget::Fork { .. } => String::new(),
                crate::acp::establishment::EstablishmentTarget::Load { session_id, .. } => {
                    session_id.clone()
                }
            },
            None => record.stored_acp_session_id.clone().unwrap_or_default(),
        };
        let sandbox_resources = match sandbox.as_ref() {
            Some(info) => {
                let info = info.clone();
                let cwd = cwd.clone();
                let profile = record.source_profile.clone();
                Some(
                    tokio::task::spawn_blocking(move || {
                        crate::acp::acp_client::SessionSandbox::from_info(
                            &info,
                            cwd.as_path(),
                            profile,
                        )
                    })
                    .await
                    .map_err(|e| {
                        AcpError::Spawn(format!("sandbox resolve task panicked: {e}"))
                    })??,
                )
            }
            None => None,
        };
        let context_reset = if sandbox.as_ref().is_some_and(|info| info.enabled) {
            let profile = record.source_profile.clone().unwrap_or_default();
            let id = session_id.clone();
            let native_agent = if record.prime_runtime.is_some() {
                Some("prime-agent")
            } else {
                crate::acp::agent_profiles::resolve(&agent_key).native_config_agent
            };
            let generation = reservation.lease().epoch();
            let context = tokio::task::spawn_blocking(move || {
                crate::migrations::v033_isolate_sandbox_content::prepare_acp_context(
                    &profile,
                    &id,
                    native_agent,
                    generation,
                    crate::migrations::v033_isolate_sandbox_content::AcpContextUse::Attach,
                    super::SandboxContinuation::Persisted,
                )
            })
            .await
            .map_err(|error| {
                SupervisorError::Acp(AcpError::Spawn(format!(
                    "sandbox context handoff task: {error}"
                )))
            })?
            .map_err(|error| {
                SupervisorError::Acp(AcpError::Spawn(format!("sandbox context handoff: {error}")))
            })?;
            context
                .notice
                .map(|(reason, transactions)| PendingContextReset {
                    profile: context.profile,
                    reason,
                    transactions,
                })
        } else {
            None
        };
        revalidate_attach_record(&record, reservation.registry_deadline).await?;
        self.consume_resume_stop(&mut reservation).await?;

        let sink = Arc::clone(&self.sink);
        let outcome_id = session_id.clone();
        let unsuccessful_main =
            tokio::task::spawn_blocking(move || sink.unsuccessful_main(&outcome_id))
                .await
                .map_err(|_| SupervisorError::NativeStoreUnavailable)?
                .map_err(|_| SupervisorError::NativeStoreUnavailable)?;
        let unsuccessful_main = matches!(
            unsuccessful_main,
            crate::acp::event_store::ControlKnowledge::Known(true)
        );
        let client = await_resume(
            reservation.cancellation(),
            AcpClient::attach(
                record.socket_path.clone(),
                cwd,
                additional_dirs,
                stored_acp_session_id,
                in_flight_turn,
                unsuccessful_main,
                AcpSessionId(session_id.clone()),
                sandbox_resources,
                agent_key,
                record.source_profile.clone(),
                Some(reservation.registry_deadline),
                Some(reservation.lease().clone()),
                claude_home,
                native_store,
            ),
        )
        .await;
        let mut client = match client {
            Ok(Ok(client)) => client,
            Ok(Err(AcpError::RegistryBusy)) => return Err(SupervisorError::RegistryBusy),
            Ok(Err(error)) => {
                self.reap_failed_launch(reservation.lease()).await;
                return Err(error.into());
            }
            Err(()) => return Err(self.cancelled_spawn(reservation.lease()).await),
        };

        let inbound = client
            .take_inbound()
            .expect("freshly attached AcpClient always has inbound receiver");
        let client = Arc::new(client);
        reservation
            .lease()
            .retirement()
            .record_client(&client, false);
        if let Err(refused) = admit_durable_launch(admission.as_ref(), &session_id).await {
            let lease = reservation.lease().clone();
            super::teardown::retire_client(&self.lifecycle, &lease, &client, Some(identity)).await;
            drop(client);
            self.reap_failed_launch(&lease).await;
            return Err(refused);
        }
        #[cfg(test)]
        let gate = { ATTACH_INSTALL_GATE.lock().unwrap().take() };
        #[cfg(test)]
        if let Some((entered, release)) = gate {
            let _ = entered.send(());
            let _ = release.await;
        }
        self.install_worker(
            &session_id,
            reservation,
            client,
            inbound,
            Some(identity),
            super::ConstructionFinish::new(WorkerKind::Attached, context_reset, None, false),
        )
        .await?;
        info!(
            target: "acp.supervisor",
            session = %session_id,
            socket = %record.socket_path.display(),
            pid = record.pid,
            "reattached to existing structured view worker"
        );
        Ok(())
    }
}

pub(super) async fn apply_mode(
    client: &AcpClient,
    session_id: &str,
    mode_id: Option<&str>,
    after: &str,
) {
    let Some(mode_id) = mode_id else {
        return;
    };
    if let Err(e) = client.set_mode(mode_id, None).await {
        warn!(
            target: "acp.supervisor",
            session = %session_id,
            "set_mode({mode_id}) after {after} failed: {e}"
        );
    }
}

/// Publish what a launch rejection means for the session; false for any other error.
pub(super) fn publish_rejection(err: &AcpError, mut publish: impl FnMut(Event)) -> bool {
    match err {
        AcpError::IncompatibleAgent(payload) => {
            publish(Event::IncompatibleAgent {
                detail: payload.detail.clone(),
            });
            publish(Event::AgentStartupError {
                message: payload.message.clone(),
                native_subject: None,
            });
        }
        // The provider limit parks the session rather than reporting a crash.
        AcpError::RateLimited(info) => {
            publish(Event::RateLimit {
                info: (**info).clone(),
            });
            publish(Event::Stopped {
                reason: "rate_limited".into(),
            });
        }
        _ => return false,
    }
    true
}

async fn await_resume<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, ()> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(()),
        result = future => Ok(result),
    }
}

/// Cancellation stops launch admission, but a started blocking commit must finish
/// before its caller can release the old generation's ownership.
pub(super) async fn await_resume_to_completion<T>(
    cancellation: &CancellationToken,
    future: impl Future<Output = T>,
) -> Result<T, ()> {
    if cancellation.is_cancelled() {
        return Err(());
    }
    tokio::pin!(future);
    tokio::select! {
        biased;
        result = &mut future => Ok(result),
        _ = cancellation.cancelled() => {
            let _ = future.await;
            Err(())
        }
    }
}

async fn capture_durable_launch(
    profile: Option<&str>,
    session_id: &str,
    expected: Option<u64>,
    capture_store: bool,
) -> Result<(Option<super::LaunchAdmission>, Option<PathBuf>), SupervisorError> {
    let Some(profile) = profile.map(str::to_owned) else {
        return Ok((None, None));
    };
    let id = session_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let storage = crate::session::Storage::open_unwatched(&profile)
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?;
        let _lock = storage
            .acquire_instance_lifecycle_lock(&id)
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?;
        let row = storage
            .load_instances_complete()
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or_else(|| SupervisorError::SessionGone(id.clone()))?;
        let claude_home = if capture_store {
            row.selected_claude_conversation()
                .and_then(|(_, binding)| binding.stores.first())
                .cloned()
        } else {
            None
        };
        let establishment = if capture_store {
            match row.acp_establishment.as_ref() {
                Some(crate::acp::establishment::AcpEstablishment::Pending { attempt })
                    if attempt.producer.is_some() =>
                {
                    Some(std::sync::Arc::new(attempt.as_ref().clone()))
                }
                _ => None,
            }
        } else {
            None
        };
        let native_scope = crate::acp::state::NativeObservationScope::capture(&row, &storage)
            .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?;
        Ok((
            Some(super::LaunchAdmission {
                establishment,
                store: std::sync::Arc::new(storage),
                generation: expected.unwrap_or(row.lifecycle_generation),
                namespace: None,
                native_scope: Some(native_scope),
            }),
            claude_home,
        ))
    })
    .await
    .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?
}

fn admission_error(error: anyhow::Error, session_id: &str) -> SupervisorError {
    if let Some(blocked) = error.downcast_ref::<crate::session::StartBlocked>() {
        SupervisorError::Blocked(*blocked)
    } else if error.is::<crate::session::SessionGone>() {
        SupervisorError::SessionGone(session_id.to_owned())
    } else if error.is::<crate::session::LifecycleReservationError>() {
        SupervisorError::SpawnCancelled(session_id.to_owned())
    } else if let Some(refusal) =
        error.downcast_ref::<crate::acp::establishment::EstablishmentRefusal>()
    {
        SupervisorError::Establishment(*refusal)
    } else if error.is::<crate::session::NativeStoreUnavailable>() {
        SupervisorError::NativeStoreUnavailable
    } else {
        SupervisorError::Acp(AcpError::Spawn(format!("launch admission: {error:#}")))
    }
}

/// Admit a launch with no captured admission, such as a crash respawn, against the row as stored now.
pub(super) async fn admit_stored_row(
    profile: Option<&str>,
    session_id: &str,
) -> Result<(), SupervisorError> {
    let (admission, _) = capture_durable_launch(profile, session_id, None, false).await?;
    admit_durable_launch(admission.as_ref(), session_id).await
}

async fn capture_admission_scope(
    admission: &mut super::LaunchAdmission,
    session_id: &str,
    namespace_held: Option<&tokio::sync::OwnedRwLockReadGuard<()>>,
) -> Result<(), SupervisorError> {
    if admission.native_scope.is_some() {
        return Ok(());
    }
    let store = admission.store.clone();
    let generation = admission.generation;
    let id = session_id.to_owned();
    let namespace = match (namespace_held, admission.namespace.as_ref()) {
        (None, Some(namespace)) => Some(namespace.clone().read_owned().await),
        _ => None,
    };
    let scope = tokio::task::spawn_blocking(move || {
        let _namespace = namespace;
        let _lifecycle = store
            .storage()
            .acquire_instance_lifecycle_lock(&id)
            .map_err(|error| admission_error(error, &id))?;
        let row = store
            .storage()
            .load_instances_complete()
            .map_err(|error| admission_error(error, &id))?
            .into_iter()
            .find(|row| row.id == id)
            .ok_or_else(|| SupervisorError::SessionGone(id.clone()))?;
        if row.lifecycle_generation != generation {
            return Err(SupervisorError::SpawnCancelled(id));
        }
        crate::acp::state::NativeObservationScope::capture(&row, store.storage())
            .map_err(|error| admission_error(error, &id))
    })
    .await
    .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))??;
    admission.native_scope = Some(scope);
    Ok(())
}

async fn prepare_establishment(
    admission: &mut super::LaunchAdmission,
    session_id: &str,
    producer: Option<uuid::Uuid>,
    namespace_held: Option<&tokio::sync::OwnedRwLockReadGuard<()>>,
) -> Result<crate::session::Instance, SupervisorError> {
    let store = admission.store.clone();
    let generation = admission.generation;
    let expected = admission.establishment.clone();
    let original_scope = admission.native_scope.clone();
    let id = session_id.to_owned();
    let namespace = match (namespace_held, admission.namespace.as_ref()) {
        (None, Some(namespace)) => Some(namespace.clone().read_owned().await),
        _ => None,
    };
    let (attempt, scope, row) = tokio::task::spawn_blocking(move || {
        let _namespace = namespace;
        let _lifecycle = store
            .storage()
            .acquire_instance_lifecycle_lock(&id)
            .map_err(|error| admission_error(error, &id))?;
        let mut prepared = None;
        let mut mutation = |rows: &mut Vec<crate::session::Instance>,
                            _: &mut Vec<crate::session::Group>|
         -> anyhow::Result<()> {
            let row = rows
                .iter_mut()
                .find(|row| row.id == id)
                .ok_or(crate::session::SessionGone)?;
            anyhow::ensure!(
                row.lifecycle_generation == generation,
                crate::session::LifecycleReservationError::Superseded
            );
            if let Some(scope) = &original_scope {
                anyhow::ensure!(
                    scope.context
                        == crate::acp::state::NativeEventContext::capture(row, store.storage())?
                        && scope
                            .choice
                            .as_ref()
                            .is_some_and(|choice| choice.matches(row)),
                    crate::acp::establishment::EstablishmentRefusal::Superseded
                );
            }
            let attempt = match &expected {
                Some(attempt) => {
                    anyhow::ensure!(
                        attempt.matches_pending(row, store.storage())?,
                        crate::acp::establishment::EstablishmentRefusal::Superseded
                    );
                    attempt.as_ref().clone()
                }
                None => row.arm_acp_establishment(store.storage(), None)?,
            };
            let attempt = match producer {
                Some(producer) => row.bind_acp_producer(store.storage(), &attempt, producer)?,
                None => attempt,
            };
            prepared = Some((attempt, row.clone()));
            Ok(())
        };
        store
            .commit(&mut mutation)
            .map_err(|error| admission_error(error, &id))?;
        let (attempt, row) = prepared.ok_or_else(|| {
            admission_error(
                anyhow::anyhow!("establishment mutation was not applied"),
                &id,
            )
        })?;
        let scope = crate::acp::state::NativeObservationScope::capture(&row, store.storage())
            .map_err(|error| admission_error(error, &id))?;
        Ok::<_, SupervisorError>((std::sync::Arc::new(attempt), scope, row))
    })
    .await
    .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))??;
    admission.establishment = Some(attempt);
    admission.native_scope = Some(scope);
    Ok(row)
}

/// Capture before connecting: the incoming Resume receipt cannot choose its own store.
async fn capture_attach_store(
    admission: Option<&super::LaunchAdmission>,
    journal: Option<Arc<crate::acp::event_store::EventStore>>,
    session_id: &str,
    owner: RunnerIdentity,
) -> Result<Option<crate::session::ExecutionBinding>, SupervisorError> {
    use crate::acp::establishment::EstablishmentRefusal;
    let admission = admission
        .cloned()
        .ok_or(EstablishmentRefusal::Unconfirmed)?;
    let journal = journal.ok_or(EstablishmentRefusal::Unconfirmed)?;
    let captured = admission.clone();
    let id = session_id.to_owned();
    with_durable_launch(Some(admission), session_id.to_owned(), move || {
        let validate = || -> anyhow::Result<Option<crate::session::ExecutionBinding>> {
            let row = captured
                .store
                .load()?
                .into_iter()
                .find(|row| row.id == id)
                .ok_or(EstablishmentRefusal::Superseded)?;
            let scope = captured
                .native_scope
                .as_ref()
                .ok_or(EstablishmentRefusal::Unconfirmed)?;
            anyhow::ensure!(
                row.lifecycle_generation == captured.generation
                    && crate::acp::state::NativeEventContext::capture(
                        &row,
                        captured.store.storage(),
                    )? == scope.context
                    && scope
                        .choice
                        .as_ref()
                        .is_some_and(|choice| choice.matches(&row)),
                EstablishmentRefusal::Superseded
            );
            journal.certified_attach_store(&row, captured.store.storage(), &owner)
        };
        validate()
            .map_err(|error| AcpError::Spawn(format!("attach store certification: {error:#}")))
    })
    .await
}

/// Check the same authority and generation before hooks, after hooks and after handshake.
async fn admit_durable_launch(
    admission: Option<&super::LaunchAdmission>,
    session_id: &str,
) -> Result<(), SupervisorError> {
    with_durable_launch(admission.cloned(), session_id.to_owned(), || Ok(())).await
}

/// The blocking producer owns the native birth check, child spawn and publication.
pub(crate) async fn with_durable_launch<T: Send + 'static>(
    admission: Option<super::LaunchAdmission>,
    session_id: String,
    operation: impl FnOnce() -> Result<T, AcpError> + Send + 'static,
) -> Result<T, SupervisorError> {
    let Some(mut admission) = admission else {
        return operation().map_err(SupervisorError::Acp);
    };
    let namespace = match admission.namespace.take() {
        Some(namespace) => Some(namespace.read_owned().await),
        None => None,
    };
    tokio::task::spawn_blocking(move || {
        let _namespace = namespace;
        let _lock = admission
            .store
            .storage()
            .acquire_instance_lifecycle_lock(&session_id)
            .map_err(|error| admission_error(error, &session_id))?;
        let stored = admission
            .store
            .load()
            .map_err(|error| admission_error(error, &session_id))?
            .into_iter()
            .find(|row| row.id == session_id);
        match stored {
            None => Err(SupervisorError::SessionGone(session_id)),
            Some(row) => {
                row.ensure_startable().map_err(SupervisorError::Blocked)?;
                use crate::acp::establishment::{AcpEstablishment, EstablishmentRefusal};
                match (&row.acp_establishment, &admission.establishment) {
                    (Some(AcpEstablishment::Pending { attempt }), Some(expected))
                        if attempt.as_ref() == expected.as_ref() => {}
                    (Some(AcpEstablishment::Applied { attempt_id }), Some(expected))
                        if *attempt_id == expected.id => {}
                    (Some(AcpEstablishment::Applied { .. }), None) => {}
                    (None, None) => {
                        row.selected_acp_target()?;
                    }
                    (
                        Some(AcpEstablishment::Pending { .. } | AcpEstablishment::Blocked { .. }),
                        _,
                    ) => {
                        return Err(EstablishmentRefusal::Unresolved.into());
                    }
                    _ => return Err(EstablishmentRefusal::Superseded.into()),
                }
                if let Some(scope) = admission.native_scope.as_ref() {
                    if crate::acp::state::NativeEventContext::capture(
                        &row,
                        admission.store.storage(),
                    )
                    .map_err(|error| admission_error(error, &session_id))?
                        != scope.context
                    {
                        return Err(SupervisorError::SpawnCancelled(session_id));
                    }
                }
                if row.lifecycle_generation != admission.generation
                    || row.status == crate::session::Status::Stopped
                    || row.is_idle_dormant()
                {
                    return Err(SupervisorError::SpawnCancelled(session_id));
                }
                if row.terminal_handoff.is_some()
                    || !row.is_structured()
                    || !row.launch_is_finalized()
                {
                    return Err(SupervisorError::Acp(AcpError::Spawn(
                        "structured session launch is not finalized".into(),
                    )));
                }
                operation().map_err(SupervisorError::Acp)
            }
        }
    })
    .await
    .map_err(|error| SupervisorError::Acp(AcpError::Spawn(error.to_string())))?
}

/// Unsandboxed host environment as `(base, host)`: trusted global/profile `[environment]` pairs
/// (repo overrides cannot contribute them), then `host` overlays the before_session hook output.
pub(crate) async fn host_spawn_environment(
    cfg: &crate::session::Config,
    session_id: &str,
    tool: &str,
    profile: String,
    cwd: PathBuf,
) -> Result<(Vec<(String, String)>, Vec<(String, String)>), SupervisorError> {
    let base = crate::session::environment::resolve_host_environment_pairs(&cfg.environment);
    let mut host = base.clone();
    if !cfg.host_hooks.before_session.is_empty() {
        let minted = before_session_env(session_id, tool, profile, cwd)
            .await
            .map_err(|e| {
                SupervisorError::InvalidAgentCommand(format!(
                    "before_session hook task failed: {e}"
                ))
            })?
            .map_err(|e| {
                SupervisorError::Acp(AcpError::Spawn(format!("before_session hook: {e}")))
            })?;
        overlay_env(&mut host, minted);
    }
    Ok((base, host))
}

/// Run the profile's `before_session` host hooks and return the env they mint.
pub(super) async fn before_session_env(
    session_id: &str,
    tool: &str,
    profile: String,
    cwd: PathBuf,
) -> Result<anyhow::Result<Vec<(String, String)>>, tokio::task::JoinError> {
    let session_id = session_id.to_string();
    let tool = tool.to_string();
    tokio::task::spawn_blocking(move || {
        use crate::session::config::repo_config::{
            resolve_before_session_hooks, run_before_session_hooks,
        };
        let commands = resolve_before_session_hooks(&profile);
        if commands.is_empty() {
            return Ok(Vec::new());
        }
        let hook_env: Vec<(&'static str, String)> = vec![
            ("AOE_SESSION_ID", session_id),
            ("AOE_PROFILE", profile.clone()),
            ("AOE_TOOL", tool),
            ("AOE_PROJECT_PATH", cwd.to_string_lossy().to_string()),
        ];
        run_before_session_hooks(&commands, &cwd, &hook_env, &[])
    })
    .await
}

pub(super) fn overlay_env(env: &mut Vec<(String, String)>, minted: Vec<(String, String)>) {
    for (key, value) in minted {
        env.retain(|(k, _)| k != &key);
        env.push((key, value));
    }
}
/// Exports the selected target's declared `agent_config_dir` as its native agent's config-dir
/// variable, as a terminal launch does. `target` is the selected backend's own key (a wrapper's
/// name, not its base) and `native` the backend it resolves to. Returns the directory when the
/// agent is Claude, for native MCP discovery.
pub(super) fn apply_declared_config_dir(
    environment: &mut Vec<(String, String)>,
    cfg: &crate::session::Config,
    tool: &str,
    target: &str,
    native: &str,
) -> Option<std::path::PathBuf> {
    let session = &cfg.session;
    let native_of = |name: &str| crate::acp::agent_profiles::resolve(name).native_config_agent;
    // After an agent switch `tool` is unchanged but may name another backend: the tool's entry
    // applies only to a target it launches itself (same key, a built-in alias, or its base).
    let tool_runs_target = target == tool
        || native_of(target).is_some_and(|built_in| {
            native_of(tool) == Some(built_in)
                || [&session.agent_execution_as, &session.agent_detect_as]
                    .iter()
                    .any(|map| map.get(tool).map(String::as_str) == Some(native))
        });
    let key = if tool_runs_target { tool } else { target };
    let native = session
        .agent_execution_as
        .get(key)
        .map_or(native, String::as_str);
    let variable = match native_of(native)? {
        "claude" => "CLAUDE_CONFIG_DIR",
        "codex" => "CODEX_HOME",
        "opencode" => "OPENCODE_CONFIG_DIR",
        "pi" => "PI_CODING_AGENT_DIR",
        _ => return None,
    };
    let home = environment
        .iter()
        .rev()
        .find(|(name, value)| name == "HOME" && !value.is_empty())
        .map(|(_, value)| value.clone())
        .or_else(|| {
            crate::session::environment::resolve_host_environment_value(&cfg.environment, "HOME")
        })
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
        .filter(|home| home.is_absolute())?;
    let dir = session.agent_config_dir_for(key, &home)?;
    if variable == "CLAUDE_CONFIG_DIR" {
        // Setting the variable moves `.claude.json` into the store, so an implicit default stays unset.
        let ambient = environment
            .iter()
            .rev()
            .find(|(name, _)| name == variable)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(variable).ok())
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from);
        if crate::session::capture::is_default_claude_store(&dir, &home)
            && !crate::session::capture::is_explicit_claude_store_route(
                &dir,
                &home,
                Some(&dir),
                ambient.as_deref(),
            )
        {
            return None;
        }
    }
    overlay_env(
        environment,
        vec![(variable.into(), dir.to_string_lossy().into_owned())],
    );
    (variable == "CLAUDE_CONFIG_DIR").then_some(dir)
}

pub(super) fn apply_claude_store_pin(
    environment: &mut Vec<(String, String)>,
    pin: Option<&crate::session::capture::ClaudeStorePin>,
) -> Option<std::path::PathBuf> {
    let pin = pin?;
    let value = |key: &str| {
        environment
            .iter()
            .rev()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.clone())
            .or_else(|| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
    };
    let home = value("HOME").map(std::path::PathBuf::from);
    let export = crate::session::capture::exports_claude_store(pin, home.as_deref());
    environment.retain(|(key, _)| key != "CLAUDE_CONFIG_DIR");
    if export {
        environment.push((
            "CLAUDE_CONFIG_DIR".into(),
            pin.store.to_string_lossy().into_owned(),
        ));
        Some(pin.store.clone())
    } else {
        home
    }
}

pub(super) async fn resolve_mcp_servers(
    agent_key: &str,
    session_id: &str,
    profile: Option<String>,
    cwd: PathBuf,
    session_env: Vec<(String, String)>,
    native_config_override: Option<PathBuf>,
    failure: &'static str,
) -> Vec<agent_client_protocol::schema::v1::McpServer> {
    let agent_key = agent_key.to_string();
    let session = session_id.to_string();
    tokio::task::spawn_blocking(move || {
        resolve_mcp_layers(
            &agent_key,
            &session,
            profile.as_deref(),
            &cwd,
            &session_env,
            native_config_override.as_deref(),
        )
    })
    .await
    .unwrap_or_else(|e| {
        warn!(
            target: "acp.mcp",
            session = %session_id,
            error = %e,
            "{failure}; forwarding no servers"
        );
        Vec::new()
    })
}

fn resolve_mcp_layers(
    agent_key: &str,
    session_id: &str,
    profile: Option<&str>,
    cwd: &std::path::Path,
    session_env: &[(String, String)],
    native_config_override: Option<&std::path::Path>,
) -> Vec<agent_client_protocol::schema::v1::McpServer> {
    use crate::session::mcp::mcp_model::{resolve_effective, summarize};

    let merged = resolve_effective(agent_key, profile, cwd, session_env, native_config_override);
    if !merged.is_empty() {
        info!(
            target: "acp.mcp",
            session = %session_id,
            count = merged.len(),
            servers = %summarize(&merged),
            "forwarding MCP servers"
        );
    }
    crate::acp::mcp_config::project_servers_to_acp(merged, session_env)
}

/// Apply current model pins and effort defaults to a cached respawn config,
/// keeping an explicit effort.
pub(super) fn refresh_spawn_model_effort(
    config: &mut SpawnConfig,
    defaults: Option<&crate::session::config::AcpAgentDefaults>,
) {
    let cached_model = config
        .provider_env
        .iter()
        .find(|(key, _)| key == "AOE_AGENT_MODEL")
        .map(|(_, value)| value.clone());
    let explicit_effort = if config.default_effort_explicit {
        config.default_effort.take()
    } else {
        None
    };
    let (model, effort) =
        crate::session::config::resolve_spawn_model_effort(defaults, cached_model, explicit_effort);
    set_spawn_model(config, model);
    config.default_effort = effort;
}

/// Point both model channels of a cached respawn config at `model`.
pub(super) fn set_spawn_model(config: &mut SpawnConfig, model: Option<String>) {
    config
        .provider_env
        .retain(|(key, _)| key != "AOE_AGENT_MODEL");
    if let Some(model) = model.clone() {
        config.provider_env.push(("AOE_AGENT_MODEL".into(), model));
    }
    config.default_model = model;
}

#[cfg(test)]
pub(super) mod tests {
    use super::super::test_support::*;
    use super::super::WorkerKind;
    use super::*;
    use crate::acp::approvals::{ApprovalDecision, Nonce};
    use crate::acp::runner_lifecycle::test_support::FakeProcessControl;
    use crate::daemon::AcpWorkerState;
    use std::time::Duration;

    async fn managed_attach_original(
        scope: &mut super::super::test_support::managed::ManagedScope,
        label: &str,
    ) -> (
        super::super::test_support::managed::ManagedClient,
        super::super::test_support::managed::NativePeer,
    ) {
        let directory = scope.tempdir();
        let (config, peer) =
            scope.native_peer(directory.path().into(), "gemini", "gemini", |request| {
                let result = match request["method"].as_str()? {
                    "initialize" => serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                    "session/new" => serde_json::json!({"sessionId":"original-attach-native"}),
                    "session/prompt" => serde_json::json!({"stopReason":"end_turn"}),
                    _ => return None,
                };
                Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
            });
        let client = scope
            .spawn(config, label)
            .await
            .expect("real original Native");
        client.ready().await;
        (client, peer)
    }

    #[tokio::test(flavor = "current_thread")]
    #[serial_test::serial]
    async fn registry_contention_is_bounded_without_late_resume_mutation() {
        let (_home, _tmp) = isolate_home();
        let id = "registry-bounded";
        let sup = Supervisor::new(VecSink::new());
        sup.test_insert_worker(id).await;
        let original = sup.running_lease(id).unwrap();
        let entry = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(
                worker_registry::record_path(id)
                    .unwrap()
                    .with_extension("lock"),
            )
            .unwrap();
        fs2::FileExt::lock_exclusive(&entry).unwrap();
        assert!(
            worker_registry::try_registry_lock(id).unwrap().is_none(),
            "the real entry flock is held independently of supervisor locks"
        );
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            sup.begin_resume(
                id,
                ResumeKind::Spawn,
                worker_registry::ResumeAuthority::Explicit,
            ),
        )
        .await
        .expect("registry admission remains pollable within its existing budget");
        assert!(matches!(result, Err(SupervisorError::RegistryBusy)));
        assert_eq!(sup.running_lease(id), Some(original.clone()));
        assert_eq!(
            lock_recover(&sup.lifecycle).last_generation(id),
            original.epoch()
        );
        drop(entry);
        tokio::task::yield_now().await;
        assert_eq!(
            sup.running_lease(id),
            Some(original.clone()),
            "no abandoned mutator runs after release"
        );
        assert!(sup
            .begin_resume(
                id,
                ResumeKind::Spawn,
                worker_registry::ResumeAuthority::Explicit
            )
            .await
            .unwrap()
            .is_none());
        assert_eq!(sup.running_lease(id), Some(original));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn corrupt_stop_after_borrowed_handshake_preserves_original_actor_for_readmission() {
        super::super::test_support::managed::with_managed(|scope| {
            Box::pin(async move {
                let (original, peer) =
                    managed_attach_original(scope, "borrowed-corrupt-install").await;
                let id = original.id.clone();
                let identity = original.actual().connected_runner().unwrap();
                let original_record = worker_registry::load_strict(&id).unwrap().unwrap();
                let methods = peer.requests();
                let original_supervisor = scope.state.acp_supervisor.clone();
                let control = Arc::new(ObservedProcessControl::default());
                let observed = control.clone();
                let attached = scope
                    .adopt_with(original, false, || async {
                        let supervisor = Arc::new(
                            Supervisor::new(original_supervisor.sink.clone())
                                .with_native_admission()
                                .with_process_control(control),
                        );
                        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                        assert!(ATTACH_INSTALL_GATE
                            .lock()
                            .unwrap()
                            .replace((entered_tx, release_rx))
                            .is_none());
                        let attaching = tokio::spawn({
                            let supervisor = supervisor.clone();
                            let id = id.clone();
                            let cwd = original_record.cwd.clone();
                            async move { supervisor.attach(id, cwd, vec![], false, None).await }
                        });
                        tokio::time::timeout(Duration::from_secs(10), entered_rx)
                            .await
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            peer.requests(),
                            methods,
                            "attachment must not issue Native management RPCs"
                        );
                        let stop_path = worker_registry::record_path(&id)
                            .unwrap()
                            .with_extension("stop");
                        std::fs::write(&stop_path, b"{\"token\":").unwrap();
                        release_tx.send(()).unwrap();
                        let result = tokio::time::timeout(Duration::from_secs(10), attaching)
                            .await
                            .unwrap()
                            .unwrap();
                        assert!(matches!(
                            result,
                            Err(SupervisorError::Acp(AcpError::RetainedRuntimeUnavailable(
                                _
                            )))
                        ));
                        assert!(!supervisor.is_owned(&id).await);
                        assert!(observed.signals().is_empty());
                        assert!(observed.scopes().is_empty());
                        let after = worker_registry::load_strict(&id).unwrap().unwrap();
                        assert!(identity.owns_record(&after));
                        assert_eq!(after.scope, original_record.scope);
                        assert_eq!(
                            after.stored_acp_session_id,
                            original_record.stored_acp_session_id
                        );
                        assert_eq!(
                            crate::process::process_birth_identity(identity.pid)
                                .unwrap()
                                .unwrap()
                                .fingerprint(),
                            identity.birth_fingerprint.unwrap(),
                        );
                        std::fs::remove_file(stop_path).unwrap();
                    })
                    .await;
                attached.ready().await;
                assert_eq!(attached.actual().connected_runner(), Some(identity));
                assert_eq!(peer.requests(), methods);
                assert!(observed.signals().is_empty());
                assert!(observed.scopes().is_empty());
                attached
                    .shutdown()
                    .await
                    .expect("checked original retirement");
            })
        })
        .await;
    }

    #[tokio::test]
    async fn cancellation_waits_for_started_work_to_finish() {
        let cancellation = CancellationToken::new();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let token = cancellation.clone();
        let mut work = tokio::spawn(async move {
            await_resume_to_completion(&token, async {
                started_tx.send(()).unwrap();
                release_rx.await.unwrap();
            })
            .await
        });
        started_rx.await.unwrap();
        cancellation.cancel();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut work)
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        assert!(work.await.unwrap().is_err());
    }

    #[tokio::test]
    #[serial_test::serial]
    #[cfg(unix)]
    async fn lease_cancellation_releases_a_real_stalled_handshake() -> anyhow::Result<()> {
        let _home = isolate_home();
        let project = tempfile::tempdir()?;
        let ready = project.path().join("request-received");
        let mut row =
            crate::session::Instance::new("stalled-handshake", project.path().to_str().unwrap());
        row.id = "stalled-handshake".into();
        row.view = crate::session::View::Structured;
        crate::server::test_support::select_retained_for_test(&mut row);
        let storage = crate::session::Storage::new_unwatched(&row.source_profile)?;
        storage.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        let supervisor = Supervisor::new(VecSink::new());
        let mut reservation = reserve(
            supervisor
                .begin_resume(
                    &row.id,
                    ResumeKind::Spawn,
                    worker_registry::ResumeAuthority::Explicit,
                )
                .await,
        );
        let lease = reservation.lease().clone();
        let (admission, _) =
            capture_durable_launch(Some(&row.source_profile), &row.id, None, false).await?;
        let mut admission = admission.expect("persisted explicit launch proposal");
        supervisor
            .prepare_launch_admission(&mut admission, &mut reservation, None)
            .await?;
        let mut config = runner_config(project.path().join("unused.sock"));
        config.socket_path = None;
        config.launch_lease = Some(lease.clone());
        config.source_profile = Some(row.source_profile.clone());
        config.cwd = project.path().to_path_buf();
        config.stored_acp_session_id = match &admission.establishment.as_ref().unwrap().target {
            crate::acp::establishment::EstablishmentTarget::Load { session_id, .. } => {
                Some(session_id.clone())
            }
            _ => panic!("explicit retained fixture target"),
        };
        config.spec.command = "python3".into();
        config.spec.args = vec![
            "-c".into(),
            r#"import json, os, sys, time
request = json.loads(sys.stdin.readline())
assert request['method'] == 'initialize'
with open(sys.argv[1], 'w') as marker:
    marker.write(str(os.getpid()))
time.sleep(60)
"#
            .into(),
            ready.to_string_lossy().into_owned(),
        ];
        let spawning = tokio::spawn(AcpClient::spawn(
            config,
            AcpSessionId("stalled-handshake".into()),
        ));
        let pid = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(pid) = std::fs::read_to_string(&ready)
                    .unwrap_or_default()
                    .parse::<u32>()
                {
                    break pid;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;
        assert!(worker_registry::is_pid_alive(pid));
        lease.cancellation().cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), spawning).await??;
        assert!(result.is_err());
        assert!(!worker_registry::is_pid_alive(pid));
        Ok(())
    }

    #[tokio::test]
    async fn ordinary_spawn_is_rejected_while_restart_claim_is_held() {
        let supervisor = Supervisor::new(VecSink::new());
        let _restart = supervisor
            .try_begin_restart_with_reason("s-restart")
            .await
            .expect("restart claims the session");

        let result = supervisor
            .spawn(
                spawn_request("s-restart"),
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await;

        assert!(matches!(
            result,
            Err(SupervisorError::RestartInProgress(id)) if id == "s-restart"
        ));
        assert_eq!(
            supervisor.worker_state("s-restart").await,
            AcpWorkerState::Absent
        );
    }

    #[tokio::test]
    async fn cancelled_restart_claim_does_not_admit_a_replacement() {
        let supervisor = Supervisor::new(VecSink::new());
        let restart = supervisor
            .try_begin_restart_with_reason("s-restart")
            .await
            .expect("restart claims the session");

        supervisor.cancel_restart_request("s-restart").await;
        let result = supervisor
            .reserve_spawn_with_restart(
                "s-restart",
                ResumeKind::SpawnFirst,
                crate::process::worker_registry::ResumeAuthority::Explicit,
                Some(&restart),
            )
            .await;

        assert!(matches!(result, Err(SupervisorError::SpawnCancelled(id)) if id == "s-restart"));
        assert_eq!(
            supervisor.worker_state("s-restart").await,
            AcpWorkerState::Absent
        );
    }

    fn mcp_names(servers: &[agent_client_protocol::schema::v1::McpServer]) -> Vec<&str> {
        use agent_client_protocol::schema::v1::McpServer;
        servers
            .iter()
            .map(|server| match server {
                McpServer::Stdio(server) => server.name.as_str(),
                McpServer::Http(server) => server.name.as_str(),
                McpServer::Sse(server) => server.name.as_str(),
                _ => "unknown",
            })
            .collect()
    }

    #[tokio::test]
    async fn host_spawn_environment_forwards_configured_pairs() {
        let cfg = crate::session::Config {
            environment: vec![
                "PI_CODING_AGENT_DIR=/stores/pi".into(),
                "PI_ACP_PI_COMMAND=pi-wrapper".into(),
            ],
            ..Default::default()
        };
        let (base, host) =
            host_spawn_environment(&cfg, "s", "pi", String::new(), PathBuf::from("/"))
                .await
                .unwrap();
        let expected = vec![
            ("PI_CODING_AGENT_DIR".to_string(), "/stores/pi".to_string()),
            ("PI_ACP_PI_COMMAND".to_string(), "pi-wrapper".to_string()),
        ];
        assert_eq!(base, expected);
        assert_eq!(host, expected);
    }

    #[test]
    fn declared_config_dir_is_exported_under_each_agents_variable() {
        let cfg = |entries: &[(&str, &str)]| {
            let mut cfg = crate::session::Config::default();
            for (tool, dir) in entries {
                cfg.session
                    .agent_config_dir
                    .insert((*tool).into(), (*dir).into());
            }
            cfg
        };
        let run = |cfg: &crate::session::Config, tool: &str, agent: &str| {
            let mut env = vec![("CLAUDE_CONFIG_DIR".to_string(), "/hook".to_string())];
            let claude_dir = apply_declared_config_dir(&mut env, cfg, tool, agent, agent);
            (env, claude_dir)
        };
        let pair = |key: &str, value: &str| (key.to_string(), value.to_string());

        // (config, tool, agent, expected env, expected Claude dir)
        let cases = [
            // A wrapper reaches its base through `agent_detect_as`.
            (
                {
                    let mut cfg = cfg(&[("claude-c", "/acct")]);
                    cfg.session
                        .agent_detect_as
                        .insert("claude-c".into(), "claude".into());
                    cfg
                },
                "claude-c",
                "claude",
                vec![pair("CLAUDE_CONFIG_DIR", "/acct")],
                Some(PathBuf::from("/acct")),
            ),
            (
                cfg(&[("codex", "/acct")]),
                "codex",
                "codex",
                vec![
                    pair("CLAUDE_CONFIG_DIR", "/hook"),
                    pair("CODEX_HOME", "/acct"),
                ],
                None,
            ),
            (
                cfg(&[("opencode", "/acct")]),
                "opencode",
                "opencode",
                vec![
                    pair("CLAUDE_CONFIG_DIR", "/hook"),
                    pair("OPENCODE_CONFIG_DIR", "/acct"),
                ],
                None,
            ),
            (
                cfg(&[("pi", "/acct")]),
                "pi",
                "pi",
                vec![
                    pair("CLAUDE_CONFIG_DIR", "/hook"),
                    pair("PI_CODING_AGENT_DIR", "/acct"),
                ],
                None,
            ),
            // A custom tool's declared native identity selects the variable.
            (
                {
                    let mut cfg = cfg(&[("my-claude", "/acct")]);
                    cfg.session
                        .agent_execution_as
                        .insert("my-claude".into(), "claude".into());
                    cfg
                },
                "my-claude",
                "my-claude",
                vec![pair("CLAUDE_CONFIG_DIR", "/acct")],
                Some(PathBuf::from("/acct")),
            ),
            // After a switch the tool's entry is not the new backend's: use the backend's own.
            (
                cfg(&[("claude", "/acct"), ("codex", "/codex")]),
                "claude",
                "codex",
                vec![
                    pair("CLAUDE_CONFIG_DIR", "/hook"),
                    pair("CODEX_HOME", "/codex"),
                ],
                None,
            ),
            (
                cfg(&[("claude", "/acct")]),
                "claude",
                "codex",
                vec![pair("CLAUDE_CONFIG_DIR", "/hook")],
                None,
            ),
            (
                {
                    let mut cfg = cfg(&[("my-claude", "/acct")]);
                    cfg.session
                        .agent_execution_as
                        .insert("my-claude".into(), "claude".into());
                    cfg
                },
                "my-claude",
                "codex",
                vec![pair("CLAUDE_CONFIG_DIR", "/hook")],
                None,
            ),
            // No entry for the tool, and an agent with no config-dir variable.
            (
                cfg(&[("other", "/acct")]),
                "claude",
                "claude",
                vec![pair("CLAUDE_CONFIG_DIR", "/hook")],
                None,
            ),
            (
                cfg(&[("gemini", "/acct")]),
                "gemini",
                "gemini",
                vec![pair("CLAUDE_CONFIG_DIR", "/hook")],
                None,
            ),
        ];
        for (cfg, tool, agent, env, claude_dir) in cases {
            assert_eq!(run(&cfg, tool, agent), (env, claude_dir), "{tool}");
        }

        // Switching into a custom target selects the target's own key and identity.
        let into = |cfg: &crate::session::Config, tool: &str, target: &str, native: &str| {
            let mut env = vec![];
            let claude_dir = apply_declared_config_dir(&mut env, cfg, tool, target, native);
            (env, claude_dir)
        };
        let mut explicit = cfg(&[
            ("claude", "/acct"),
            ("work-codex", "/work"),
            ("codex", "/c"),
        ]);
        explicit
            .session
            .agent_execution_as
            .insert("work-codex".into(), "codex".into());
        assert_eq!(
            into(&explicit, "claude", "work-codex", "work-codex"),
            (vec![pair("CODEX_HOME", "/work")], None)
        );
        let mut inherited = cfg(&[
            ("claude", "/acct"),
            ("work-codex", "/work"),
            ("codex", "/c"),
        ]);
        inherited
            .session
            .agent_detect_as
            .insert("work-codex".into(), "codex".into());
        assert_eq!(
            into(&inherited, "claude", "work-codex", "codex"),
            (vec![pair("CODEX_HOME", "/work")], None)
        );
        // An unrelated wrapper of the tool's own native agent keeps its own key too.
        let mut sibling = cfg(&[("claude", "/acct"), ("claude-d", "/d")]);
        sibling
            .session
            .agent_detect_as
            .insert("claude-d".into(), "claude".into());
        assert_eq!(
            into(&sibling, "claude", "claude-d", "claude"),
            (
                vec![pair("CLAUDE_CONFIG_DIR", "/d")],
                Some(PathBuf::from("/d"))
            )
        );

        // The exact default store is implicit unless an ambient route names it.
        let default_store = cfg(&[("claude", "~/.claude")]);
        let mut env = vec![pair("HOME", "/home/u")];
        assert_eq!(
            apply_declared_config_dir(&mut env, &default_store, "claude", "claude", "claude"),
            None
        );
        assert_eq!(env, vec![pair("HOME", "/home/u")]);
        let mut env = vec![
            pair("HOME", "/home/u"),
            pair("CLAUDE_CONFIG_DIR", "/home/u/.claude"),
        ];
        assert_eq!(
            apply_declared_config_dir(&mut env, &default_store, "claude", "claude", "claude"),
            Some(PathBuf::from("/home/u/.claude"))
        );

        // A relative effective HOME would resolve differently per process: no export.
        let mut env = vec![pair("HOME", "account")];
        let relative_home = cfg(&[("claude", "~/.claude-work")]);
        assert_eq!(
            apply_declared_config_dir(&mut env, &relative_home, "claude", "claude", "claude"),
            None
        );
        assert_eq!(env, vec![pair("HOME", "account")]);

        // `~` expands against the worker's effective HOME, not the configured one.
        let mut cfg = cfg(&[("claude", "~/.claude-work")]);
        cfg.environment = vec!["HOME=/home/operator".into()];
        let mut env = vec![pair("HOME", "/srv/account")];
        let claude_dir = apply_declared_config_dir(&mut env, &cfg, "claude", "claude", "claude");
        assert_eq!(claude_dir, Some(PathBuf::from("/srv/account/.claude-work")));
        assert_eq!(
            env,
            vec![
                pair("HOME", "/srv/account"),
                pair("CLAUDE_CONFIG_DIR", "/srv/account/.claude-work"),
            ]
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_session_exports_declared_config_dir_and_a_recorded_store_outranks_it() {
        let (_home, temp) = isolate_home();
        let declared = temp.path().join("declared");
        let recorded = temp.path().join("recorded");
        std::fs::create_dir_all(&declared).unwrap();
        std::fs::create_dir_all(&recorded).unwrap();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            format!(
                "[session.agent_config_dir]\nclaude-code = \"{}\"\n",
                declared.display()
            ),
        )
        .unwrap();
        let exported = |config: &SpawnConfig| {
            config
                .host_environment
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| PathBuf::from(value))
        };

        let supervisor = Supervisor::new(VecSink::new());
        let mut request = spawn_request("fresh-declared");
        let (config, _) = supervisor.spawn_config(&request, 1).await.unwrap();
        assert_eq!(exported(&config), Some(declared));

        request.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: recorded.clone(),
            exported_default_store: Some(false),
        });
        let (config, _) = supervisor.spawn_config(&request, 1).await.unwrap();
        assert_eq!(exported(&config), Some(recorded));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn selected_claude_store_controls_spawn_environment_and_native_mcp() {
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
                 [session.agent_config_dir]\nclaude-code = \"{}\"\n",
                temp.path().join("hook").display(),
                declared.display()
            ),
        )
        .unwrap();

        let supervisor = Supervisor::new(VecSink::new());
        let mut request = spawn_request("selected-store");
        request.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: selected.clone(),
            exported_default_store: Some(false),
        });
        let (config, context_reset) = supervisor.spawn_config(&request, 1).await.unwrap();
        assert!(context_reset.is_none());

        assert_eq!(mcp_names(&config.mcp_servers), ["selected"]);
        assert_eq!(
            config
                .host_environment
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| value.as_str()),
            selected.to_str()
        );
        assert!(config
            .host_environment
            .contains(&("HOOK_VALUE".into(), "kept".into())));
    }

    #[test]
    #[serial_test::serial]
    fn claude_route_distinguishes_implicit_explicit_and_custom_stores() {
        let (_home, temp) = isolate_home();
        let home = temp.path().to_path_buf();
        let default = home.join(".claude");
        let custom = home.join("custom");
        std::fs::create_dir_all(&default).unwrap();
        std::fs::create_dir_all(&custom).unwrap();
        let route = |store: &std::path::Path, provenance: Option<bool>, hook: &str| {
            let mut environment = vec![
                ("HOME".into(), home.display().to_string()),
                ("CLAUDE_CONFIG_DIR".into(), hook.into()),
                ("AUTH_SENTINEL".into(), "kept".into()),
            ];
            let effective = apply_claude_store_pin(
                &mut environment,
                Some(&crate::session::capture::ClaudeStorePin {
                    store: store.to_path_buf(),
                    exported_default_store: provenance,
                }),
            );
            let exported = environment
                .iter()
                .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
                .map(|(_, value)| PathBuf::from(value));
            assert!(environment.contains(&("AUTH_SENTINEL".into(), "kept".into())));
            (effective, exported)
        };

        assert_eq!(
            route(&default, Some(false), "/other"),
            (Some(home.clone()), None)
        );
        assert_eq!(route(&default, None, "/other"), (Some(home.clone()), None));
        assert_eq!(
            route(&default, Some(true), "/other"),
            (Some(default.clone()), Some(default.clone()))
        );
        assert_eq!(
            route(&custom, Some(false), "/other"),
            (Some(custom.clone()), Some(custom.clone()))
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn implicit_default_claude_store_aligns_native_mcp_with_home() {
        let (_home, temp) = isolate_home();
        let hook_store = temp.path().join("hook-store");
        let home = temp.path().to_path_buf();
        let default = home.join(".claude");
        std::fs::create_dir_all(&hook_store).unwrap();
        std::fs::write(
            hook_store.join(".claude.json"),
            r#"{ "mcpServers": { "stale": { "command": "stale" } } }"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".claude.json"),
            r#"{ "mcpServers": { "home": { "command": "home" } } }"#,
        )
        .unwrap();
        let app_dir = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            format!(
                "[host_hooks]\nbefore_session = \"printf 'CLAUDE_CONFIG_DIR={}\\n'\n",
                hook_store.display()
            ),
        )
        .unwrap();

        let supervisor = Supervisor::new(VecSink::new());
        let mut request = spawn_request("implicit-default");
        request.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: default,
            exported_default_store: Some(false),
        });
        let (config, _) = supervisor.spawn_config(&request, 1).await.unwrap();
        assert!(!config
            .host_environment
            .iter()
            .any(|(key, _)| key == "CLAUDE_CONFIG_DIR"));
        assert_eq!(mcp_names(&config.mcp_servers), ["home"]);
    }

    #[test]
    fn respawn_refreshes_the_model_pin_and_keeps_explicit_effort() {
        use crate::session::config::AcpAgentDefaults;
        let pin = |model: &str| AcpAgentDefaults {
            model: Some(model.into()),
            pin_model: true,
            effort: Some("low".into()),
            effort_by_model: [("model-b".to_string(), "high".to_string())].into(),
            ..Default::default()
        };
        let unpinned = AcpAgentDefaults {
            model: Some("model-b".into()),
            effort: Some("low".into()),
            ..Default::default()
        };
        // (name, cached effort explicit, defaults, want model, want effort)
        let cases = [
            (
                "pin moved to b",
                false,
                Some(pin("model-b")),
                "model-b",
                Some("high"),
            ),
            (
                "pin still a",
                false,
                Some(pin("model-a")),
                "model-a",
                Some("low"),
            ),
            ("no entry", false, None, "model-a", None),
            (
                "plain default",
                false,
                Some(unpinned),
                "model-a",
                Some("low"),
            ),
            (
                "explicit effort survives a pin move",
                true,
                Some(pin("model-b")),
                "model-b",
                Some("low"),
            ),
        ];
        for (name, explicit, defaults, want_model, want_effort) in cases {
            let mut config = runner_config(std::env::temp_dir().join("unused.sock"));
            config.provider_env = vec![
                ("AOE_AGENT_MODEL".into(), "model-a".into()),
                ("OTHER".into(), "kept".into()),
            ];
            config.default_effort = Some("low".into());
            config.default_effort_explicit = explicit;
            refresh_spawn_model_effort(&mut config, defaults.as_ref());
            let models: Vec<&str> = config
                .provider_env
                .iter()
                .filter(|(key, _)| key == "AOE_AGENT_MODEL")
                .map(|(_, value)| value.as_str())
                .collect();
            assert_eq!(models, [want_model], "{name}");
            assert_eq!(config.default_effort.as_deref(), want_effort, "{name}");
            assert!(
                config
                    .provider_env
                    .contains(&("OTHER".into(), "kept".into())),
                "{name}"
            );
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_does_not_rederive_effort_provenance_from_the_value() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4343);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control)
                .with_launcher(gated_launcher(&gate, 4343)),
        );
        let mut req = spawn_request("s-prov");
        req.effort = Some("low".into());

        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    req,
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        gate.entered.notified().await;
        gate.open.notify_one();
        spawner.await.unwrap().expect("spawn");

        let explicit = sup
            .workers
            .lock()
            .await
            .get("s-prov")
            .map(|handle| match &handle.kind {
                WorkerKind::Runner { spawn_config } => spawn_config.default_effort_explicit,
                _ => panic!("runner handle expected"),
            })
            .expect("worker installed");
        assert!(
            !explicit,
            "a resolved default effort must not read as a session pin"
        );
    }

    /// #4116: the handshake holds no lifecycle lock, so an archive (which a TUI takes on its
    /// input thread) commits without waiting; the post-handshake recheck retires the runner.
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_retires_a_runner_whose_row_was_archived_during_the_handshake() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4545);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4545)),
        );
        let mut inst = crate::session::Instance::new("s-archived", "/tmp");
        inst.id = "s-archived".into();
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = Some("native-archived-handshake".into());
        inst.resume_intent = crate::session::ResumeIntent::Use("native-archived-handshake".into());
        let storage = crate::session::Storage::new_unwatched(&inst.source_profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![inst.clone()];
                Ok(())
            })
            .unwrap();
        let mut req = spawn_request("s-archived");
        req.source_profile = Some(inst.source_profile.clone());
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    req,
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("structured launcher must reach its handshake");
        let original = sup
            .capture_retirement("s-archived")
            .unwrap()
            .identity
            .expect("the producer records proof before the handshake gate");
        assert_eq!(original, attested_test_identity(4545, original.generation));
        assert!(
            worker_registry::load_strict("s-archived")
                .unwrap()
                .is_none(),
            "producer proof must not come from a later registry record"
        );

        assert!(
            !storage.instance_lifecycle_lock_is_held_for_test("s-archived"),
            "the handshake must not hold the lifecycle lock"
        );
        {
            let _lock = storage
                .acquire_instance_lifecycle_lock("s-archived")
                .unwrap();
            storage
                .update(|rows, _| {
                    rows[0].archive();
                    Ok(())
                })
                .unwrap();
        }
        gate.open.notify_one();

        assert!(matches!(
            spawner.await.unwrap(),
            Err(SupervisorError::Blocked(
                crate::session::StartBlocked::Archived
            ))
        ));
        assert_eq!(sup.worker_state("s-archived").await, AcpWorkerState::Absent);
        assert!(
            control.signals().iter().any(|(pid, _)| *pid == 4545),
            "the runner launched for the archived row must be torn down"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancelled_terminal_handoff_cannot_forget_a_pending_runner() {
        use crate::acp::runner_lifecycle::ProcessControl;
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4646);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4646)),
        );
        let mut row = crate::session::Instance::new("s-terminal-handoff", "/tmp");
        row.id = "s-terminal-handoff".into();
        row.view = crate::session::View::Structured;
        row.acp_session_id = Some("native-terminal-handoff".into());
        row.resume_intent = crate::session::ResumeIntent::Use("native-terminal-handoff".into());
        let storage = crate::session::Storage::new_unwatched(&row.source_profile).unwrap();
        storage
            .update(|rows, _| {
                *rows = vec![row.clone()];
                Ok(())
            })
            .unwrap();
        let mut request = spawn_request(&row.id);
        request.source_profile = Some(row.source_profile.clone());
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    request,
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        gate.entered.notified().await;
        let original = sup
            .capture_retirement(&row.id)
            .unwrap()
            .identity
            .expect("the producer records proof before the handshake gate");
        assert_eq!(original, attested_test_identity(4646, original.generation));
        assert!(
            worker_registry::load_strict(&row.id).unwrap().is_none(),
            "producer proof must not come from a later registry record"
        );
        storage
            .update(|rows, _| {
                rows[0].view = crate::session::View::Terminal;
                rows[0].lifecycle_generation += 1;
                Ok(())
            })
            .unwrap();
        sup.shutdown(&row.id).await.unwrap();
        sup.forget_session(&row.id)
            .expect_err("a pending producer retains its matching teardown authority");
        gate.open.notify_one();
        assert!(matches!(
            spawner.await.unwrap(),
            Err(SupervisorError::SpawnCancelled(_))
        ));
        assert!(
            control.signals().contains(&(4646, "TERM")),
            "cancelled handoff must retain the matching teardown lease"
        );
        assert!(!control.is_alive(original));
        assert!(worker_registry::load(&row.id).unwrap().is_none());
        assert_eq!(sup.worker_state(&row.id).await, AcpWorkerState::Absent);
        assert_eq!(stopped_reasons(&sup.sink, &row.id), ["user_stopped"]);
        sup.retry_pending_teardowns().await;
        assert_eq!(stopped_reasons(&sup.sink, &row.id), ["user_stopped"]);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_rejects_unknown_agents_and_running_sessions() {
        let _home = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let mut req = spawn_request("s-1");
        req.agent = "no-such-agent".into();
        assert!(matches!(
            sup.spawn(
                req,
                crate::process::worker_registry::ResumeAuthority::Explicit
            )
            .await,
            Err(SupervisorError::UnknownAgent(_))
        ));

        sup.test_insert_worker("s-1").await;
        assert!(matches!(
            sup.spawn(
                spawn_request("s-1"),
                crate::process::worker_registry::ResumeAuthority::Explicit
            )
            .await,
            Err(SupervisorError::AlreadyRunning(_))
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn capacity_counts_workers_detached_runners_and_pending_spawns() {
        let (_home, _tmp) = isolate_home();

        let sup = Supervisor::with_capacity(VecSink::new(), 1);
        sup.test_insert_worker("s-1").await;
        match sup
            .spawn(
                spawn_request("s-2"),
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!((current, limit), (1, 1));
            }
            other => panic!("in-memory worker: expected CapacityFull, got {other:?}"),
        }

        let sup = Supervisor::with_capacity(VecSink::new(), 1);
        let socket = worker_registry::workers_dir()
            .unwrap()
            .join("detached-1.sock");
        worker_registry::touch_live_socket(&socket);
        let record = worker_record("detached-1", std::process::id(), socket);
        worker_registry::save(&record).unwrap();
        assert!(worker_registry::is_record_live(&record));
        match sup
            .spawn(
                spawn_request("fresh"),
                crate::process::worker_registry::ResumeAuthority::Explicit,
            )
            .await
        {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!(
                    (current, limit),
                    (1, 1),
                    "detached registry entry must count"
                );
            }
            other => panic!("detached runner: expected CapacityFull, got {other:?}"),
        }
        worker_registry::delete("detached-1").unwrap();

        let sup = Supervisor::with_capacity(VecSink::new(), 2);
        let _a = reserve(
            sup.begin_resume(
                "s-a",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        let _attach = reserve(
            sup.begin_resume(
                "s-attach",
                ResumeKind::Attach,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        let _b = reserve(
            sup.begin_resume(
                "s-b",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        match sup
            .begin_resume(
                "s-c",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await
        {
            Err(SupervisorError::CapacityFull { current, limit }) => {
                assert_eq!(
                    (current, limit),
                    (2, 2),
                    "in-flight spawns hold a slot, an attach does not"
                );
            }
            Err(other) => panic!("expected CapacityFull, got {other:?}"),
            Ok(_) => panic!("expected CapacityFull, got an admission"),
        }
        assert_eq!(
            sup.worker_state("s-c").await,
            AcpWorkerState::Absent,
            "a refused admission leaves nothing behind"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resolve_mcp_layers_merges_native_global_profile_and_trusted_project() {
        let (_home, tmp) = isolate_home();
        let _env = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let write = |path: std::path::PathBuf, servers: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, format!(r#"{{ "mcpServers": {{ {servers} }} }}"#)).unwrap();
        };
        write(
            tmp.path().join(".claude.json"),
            r#""native-only": { "command": "n" }, "shared": { "command": "from-native" }"#,
        );
        write(
            crate::session::get_app_dir().unwrap().join("mcp.json"),
            r#""global-only": { "command": "g" }, "shared": { "command": "from-global" }"#,
        );
        write(
            crate::session::get_profile_dir_path("work")
                .unwrap()
                .join("mcp.json"),
            r#""profile-only": { "command": "p" }, "shared": { "command": "from-profile" }"#,
        );
        let repo = tmp.path().join("repo");
        write(
            repo.join(".mcp.json"),
            r#""project-only": { "command": "pl" }, "shared": { "command": "from-project" }"#,
        );

        let resolve = |profile: Option<&'static str>, cwd: std::path::PathBuf| async move {
            let merged = tokio::task::spawn_blocking(move || {
                resolve_mcp_layers("claude", "resolve-test", profile, &cwd, &[], None)
            })
            .await
            .unwrap();
            let val = serde_json::to_value(&merged).unwrap();
            let mut names: Vec<String> = val
                .as_array()
                .unwrap()
                .iter()
                .map(|s| s["name"].as_str().unwrap().to_string())
                .collect();
            names.sort();
            let shared = val
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["name"] == "shared")
                .map(|s| s["command"].as_str().unwrap().to_string());
            (names, shared)
        };

        let (names, shared) = resolve(Some("work"), tmp.path().to_path_buf()).await;
        assert_eq!(
            names,
            ["global-only", "native-only", "profile-only", "shared"],
            "native + global + profile union"
        );
        assert_eq!(shared.as_deref(), Some("from-profile"), "profile wins");

        // A profile with no mcp.json, so the project-local trust gate is read
        // against the global layer: with no profile argument at all,
        // `resolve_default_profile` would pick "work" (the only profile on
        // disk) and its `shared` would win.
        let (names, shared) = resolve(Some("empty"), repo.clone()).await;
        assert!(
            !names.contains(&"project-only".to_string()),
            "untrusted project is skipped"
        );
        assert_eq!(shared.as_deref(), Some("from-global"));

        let servers = crate::session::mcp::project_mcp::load_project_mcp_servers(&repo).unwrap();
        let hash = crate::session::mcp::project_mcp::fingerprint(&servers);
        crate::session::config::repo_config::trust_repo(&repo, None, Some(&hash)).unwrap();
        let (names, shared) = resolve(Some("empty"), repo).await;
        assert!(
            names.contains(&"project-only".to_string()),
            "trusted project is forwarded"
        );
        assert_eq!(
            shared.as_deref(),
            Some("from-project"),
            "trusted project wins"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_retires_old_approval_before_publishing_queued_request() {
        use crate::acp::approvals::Approval;
        use crate::acp::state::ToolCall;

        let (_home, _tmp) = isolate_home();
        let (sink, store, mut rx, _tmp) = channel_sink();
        let approval = |nonce: &str| Approval {
            nonce: Nonce(nonce.into()),
            tool_call: ToolCall {
                id: nonce.into(),
                name: "Bash".into(),
                kind: "execute".into(),
                args_preview: r#"{"command":"pwd"}"#.into(),
                started_at: chrono::Utc::now(),
                parent_tool_call_id: None,
                memory_recall: None,
                diffs: Vec::new(),
            },
            destructive: false,
            options: Vec::new(),
            choice: false,
            plan: None,
            requested_at: chrono::Utc::now(),
            resolved: None,
        };
        sink.publish(
            "s-startup",
            1,
            &crate::acp::event_sender::AcpEventData::from_history(Event::ApprovalRequested {
                approval: approval("old"),
            }),
        );
        let fresh = approval("live");
        let senders: Arc<std::sync::Mutex<Vec<crate::acp::event_sender::EventSender>>> =
            Default::default();
        let launcher: super::super::Launcher = Arc::new(move |config, session_id| {
            let fresh = fresh.clone();
            let senders = senders.clone();
            Box::pin(async move {
                save_record(&session_id.0, 4345, config.generation);
                let (client, tx) = AcpClient::fake_for_test(session_id);
                tx.send(Event::ApprovalRequested { approval: fresh })
                    .await
                    .unwrap();
                senders.lock().unwrap().push(tx);
                Ok(client.with_runner_pid(4345))
            })
        });
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4345);
        let sup = Supervisor::new(sink)
            .with_process_control(control)
            .with_launcher(launcher);
        sup.hydrate_seqs(store.all_session_seqs().unwrap());
        sup.spawn(
            spawn_request("s-startup"),
            crate::process::worker_registry::ResumeAuthority::Explicit,
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !store
                .unresolved_approval_nonces("s-startup")
                .contains(&Nonce("live".into()))
            {
                rx.recv().await.unwrap();
            }
        })
        .await
        .expect("queued approval must reach the durable log");
        assert_eq!(
            store.unresolved_approval_nonces("s-startup"),
            vec![Nonce("live".into())]
        );
        let events: Vec<_> = store
            .replay_from("s-startup", 0)
            .expect("durable replay")
            .into_iter()
            .filter_map(|stored| match stored.event.as_ref() {
                Event::ApprovalRequested { approval } => {
                    Some(format!("requested:{}", approval.nonce.0))
                }
                Event::ApprovalResolved { nonce, decision } => {
                    assert_eq!(*decision, ApprovalDecision::Cancelled);
                    Some(format!("cancelled:{}", nonce.0))
                }
                _ => None,
            })
            .collect();
        assert_eq!(events, ["requested:old", "cancelled:old", "requested:live"]);
        sup.shutdown("s-startup").await.unwrap();
    }

    /// A launch left outstanding by a previous daemon (its tailer died with
    /// that daemon, so no completion will ever arrive) gets a synthetic
    /// `Detached` the moment a fresh worker spawns over it, instead of showing
    /// as running forever (#4001).
    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_detaches_background_agent_orphaned_by_previous_daemon() {
        use crate::acp::state::BackgroundAgentStatus;

        let (_home, _tmp) = isolate_home();
        let (sink, store, mut rx, _store_tmp) = channel_sink();
        sink.publish(
            "s-startup",
            1,
            &crate::acp::event_sender::AcpEventData::from_history(Event::BackgroundAgentLaunched {
                agent_id: "sub-1".into(),
                tool_call_id: "tc-1".into(),
                description: "do a thing".into(),
                prompt: "do a thing".into(),
                model: "claude".into(),
                output_file: "/tmp/nonexistent.jsonl".into(),
                started_at: chrono::Utc::now(),
                workflow: None,
            }),
        );
        sink.publish(
            "s-startup",
            2,
            &crate::acp::event_sender::AcpEventData::from_history(Event::BackgroundAgentProgress {
                agent_id: "sub-1".into(),
                status: BackgroundAgentStatus::Running,
                tool_count: 1,
                tools: Vec::new(),
                last_tool: None,
                last_text: None,
                at: chrono::Utc::now(),
            }),
        );
        let launcher: super::super::Launcher = Arc::new(move |config: SpawnConfig, session_id| {
            Box::pin(async move {
                save_record(&session_id.0, 4345, config.generation);
                let (client, _tx) = AcpClient::fake_for_test(session_id);
                Ok(client.with_runner_pid(4345))
            })
        });
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4345);
        let sup = Supervisor::new(sink)
            .with_process_control(control)
            .with_launcher(launcher);
        sup.hydrate_seqs(store.all_session_seqs().unwrap());
        sup.spawn(
            spawn_request("s-startup"),
            crate::process::worker_registry::ResumeAuthority::Explicit,
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !store
                .unresolved_background_agent_ids("s-startup")
                .is_empty()
            {
                rx.recv().await.unwrap();
            }
        })
        .await
        .expect("stale background agent must be detached in the durable log");
        let detached = store
            .replay_from("s-startup", 0)
            .expect("durable replay")
            .into_iter()
            .any(|stored| {
                matches!(
                    stored.event.as_ref(),
                    Event::BackgroundAgentCompleted {
                        status: BackgroundAgentStatus::Detached,
                        ..
                    }
                )
            });
        assert!(
            detached,
            "the orphaned launch must be closed out as Detached"
        );
        sup.shutdown("s-startup").await.unwrap();
    }

    /// Reaps the runner and aborts its handshake, including when attach fails.
    pub(in crate::acp::supervisor) struct AttachBackgroundAgentFixture {
        pub(in crate::acp::supervisor) store: Arc<crate::acp::event_store::EventStore>,
        pub(in crate::acp::supervisor) client: super::super::test_support::managed::ManagedClient,
        pub(in crate::acp::supervisor) peer: super::super::test_support::managed::NativePeer,
        pub(in crate::acp::supervisor) native: crate::process::CapturedProcess,
        pub(in crate::acp::supervisor) task_transcript: std::path::PathBuf,
    }

    async fn publish_native_background_start(
        client: &mut super::super::test_support::managed::ManagedClient,
        peer: &super::super::test_support::managed::NativePeer,
        store: &crate::acp::event_store::EventStore,
        frame: serde_json::Value,
        trackable: bool,
        matches: impl Fn(&Event) -> bool,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                peer.send(frame.clone()).await;
                loop {
                    let event = client
                        .recv()
                        .await
                        .expect("original Native observer remains open until resource publication");
                    if matches(&event) {
                        break;
                    }
                }
                if !trackable
                    || store
                        .replay_from(&client.id, 0)
                        .expect("durable original resource history")
                        .iter()
                        .any(|stored| {
                            matches(stored.event.as_ref())
                                && stored.metadata.resource_witness.is_some()
                        })
                {
                    return;
                }
                // A producer-side try-lock may admit only history. A fresh
                // genuine Native observation, not that history, must start I/O.
            }
        })
        .await
        .expect("actual Native resource observation reaches first I/O");
    }

    pub(in crate::acp::supervisor) async fn managed_background_original(
        scope: &mut super::super::test_support::managed::ManagedScope,
        label: &str,
        output_file: &str,
        open_task: Option<&str>,
        failed_main: bool,
    ) -> AttachBackgroundAgentFixture {
        use crate::acp::event_sender::EventScope;
        let workdir = scope.tempdir();
        let test_name = std::thread::current()
            .name()
            .expect("managed libtest name")
            .to_owned();
        let inspection = scope.inspect_next_runner(&test_name);
        let mut fail_next_prompt = failed_main;
        let (config, peer) = scope.native_peer(
            workdir.path().to_path_buf(),
            "claude",
            "claude",
            move |request| {
                let id = request.get("id")?;
                let result = match request["method"].as_str()? {
                    "initialize" => serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                    "session/new" => serde_json::json!({"sessionId":"background-native"}),
                    "session/prompt" if fail_next_prompt => {
                        fail_next_prompt = false;
                        return Some(serde_json::json!({"jsonrpc":"2.0","id":id,
                                "error":{"code":-32603,"message":"original failed main"}}));
                    }
                    "session/prompt" => serde_json::json!({"stopReason":"end_turn"}),
                    _ => return None,
                };
                Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}))
            },
        );
        let mut client = scope
            .spawn(config, label)
            .await
            .expect("real Native launch");
        client.ready().await;
        let proof = inspection
            .native_spawn_proof(&client.lease)
            .expect("original Native birth observation")
            .expect("actual Native child");
        let native = crate::process::CapturedProcess::capture(proof.pid, &proof.birth)
            .expect("capture original Native incarnation")
            .expect("Native is still alive");
        let receipt = client
            .actual()
            .establishment_projection()
            .unwrap()
            .receipt_snapshot()
            .expect("actual Applied receipt");
        let native_session = receipt.session_id().expect("actual Native SID").to_owned();
        let home = client
            .actual()
            .native_store
            .as_ref()
            .filter(|binding| binding.agent == "claude" && binding.filesystem == "host")
            .and_then(|binding| binding.stores.first())
            .expect("actual original host Claude store")
            .clone();
        let project = home.join("projects/-enc");
        std::fs::create_dir_all(&project).unwrap();
        let task_transcript = project.join(format!("{native_session}.jsonl"));
        std::fs::write(&task_transcript, "").unwrap();
        if failed_main {
            scope
                .state
                .acp_supervisor
                .publish_user_prompt(&client.id, "original failed main".into())
                .await;
            client
                .send_prompt("original failed main", &[])
                .await
                .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !matches!(client.recv().await.expect("original prompt observer remains open"),
                        Event::Stopped { reason } if reason == "prompt_failed")
                {}
            })
            .await
            .expect("original correlated Native failure is durable");
        }
        let store = scope.state.acp_event_store.clone();
        publish_native_background_start(&mut client, &peer, &store,
            serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":native_session,"update":{
                    "sessionUpdate":"tool_call_update","toolCallId":"tc-1",
                    "_meta":{"claudeCode":{"toolName":"Agent","toolResponse":{
                        "agentId":"sub-1","description":"do a thing","prompt":"do a thing",
                        "resolvedModel":"test-model","outputFile":output_file,"status":"async_launched"
                    }}}
                }
            }}),
            !output_file.is_empty(),
            |event| matches!(event, Event::BackgroundAgentLaunched { agent_id, .. } if agent_id == "sub-1"),
        ).await;
        let launches = store
            .unresolved_background_agent_launches(&client.id)
            .unwrap();
        let launch = launches
            .iter()
            .find(|launch| launch.agent_id == "sub-1")
            .unwrap();
        if output_file.is_empty() {
            // An untrackable launch grants no transcript access.
            assert!(launch.witness.is_none());
        } else {
            assert!(
                launch.witness.is_some(),
                "the actual first resource I/O must mint its durable witness"
            );
            let stored = store.replay_from(&client.id, 0).unwrap().into_iter().find(|stored|
                    matches!(stored.event.as_ref(), Event::BackgroundAgentLaunched { agent_id, .. } if agent_id == "sub-1")
                        && stored.metadata.resource_witness.is_some()).unwrap();
            assert_eq!(stored.metadata.scope, EventScope::HistoryOnly);
            assert_eq!(
                stored.metadata.resource_ref,
                stored
                    .metadata
                    .resource_witness
                    .as_ref()
                    .map(|witness| witness.allocation)
            );
        }
        if let Some(task_id) = open_task {
            publish_native_background_start(&mut client, &peer, &store,
                serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":native_session,"update":{
                        "sessionUpdate":"tool_call_update","toolCallId":"toolu_task",
                        "_meta":{"claudeCode":{"toolName":"Bash","toolResponse":{"backgroundTaskId":task_id}}}
                    }
                }}),
                true,
                |event| matches!(event, Event::BackgroundTaskStarted { task_id: actual, .. } if actual == task_id),
            ).await;
            let launches = store
                .unresolved_background_task_launches(&client.id)
                .unwrap();
            assert!(launches
                .iter()
                .find(|launch| launch.task_id == task_id)
                .unwrap()
                .witness
                .is_some());
        }
        AttachBackgroundAgentFixture {
            store,
            client,
            peer,
            native,
            task_transcript,
        }
    }

    pub(in crate::acp::supervisor) fn fold_background_journal(
        store: &crate::acp::event_store::EventStore,
        id: &str,
    ) -> crate::acp::state::AcpState {
        let mut state = crate::acp::state::AcpState::new(
            crate::acp::state::AcpSessionId(id.into()),
            crate::acp::state::AgentName("claude".into()),
            None,
        );
        for stored in store.replay_from(id, 0).expect("durable scoped replay") {
            state
                .apply_scoped_event(stored.seq, stored.event.as_ref(), &stored.metadata)
                .unwrap();
        }
        state
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn cancelled_attach_retires_only_its_recorded_scope() {
        use crate::acp::runner_lifecycle::ProcessControl as _;
        super::super::test_support::managed::with_managed(|scope| {
            Box::pin(async move {
                let test_name = std::thread::current().name().unwrap().to_owned();
                let inspection = scope.inspect_next_runner(&test_name);
                let (original, peer) =
                    managed_attach_original(scope, "cancelled-scope-attach").await;
                let id = original.id.clone();
                let identity = original.actual().connected_runner().unwrap();
                let proof = inspection
                    .native_spawn_proof(&original.lease)
                    .unwrap()
                    .unwrap();
                let native = crate::process::CapturedProcess::capture(proof.pid, &proof.birth)
                    .unwrap()
                    .expect("actual original Native incarnation");
                let methods = peer.requests();
                scope.state.acp_supervisor.detach_all().await;
                let record = runner_record(&id).await.expect("retained original record");
                assert!(identity.owns_record(&record));
                let birth = crate::process::process_birth_identity(identity.pid)
                    .unwrap()
                    .expect("original runner is alive before cancellation");
                assert_eq!(Some(birth.fingerprint()), identity.birth_fingerprint);
                let original_scope = record.scope.clone();
                assert_eq!(original_scope, worker_registry::ScopeOwnership::Unscoped);
                let control = Arc::new(ObservedProcessControl::default());
                let supervisor = Arc::new(
                    Supervisor::new(scope.state.acp_supervisor.sink.clone())
                        .with_native_admission()
                        .with_process_control(control.clone()),
                );
                let (gate, release, entered) = scope.gate();
                assert!(ATTACH_INSTALL_GATE.lock().unwrap().replace(gate).is_none());
                let attaching = tokio::spawn({
                    let supervisor = supervisor.clone();
                    let id = id.clone();
                    async move { supervisor.attach(id, record.cwd, vec![], false, None).await }
                });
                tokio::time::timeout(Duration::from_secs(10), entered)
                    .await
                    .unwrap()
                    .unwrap();
                let target = supervisor
                    .capture_retirement(&id)
                    .expect("frozen original pre-install ownership");
                let lease = target
                    .lease
                    .as_ref()
                    .expect("actual borrowed constructor")
                    .clone();
                supervisor.shutdown(&id).await.unwrap();
                release.release();
                assert!(tokio::time::timeout(Duration::from_secs(10), attaching)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err());
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !lease.original_constructor_completed() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("cancelled original constructor and SDK custody finish");
                supervisor
                    .shutdown_captured_and_wait(&id, &target, Duration::from_secs(5))
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !native.exited().unwrap() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .expect("the same captured Native retires with its original runner");
                assert!(!control.is_alive(identity));
                assert!(control
                    .signals()
                    .iter()
                    .any(|(owner, _)| *owner == identity));
                assert!(control
                    .signals()
                    .iter()
                    .all(|(owner, _)| *owner == identity));
                let scope_probes = control.scopes();
                assert!(scope_probes.iter().all(|scope| scope == &original_scope));
                // An actual unscoped birth grants no foreign unit retirement.
                // Owned-unit release policy has separate pure lifecycle coverage.
                assert!(!supervisor.is_owned(&id).await);
                assert_eq!(
                    peer.requests(),
                    methods,
                    "cancellation must not submit Native work"
                );
                original
                    .shutdown()
                    .await
                    .expect("checked original checkpoint retirement");
            })
        })
        .await;
    }

    /// Reattach the actual retained runner after its Native published an
    /// unresolved background launch. Trackable launches carry the witness
    /// minted by their original transcript I/O; untrackable launches do not.
    async fn attach_with_orphaned_background_agent(
        scope: &mut super::super::test_support::managed::ManagedScope,
        label: &str,
        output_file: &str,
        open_task: Option<&str>,
        failed_main: bool,
    ) -> AttachBackgroundAgentFixture {
        let mut fixture =
            managed_background_original(scope, label, output_file, open_task, failed_main).await;
        let original = fixture.client.actual().connected_runner();
        let methods: Vec<_> = fixture
            .peer
            .requests()
            .into_iter()
            .filter(|method| {
                matches!(
                    method.as_str(),
                    "session/new" | "session/load" | "session/prompt"
                )
            })
            .collect();
        fixture.client = scope.adopt(fixture.client, false).await;
        fixture.client.ready().await;
        assert_eq!(fixture.client.actual().connected_runner(), original);
        assert_eq!(
            fixture
                .peer
                .requests()
                .into_iter()
                .filter(|method| matches!(
                    method.as_str(),
                    "session/new" | "session/load" | "session/prompt"
                ))
                .collect::<Vec<_>>(),
            methods,
            "read-only attachment must not create, load, or resubmit Native work"
        );
        fixture
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reattached_failed_main_keeps_its_terminal_through_ambient_accounting_and_background_work(
    ) {
        super::super::test_support::managed::with_managed(|scope| Box::pin(async move {
                let dir = scope.tempdir();
                let transcript = dir.path().join("agent.jsonl");
                std::fs::write(&transcript, "").unwrap();
                let mut fixture = managed_background_original(scope, "retained-failed-background", transcript.to_str().unwrap(), Some("surviving-task"), true).await;
                let failed = fold_background_journal(&fixture.store, &fixture.client.id);
                assert!(!failed.turn_active);
                assert_eq!(failed.sidebar_status(), crate::session::Status::Error);
                let original = fixture.client.actual().connected_runner();
                let methods = fixture.peer.requests();
                fixture.client = scope.adopt(fixture.client, false).await;
                fixture.client.ready().await;
                assert_eq!(fixture.client.actual().connected_runner(), original);
                assert_eq!(fixture.peer.requests(), methods);
                let sid = fixture.client.actual().establishment_projection().unwrap()
                    .receipt_snapshot().unwrap().session_id().unwrap().to_owned();
                fixture.peer.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":sid,"update":{"sessionUpdate":"usage_update","used":300,"size":200000}
                }})).await;
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !matches!(fixture.client.recv().await.expect("retained accounting observer remains open"),
                        Event::UsageUpdated { .. }) {}
                }).await.expect("actual ambient accounting reaches the retained SDK");
                let state = fold_background_journal(&fixture.store, &fixture.client.id);
                assert_eq!(state.sidebar_phase, failed.sidebar_phase);
                assert!(!state.turn_active && state.has_active_background_agent());
                let terminals = fixture.store.replay_from(&fixture.client.id, 0).unwrap();
                assert_eq!(terminals.iter().filter(|stored| matches!(stored.event.as_ref(), Event::Stopped { .. })).count(), 1);
                fixture.client.send_prompt("next explicit input", &[]).await.unwrap();
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !matches!(fixture.client.recv().await.expect("retained prompt observer remains open"),
                        Event::Stopped { reason } if reason == "prompt_complete") {}
                }).await.expect("a distinct explicit prompt completes on the same Native");
                assert_eq!(fixture.client.actual().connected_runner(), original);
                assert_eq!(fixture.peer.requests().iter().filter(|method| method.as_str() == "session/new").count(), 1);
                assert!(!fixture.peer.requests().iter().any(|method| method == "session/load"));
                fixture.client.shutdown().await.expect("checked original retirement and checkpoint cleanup");
            })).await;
    }

    async fn await_background_agent_resolved(
        fixture: &mut AttachBackgroundAgentFixture,
        within: std::time::Duration,
        what: &str,
    ) -> crate::acp::state::AcpState {
        tokio::time::timeout(within, async {
            while !fixture
                .store
                .unresolved_background_agent_ids(&fixture.client.id)
                .is_empty()
            {
                fixture
                    .client
                    .recv()
                    .await
                    .expect("the actual attached observer remains open");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{what}"));
        fold_background_journal(&fixture.store, &fixture.client.id)
    }

    /// The primary daemon-restart path: the worker `attach` just reached is
    /// provably alive, so an orphaned launch with a real transcript resumes
    /// tailing instead of being eagerly detached, and the tailer picks up the
    /// `end_turn` record already on disk (#4029).
    #[tokio::test]
    #[serial_test::serial]
    async fn attach_resumes_tailing_a_background_agent_that_survived_the_restart() {
        super::super::test_support::managed::with_managed(|scope| Box::pin(async move {
                let dir = scope.tempdir();
                let transcript = dir.path().join("agent.jsonl");
                std::fs::write(&transcript, "").unwrap();
                let mut fixture = managed_background_original(scope, "attach-survivor", transcript.to_str().unwrap(), None, false).await;
                let original = fixture.client.actual().connected_runner();
                let methods: Vec<_> = fixture.peer.requests().into_iter().filter(|method|
                    matches!(method.as_str(), "session/new" | "session/load" | "session/prompt")).collect();
                fixture.client = scope.adopt_with(fixture.client, false, || async {
                    std::fs::write(&transcript,
                        "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\",\"content\":[{\"type\":\"text\",\"text\":\"final answer\"}]}}\n").unwrap();
                }).await;
                fixture.client.ready().await;
                let state = await_background_agent_resolved(&mut fixture, std::time::Duration::from_secs(10), "resumed tailer must observe the actual resource outcome").await;
                assert_eq!(state.background_agents[0].status, crate::acp::state::BackgroundAgentStatus::Completed);
                assert!(state.background_agents[0].warning.is_none(), "no synthetic Detached may precede Completed");
                assert_eq!(fixture.client.actual().connected_runner(), original);
                assert_eq!(fixture.peer.requests().into_iter().filter(|method|
                    matches!(method.as_str(), "session/new" | "session/load" | "session/prompt")).collect::<Vec<_>>(), methods);
                fixture.client.shutdown().await.expect("checked original retirement and checkpoint cleanup");
            })).await;
    }

    /// The untrackable counterpart: a launch with no transcript path can never
    /// be resumed, so `attach` still detaches it eagerly, same as `spawn`.
    #[tokio::test]
    #[serial_test::serial]
    async fn attach_still_detaches_a_background_agent_with_no_transcript_path() {
        super::super::test_support::managed::with_managed(|scope| {
            Box::pin(async move {
                let mut fixture = attach_with_orphaned_background_agent(
                    scope,
                    "attach-untrackable",
                    "",
                    None,
                    false,
                )
                .await;
                let state = await_background_agent_resolved(
                    &mut fixture,
                    std::time::Duration::from_secs(10),
                    "untrackable launch must be detached without inventing resource authority",
                )
                .await;
                assert_eq!(
                    state.background_agents[0].status,
                    crate::acp::state::BackgroundAgentStatus::Detached
                );
                assert!(state.background_agents[0].resource_ref.is_none());
                fixture
                    .client
                    .shutdown()
                    .await
                    .expect("checked original retirement and checkpoint cleanup");
            })
        })
        .await;
    }

    /// A task left open when the daemon restarted may have ended meanwhile:
    /// `attach` resumes its transcript tailer, which reads the end already on
    /// disk instead of leaving the session busy for good.
    #[tokio::test]
    #[serial_test::serial]
    async fn attach_resumes_tailing_a_background_task_that_ended_during_the_restart() {
        super::super::test_support::managed::with_managed(|scope| Box::pin(async move {
                let mut fixture = managed_background_original(scope, "attach-task", "", Some("task-1"), false).await;
                assert_eq!(fixture.store.unresolved_background_task_ids(&fixture.client.id), ["task-1"]);
                let original = fixture.client.actual().connected_runner();
                let methods: Vec<_> = fixture.peer.requests().into_iter().filter(|method|
                    matches!(method.as_str(), "session/new" | "session/load" | "session/prompt")).collect();
                let transcript = fixture.task_transcript.clone();
                fixture.client = scope.adopt_with(fixture.client, false, || async {
                    std::fs::write(&transcript,
                        "{\"type\":\"queue-operation\",\"operation\":\"enqueue\",\"content\":\"<task-notification>\\n<task-id>task-1</task-id>\\n<tool-use-id>toolu_task</tool-use-id>\\n<status>completed</status>\\n</task-notification>\"}\n").unwrap();
                }).await;
                fixture.client.ready().await;
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    while !fixture.store.unresolved_background_task_ids(&fixture.client.id).is_empty() {
                        fixture.client.recv().await.expect("real attached observer remains open");
                    }
                }).await.expect("resumed first-I/O-backed task tailer observes the retained completion");
                assert_eq!(fixture.client.actual().connected_runner(), original);
                assert_eq!(fixture.peer.requests().into_iter().filter(|method|
                    matches!(method.as_str(), "session/new" | "session/load" | "session/prompt")).collect::<Vec<_>>(), methods);
                fixture.client.shutdown().await.expect("checked original retirement and checkpoint cleanup");
            })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn attach_terminates_a_worker_whose_agent_is_no_longer_allowed() {
        super::super::test_support::managed::with_managed(|scope| {
            Box::pin(async move {
                crate::session::config::update_config(|config| {
                    config.acp.restrict_agents = true;
                    config.acp.allowed_agents = vec!["claude".into(), "codex".into()];
                })
                .unwrap();
                let directory = scope.tempdir();
                let (config, peer) = scope.native_peer(
                    directory.path().to_path_buf(),
                    "codex",
                    "codex",
                    |request| {
                        let id = request.get("id")?;
                        let result = match request["method"].as_str()? {
                            "initialize" => {
                                serde_json::json!({"protocolVersion":1,"agentCapabilities":{}})
                            }
                            "session/new" => {
                                serde_json::json!({"sessionId":"policy-original-native"})
                            }
                            _ => return None,
                        };
                        Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}))
                    },
                );
                let original = scope
                    .spawn(config, "policy-original")
                    .await
                    .expect("permitted agent establishes its actual Native");
                original.ready().await;
                let identity = original.actual().connected_runner().unwrap();
                let birth = crate::process::process_birth_identity(identity.pid)
                    .unwrap()
                    .expect("original runner is alive");
                assert_eq!(Some(birth.fingerprint()), identity.birth_fingerprint);
                let runner = crate::process::CapturedProcess::capture(identity.pid, &birth)
                    .unwrap()
                    .expect("capture exact original runner");
                let requests = peer.requests();
                let original_supervisor = scope.state.acp_supervisor.clone();
                original_supervisor.detach_all().await;
                let supervisor = Supervisor::new(original_supervisor.sink.clone())
                    .with_native_admission();
                crate::session::config::update_config(|config| {
                    config.acp.allowed_agents = vec!["claude".into()];
                })
                .unwrap();
                let result = supervisor
                    .attach(
                        original.id.clone(),
                        directory.path().to_path_buf(),
                        Vec::new(),
                        false,
                        None,
                    )
                    .await;
                assert!(
                    matches!(&result, Err(SupervisorError::AgentNotAllowed(name)) if name == "codex"),
                    "expected AgentNotAllowed(codex), got {result:?}"
                );
                assert!(
                    worker_registry::load(&original.id).unwrap().is_none(),
                    "the disallowed original's registry record must be cleared"
                );
                assert!(runner.exited().unwrap(), "the exact disallowed runner must exit");
                assert_eq!(
                    peer.requests(),
                    requests,
                    "policy denial must not create, load, or resubmit Native work"
                );
                original.stop_and_forget().await;
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn borrowed_attach_timeout_and_drop_preserve_pending_native_request_for_retry() {
        super::super::test_support::managed::with_managed(|scope| Box::pin(async move {
            for drop_waiter in [false, true] {
                let directory = scope.tempdir();
                let (request_tx, request_rx) = tokio::sync::oneshot::channel();
                let mut request_tx = Some(request_tx);
                let (config, peer) = scope.native_peer(directory.path().into(), "gemini", "gemini", move |request| {
                    let id = request.get("id")?;
                    let result = match request["method"].as_str()? {
                        "initialize" => serde_json::json!({"protocolVersion":1,"agentCapabilities":{}}),
                        "session/new" => serde_json::json!({"sessionId":"borrowed-pending-native"}),
                        "session/prompt" => {
                            request_tx.take().unwrap().send(id.clone()).unwrap();
                            return None;
                        }
                        _ => return None,
                    };
                    Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}))
                });
                let original = scope.spawn(config, "borrowed-pending-retry").await.unwrap();
                original.ready().await;
                let id = original.id.clone();
                let original_supervisor = scope.state.acp_supervisor.clone();
                let store = scope.state.acp_event_store.clone();
                let mut frames = scope.state.acp_events_tx.subscribe();
                original_supervisor.publish_user_prompt(&id, "original pending input".into()).await;
                original.send_prompt("original pending input", &[]).await.unwrap();
                let request_id = tokio::time::timeout(Duration::from_secs(10), request_rx).await.unwrap().unwrap();
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let frame = frames.recv().await.unwrap();
                        if frame.session_id == id && matches!(frame.event.as_ref(), Event::ThinkingStarted) { break; }
                    }
                }).await.expect("actual original admission is durable");
                let prior = store.has_in_flight_turn(&id).expect("durable original ownership");
                let identity = original.actual().connected_runner().expect("actual original runner");
                let birth = crate::process::process_birth_identity(identity.pid).unwrap().expect("owned runner is alive");
                assert_eq!(Some(birth.fingerprint()), identity.birth_fingerprint);
                let (gate, release, entered) = scope.gate();
                let mut attached = scope.adopt_with(original, true, || async {
                let supervisor = Arc::new(
                    Supervisor::new(original_supervisor.sink.clone()).with_native_admission(),
                );
                let registry_before = runner_record(&id).await.expect("actual original registry owner");
                let reservation = reserve(supervisor.begin_resume(
                    &id,
                    ResumeKind::Attach,
                    worker_registry::ResumeAuthority::Automatic,
                ).await);
                let deadline = reservation.registry_deadline;
                assert!(reservation.lease().is_borrowed_attach());
                assert_eq!(reservation.lease().spawned_runner(), Some(identity));
                assert_eq!(
                    lock_recover(&supervisor.lifecycle).phase(&id),
                    crate::acp::runner_lifecycle::WorkerPhase::Resuming,
                );
                let attaching = tokio::spawn({
                    let supervisor = supervisor.clone();
                    let id = id.clone();
                    let cwd = directory.path().to_path_buf();
                    async move {
                        AcpClient::with_sdk_test_options(
                            crate::acp::acp_client::SdkTestOptions {
                                initialize_response_application: Some(gate),
                                ..Default::default()
                            },
                            supervisor.attach_inner(id, cwd, Vec::new(), true, None, reservation),
                        ).await
                    }
                });
                tokio::time::timeout_at(deadline, entered)
                    .await
                    .expect("actual borrowed cached initialize reaches its pre-ready application boundary")
                    .unwrap();
                if drop_waiter {
                    attaching.abort();
                    assert!(attaching.await.unwrap_err().is_cancelled());
                } else {
                    let result = tokio::time::timeout(Duration::from_secs(10), attaching).await.unwrap().unwrap();
                    assert!(matches!(result, Err(SupervisorError::Acp(AcpError::AttachTimedOut))),
                        "held actual initialize must reach its original attach deadline, got {result:?}");
                }
                release.release();
                assert!(!supervisor.is_owned(&id).await,
                    "failed borrowed reservation releases without retiring the Native");
                let registry_after = runner_record(&id).await.expect("same original survives failed observation");
                assert!(identity.owns_record(&registry_after));
                assert_eq!(registry_after.scope, registry_before.scope);
                assert_eq!(registry_after.stored_acp_session_id, registry_before.stored_acp_session_id);
                assert_eq!(crate::process::process_birth_identity(identity.pid).unwrap().unwrap().fingerprint(), birth.fingerprint());
                assert_eq!(store.has_in_flight_turn(&id).expect("ownership after failed observer"), prior);
                assert!(!store.replay_from(&id, 0).unwrap().iter().any(|stored| matches!(stored.event.as_ref(), Event::Stopped { .. } | Event::SessionContextReset { .. })));
                }).await;
                attached.ready().await;
                assert_eq!(attached.actual().connected_runner(), Some(identity));
                peer.send(serde_json::json!({"jsonrpc":"2.0","id":request_id,"result":{"stopReason":"end_turn"}})).await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !matches!(attached.recv().await.expect("reattached original prompt observer remains open"),
                        Event::Stopped { reason } if reason == "prompt_complete") {}
                }).await.expect("the same original Native RPC completes after actual retry");
                assert_eq!(peer.requests().iter().filter(|method| method.as_str() == "session/new").count(), 1);
                assert_eq!(peer.requests().iter().filter(|method| method.as_str() == "session/prompt").count(), 1);
                assert!(!peer.requests().iter().any(|method| matches!(method.as_str(), "session/load" | "session/cancel")));
                let events = store.replay_from(&id, 0).unwrap();
                assert_eq!(events.iter().filter(|stored| matches!(stored.event.as_ref(), Event::Stopped { reason } if reason == "prompt_complete")).count(), 1);
                attached.stop_and_forget().await;
            }
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn captured_stop_retires_original_across_attach_deadline_and_drop_not_replacement() {
        use crate::acp::runner_lifecycle::ProcessControl as _;

        for (stop_before_deadline, drop_waiter) in [(true, false), (false, false), (false, true)] {
            super::super::test_support::managed::with_managed(|scope| {
                Box::pin(async move {
                    let directory = scope.tempdir();
                    let test_name = std::thread::current().name().unwrap().to_owned();
                    let inspection = scope.inspect_next_runner(&test_name);
                    let (request_tx, request_rx) = tokio::sync::oneshot::channel();
                    let mut request_tx = Some(request_tx);
                    let (config, peer) = scope.native_peer(directory.path().into(), "gemini", "gemini", move |request| {
                        let result = match request["method"].as_str()? {
                            "initialize" => serde_json::json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true}}),
                            "session/new" => serde_json::json!({"sessionId":"original-stop-native"}),
                            "session/prompt" => {
                                request_tx.take().unwrap().send(request["id"].clone()).unwrap();
                                return None;
                            }
                            _ => return None,
                        };
                        Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
                    });
                    let mut original = scope.spawn(config, "borrowed-attach-stop-race").await.unwrap();
                    original.ready().await;
                    inspection.query("observe-producer").await;
                    let id = original.id.clone();
                    let identity = original.actual().connected_runner().unwrap();
                    let proof = inspection.native_spawn_proof(&original.lease).unwrap().unwrap();
                    let native = crate::process::CapturedProcess::capture(proof.pid, &proof.birth)
                        .unwrap().expect("actual original Native incarnation");
                    let selected = original.actual().establishment_projection().unwrap()
                        .receipt_snapshot().unwrap().session_id().unwrap().to_owned();
                    let store = scope.state.acp_event_store.clone();
                    let mut frames = scope.state.acp_events_tx.subscribe();
                    scope.state.acp_supervisor.publish_user_prompt(&id, "original pending input".into()).await;
                    original.send_prompt("original pending input", &[]).await.unwrap();
                    let request_id = tokio::time::timeout(Duration::from_secs(10), request_rx).await.unwrap().unwrap();
                    tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let frame = frames.recv().await.unwrap();
                            if frame.session_id == id && matches!(frame.event.as_ref(), Event::ThinkingStarted) { break; }
                        }
                    }).await.expect("actual original admission is durable");
                    let pending = store.has_in_flight_turn(&id).expect("actual original pending turn");
                    assert_eq!(pending, crate::acp::event_store::ControlKnowledge::Known(true));
                    let issued = tokio::time::timeout(Duration::from_secs(10), async {
                        loop {
                            let snapshot = inspection.query("producer-snapshot").await;
                            if let Some(call) = snapshot["pending"].as_array().unwrap().iter()
                                .find(|call| call["id"] == request_id && call["progress"] == "complete")
                            {
                                break call.clone();
                            }
                            tokio::task::yield_now().await;
                        }
                    }).await.expect("actual original Native call remains pending after its complete write");
                    let methods = peer.requests();
                    let original_row = crate::session::Storage::open_unwatched("test").unwrap()
                        .load_instances_complete().unwrap().into_iter().find(|row| row.id == id).unwrap();
                    scope.state.acp_supervisor.detach_all().await;
                    let record = runner_record(&id).await.expect("retained original record");
                    assert!(identity.owns_record(&record));
                    let control = Arc::new(ObservedProcessControl::default());
                    let supervisor = Arc::new(
                        Supervisor::new(scope.state.acp_supervisor.sink.clone())
                            .with_native_admission()
                            .with_process_control(control.clone()),
                    );
                    let reservation = reserve(supervisor.begin_resume(
                        &id, ResumeKind::Attach, worker_registry::ResumeAuthority::Automatic,
                    ).await);
                    assert!(reservation.lease().is_borrowed_attach());
                    let deadline = reservation.registry_deadline;
                    let target = tokio::time::timeout_at(deadline, async {
                        loop {
                            match supervisor.capture_retirement(&id) {
                                Ok(target) => break target,
                                Err(SupervisorError::RegistryBusy) => tokio::task::yield_now().await,
                                Err(error) => panic!("capturing original retirement: {error}"),
                            }
                        }
                    }).await.expect("capture original retirement within the actual attach deadline");
                    assert_eq!(target.identity, Some(identity));
                    assert_eq!(target.lease.as_ref(), Some(reservation.lease()));
                    assert!(target.scopes.contains(&record.scope));
                    let (gate, release, entered) = scope.gate();
                    let attaching = tokio::spawn({
                        let supervisor = supervisor.clone();
                        let id = id.clone();
                        let cwd = record.cwd.clone();
                        async move {
                            AcpClient::with_sdk_test_options(
                                crate::acp::acp_client::SdkTestOptions {
                                    initialize_response_application: Some(gate),
                                    ..Default::default()
                                },
                                supervisor.attach_inner(id, cwd, vec![], true, None, reservation),
                            ).await
                        }
                    });
                    tokio::time::timeout_at(deadline, entered).await
                        .expect("actual borrowed initialize reaches its application boundary").unwrap();
                    assert_eq!(store.has_in_flight_turn(&id).unwrap(), pending);
                    let during = inspection.query("producer-snapshot").await;
                    assert!(during["pending"].as_array().unwrap().contains(&issued),
                        "real borrowed initialize cannot replace the pending original call");
                    if stop_before_deadline {
                        supervisor.shutdown(&id).await.unwrap();
                        release.release();
                        assert!(tokio::time::timeout(Duration::from_secs(10), attaching)
                            .await.unwrap().unwrap().is_err());
                    } else {
                        if drop_waiter {
                            attaching.abort();
                            assert!(attaching.await.unwrap_err().is_cancelled());
                        } else {
                            let result = tokio::time::timeout(Duration::from_secs(10), attaching)
                                .await.unwrap().unwrap();
                            assert!(matches!(result, Err(SupervisorError::Acp(AcpError::AttachTimedOut))),
                                "held actual initialize must reach its attach deadline: {result:?}");
                        }
                        release.release();
                        assert!(control.is_alive(identity), "failed borrowed observation cannot retire its Original");
                        assert!(!native.exited().unwrap());
                        let after = runner_record(&id).await.expect("same original survives failed observation");
                        assert!(identity.owns_record(&after));
                        assert_eq!(after.scope, record.scope);
                        assert_eq!(store.has_in_flight_turn(&id).unwrap(), pending);
                        assert_eq!(peer.requests(), methods, "failed observation cannot resubmit the original RPC");
                        original = scope.attach_retained_parent(
                            original, original_row, Default::default(), true,
                        ).await;
                        original.ready().await;
                        assert_eq!(original.actual().connected_runner(), Some(identity));
                        peer.send(serde_json::json!({"jsonrpc":"2.0","id":request_id,
                            "result":{"stopReason":"end_turn"}})).await;
                        tokio::time::timeout(Duration::from_secs(10), async {
                            while !matches!(original.recv().await.expect("same original survives real read-only retry"),
                                Event::Stopped { reason } if reason == "prompt_complete") {}
                        }).await.expect("same pending Native RPC completes after borrowed deadline or drop");
                        assert_eq!(peer.requests(), methods, "real read-only retry never resubmits Native work");
                    }
                    original.stop_session().await;
                    let stopped = crate::session::Storage::open_unwatched("test").unwrap()
                        .load_instances_complete().unwrap().into_iter().find(|row| row.id == id).unwrap();
                    assert_eq!(stopped.status, crate::session::Status::Stopped);
                    assert_eq!(stopped.acp_session_id.as_deref(), Some(selected.as_str()));
                    let retired = supervisor.shutdown_captured_and_wait(&id, &target, Duration::from_secs(5)).await;
                    assert!(matches!(retired, Ok(()) | Err(SupervisorError::TeardownPending(_))),
                        "public Stop has retired the original; an abandoned observer may retain only its own debt: {retired:?}");
                    assert!(!control.is_alive(identity));
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while !native.exited().unwrap() {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }).await.expect("public Stop retires the exact original Native before replacement");
                    if stop_before_deadline {
                        assert!(control.signals().iter().any(|(owner, _)| *owner == identity));
                    }
                    assert!(control.signals().iter().all(|(owner, _)| *owner == identity));
                    assert!(control.scopes().iter().all(|scope| scope == &record.scope));

                    // Only explicit Stop followed by retained Start may create the replacement.
                    // The replacement is an actual Load actor, not a forged registry owner.
                    let (replacement_tx, replacement_rx) = tokio::sync::oneshot::channel();
                    let mut replacement_tx = Some(replacement_tx);
                    let retained = selected.clone();
                    let (config, replacement_peer) = scope.native_peer(directory.path().into(), "gemini", "gemini", move |request| {
                        let result = match request["method"].as_str()? {
                            "initialize" => serde_json::json!({"protocolVersion":1,"agentCapabilities":{"loadSession":true}}),
                            "session/load" => {
                                assert_eq!(request["params"]["sessionId"], retained);
                                serde_json::json!({})
                            }
                            "session/prompt" => {
                                replacement_tx.take().unwrap().send(request["id"].clone()).unwrap();
                                return None;
                            }
                            "session/new" => panic!("retained replacement must not create a new conversation"),
                            _ => return None,
                        };
                        Some(serde_json::json!({"jsonrpc":"2.0","id":request["id"],"result":result}))
                    });
                    let mut replacement = scope.resume_stopped(&id, config).await;
                    replacement.ready().await;
                    let replacement_client = replacement.actual();
                    let replacement_identity = replacement_client.connected_runner().unwrap();
                    assert_ne!(replacement_identity, identity);
                    assert_eq!(replacement_client.establishment_projection().unwrap()
                        .receipt_snapshot().unwrap().session_id(), Some(selected.as_str()));
                    let replacement_supervisor = replacement.app_state().acp_supervisor.clone();
                    replacement_supervisor.publish_user_prompt(&id, "replacement continuation".into()).await;
                    replacement.send_prompt("replacement continuation", &[]).await.unwrap();
                    let replacement_request = tokio::time::timeout(Duration::from_secs(10), replacement_rx)
                        .await.unwrap().unwrap();
                    let prefix = store.highest_seq(&id);
                    let replacement_pending = store.has_in_flight_turn(&id).unwrap();
                    assert_eq!(replacement_pending, crate::acp::event_store::ControlKnowledge::Known(true));
                    let record_path = worker_registry::record_path(&id).unwrap();
                    let replacement_record = std::fs::read(&record_path).unwrap();
                    let late = replacement_supervisor.shutdown_captured_and_wait(&id, &target, Duration::from_secs(5)).await;
                    assert!(matches!(late, Ok(()) | Err(SupervisorError::TeardownPending(_))),
                        "old captured retirement must not target another installed owner: {late:?}");
                    replacement_supervisor.test_assert_installed_client(&id, &replacement_client).await;
                    assert!(control.is_alive(replacement_identity));
                    assert_eq!(std::fs::read(&record_path).unwrap(), replacement_record);
                    assert_eq!(store.has_in_flight_turn(&id).unwrap(), replacement_pending);
                    assert!(!store.replay_from(&id, prefix).unwrap().iter()
                        .any(|stored| matches!(stored.event.as_ref(), Event::Stopped { .. })));
                    replacement_peer.send(serde_json::json!({"jsonrpc":"2.0","id":replacement_request,
                        "result":{"stopReason":"end_turn"}})).await;
                    tokio::time::timeout(Duration::from_secs(10), async {
                        while !matches!(replacement.recv().await.expect("actual replacement remains attached"),
                            Event::Stopped { reason } if reason == "prompt_complete") {}
                    }).await.expect("same replacement Native request completes after the stale captured Stop");
                    assert_eq!(replacement_peer.requests().iter().filter(|method| method.as_str() == "session/load").count(), 1);
                    assert_eq!(replacement_peer.requests().iter().filter(|method| method.as_str() == "session/prompt").count(), 1);
                    assert!(!replacement_peer.requests().iter().any(|method| method == "session/new"));
                    assert_eq!(peer.requests(), methods);
                    replacement.stop_and_forget().await;
                })
            }).await;
        }
    }
}
