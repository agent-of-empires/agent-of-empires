//! Stopping workers: shutdown, reaping user stops, and proving runners dead.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::{lock_recover, BroadcastSink, Supervisor, SupervisorError, WorkerHandle, WorkerKind};
use crate::acp::acp_client::{AcpClient, DeleteSessionOutcome};
use crate::acp::runner_lifecycle::{
    Lease, LifecycleTable, PrimeOwnership, ProcessControl, RunnerIdentity, Settlement, StopDecision,
};
use crate::acp::state::{BackgroundAgentStatus, Event};
use crate::process::worker_registry;

/// Grace after SIGTERM before SIGKILL, and after SIGKILL before parking the teardown.
const TEARDOWN_TERM_GRACE: Duration = Duration::from_secs(2);
const TEARDOWN_KILL_GRACE: Duration = Duration::from_millis(500);
const TEARDOWN_POLL: Duration = Duration::from_millis(50);
/// Retries after which a dead runner whose record cannot be settled is released anyway.
const TEARDOWN_RETRY_CAP: u32 = 30;
/// A teardown claimed this long without settling lost its driver; the retry pass takes over.
const TEARDOWN_ORPHAN_GRACE: Duration = Duration::from_secs(15);

type RetiringDrain = Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>;

struct RetiringClient {
    client: Arc<AcpClient>,
    close_requested: std::sync::atomic::AtomicBool,
    close_done: std::sync::atomic::AtomicBool,
    shutdown_done: std::sync::atomic::AtomicBool,
}

/// Original connections and their publisher stay owned across request cancellation.
pub(crate) struct RetirementResources {
    gate: tokio::sync::Mutex<()>,
    clients: std::sync::Mutex<[Option<Arc<RetiringClient>>; 2]>,
    drain: std::sync::Mutex<Option<RetiringDrain>>,
    emit_events: std::sync::atomic::AtomicBool,
}

impl Default for RetirementResources {
    fn default() -> Self {
        Self {
            gate: tokio::sync::Mutex::new(()),
            clients: std::sync::Mutex::new([None, None]),
            drain: std::sync::Mutex::new(None),
            emit_events: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl RetirementResources {
    pub(super) fn record_client(&self, client: &Arc<AcpClient>, close: bool) {
        use std::sync::atomic::Ordering;
        let mut clients = lock_recover(&self.clients);
        if let Some(existing) = clients
            .iter()
            .flatten()
            .find(|entry| Arc::ptr_eq(&entry.client, client))
        {
            if close {
                existing.close_requested.store(true, Ordering::Release);
            }
            return;
        }
        let slot = clients
            .iter_mut()
            .find(|slot| slot.is_none())
            .expect("one construction retains only its original and refused replacement client");
        *slot = Some(Arc::new(RetiringClient {
            client: Arc::clone(client),
            close_requested: std::sync::atomic::AtomicBool::new(close),
            close_done: std::sync::atomic::AtomicBool::new(false),
            shutdown_done: std::sync::atomic::AtomicBool::new(false),
        }));
    }

    pub(super) fn record_worker(&self, handle: WorkerHandle) {
        if is_test_worker(&handle) {
            self.emit_events
                .store(false, std::sync::atomic::Ordering::Release);
        }
        self.record_client(&handle.client, true);
        let mut slot = lock_recover(&self.drain);
        assert!(
            slot.is_none(),
            "one worker handle may retire only once per construction"
        );
        *slot = Some(Arc::new(tokio::sync::Mutex::new(Some(handle.drain_task))));
    }

    pub(crate) fn clear(&self) {
        *lock_recover(&self.clients) = [None, None];
        lock_recover(&self.drain).take();
    }
}

#[derive(Clone, Copy)]
pub(super) enum CompletionPublisher {
    JoinDrain,
    CurrentDrain,
}

async fn quiesce_retained_drain(lease: &Lease) {
    let drain = lock_recover(&lease.retirement().drain).clone();
    if let Some(drain) = drain {
        let mut slot = drain.lock().await;
        if let Some(task) = slot.as_mut() {
            task.abort();
            let _ = task.await;
            slot.take();
        }
    }
}

async fn retire_registered_clients_locked(
    lifecycle: &std::sync::Mutex<LifecycleTable>,
    lease: &Lease,
) {
    use std::sync::atomic::Ordering;
    service_adapter_delete_debt(lifecycle, lease).await;
    let clients = lock_recover(&lease.retirement().clients).clone();
    for owned in clients.into_iter().flatten() {
        if owned.close_requested.load(Ordering::Acquire)
            && !owned.close_done.load(Ordering::Acquire)
        {
            let close = owned.client.close_session().await;
            if matches!(
                close,
                crate::acp::acp_client::CloseSessionOutcome::Failed(_)
                    | crate::acp::acp_client::CloseSessionOutcome::TimedOut
            ) {
                warn!(target: "acp.supervisor", session = lease.session_id(), ?close,
                    "session close failed; continuing owned runtime cleanup");
            }
            owned.close_done.store(true, Ordering::Release);
        }
        service_adapter_delete_debt(lifecycle, lease).await;
        if !owned.shutdown_done.load(Ordering::Acquire) {
            if let Err(error) = owned.client.shutdown().await {
                warn!(target: "acp.supervisor", session = lease.session_id(), %error,
                    "client shutdown failed; frozen physical ownership must still settle");
            }
            owned.shutdown_done.store(true, Ordering::Release);
        }
        mark_adapter_route_retired(lease);
        service_adapter_delete_debt(lifecycle, lease).await;
    }
}

pub(super) async fn retire_registered_clients(
    lifecycle: &std::sync::Mutex<LifecycleTable>,
    lease: &Lease,
) {
    let _driver = lease.retirement().gate.lock().await;
    retire_registered_clients_locked(lifecycle, lease).await;
}

fn finish_stop_completion<S: BroadcastSink>(sink: &S, seqs: &super::SeqMap, lease: &Lease) -> bool {
    let Some(debt) = lease.pending_stop_completion() else {
        return true;
    };
    if lease
        .retirement()
        .emit_events
        .load(std::sync::atomic::Ordering::Acquire)
    {
        let id = lease.session_id();
        for task_id in sink.unresolved_background_task_ids(id) {
            if !sink.publish_persisted(
                id,
                super::next_seq(seqs, id),
                &Event::BackgroundTaskFinished { task_id },
            ) {
                return false;
            }
        }
        for agent_id in sink.unresolved_background_agent_ids(id) {
            let event = Event::BackgroundAgentCompleted {
                agent_id,
                status: BackgroundAgentStatus::Detached,
                tools: Vec::new(),
                result: None,
                warning: None,
                ended_at: chrono::Utc::now(),
            };
            if !sink.publish_persisted(id, super::next_seq(seqs, id), &event) {
                return false;
            }
        }
        if let Some(reason) = debt.reason {
            if !sink.publish_persisted(id, super::next_seq(seqs, id), &Event::Stopped { reason }) {
                return false;
            }
        }
    }
    lease.stop_completion_done();
    true
}

pub(super) async fn tear_down_lease(
    control: &dyn ProcessControl,
    lease: &Lease,
    killed_before: bool,
) -> Settlement {
    if lease.is_borrowed_attach() {
        return Settlement::Unproven(lease.spawned_runner());
    }
    if lease.was_cancelled_before_birth() && lease.retirement_runners().next().is_none() {
        // Birth exclusion proves this constructor never executed. An unrelated
        // registry replacement cannot invalidate that proof; scopes settle below.
        return Settlement::Proven;
    }
    // Only the owner of the original unreaped Child can fill a missing birth.
    let _ = lease.attest_runner().await;
    // A stop can add the captured predecessor while the current runner awaits exit.
    // Its owned Child can also certify a weak current role during another await.
    let mut current_attempted = None;
    let mut previous_attempted = false;
    let mut combined = Settlement::Proven;
    loop {
        let current = lease.spawned_runner();
        let identity = match current.filter(|identity| Some(*identity) != current_attempted) {
            Some(identity) => {
                current_attempted = Some(identity);
                identity
            }
            None if !previous_attempted => {
                let Some(previous) = lease.retirement_previous_runner() else {
                    break;
                };
                previous_attempted = true;
                if Some(previous) == current_attempted {
                    continue;
                }
                previous
            }
            None => break,
        };
        let result = if identity.birth_fingerprint.is_none() {
            // No numeric PID/group signal is authorized by missing birth evidence.
            Settlement::Unproven(Some(identity))
        } else {
            tear_down_runner_from(control, lease.session_id(), Some(identity), killed_before).await
        };
        if combined == Settlement::Proven && result != Settlement::Proven {
            combined = result;
        }
    }
    if current_attempted.is_none() && !previous_attempted {
        return tear_down_runner_from(control, lease.session_id(), None, killed_before).await;
    }
    combined
}

pub(super) fn retain_worker(lease: &Lease, handle: WorkerHandle, identity: Option<RunnerIdentity>) {
    capture_adapter_route(lease, &handle.client, identity);
    lease.retirement().record_worker(handle);
}

/// Frozen retirement authority, captured before committed removal releases exclusion.
#[derive(Clone)]
pub(crate) struct RetirementTarget {
    pub(crate) lease: Option<Lease>,
    pub(crate) identity: Option<RunnerIdentity>,
    pub(crate) scopes: Vec<worker_registry::ScopeOwnership>,
    pub(crate) prime: PrimeOwnership,
    pub(crate) journalled: bool,
}

impl<S: BroadcastSink> Supervisor<S> {
    pub(crate) fn capture_handoff_retirement(
        &self,
        session_id: &str,
        profile: &str,
        profile_identity: (u64, u64),
        source_generation: u64,
    ) -> Result<(RetirementTarget, bool), SupervisorError> {
        let target = self.capture_retirement(session_id)?;
        if target.identity.is_some() {
            return Ok((target, false));
        }
        let Some(lease) = target.lease.as_ref() else {
            return Ok((target, false));
        };
        if lease.was_cancelled_before_birth() {
            return Ok((target, false));
        }
        if !self.native_admission_required
            || !target.scopes.is_empty()
            || lease.previous_runner().is_some()
            || !lock_recover(&self.lifecycle).is_starting_spawn(lease)
        {
            return Err(SupervisorError::TeardownPending(session_id.to_owned()));
        }
        let Some(admission) = lease.admission() else {
            return Err(SupervisorError::TeardownPending(session_id.to_owned()));
        };
        let binding = admission.store.storage();
        let identity = binding
            .bound_profile_identity()
            .map_err(|_| SupervisorError::NativeStoreUnavailable)?;
        if binding.profile() != profile
            || identity != profile_identity
            || admission.generation > source_generation
        {
            return Err(SupervisorError::NativeStoreUnavailable);
        }
        Ok((target, true))
    }

    pub(crate) async fn complete_unowned_fenced_handoff(
        &self,
        session_id: &str,
        stopped_already_persisted: bool,
    ) -> Result<(), SupervisorError> {
        let workers = self.workers.lock().await;
        if !self.native_admission_required
            || workers.contains_key(session_id)
            || lock_recover(&self.lifecycle)
                .current_lease(session_id)
                .is_some()
        {
            return Err(SupervisorError::TeardownPending(session_id.to_owned()));
        }
        for task_id in self.sink.unresolved_background_task_ids(session_id) {
            if !self.sink.publish_persisted(
                session_id,
                super::next_seq(&self.next_seqs, session_id),
                &Event::BackgroundTaskFinished { task_id },
            ) {
                return Err(SupervisorError::TeardownPending(session_id.to_owned()));
            }
        }
        for agent_id in self.sink.unresolved_background_agent_ids(session_id) {
            if !self.sink.publish_persisted(
                session_id,
                super::next_seq(&self.next_seqs, session_id),
                &Event::BackgroundAgentCompleted {
                    agent_id,
                    status: BackgroundAgentStatus::Detached,
                    tools: Vec::new(),
                    result: None,
                    warning: None,
                    ended_at: chrono::Utc::now(),
                },
            ) {
                return Err(SupervisorError::TeardownPending(session_id.to_owned()));
            }
        }
        if !stopped_already_persisted
            && !self.sink.publish_persisted(
                session_id,
                super::next_seq(&self.next_seqs, session_id),
                &Event::Stopped {
                    reason: "user_stopped".to_owned(),
                },
            )
        {
            return Err(SupervisorError::TeardownPending(session_id.to_owned()));
        }
        Ok(())
    }
    pub(crate) fn capture_retirement(
        &self,
        session_id: &str,
    ) -> Result<RetirementTarget, SupervisorError> {
        let (lease, running_identity, pending_attach) = {
            let lifecycle = lock_recover(&self.lifecycle);
            let lease = lifecycle.current_lease(session_id);
            let pending_attach = lease
                .as_ref()
                .is_some_and(|lease| lifecycle.pending_attach(lease));
            (
                lease,
                lifecycle
                    .running(session_id)
                    .and_then(|(_, identity)| identity),
                pending_attach,
            )
        };
        let record = worker_registry::load_strict(session_id).map_err(|error| {
            SupervisorError::Acp(crate::acp::acp_client::AcpError::Spawn(error.to_string()))
        })?;
        let constructed = lease.as_ref().map(Lease::constructed_ownership);
        let identity = constructed
            .as_ref()
            .and_then(|(identity, _)| *identity)
            .or(running_identity)
            .or_else(|| {
                (!pending_attach)
                    .then(|| record.as_ref().map(RunnerIdentity::from_record))
                    .flatten()
            });
        let mut scopes = lease.as_ref().map_or_else(Vec::new, Lease::scopes);
        let mut prime = constructed.map(|(_, prime)| prime).unwrap_or_else(|| {
            if record.is_none() {
                PrimeOwnership::NotPrime
            } else {
                PrimeOwnership::Unresolved
            }
        });
        if let Some(record) = record
            .as_ref()
            .filter(|record| identity.is_some_and(|identity| identity.owns_record(record)))
        {
            let recorded = PrimeOwnership::from_record(record);
            if prime == PrimeOwnership::Unresolved {
                prime = recorded;
            } else if prime != recorded {
                return Err(SupervisorError::TeardownPending(session_id.into()));
            }
            if !scopes.contains(&record.scope) {
                scopes.push(record.scope.clone());
            }
        }
        Ok(RetirementTarget {
            lease,
            identity,
            scopes,
            prime,
            journalled: false,
        })
    }

    pub(crate) fn acknowledge_retirement(
        &self,
        target: &RetirementTarget,
    ) -> Result<(), SupervisorError> {
        if let Some(lease) = &target.lease {
            if !lock_recover(&self.lifecycle).acknowledge_retirement(lease) {
                return Err(SupervisorError::TeardownPending(lease.session_id().into()));
            }
        }
        Ok(())
    }

    pub(crate) async fn shutdown_captured_and_wait(
        &self,
        session_id: &str,
        target: &RetirementTarget,
        deadline: Duration,
    ) -> Result<(), SupervisorError> {
        self.shutdown_and_wait_with_policy(session_id, deadline, false, Some(target))
            .await
    }
    pub(crate) async fn shutdown_captured_and_delete_and_wait(
        &self,
        session_id: &str,
        target: &RetirementTarget,
        deadline: Duration,
    ) -> Result<(), SupervisorError> {
        self.shutdown_and_wait_with_policy(session_id, deadline, true, Some(target))
            .await
    }
    /// Stop a worker, keeping its agent-side transcript resumable.
    pub async fn shutdown(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.shutdown_with_reason(session_id, "user_stopped", false, None)
            .await
    }

    /// Stop a worker reclaimed for inactivity.
    pub async fn shutdown_idle(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.shutdown_with_reason(session_id, "idle_auto_stop", false, None)
            .await
    }

    /// Permanently discard a worker and wait for its matching retirement.
    /// Reversible stops must not delete the adapter-side transcript.
    pub async fn shutdown_and_delete_and_wait(
        &self,
        session_id: &str,
        deadline: Duration,
    ) -> Result<(), SupervisorError> {
        self.shutdown_and_wait_with_policy(session_id, deadline, true, None)
            .await
    }

    /// Wait for an in-flight launch and its teardown, not merely cancellation.
    pub async fn shutdown_and_wait(
        &self,
        session_id: &str,
        deadline: Duration,
    ) -> Result<(), SupervisorError> {
        self.shutdown_and_wait_with_policy(session_id, deadline, false, None)
            .await
    }

    async fn shutdown_and_wait_with_policy(
        &self,
        session_id: &str,
        deadline: Duration,
        delete_adapter_state: bool,
        expected: Option<&RetirementTarget>,
    ) -> Result<(), SupervisorError> {
        let record_before = if expected.is_none() {
            worker_registry::load_strict(session_id).map_err(|error| {
                SupervisorError::Acp(crate::acp::acp_client::AcpError::Spawn(error.to_string()))
            })?
        } else {
            None
        };
        let identity_before = expected
            .and_then(|target| target.identity)
            .or_else(|| record_before.as_ref().map(RunnerIdentity::from_record));
        let stopped = self
            .shutdown_with_reason(session_id, "user_stopped", delete_adapter_state, expected)
            .await;
        // The existing synchronous teardown grace precedes the settlement wait.
        let deadline = tokio::time::Instant::now() + deadline;
        match stopped {
            Ok(()) => {}
            Err(SupervisorError::UnknownSession(_)) => {
                if record_before
                    .as_ref()
                    .is_some_and(worker_registry::is_record_live)
                {
                    return Err(SupervisorError::TeardownPending(session_id.into()));
                }
            }
            Err(error) => return Err(error),
        }
        let still_owned = || {
            let lifecycle = lock_recover(&self.lifecycle);
            match expected {
                None => lifecycle.is_owned(session_id),
                Some(target) => {
                    lifecycle.is_owned(session_id)
                        && lifecycle.current_lease(session_id).is_some_and(|current| {
                            target.lease.as_ref().map_or_else(
                                || current.spawned_runner() == target.identity,
                                |captured| current == *captured,
                            )
                        })
                }
            }
        };
        loop {
            let notified = self.worker_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !still_owned() {
                break;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() && still_owned() {
                return Err(SupervisorError::TeardownPending(session_id.into()));
            }
        }
        #[cfg(unix)]
        if let Some(identity) = identity_before {
            while self.process_control.is_alive(identity) {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return Err(SupervisorError::TeardownPending(session_id.into()));
                }
                tokio::time::sleep(TEARDOWN_POLL.min(remaining)).await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = identity_before;
            warn!(target: "acp.supervisor", session = %session_id,
                "shutdown_and_wait called on non-Unix host; PID wait is unimplemented for this platform");
        }
        Ok(())
    }

    async fn shutdown_with_reason(
        &self,
        session_id: &str,
        stop_reason: &str,
        delete_adapter_state: bool,
        expected: Option<&RetirementTarget>,
    ) -> Result<(), SupervisorError> {
        // Same lock order as `begin_resume`, so a resume cannot slip between
        // the decision and the handle removal.
        let mut workers = self.workers.lock().await;
        let decision = {
            let mut lifecycle = lock_recover(&self.lifecycle);
            if let Some(target) = expected {
                if !lifecycle.prepare_captured_stop(session_id, target.lease.as_ref()) {
                    return Err(SupervisorError::TeardownPending(session_id.into()));
                }
            }
            if let Some(target) = expected.filter(|target| target.journalled) {
                if let Some(lease) = &target.lease {
                    if !lifecycle.require_retirement_ack(lease) {
                        return Err(SupervisorError::TeardownPending(session_id.into()));
                    }
                }
            }
            lifecycle.begin_stop(session_id, stop_reason, delete_adapter_state)
        };
        match decision {
            StopDecision::TearDown { lease, identity } => {
                if let Some(target) = expected {
                    lease.record_prime(target.prime.clone());
                    for scope in &target.scopes {
                        lease.record_scope(scope.clone());
                    }
                } else if let Ok(Some(record)) = worker_registry::load_strict(session_id) {
                    if identity.is_some_and(|identity| identity.owns_record(&record)) {
                        if lease.prime() == PrimeOwnership::Unresolved {
                            lease.record_prime(PrimeOwnership::from_record(&record));
                        }
                        lease.record_scope(record.scope);
                    }
                }
                if let Some(handle) = workers.get(session_id) {
                    if handle.lease != lease {
                        return Err(SupervisorError::TeardownPending(session_id.into()));
                    }
                    capture_adapter_route(&lease, &handle.client, identity);
                }
                if let Some(handle) = workers.remove(session_id) {
                    // No await separates removal from ownership transfer.
                    lease.retirement().record_worker(handle);
                }
                drop(workers);
                worker_registry::clear_restart_marker(session_id);
                retire_registered_clients(&self.lifecycle, &lease).await;
                let settlement = tear_down_lease(&*self.process_control, &lease, false).await;
                self.settle_sweeping_if_owed(&lease, settlement).await;
                Ok(())
            }
            StopDecision::CancelRequested => {
                // Attach admission already froze its original runner and scopes.
                // Never fill missing authority from a replacement's registry row.
                drop(workers);
                worker_registry::clear_restart_marker(session_id);
                debug!(target: "acp.supervisor", session = %session_id,
                        "shutdown: resume in flight; matching construction retains completion debt");
                Ok(())
            }
            StopDecision::AlreadyStopping => Ok(()),
            StopDecision::NotOwned => {
                let identity = match expected {
                    Some(target) => target.identity,
                    None => worker_registry::load_strict(session_id)
                        .ok()
                        .flatten()
                        .map(|record| RunnerIdentity::from_record(&record)),
                };
                let Some(identity) = identity else {
                    drop(workers);
                    return Err(SupervisorError::UnknownSession(session_id.into()));
                };
                let lease = {
                    let mut table = lock_recover(&self.lifecycle);
                    table.note_generation(session_id, identity.generation);
                    let lease = table.adopt_for_stop(session_id);
                    if let Some(lease) = &lease {
                        lease.record_runner(identity);
                        if let Some(target) = expected {
                            lease.record_prime(target.prime.clone());
                            for scope in &target.scopes {
                                lease.record_scope(scope.clone());
                            }
                        } else if let Ok(Some(record)) = worker_registry::load_strict(session_id) {
                            if identity.owns_record(&record) {
                                lease.record_prime(PrimeOwnership::from_record(&record));
                                lease.record_scope(record.scope);
                            }
                        }
                        // Adopt first, then record the same stop reason/deletion debt.
                        let _ = table.begin_stop(session_id, stop_reason, delete_adapter_state);
                    }
                    lease
                };
                drop(workers);
                worker_registry::clear_restart_marker(session_id);
                if let Some(lease) = lease {
                    retire_registered_clients(&self.lifecycle, &lease).await;
                    let settlement = tear_down_lease(&*self.process_control, &lease, false).await;
                    self.settle_sweeping_if_owed(&lease, settlement).await;
                }
                Ok(())
            }
        }
    }

    /// Drop every worker handle without killing the runners (daemon restart).
    pub async fn detach_all(&self) {
        let drained: Vec<(String, Lease, Arc<AcpClient>)> = {
            let mut workers = self.workers.lock().await;
            workers
                .drain()
                .map(|(id, handle)| {
                    let lease = handle.lease.clone();
                    let client = Arc::clone(&handle.client);
                    let identity = lease.spawned_runner().or_else(|| lease.previous_runner());
                    // Ownership only: no begin_stop, completion debt, signal, or scope release.
                    retain_worker(&lease, handle, identity);
                    (id, lease, client)
                })
                .collect()
        };
        info!(target: "acp.supervisor", count = drained.len(),
                "detaching structured view workers; they continue running. Use `aoe acp stop` to terminate.");
        for (id, lease, client) in drained {
            debug!(target: "acp.supervisor", session = %id, "detaching");
            let _driver = lease.retirement().gate.lock().await;
            let _ = client.shutdown().await;
            let clients = lock_recover(&lease.retirement().clients).clone();
            if let Some(owned) = clients
                .iter()
                .flatten()
                .find(|owned| Arc::ptr_eq(&owned.client, &client))
            {
                owned
                    .shutdown_done
                    .store(true, std::sync::atomic::Ordering::Release);
            }
            mark_adapter_route_retired(&lease);
            quiesce_retained_drain(&lease).await;
        }
    }

    pub(super) async fn settle_sweeping_if_owed(&self, lease: &Lease, settlement: Settlement) {
        settle_sweeping_if_owed(
            &self.process_control,
            &self.lifecycle,
            &self.worker_notify,
            StopPublication {
                workers: &self.workers,
                sink: &*self.sink,
                seqs: &self.next_seqs,
            },
            lease,
            settlement,
            CompletionPublisher::JoinDrain,
        )
        .await;
    }

    /// Drive every parked teardown once more.
    pub async fn retry_pending_teardowns(&self) {
        let ids = lock_recover(&self.lifecycle).retry_ids_after(TEARDOWN_ORPHAN_GRACE);
        for id in ids {
            let claim = lock_recover(&self.lifecycle).claim_retry(&id, TEARDOWN_ORPHAN_GRACE);
            let Some(claim) = claim else {
                continue;
            };
            // A constructor can disappear while its original handle is still installed.
            // Recover only the matching lease; a replacement never supplies our resources.
            {
                let mut workers = self.workers.lock().await;
                let current = lock_recover(&self.lifecycle).current_lease(&id);
                if current.as_ref() != Some(&claim.lease) {
                    continue;
                }
                if workers
                    .get(&id)
                    .is_some_and(|handle| handle.lease == claim.lease)
                {
                    if let Some(handle) = workers.remove(&id) {
                        capture_adapter_route(
                            &claim.lease,
                            &handle.client,
                            claim.identity.or_else(|| claim.lease.spawned_runner()),
                        );
                        claim.lease.retirement().record_worker(handle);
                    }
                }
            }
            retire_registered_clients(&self.lifecycle, &claim.lease).await;
            let identity = claim.identity.or_else(|| claim.lease.spawned_runner());
            let pid = identity.map(|identity| identity.pid);
            let native_unproven = claim.lease.prime() != PrimeOwnership::NotPrime
                || match worker_registry::load_strict(&id) {
                    Ok(Some(record)) => {
                        record.prime_runtime.is_some() || record.agent_key == "prime-agent"
                    }
                    Ok(None) => false,
                    Err(_) => true,
                };
            if !native_unproven
                && claim.attempts > TEARDOWN_RETRY_CAP
                && claim
                    .lease
                    .retirement_runners()
                    .all(|identity| !self.process_control.is_alive(identity))
            {
                warn!(
                    target: "acp.supervisor",
                    session = %id,
                    pid,
                    attempts = claim.attempts,
                    "runner is dead but its registry record could not be settled; releasing the session"
                );
                for identity in claim.lease.retirement_runners() {
                    self.process_control.forget(identity);
                }
                self.settle_sweeping_if_owed(&claim.lease, Settlement::Proven)
                    .await;
                continue;
            }
            match identity {
                // Loud for the first few ticks; past that the process is in the kernel's hands.
                Some(identity) if claim.attempts <= 3 => warn!(
                    target: "acp.supervisor",
                    session = %id,
                    pid = identity.pid,
                    attempt = claim.attempts,
                    "runner still alive after SIGKILL; retrying teardown"
                ),
                Some(identity) => debug!(
                    target: "acp.supervisor",
                    session = %id,
                    pid = identity.pid,
                    attempt = claim.attempts,
                    "runner still alive after SIGKILL; retrying teardown"
                ),
                None => warn!(
                    target: "acp.supervisor",
                    session = %id,
                    attempt = claim.attempts,
                    "teardown has no frozen runner identity"
                ),
            }
            let killed_before = claim.identity.is_some();
            let settlement =
                tear_down_lease(&*self.process_control, &claim.lease, killed_before).await;
            self.settle_sweeping_if_owed(&claim.lease, settlement).await;
        }
    }

    /// Consume a restart marker found outside the reaper; honored only for the
    /// newest generation known for the session.
    pub fn take_late_restart_marker(&self, session_id: &str) -> bool {
        let Some(Some(marker)) = worker_registry::claim_restart_marker(session_id) else {
            return false;
        };
        let on_disk = worker_registry::load(session_id)
            .ok()
            .flatten()
            .map_or(0, |r| r.generation);
        let known = lock_recover(&self.lifecycle)
            .last_generation(session_id)
            .max(on_disk);
        let honored = marker >= known;
        if !honored {
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                marker,
                known,
                "discarding stale restart marker"
            );
        }
        honored
    }

    /// Tear down workers whose registry entry disappeared under a live handle;
    /// returns the sessions whose stop was a restart request.
    pub async fn reap_user_stopped(&self) -> Vec<String> {
        let requested: Vec<String> = self
            .workers
            .lock()
            .await
            .keys()
            .filter(|id| worker_registry::stop_requested(id))
            .cloned()
            .collect();
        for id in requested {
            if let Err(error) = self.shutdown(&id).await {
                warn!(target: "acp.supervisor", %id, %error, "requested worker stop remains unsettled");
            }
        }
        let mut restart_pending = Vec::new();
        for candidate in self.reap_candidates().await {
            let id = candidate.id.clone();
            if self.reap_candidate(candidate).await == Some(true) {
                restart_pending.push(id);
            }
        }
        restart_pending
    }

    async fn reap_candidates(&self) -> Vec<ReapCandidate> {
        let workers = self.workers.lock().await;
        let table = lock_recover(&self.lifecycle);
        workers
            .iter()
            .filter(|(_, h)| !is_test_worker(h))
            .filter_map(|(id, _)| {
                table.running(id).map(|(lease, identity)| ReapCandidate {
                    id: id.clone(),
                    lease,
                    identity,
                })
            })
            .filter(|c| registry_disowns(&c.id, c.identity))
            .collect()
    }

    /// Tear down one candidate; `None` when a newer epoch replaced it since the snapshot.
    async fn reap_candidate(&self, candidate: ReapCandidate) -> Option<bool> {
        let ReapCandidate {
            id,
            lease,
            identity,
        } = candidate;
        let is_restart = {
            let mut workers = self.workers.lock().await;
            let mut lifecycle = lock_recover(&self.lifecycle);
            if !lifecycle
                .running(&id)
                .is_some_and(|(running, _)| running == lease)
            {
                return None;
            }
            if workers.get(&id).is_some_and(|handle| handle.lease != lease) {
                return None;
            }
            let generation = identity.map_or(0, |identity| identity.generation);
            let restart = worker_registry::take_restart_marker(&id, generation);
            let reason = if restart {
                "restart_pending"
            } else {
                "user_stopped"
            };
            let _ = lifecycle.begin_stop(&id, reason, false);
            if let Some(handle) = workers.remove(&id) {
                capture_adapter_route(&lease, &handle.client, identity);
                lease.retirement().record_worker(handle);
            }
            restart
        };
        let reason = if is_restart {
            "restart_pending"
        } else {
            "user_stopped"
        };
        info!(target: "acp.supervisor", session = %id, reason,
                "registry entry gone while worker handle live; tearing down");
        retire_registered_clients(&self.lifecycle, &lease).await;
        let settlement = tear_down_lease(&*self.process_control, &lease, false).await;
        self.settle_sweeping_if_owed(&lease, settlement).await;
        Some(is_restart)
    }
}

/// Retire only exact units captured by the matching producer/registry authority.
pub(super) async fn release_scope(control: &Arc<dyn ProcessControl>, lease: &Lease) -> bool {
    let control = Arc::clone(control);
    let id = lease.session_id().to_string();
    let scopes = lease.scopes();
    tokio::task::spawn_blocking(move || control.release_scope(&id, &scopes))
        .await
        .unwrap_or(false)
}

fn is_test_worker(handle: &WorkerHandle) -> bool {
    match handle.kind {
        WorkerKind::Runner { .. } | WorkerKind::Attached => false,
        #[cfg(test)]
        WorkerKind::Stdio => true,
    }
}

struct ReapCandidate {
    id: String,
    lease: Lease,
    identity: Option<RunnerIdentity>,
}

fn registry_disowns(session_id: &str, identity: Option<RunnerIdentity>) -> bool {
    match worker_registry::load_strict(session_id) {
        Ok(None) => true,
        Ok(Some(record)) => identity.is_some_and(|i| i.disowns_record(&record)),
        Err(_) => false,
    }
}

/// A cancelled launch may finish its handshake after a permanent stop was requested.
pub(super) async fn retire_client(
    lifecycle: &std::sync::Mutex<LifecycleTable>,
    lease: &Lease,
    client: &Arc<AcpClient>,
    identity: Option<RunnerIdentity>,
) {
    lease.retirement().record_client(client, false);
    if lease.spawned_runner().is_none() {
        if let Some(identity) = identity {
            lease.record_runner(identity);
        }
    }
    // A registry observation never upgrades an existing weak current role.
    capture_adapter_route(lease, client, lease.spawned_runner());
    retire_registered_clients(lifecycle, lease).await;
}

fn capture_adapter_route(lease: &Lease, client: &Arc<AcpClient>, identity: Option<RunnerIdentity>) {
    if lease.adapter_route().is_some() {
        return;
    }
    let Some(identity) = identity.filter(|identity| identity.birth_fingerprint.is_some()) else {
        return;
    };
    let Ok(Some(record)) = worker_registry::load_strict(lease.session_id()) else {
        return;
    };
    if !identity.owns_record(&record) {
        return;
    }
    let Some(acp_id) = record.stored_acp_session_id else {
        return;
    };
    lease.record_adapter_route(crate::acp::runner_lifecycle::AdapterRetirementRoute {
        client: client.clone(),
        identity,
        acp_id,
        adapter: record.agent_key,
        gate: tokio::sync::Mutex::new(()),
        retired: std::sync::atomic::AtomicBool::new(false),
    });
}

fn mark_adapter_route_retired(lease: &Lease) {
    if let Some(route) = lease.adapter_route() {
        route
            .retired
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

async fn service_adapter_delete_debt(lifecycle: &std::sync::Mutex<LifecycleTable>, lease: &Lease) {
    if !lock_recover(lifecycle).adapter_delete_pending(lease) {
        return;
    }
    let Some(route) = lease.adapter_route() else {
        warn!(target: "acp.protocol", session = lease.session_id(),
            "session/delete unavailable: original attested adapter route was not captured");
        lock_recover(lifecycle).adapter_delete_handled(lease);
        return;
    };
    let _attempt = route.gate.lock().await;
    if !lock_recover(lifecycle).adapter_delete_pending(lease) {
        return;
    }
    let session_id = lease.session_id();
    let acp_id = &route.acp_id;
    let adapter = &route.adapter;
    let started = Instant::now();
    let outcome = if route.retired.load(std::sync::atomic::Ordering::SeqCst) {
        DeleteSessionOutcome::Failed("original adapter connection already retired".into())
    } else {
        match worker_registry::load_strict(session_id) {
            Ok(Some(record)) if route.identity.owns_record(&record) => {
                route.client.delete_session(acp_id.clone()).await
            }
            _ => DeleteSessionOutcome::Failed(
                "original adapter route is unavailable or its registry owner changed".into(),
            ),
        }
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match outcome {
        DeleteSessionOutcome::Deleted => debug!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter,
            acp_session_id = %acp_id,
            elapsed_ms,
            "session/delete RPC succeeded"
        ),
        DeleteSessionOutcome::UnsupportedMethod => debug!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter,
            acp_session_id = %acp_id,
            "adapter does not support session/delete; proceeding to SIGTERM"
        ),
        DeleteSessionOutcome::TimedOut => warn!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter,
            acp_session_id = %acp_id,
            elapsed_ms,
            "session/delete RPC timed out; proceeding to SIGTERM"
        ),
        DeleteSessionOutcome::Failed(msg) => warn!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter,
            acp_session_id = %acp_id,
            elapsed_ms,
            "session/delete RPC failed: {msg}; proceeding to SIGTERM"
        ),
    }
    lock_recover(lifecycle).adapter_delete_handled(lease);
}

/// Signal only the frozen runner identity and prove it gone.
/// SIGTERM and SIGKILL have bounded waits before settling its record.
pub(super) async fn tear_down_runner(
    control: &dyn ProcessControl,
    session_id: &str,
    identity: Option<RunnerIdentity>,
) -> Settlement {
    tear_down_runner_from(control, session_id, identity, false).await
}

/// `killed_before` skips straight to SIGKILL for a process that already ignored a full escalation.
async fn tear_down_runner_from(
    control: &dyn ProcessControl,
    session_id: &str,
    identity: Option<RunnerIdentity>,
    killed_before: bool,
) -> Settlement {
    let Some(identity) = identity else {
        return match worker_registry::load_strict(session_id) {
            Ok(None) => Settlement::Proven,
            _ => Settlement::Unproven(None),
        };
    };
    if worker_registry::load_strict(session_id)
        .ok()
        .flatten()
        .is_some_and(|record| record.pid == identity.pid && !identity.owns_record(&record))
    {
        return Settlement::Unproven(Some(identity));
    }
    let pid = identity.pid;
    if !killed_before {
        // Sent even to a dead leader: its group's descendants still need the signal.
        control.terminate_group(identity);
        wait_for_exit(control, identity, TEARDOWN_TERM_GRACE).await;
    }
    if control.is_alive(identity) {
        if !killed_before {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                pid,
                "runner ignored SIGTERM; escalating to SIGKILL"
            );
        }
        control.kill_group(identity);
        wait_for_exit(control, identity, TEARDOWN_KILL_GRACE).await;
    }
    if control.is_alive(identity) {
        warn!(
            target: "acp.supervisor",
            session = %session_id,
            pid,
            "runner survived SIGKILL; holding the session until it exits"
        );
        return Settlement::Unproven(Some(identity));
    }
    let cleanup = match worker_registry::load_strict(session_id) {
        Ok(Some(record)) if identity.owns_record(&record) => Some(record),
        Ok(Some(record)) if identity.disowns_record(&record) => None,
        Ok(None) => None,
        _ => return Settlement::Unproven(Some(identity)),
    };
    if let Some(record) = cleanup {
        if !matches!(
            tokio::task::spawn_blocking(move || worker_registry::cleanup_runtime(&record)).await,
            Ok(Ok(()))
        ) {
            warn!(target: "acp.supervisor", session = %session_id, "native runtime termination unproven; retaining registry route");
            return Settlement::Unproven(Some(identity));
        }
    }
    if !worker_registry::delete_if_owned_by(session_id, identity) {
        warn!(
            target: "acp.supervisor",
            session = %session_id,
            pid,
            "runner exited but its registry record could not be read; retrying settlement"
        );
        return Settlement::Unproven(Some(identity));
    }
    control.forget(identity);
    Settlement::Proven
}

/// Polls on the tokio clock so paused-time tests advance through the grace.
pub(super) async fn wait_for_exit(
    control: &dyn ProcessControl,
    identity: RunnerIdentity,
    grace: Duration,
) {
    let deadline = tokio::time::Instant::now() + grace;
    while control.is_alive(identity) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(TEARDOWN_POLL).await;
    }
}

pub(super) fn settle_lease(
    lifecycle: &std::sync::Mutex<LifecycleTable>,
    notify: &tokio::sync::Notify,
    lease: &Lease,
    settlement: Settlement,
) {
    lock_recover(lifecycle).settle(lease, settlement);
    notify.notify_waiters();
}

pub(super) struct StopPublication<'a, S> {
    pub(super) workers: &'a super::Workers,
    pub(super) sink: &'a S,
    pub(super) seqs: &'a super::SeqMap,
}

/// Scope retirement precedes Proven; a failed stop preserves cleanup debt and authority.
pub(super) async fn settle_sweeping_if_owed<S: BroadcastSink>(
    control: &Arc<dyn ProcessControl>,
    lifecycle: &std::sync::Mutex<LifecycleTable>,
    notify: &tokio::sync::Notify,
    publication: StopPublication<'_, S>,
    lease: &Lease,
    settlement: Settlement,
    publisher: CompletionPublisher,
) {
    if lease.is_borrowed_attach() {
        return;
    }
    let StopPublication {
        workers,
        sink,
        seqs,
    } = publication;
    let _driver = lease.retirement().gate.lock().await;
    if lock_recover(lifecycle)
        .current_lease(lease.session_id())
        .as_ref()
        != Some(lease)
    {
        return;
    }
    lease.owe_stop_completion(None);
    retire_registered_clients_locked(lifecycle, lease).await;
    if matches!(publisher, CompletionPublisher::JoinDrain) {
        quiesce_retained_drain(lease).await;
    }
    // Share admission's workers gate; call the sink without the lifecycle mutex.
    let workers_gate = workers.lock().await;
    if lock_recover(lifecycle)
        .current_lease(lease.session_id())
        .as_ref()
        != Some(lease)
    {
        return;
    }
    if workers_gate
        .get(lease.session_id())
        .is_some_and(|handle| handle.lease != *lease)
    {
        drop(workers_gate);
        settle_lease(
            lifecycle,
            notify,
            lease,
            Settlement::Unproven(lease.spawned_runner()),
        );
        return;
    }
    if !finish_stop_completion(sink, seqs, lease) {
        drop(workers_gate);
        settle_lease(
            lifecycle,
            notify,
            lease,
            Settlement::Unproven(lease.spawned_runner()),
        );
        return;
    }
    drop(workers_gate);
    // Logical completion is final even if a physical proof needs another tick.
    if settlement != Settlement::Proven {
        settle_lease(lifecycle, notify, lease, settlement);
        return;
    }
    let Some(owed) = lock_recover(lifecycle).sweep_owed(lease) else {
        return;
    };
    let mut empty = if owed {
        release_scope(control, lease).await
    } else {
        let control = Arc::clone(control);
        let id = lease.session_id().to_owned();
        let scopes = lease.scopes();
        tokio::task::spawn_blocking(move || control.scopes_stopped(&id, &scopes))
            .await
            .unwrap_or(false)
    };
    if empty {
        let prime = lease.prime();
        empty = matches!(
            tokio::task::spawn_blocking(move || prime.retire()).await,
            Ok(Ok(()))
        );
    }
    // Recheck frozen respawn authority under admission exclusion after physical proof.
    let _workers_gate = workers.lock().await;
    let mut table = lock_recover(lifecycle);
    if table.current_lease(lease.session_id()).as_ref() != Some(lease) {
        return;
    }
    let unproven_runner = lease
        .retirement_runners()
        .find(|identity| identity.birth_fingerprint.is_none() || control.is_alive(*identity));
    let settlement = if empty && unproven_runner.is_none() {
        Settlement::Proven
    } else {
        Settlement::Unproven(unproven_runner.or_else(|| lease.spawned_runner()))
    };
    table.settle(lease, settlement);
    drop(table);
    notify.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::Launcher;
    use super::super::{AcpWorkerState, ResumeKind, ResumeReservationOutcome};
    use super::*;
    use crate::acp::acp_client::{AcpError, SpawnConfig};
    use crate::acp::runner_lifecycle::test_support::FakeProcessControl;
    use crate::acp::runner_lifecycle::WorkerPhase;
    use crate::acp::state::AcpSessionId;
    use std::sync::Arc;

    struct CompletionAppendFailure {
        inner: Arc<super::super::ChannelSink>,
        fail_stopped: bool,
        failed: std::sync::atomic::AtomicBool,
    }

    impl BroadcastSink for CompletionAppendFailure {
        fn publish(&self, id: &str, seq: u64, event: &Event) {
            self.inner.publish(id, seq, event);
        }

        fn publish_persisted(&self, id: &str, seq: u64, event: &Event) -> bool {
            let rejects = if self.fail_stopped {
                matches!(event, Event::Stopped { .. })
            } else {
                matches!(event, Event::BackgroundAgentCompleted { .. })
            };
            if rejects && !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return false;
            }
            self.inner.publish_persisted(id, seq, event)
        }

        fn unresolved_background_task_ids(&self, id: &str) -> Vec<String> {
            self.inner.unresolved_background_task_ids(id)
        }

        fn unresolved_background_agent_ids(&self, id: &str) -> Vec<String> {
            self.inner.unresolved_background_agent_ids(id)
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_terminal_append_keeps_admission_fenced_without_replaying_durable_prefix() {
        let _home = isolate_home();
        for fail_stopped in [false, true] {
            let (inner, store, _rx, _store_tmp) = channel_sink();
            let sink = Arc::new(CompletionAppendFailure {
                inner,
                fail_stopped,
                failed: std::sync::atomic::AtomicBool::new(false),
            });
            let sup = Supervisor::new(sink.clone());
            let id = "failed-terminal-append";
            let (client, _client_tx) =
                crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId(id.into()));
            sup.test_install_handle(id, client, WorkerKind::Attached, None)
                .await;
            sup.publish_next(
                id,
                &crate::acp::state::test_support::prompt("original turn"),
            );
            sup.publish_next(
                id,
                &Event::CancelRequested {
                    escalates_at: chrono::Utc::now(),
                },
            );
            sup.publish_next(
                id,
                &Event::BackgroundTaskStarted {
                    task_id: "original-task".into(),
                    tool_call_id: None,
                    description: "work".into(),
                },
            );
            sup.publish_next(
                id,
                &Event::BackgroundAgentLaunched {
                    agent_id: "original-agent".into(),
                    tool_call_id: "original-tool".into(),
                    description: "background work".into(),
                    prompt: "work".into(),
                    model: "test-model".into(),
                    output_file: String::new(),
                    started_at: chrono::Utc::now(),
                },
            );
            sup.shutdown(id).await.unwrap();
            assert!(sink.failed.load(std::sync::atomic::Ordering::SeqCst));
            let prefix = store.replay_from(id, 0);
            assert_eq!(prefix.iter().filter(|(_, event)|
                matches!(event, Event::BackgroundTaskFinished { task_id } if task_id == "original-task")).count(), 1);
            assert_eq!(prefix.iter().filter(|(_, event)|
                matches!(event, Event::BackgroundAgentCompleted { agent_id, .. } if agent_id == "original-agent")).count(),
                usize::from(fail_stopped));
            assert_eq!(
                prefix
                    .iter()
                    .filter(|(_, event)| matches!(event, Event::Stopped { .. }))
                    .count(),
                0
            );
            let pending = replay_control_state(&store, id);
            assert!(pending.turn_active && pending.cancelling);
            assert_eq!(
                lock_recover(&sup.lifecycle)
                    .admit(id, ResumeKind::Spawn)
                    .unwrap_err(),
                crate::acp::runner_lifecycle::AdmitError::TeardownPending,
            );
            sup.retry_pending_teardowns().await;
            let terminal_seqs = assert_stop_terminals(
                &store,
                id,
                "original-task",
                "original-agent",
                "user_stopped",
            );
            let completed = replay_control_state(&store, id);
            assert!(!completed.turn_active && !completed.cancelling);
            assert!(completed.background_tasks.is_empty());
            assert!(completed
                .background_agents
                .iter()
                .all(|agent| agent.status == BackgroundAgentStatus::Detached));
            assert_eq!(sup.worker_state(id).await, AcpWorkerState::Absent);
            sup.retry_pending_teardowns().await;
            assert_eq!(
                assert_stop_terminals(
                    &store,
                    id,
                    "original-task",
                    "original-agent",
                    "user_stopped"
                ),
                terminal_seqs
            );
        }
    }

    fn replay_control_state(
        store: &crate::acp::event_store::EventStore,
        id: &str,
    ) -> crate::acp::state::AcpState {
        let mut state = crate::acp::state::AcpState::new(
            AcpSessionId(id.into()),
            crate::acp::state::AgentName("claude".into()),
            None,
        );
        for (seq, event) in store.replay_from(id, 0) {
            state.apply_event(seq, event).unwrap();
        }
        state
    }

    fn assert_stop_terminals(
        store: &crate::acp::event_store::EventStore,
        id: &str,
        task: &str,
        agent: &str,
        reason: &str,
    ) -> [u64; 3] {
        let mut terminals = [None; 3];
        for (seq, event) in store.replay_from(id, 0) {
            let index = match event {
                Event::BackgroundTaskFinished { task_id } if task_id == task => Some(0),
                Event::BackgroundAgentCompleted {
                    agent_id,
                    status: BackgroundAgentStatus::Detached,
                    ..
                } if agent_id == agent => Some(1),
                Event::Stopped { reason: actual } if actual == reason => Some(2),
                _ => None,
            };
            if let Some(index) = index {
                assert!(
                    terminals[index].replace(seq).is_none(),
                    "duplicate terminal event"
                );
            }
        }
        let terminals = terminals.map(|seq| seq.expect("owed terminal event persisted"));
        assert!(terminals[0] < terminals[2] && terminals[1] < terminals[2]);
        terminals
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn cancelled_shutdown_retries_original_completion_without_stopping_replacement() {
        let _home = isolate_home();
        let mut p = parked(|event| matches!(event, Event::BackgroundTaskStarted { .. }));
        let id = "cancelled-stop-completion";
        let original = attested_test_identity(404404, 41);
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(original.pid);
        Arc::get_mut(&mut p.sup).unwrap().process_control = control.clone();
        save_attested_test_record(id, original.pid, original.generation);
        let (client, _client_tx) =
            crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId(id.into()));
        let lease = p
            .sup
            .test_install_handle(id, client, WorkerKind::Attached, Some(original))
            .await;
        lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
        p.sup.publish_next(
            id,
            &crate::acp::state::test_support::prompt("original turn"),
        );
        p.sup.publish_next(
            id,
            &Event::CancelRequested {
                escalates_at: chrono::Utc::now(),
            },
        );
        p.sup.publish_next(
            id,
            &Event::BackgroundAgentLaunched {
                agent_id: "original-agent".into(),
                tool_call_id: "original-tool".into(),
                description: "original background work".into(),
                prompt: "work".into(),
                model: "test-model".into(),
                output_file: String::new(),
                started_at: chrono::Utc::now(),
            },
        );
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel(16);
        let drain = p
            .sup
            .start_drain_task(id.into(), lease.clone(), inbound_rx, None);
        let previous = {
            let mut workers = p.sup.workers.lock().await;
            std::mem::replace(&mut workers.get_mut(id).unwrap().drain_task, drain)
        };
        previous.abort();
        let _ = previous.await;
        inbound_tx
            .send(Event::BackgroundTaskStarted {
                task_id: "original-task".into(),
                tool_call_id: None,
                description: "work".into(),
            })
            .await
            .unwrap();
        p.entered().await;
        let shutdown = {
            let sup = Arc::clone(&p.sup);
            tokio::spawn(async move { sup.shutdown(id).await })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let retained = loop {
            let retained = lease.retirement().drain.lock().unwrap().clone();
            if let Some(retained) = retained {
                break retained;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "original drain was not retained"
            );
            tokio::task::yield_now().await;
        };
        while retained.try_lock().is_ok() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "stop did not join its retained drain"
            );
            tokio::task::yield_now().await;
        }
        assert!(!p.sup.workers.lock().await.contains_key(id));
        shutdown.abort();
        assert!(shutdown.await.unwrap_err().is_cancelled());
        assert!(
            retained.lock().await.is_some(),
            "cancelled join lost its original handle"
        );
        let before = replay_control_state(&p.store, id);
        assert!(before.turn_active && before.cancelling);
        assert!(before
            .background_agents
            .iter()
            .any(|agent| agent.agent_id == "original-agent"
                && agent.status == BackgroundAgentStatus::Running));
        assert_eq!(
            lock_recover(&p.sup.lifecycle)
                .admit(id, ResumeKind::Spawn)
                .unwrap_err(),
            crate::acp::runner_lifecycle::AdmitError::TeardownPending,
        );
        control.exit(original.pid);
        lock_recover(&p.sup.lifecycle).age_stopping(id, TEARDOWN_ORPHAN_GRACE);
        let retry = {
            let sup = Arc::clone(&p.sup);
            tokio::spawn(async move { sup.retry_pending_teardowns().await })
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while retained.try_lock().is_ok() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "retry did not rejoin original drain"
            );
            tokio::task::yield_now().await;
        }
        assert!(replay_control_state(&p.store, id).turn_active);
        p.release_and_wait().await;
        tokio::time::timeout(Duration::from_secs(5), retry)
            .await
            .unwrap()
            .unwrap();
        let terminal_seqs = assert_stop_terminals(
            &p.store,
            id,
            "original-task",
            "original-agent",
            "user_stopped",
        );
        let completed = replay_control_state(&p.store, id);
        assert!(!completed.turn_active && !completed.cancelling);
        assert!(completed.background_tasks.is_empty());
        assert!(completed
            .background_agents
            .iter()
            .all(|agent| agent.status == BackgroundAgentStatus::Detached));
        assert_eq!(p.sup.worker_state(id).await, AcpWorkerState::Absent);
        p.sup.retry_pending_teardowns().await;
        assert_eq!(
            assert_stop_terminals(
                &p.store,
                id,
                "original-task",
                "original-agent",
                "user_stopped"
            ),
            terminal_seqs
        );

        let replacement = attested_test_identity(404405, 99);
        control.alive(replacement.pid);
        save_attested_test_record(id, replacement.pid, replacement.generation);
        let (client, _replacement_tx) =
            crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId(id.into()));
        let replacement_lease = p
            .sup
            .test_install_handle(id, client, WorkerKind::Attached, Some(replacement))
            .await;
        replacement_lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
        p.sup.publish_next(
            id,
            &crate::acp::state::test_support::prompt("replacement turn"),
        );
        p.sup.sink.inner.publish(
            id,
            super::super::next_seq(&p.sup.next_seqs, id),
            &Event::BackgroundTaskStarted {
                task_id: "replacement-task".into(),
                tool_call_id: None,
                description: "new work".into(),
            },
        );
        let record_path = worker_registry::record_path(id).unwrap();
        let replacement_record = std::fs::read(&record_path).unwrap();
        p.sup
            .settle_sweeping_if_owed(&lease, Settlement::Proven)
            .await;
        p.sup.retry_pending_teardowns().await;
        let current = replay_control_state(&p.store, id);
        assert!(current.turn_active);
        assert_eq!(current.background_tasks, ["replacement-task"]);
        assert!(control.is_alive(replacement));
        assert!(control
            .signals()
            .iter()
            .all(|(pid, _)| *pid == original.pid));
        assert_eq!(std::fs::read(record_path).unwrap(), replacement_record);
        assert_eq!(
            assert_stop_terminals(
                &p.store,
                id,
                "original-task",
                "original-agent",
                "user_stopped"
            ),
            terminal_seqs
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    #[serial_test::serial]
    async fn cancelled_birth_attestation_keeps_original_child_proof_and_reaps_it() {
        use tokio::io::AsyncReadExt;

        let _home = isolate_home();
        let (sink, _store, _rx, _store_tmp) = channel_sink();
        let sup = Supervisor::new(sink);
        let id = "owned-child-attestation";
        let lease = lock_recover(&sup.lifecycle)
            .admit(id, ResumeKind::Spawn)
            .unwrap();
        lease.record_prime(PrimeOwnership::NotPrime);
        lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "printf ready"])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let mut output = child.stdout.take().unwrap();
        lease.record_spawned_child(child, RunnerIdentity::unattested(pid, lease.epoch()));
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        let birth = crate::process::process_birth_identity(pid)
            .unwrap()
            .expect("unreaped original child");
        let identity = RunnerIdentity {
            pid,
            generation: lease.epoch(),
            birth_fingerprint: Some(birth.fingerprint()),
        };
        assert_eq!(
            lock_recover(&sup.lifecycle).install(&lease, Some(identity)),
            Err(crate::acp::runner_lifecycle::InstallError::Stale)
        );
        {
            let request = lease.attest_runner();
            tokio::pin!(request);
            std::future::poll_fn(|context| {
                assert!(std::future::Future::poll(request.as_mut(), context).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let attested = lease.spawned_runner() == Some(identity);
            let reaped = crate::process::process_birth_identity(pid)
                .unwrap()
                .is_none_or(|current| current.fingerprint() != birth.fingerprint());
            if attested && reaped {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "cancelled requester lost original child proof or reaping"
            );
            tokio::task::yield_now().await;
        }
        lock_recover(&sup.lifecycle).age_stopping(id, TEARDOWN_ORPHAN_GRACE);
        sup.retry_pending_teardowns().await;
        assert_eq!(sup.worker_state(id).await, AcpWorkerState::Absent);
        assert!(
            lock_recover(&sup.lifecycle)
                .admit(id, ResumeKind::Spawn)
                .is_ok(),
            "completed original proof must release admission"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn frozen_zero_generation_cannot_capture_or_signal_a_newer_physical_owner() {
        let (_home, temp) = isolate_home();
        let id = "stale-zero";
        let pid = 8803;
        let control = Arc::new(FakeProcessControl::default());
        control.alive(pid);
        let mut record = worker_record(id, pid, temp.path().join("new.sock")).with_generation(9);
        let new_scope = worker_registry::ScopeOwnership::Owned {
            unit: "aoe-session-stale_2dzero-9-new.scope".into(),
        };
        record.scope = new_scope.clone();
        worker_registry::save(&record).unwrap();
        let record_path = worker_registry::record_path(id).unwrap();
        let bytes = std::fs::read(&record_path).unwrap();
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        sup.test_install_runner(
            id,
            runner_config(record.socket_path.clone()),
            Some(RunnerIdentity::unattested(pid, 0)),
        )
        .await;
        let target = sup.capture_retirement(id).unwrap();
        assert!(!target.scopes.contains(&new_scope));
        assert!(sup
            .shutdown_captured_and_delete_and_wait(id, &target, Duration::from_millis(20))
            .await
            .is_err());
        assert!(control.signals().is_empty());
        assert!(control.scope_units().is_empty());
        assert!(control.is_alive(RunnerIdentity::from_record(&record)));
        assert_eq!(std::fs::read(record_path).unwrap(), bytes);
        assert_eq!(sup.worker_state(id).await, AcpWorkerState::Stopping);
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn unattested_runner_cannot_borrow_birth_from_a_later_registry_record() {
        let (_home, temp) = isolate_home();
        let id = "weak-original";
        let weak = RunnerIdentity::unattested(8882, 7);
        let certified = attested_test_identity(weak.pid, weak.generation);
        let control = Arc::new(FakeProcessControl::default());
        control.alive(weak.pid);
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        let lease = sup
            .test_install_runner(id, runner_config(temp.path().join("weak.sock")), Some(weak))
            .await;
        lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
        save_attested_test_record(id, certified.pid, certified.generation);
        let record_path = worker_registry::record_path(id).unwrap();
        let record_bytes = std::fs::read(&record_path).unwrap();
        assert!(
            !lease.record_runner(certified),
            "matching registry PID and generation cannot attest an existing producer"
        );
        assert_eq!(lease.spawned_runner(), Some(weak));

        sup.shutdown(id).await.unwrap();
        assert!(control.signals().is_empty());
        assert_eq!(sup.worker_state(id).await, AcpWorkerState::Stopping);
        assert_eq!(stopped_reasons(&sup.sink, id), ["user_stopped"]);
        assert_eq!(std::fs::read(&record_path).unwrap(), record_bytes);

        control.exit(weak.pid);
        sup.retry_pending_teardowns().await;
        assert_eq!(lease.spawned_runner(), Some(weak));
        assert!(
            control.signals().is_empty(),
            "neither a later registry birth nor PID disappearance grants signal authority"
        );
        assert!(control.releases().is_empty());
        assert_eq!(sup.worker_state(id).await, AcpWorkerState::Stopping);
        assert_eq!(stopped_reasons(&sup.sink, id), ["user_stopped"]);
        assert_eq!(std::fs::read(&record_path).unwrap(), record_bytes);
        assert!(sup.forget_session(id).is_err());
        assert!(matches!(
            sup.begin_resume(
                id,
                ResumeKind::Spawn,
                worker_registry::ResumeAuthority::Automatic
            )
            .await,
            Err(SupervisorError::TeardownPending(_)),
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_proof_failure_retains_retirement_authority_after_the_runner_exits() {
        let (_home, temp) = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(8881);
        let mut record =
            worker_record("native-proof", 8881, temp.path().join("native.sock")).with_generation(3);
        record.runner_identity = Some(attested_test_birth(8881, record.generation));
        record.agent_key = "prime-agent".into();
        record.prime_runtime = Some(
            crate::process::prime::PrimeRuntime::new(
                "/bin/false".into(),
                "/tmp/store".into(),
                None,
            )
            .unwrap(),
        );
        worker_registry::save(&record).unwrap();
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        sup.test_install_runner(
            "native-proof",
            runner_config(record.socket_path.clone()),
            Some(RunnerIdentity::from_record(&record)),
        )
        .await;
        sup.shutdown("native-proof").await.unwrap();
        assert!(!control.is_alive(RunnerIdentity::from_record(&record)));
        assert_eq!(
            sup.worker_state("native-proof").await,
            AcpWorkerState::Stopping
        );
        for _ in 0..=TEARDOWN_RETRY_CAP {
            sup.retry_pending_teardowns().await;
        }
        assert_eq!(
            sup.worker_state("native-proof").await,
            AcpWorkerState::Stopping
        );
        assert!(sup.forget_session("native-proof").is_err());
        assert!(control.releases().is_empty());
        let lease = lock_recover(&sup.lifecycle)
            .current_lease("native-proof")
            .unwrap();
        assert_eq!(lease.prime(), PrimeOwnership::from_record(&record));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn failed_exact_scope_retirement_holds_authority_and_preserves_a_newer_scope() {
        let (_home, temp) = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(8801);
        let old_unit = "aoe-session-scope_2downer-1-old.scope";
        let new_unit = "aoe-session-scope_2downer-2-new.scope";
        control.fail_scope(old_unit);
        let mut record =
            worker_record("scope-owner", 8801, temp.path().join("old.sock")).with_generation(1);
        record.runner_identity = Some(attested_test_birth(8801, record.generation));
        let original = RunnerIdentity::from_record(&record);
        record.scope = worker_registry::ScopeOwnership::Owned {
            unit: old_unit.into(),
        };
        worker_registry::save(&record).unwrap();
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        sup.test_install_runner(
            "scope-owner",
            runner_config(record.socket_path),
            Some(original),
        )
        .await;
        sup.shutdown("scope-owner").await.unwrap();
        assert!(!control.is_alive(original), "the leader is already retired");
        assert_eq!(
            sup.worker_state("scope-owner").await,
            AcpWorkerState::Stopping
        );
        sup.forget_session("scope-owner")
            .expect_err("scope exit, not leader exit, releases authority");
        assert!(matches!(
            sup.begin_resume(
                "scope-owner",
                ResumeKind::Spawn,
                worker_registry::ResumeAuthority::Automatic
            )
            .await,
            Err(SupervisorError::TeardownPending(_))
        ));
        assert_eq!(control.scope_units(), [old_unit]);
        let mut replacement =
            worker_record("scope-owner", 8802, temp.path().join("new.sock")).with_generation(2);
        replacement.runner_identity = Some(attested_test_birth(8802, replacement.generation));
        replacement.scope = worker_registry::ScopeOwnership::Owned {
            unit: new_unit.into(),
        };
        worker_registry::save(&replacement).unwrap();
        control.alive(8802);
        control.exit_scope(old_unit);
        sup.retry_pending_teardowns().await;
        assert_eq!(
            sup.worker_state("scope-owner").await,
            AcpWorkerState::Absent
        );
        assert_eq!(control.scope_units(), [old_unit, old_unit]);
        assert!(control.is_alive(RunnerIdentity::from_record(&replacement)));
        assert!(control.signals().iter().all(|(pid, _)| *pid != 8802));
        assert_eq!(
            worker_registry::load("scope-owner").unwrap().unwrap().scope,
            replacement.scope
        );
    }

    /// #4001: teardown must detach every background sub-agent the dying
    /// worker's tailer will never report on again, ahead of the `Stopped` it
    /// already publishes, so a reader folding the log in order sees
    /// `has_active_background_agent` cleared no later than the turn end.
    /// Nothing outstanding adds nothing, and the two arms that never reach
    /// `TearDown` publish neither the detach nor the `Stopped`.
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_detaches_outstanding_background_agents_before_stopped() {
        let (_home, tmp) = isolate_home();
        let sink = VecSink::new();
        *sink.stale_background_agent_ids.lock().unwrap() = vec!["bg-1".into(), "bg-2".into()];
        let sup = Supervisor::new(sink.clone());
        sup.test_install_runner(
            "s-detach",
            runner_config(tmp.path().join("dummy.sock")),
            None,
        )
        .await;

        sup.shutdown("s-detach").await.expect("shutdown");

        let frames = sink.frames.lock().unwrap().clone();
        let mine: Vec<&(String, u64, Event)> = frames
            .iter()
            .filter(|(id, _, _)| id == "s-detach")
            .collect();
        assert_eq!(mine.len(), 3, "two detach completions plus the Stopped");
        for (idx, expected) in [(0, "bg-1"), (1, "bg-2")] {
            match &mine[idx].2 {
                Event::BackgroundAgentCompleted {
                    agent_id, status, ..
                } => assert_eq!(
                    (agent_id.as_str(), *status),
                    (expected, BackgroundAgentStatus::Detached)
                ),
                other => panic!("expected a Detached completion, got {other:?}"),
            }
        }
        assert!(matches!(&mine[2].2, Event::Stopped { reason } if reason == "user_stopped"));
        assert!(
            mine[0].1 < mine[1].1 && mine[1].1 < mine[2].1,
            "detach completions must be seq-ordered ahead of Stopped"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_publishes_no_detach_outside_the_teardown_arm() {
        let (_home, tmp) = isolate_home();
        // Nothing outstanding: only the Stopped teardown always publishes.
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.test_install_runner(
            "s-clean",
            runner_config(tmp.path().join("dummy.sock")),
            None,
        )
        .await;
        sup.shutdown("s-clean").await.expect("shutdown");
        let clean = sink.frames.lock().unwrap().len();
        assert_eq!(clean, 1, "only the Stopped, no synthetic completion");

        // `NotOwned`: a session id nothing ever spawned.
        let sink = VecSink::new();
        *sink.stale_background_agent_ids.lock().unwrap() = vec!["bg-1".into()];
        let sup = Supervisor::new(sink.clone());
        assert!(matches!(
            sup.shutdown("s-never-existed").await,
            Err(SupervisorError::UnknownSession(_))
        ));
        assert!(
            sink.frames.lock().unwrap().is_empty(),
            "an unowned session must publish nothing, detach included"
        );

        // `CancelRequested`: a resume is still in flight, no handle installed.
        let sink = VecSink::new();
        *sink.stale_background_agent_ids.lock().unwrap() = vec!["bg-1".into()];
        let sup = Supervisor::new(sink.clone());
        let _reservation = reserve(
            sup.begin_resume(
                "s-resuming",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        sup.shutdown("s-resuming")
            .await
            .expect("a cancel-in-flight shutdown does not error");
        assert!(
            sink.frames.lock().unwrap().is_empty(),
            "a resume-in-flight cancel must publish nothing, detach included"
        );
    }

    /// The `VecSink` cases above never exercise `ChannelSink`'s own override
    /// or the SQL behind it, so a wrong json path there would pass them all.
    /// Record a launch through a real store, tear the session down through a
    /// real `ChannelSink`, and read the durable log back.
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_detaches_through_a_real_channel_sink_and_event_store() {
        let (_home, tmp) = isolate_home();
        let (sink, store, _rx, _store_tmp) = channel_sink();
        store
            .record(
                "s-real-teardown",
                1,
                &Event::BackgroundAgentLaunched {
                    agent_id: "bg-real".into(),
                    tool_call_id: "tc-real".into(),
                    description: "map backend".into(),
                    prompt: "do it".into(),
                    model: "claude-opus-4-8".into(),
                    output_file: "/tmp/bg-real.output".into(),
                    started_at: chrono::Utc::now(),
                },
            )
            .unwrap();
        let sup = Supervisor::new(sink);
        // That seq=1 went straight to the store, so next_seqs needs the same
        // hydrate a real daemon restart performs; otherwise teardown's publish
        // collides with it and is dropped by INSERT OR IGNORE.
        sup.hydrate_seqs(store.all_session_seqs());
        sup.test_install_runner(
            "s-real-teardown",
            runner_config(tmp.path().join("dummy.sock")),
            None,
        )
        .await;
        assert_eq!(
            store.unresolved_background_agent_ids("s-real-teardown"),
            ["bg-real".to_string()],
            "precondition: the launch is genuinely outstanding before teardown"
        );

        sup.shutdown("s-real-teardown").await.expect("shutdown");

        let replayed = store.replay_from("s-real-teardown", 0);
        assert_eq!(replayed.len(), 3, "launch, detach completion, stopped");
        match &replayed[1] {
            (
                2,
                Event::BackgroundAgentCompleted {
                    agent_id, status, ..
                },
            ) => assert_eq!(
                (agent_id.as_str(), *status),
                ("bg-real", BackgroundAgentStatus::Detached)
            ),
            other => panic!("expected a Detached completion at seq 2, got {other:?}"),
        }
        assert!(matches!(replayed[2], (3, Event::Stopped { .. })));
        assert!(
            store
                .unresolved_background_agent_ids("s-real-teardown")
                .is_empty(),
            "the real ChannelSink override must reach the real scan and close it out"
        );
    }

    /// A runner left on disk by a previous daemon is stopped through the
    /// `NotOwned` arm, which has no worker handle: its persisted open
    /// background work must still be finished, or the session reads busy
    /// forever after the stop.
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_of_an_unowned_runner_finishes_its_open_background_work() {
        let (_home, _tmp) = isolate_home();
        let (sink, store, _rx, _store_tmp) = channel_sink();
        let session = "s-unowned-teardown";
        store
            .record(
                session,
                1,
                &Event::BackgroundTaskStarted {
                    task_id: "task-1".into(),
                    tool_call_id: None,
                    description: "sleep".into(),
                },
            )
            .unwrap();
        store
            .record(
                session,
                2,
                &Event::BackgroundAgentLaunched {
                    agent_id: "bg-1".into(),
                    tool_call_id: "tc-1".into(),
                    description: "map backend".into(),
                    prompt: "do it".into(),
                    model: "claude-opus-4-8".into(),
                    output_file: String::new(),
                    started_at: chrono::Utc::now(),
                },
            )
            .unwrap();
        let dead_pid = {
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .expect("spawn helper");
            let id = child.id();
            let _ = child.wait();
            id
        };
        save_record(session, dead_pid, 1);
        let sup = Supervisor::new(sink);
        sup.hydrate_seqs(store.all_session_seqs());

        sup.shutdown(session).await.expect("unowned shutdown");

        assert!(store.unresolved_background_task_ids(session).is_empty());
        assert!(store.unresolved_background_agent_ids(session).is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reaper_honors_only_a_restart_marker_for_the_stopped_generation() {
        let _home = isolate_home();
        // (session, runner identity generation, marker generation, want reason)
        let cases = [
            ("s-reap", None, None, "user_stopped"),
            ("s-restart", Some(7), Some(7), "restart_pending"),
            ("s-stale", Some(7), Some(6), "user_stopped"),
        ];
        for (id, generation, marker, reason) in cases {
            let sink = VecSink::new();
            let sup = Supervisor::new(sink.clone());
            let socket = worker_registry::socket_path_for(id).unwrap();
            let identity =
                generation.map(|generation| RunnerIdentity::unattested(999_999_999, generation));
            sup.test_install_runner(id, runner_config(socket), identity)
                .await;
            if let Some(marker) = marker {
                worker_registry::mark_restart_pending(id, marker);
            }

            let pending = sup.reap_user_stopped().await;
            let want_pending: Vec<String> = if reason == "restart_pending" {
                vec![id.to_string()]
            } else {
                Vec::new()
            };
            assert_eq!(pending, want_pending, "{id}");
            assert!(!sup.workers.lock().await.contains_key(id), "{id}");
            assert_eq!(stopped_reasons(&sink, id), [reason], "{id}");
            assert_eq!(
                worker_registry::peek_restart_marker(id),
                None,
                "{id}: the marker is consumed"
            );
        }
    }

    /// The reaper tears a worker down once its registry record is gone, ahead
    /// of the drain's own detach (it releases the lease first), so it must
    /// finish the open background work itself.
    #[tokio::test]
    #[serial_test::serial]
    async fn reaping_a_disowned_worker_finishes_its_open_background_work() {
        let _home = isolate_home();
        let (sink, store, _rx, _store_tmp) = channel_sink();
        let id = "s-reap-open-task";
        store
            .record(
                id,
                1,
                &Event::BackgroundTaskStarted {
                    task_id: "task-1".into(),
                    tool_call_id: None,
                    description: "sleep".into(),
                },
            )
            .unwrap();
        let sup = Supervisor::new(sink);
        sup.hydrate_seqs(store.all_session_seqs());
        let socket = worker_registry::socket_path_for(id).unwrap();
        sup.test_install_runner(id, runner_config(socket), None)
            .await;

        sup.reap_user_stopped().await;

        assert!(store.unresolved_background_task_ids(id).is_empty());
    }

    /// Publishes like the production sink, except that an event matching
    /// `park_on` parks inside `publish` until released, then acknowledges once
    /// the publish has returned. The publisher is then mid-publish on its own
    /// thread while a teardown runs.
    struct GatedSink {
        inner: Arc<super::super::ChannelSink>,
        park_on: fn(&Event) -> bool,
        entered: tokio::sync::mpsc::UnboundedSender<()>,
        release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
        done: tokio::sync::mpsc::UnboundedSender<()>,
        /// Signalled once a teardown has queried the open work to finish.
        swept: tokio::sync::mpsc::UnboundedSender<()>,
    }

    impl GatedSink {
        fn park(&self, event: &Event) -> bool {
            let parks = (self.park_on)(event);
            if parks {
                let _ = self.entered.send(());
                let _ = self.release.lock().unwrap().recv();
            }
            parks
        }
    }

    impl BroadcastSink for GatedSink {
        fn publish(&self, session_id: &str, seq: u64, event: &Event) {
            let parked = self.park(event);
            self.inner.publish(session_id, seq, event);
            if parked {
                let _ = self.done.send(());
            }
        }
        fn publish_persisted(&self, session_id: &str, seq: u64, event: &Event) -> bool {
            let parked = self.park(event);
            let persisted = self.inner.publish_persisted(session_id, seq, event);
            if parked {
                let _ = self.done.send(());
            }
            persisted
        }
        fn publish_from_worker(&self, session_id: &str, seq: u64, event: &Event, generation: u64) {
            let parked = self.park(event);
            self.inner
                .publish_from_worker(session_id, seq, event, generation);
            if parked {
                let _ = self.done.send(());
            }
        }
        fn unresolved_background_task_ids(&self, session_id: &str) -> Vec<String> {
            let ids = self.inner.unresolved_background_task_ids(session_id);
            let _ = self.swept.send(());
            ids
        }
        fn unresolved_background_agent_ids(&self, session_id: &str) -> Vec<String> {
            self.inner.unresolved_background_agent_ids(session_id)
        }
    }

    /// A supervisor over a [`GatedSink`] plus the handles to drive its gate.
    struct Parked {
        sup: Arc<Supervisor<GatedSink>>,
        store: Arc<crate::acp::event_store::EventStore>,
        entered: tokio::sync::mpsc::UnboundedReceiver<()>,
        done: tokio::sync::mpsc::UnboundedReceiver<()>,
        swept: tokio::sync::mpsc::UnboundedReceiver<()>,
        release: std::sync::mpsc::Sender<()>,
        _keep: (
            tokio::sync::broadcast::Receiver<crate::server::AcpBroadcastFrame>,
            tempfile::TempDir,
        ),
    }

    fn parked(park_on: fn(&Event) -> bool) -> Parked {
        let (inner, store, rx, store_tmp) = channel_sink();
        let (entered_tx, entered) = tokio::sync::mpsc::unbounded_channel();
        let (done_tx, done) = tokio::sync::mpsc::unbounded_channel();
        let (swept_tx, swept) = tokio::sync::mpsc::unbounded_channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let sup = Arc::new(Supervisor::new(Arc::new(GatedSink {
            inner,
            park_on,
            entered: entered_tx,
            release: std::sync::Mutex::new(release_rx),
            done: done_tx,
            swept: swept_tx,
        })));
        Parked {
            sup,
            store,
            entered,
            done,
            swept,
            release,
            _keep: (rx, store_tmp),
        }
    }

    impl Parked {
        /// Waits until a publish is parked in the gate. The deadline only
        /// bounds a failure.
        async fn entered(&mut self) {
            tokio::time::timeout(Duration::from_secs(5), self.entered.recv())
                .await
                .expect("a publish parked in the gate")
                .expect("sink alive");
        }

        /// Opens the gate and waits for the parked publish to return, so the
        /// event is persisted before anything asserts on the store.
        async fn release_and_wait(&mut self) {
            self.release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), self.done.recv())
                .await
                .expect("the parked publish returned")
                .expect("sink alive");
        }
    }

    /// A start the drain is publishing when the reaper runs must not land
    /// after the final sweep, or the stopped session stays busy for good.
    /// Checkpoints before the gate opens: the drain is parked mid-publish, and
    /// the reaper took the teardown (handle removed, entry `Stopping`) and is
    /// still running. A reaper that does not wait for the drain then sweeps
    /// while the drain is parked, which the sink reports. The reaper is waiting
    /// on the drain when no sweep shows within the observation window. The
    /// assertion follows the acknowledgment that the parked publish returned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn a_start_in_flight_in_the_drain_cannot_outlive_the_reap_sweep() {
        let _home = isolate_home();
        let mut p = parked(|event| matches!(event, Event::BackgroundTaskStarted { .. }));
        let id = "s-reap-inflight-start";
        let (client, _client_tx) =
            crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId(id.into()));
        let lease = p
            .sup
            .test_install_handle(id, client, WorkerKind::Attached, None)
            .await;
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<Event>(16);
        let drain = p.sup.start_drain_task(id.into(), lease, inbound_rx, None);
        {
            let mut workers = p.sup.workers.lock().await;
            let handle = workers.get_mut(id).expect("installed worker");
            handle.drain_task.abort();
            handle.drain_task = drain;
        }
        inbound_tx
            .send(Event::BackgroundTaskStarted {
                task_id: "inflight".into(),
                tool_call_id: None,
                description: "sleep".into(),
            })
            .await
            .unwrap();
        p.entered().await;

        let reaper = {
            let sup = Arc::clone(&p.sup);
            tokio::spawn(async move { sup.reap_user_stopped().await })
        };
        // Checkpoint: the reaper owns the teardown. The deadline only bounds a
        // failure.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while lock_recover(&p.sup.lifecycle).phase(id) != WorkerPhase::Stopping {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the reaper never took the teardown"
            );
            tokio::task::yield_now().await;
        }
        assert!(
            !p.sup.workers.lock().await.contains_key(id),
            "the reaper removed the worker handle"
        );
        // Observation window: a reaper that skips the drain wait sweeps now.
        let swept_while_parked = tokio::time::timeout(Duration::from_millis(300), p.swept.recv())
            .await
            .is_ok();
        p.release_and_wait().await;
        reaper.await.unwrap();

        assert!(
            p.store.unresolved_background_task_ids(id).is_empty(),
            "a start published after the sweep stays open for good \
             (swept while the drain was parked: {swept_while_parked})"
        );
        assert!(
            p.store
                .replay_from(id, 0)
                .iter()
                .any(|(_, event)| matches!(event, Event::Stopped { .. })),
            "the teardown reached its terminal publication"
        );
    }

    /// While a teardown publishes the finishes of the stopped worker's open
    /// work, a replacement must not be admitted: its own task start could
    /// otherwise be finished by that same sweep.
    async fn assert_replacement_fenced_through_sweep(
        p: &mut Parked,
        id: &str,
        teardown: tokio::task::JoinHandle<()>,
    ) {
        // The sweep is now parked publishing the synthetic finish.
        p.entered().await;
        assert!(
            lock_recover(&p.sup.lifecycle)
                .admit(id, ResumeKind::Spawn)
                .is_err(),
            "a replacement was admitted while the sweep was still publishing"
        );
        p.release_and_wait().await;
        tokio::time::timeout(Duration::from_secs(5), teardown)
            .await
            .expect("the released sweep driver settles")
            .unwrap();
        assert!(
            p.store.unresolved_background_task_ids(id).is_empty(),
            "the sweep finished the open task"
        );
        assert!(
            lock_recover(&p.sup.lifecycle)
                .admit(id, ResumeKind::Spawn)
                .is_ok(),
            "once the teardown settled, a replacement is admitted again"
        );
    }

    fn open_task(store: &crate::acp::event_store::EventStore, id: &str) {
        store
            .record(
                id,
                1,
                &Event::BackgroundTaskStarted {
                    task_id: "open".into(),
                    tool_call_id: None,
                    description: "sleep".into(),
                },
            )
            .unwrap();
    }

    fn parks_on_finish(event: &Event) -> bool {
        matches!(event, Event::BackgroundTaskFinished { .. })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn the_reaper_fences_replacement_admission_through_its_sweep() {
        let _home = isolate_home();
        let mut p = parked(parks_on_finish);
        let id = "s-fence-reaper";
        open_task(&p.store, id);
        p.sup.hydrate_seqs(p.store.all_session_seqs());
        let socket = worker_registry::socket_path_for(id).unwrap();
        p.sup
            .test_install_runner(id, runner_config(socket), None)
            .await;
        let teardown = {
            let sup = Arc::clone(&p.sup);
            tokio::spawn(async move {
                sup.reap_user_stopped().await;
            })
        };
        assert_replacement_fenced_through_sweep(&mut p, id, teardown).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn an_owned_shutdown_fences_replacement_admission_through_its_sweep() {
        let _home = isolate_home();
        let mut p = parked(parks_on_finish);
        let id = "s-fence-owned";
        open_task(&p.store, id);
        p.sup.hydrate_seqs(p.store.all_session_seqs());
        let socket = worker_registry::socket_path_for(id).unwrap();
        p.sup
            .test_install_runner(id, runner_config(socket), None)
            .await;
        let teardown = {
            let sup = Arc::clone(&p.sup);
            tokio::spawn(async move { sup.shutdown(id).await.expect("shutdown") })
        };
        assert_replacement_fenced_through_sweep(&mut p, id, teardown).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn an_unowned_shutdown_fences_replacement_admission_through_its_sweep() {
        let _home = isolate_home();
        let mut p = parked(parks_on_finish);
        let id = "s-fence-unowned";
        open_task(&p.store, id);
        p.sup.hydrate_seqs(p.store.all_session_seqs());
        let original = attested_test_identity(5656, 1);
        let control = Arc::new(FakeProcessControl::default());
        Arc::get_mut(&mut p.sup).unwrap().process_control = control.clone();
        save_attested_test_record(id, original.pid, original.generation);
        let teardown = {
            let sup = Arc::clone(&p.sup);
            tokio::spawn(async move { sup.shutdown(id).await.expect("unowned shutdown") })
        };
        assert_replacement_fenced_through_sweep(&mut p, id, teardown).await;
        assert_eq!(control.signals(), [(original.pid, "TERM")]);
        assert!(worker_registry::load_strict(id).unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn a_closed_worker_fences_replacement_admission_through_its_detach_sweep() {
        let _home = isolate_home();
        let mut p = parked(parks_on_finish);
        let id = "s-fence-eof";
        open_task(&p.store, id);
        p.sup.hydrate_seqs(p.store.all_session_seqs());
        let (client, _client_tx) =
            crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId(id.into()));
        let lease = p
            .sup
            .test_install_handle(id, client, WorkerKind::Attached, None)
            .await;
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<Event>(16);
        // No registry record, so the closed worker is dropped without a respawn.
        let drain = p.sup.start_drain_task(id.into(), lease, inbound_rx, None);
        drop(inbound_tx);
        let teardown = tokio::spawn(async move { drain.await.unwrap() });
        assert_replacement_fenced_through_sweep(&mut p, id, teardown).await;
    }

    /// A tailer waiting on a task that never reports must not keep the
    /// worker's channel open: with a registry record that still matches the
    /// worker, neither the reaper nor a reconcile steps in, so the drain's own
    /// EOF handling is the only way recovery starts.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn an_abrupt_worker_loss_is_not_held_open_by_a_pending_task_tailer() {
        use crate::acp::background_agent::TailerStart;
        use crate::acp::task_transcript::TaskTailer;

        let _home = isolate_home();
        let (sink, _store, _rx, _store_tmp) = channel_sink();
        let sup = Supervisor::new(sink);
        let id = "s-abrupt-loss";
        save_record(id, std::process::id(), 0);

        // A transcript that holds no notification for the task.
        let claude_home = tempfile::tempdir().unwrap();
        let projects = claude_home.path().join("projects/-enc");
        std::fs::create_dir_all(&projects).unwrap();
        std::fs::write(projects.join("sess.jsonl"), "{\"type\":\"user\"}\n").unwrap();

        let (client, _client_tx) =
            crate::acp::acp_client::AcpClient::fake_for_test(AcpSessionId(id.into()));
        let lease = sup
            .test_install_handle(id, client, WorkerKind::Attached, None)
            .await;
        let (inbound_tx, inbound_rx) = tokio::sync::mpsc::channel::<Event>(16);
        let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
        let (idle_tx, mut idle_rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = TaskTailer {
            claude_home: Some(claude_home.path().to_path_buf()),
            watch: crate::file_watch::FileWatchService::noop(),
            task_id: "task-1".into(),
            tool_call_id: Some("toolu_task".into()),
            session_id: "sess".into(),
            active: Arc::default(),
            event_tx: inbound_tx.downgrade(),
            start: TailerStart::Live,
            idle: Arc::new(move || {
                let _ = idle_tx.send(());
            }),
        }
        .spawn(None)
        .await;
        // The tailer is pending: it scanned the transcript and found no end.
        tokio::time::timeout(Duration::from_secs(10), idle_rx.recv())
            .await
            .expect("the tailer reached its wait")
            .expect("hook alive");

        // The connection dies abruptly.
        drop(inbound_tx);
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("the drain reaches EOF handling without a notification or an external stop")
            .unwrap();
    }

    /// A concurrent stop must not settle a teardown the reaper owns, or a
    /// replacement is admitted before the reaper's sweep has run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[serial_test::serial]
    async fn a_concurrent_shutdown_cannot_settle_a_reaper_owned_teardown() {
        let _home = isolate_home();
        let mut p = parked(parks_on_finish);
        let control = Arc::new(FakeProcessControl::default());
        let original = attested_test_identity(4747, 1);
        control.alive(original.pid);
        Arc::get_mut(&mut p.sup).unwrap().process_control = control.clone();
        let id = "s-fence-reaper-shutdown";
        open_task(&p.store, id);
        p.sup.hydrate_seqs(p.store.all_session_seqs());
        let socket = worker_registry::socket_path_for(id).unwrap();
        let lease = p
            .sup
            .test_install_runner(id, runner_config(socket), Some(original))
            .await;
        lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
        let reaper = {
            let sup = Arc::clone(&p.sup);
            tokio::spawn(async move {
                sup.reap_user_stopped().await;
            })
        };
        // The reaper's sweep is parked publishing the synthetic finish.
        p.entered().await;

        assert_eq!(control.signals(), [(original.pid, "TERM")]);
        assert!(!control.is_alive(original));
        assert_eq!(p.store.unresolved_background_task_ids(id), ["open"]);
        assert_eq!(
            p.store.replay_from(id, 0).len(),
            1,
            "the parked finish has not extended the durable prefix"
        );
        let stopping_supervisor = Arc::clone(&p.sup);
        let shutdown = stopping_supervisor.shutdown(id);
        tokio::pin!(shutdown);
        assert!(
            futures_util::poll!(&mut shutdown).is_pending(),
            "the concurrent shutdown waits for the reaper's final publication gate"
        );
        assert!(
            lock_recover(&p.sup.lifecycle)
                .admit(id, ResumeKind::Spawn)
                .is_err(),
            "the concurrent shutdown settled the reaper's teardown"
        );
        p.release_and_wait().await;
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .expect("the concurrent shutdown leaves the publication gate")
            .expect("concurrent shutdown");
        tokio::time::timeout(Duration::from_secs(5), reaper)
            .await
            .expect("the captured reaper teardown settles")
            .unwrap();
        assert!(p.store.unresolved_background_task_ids(id).is_empty());
        let completed = p.store.replay_from(id, 0);
        assert_eq!(completed.len(), 3);
        assert!(matches!(&completed[1].1,
            Event::BackgroundTaskFinished { task_id } if task_id == "open"));
        assert!(matches!(&completed[2].1,
            Event::Stopped { reason } if reason == "user_stopped"));
        assert_eq!(p.sup.worker_state(id).await, AcpWorkerState::Absent);
        p.sup.retry_pending_teardowns().await;
        assert_eq!(
            p.store.replay_from(id, 0).len(),
            completed.len(),
            "a retry cannot replay the durable terminal prefix"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn late_restart_markers_authorize_only_the_newest_generation_once() {
        let _home = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let reservation = reserve(
            sup.begin_resume(
                "s-x",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        let newest = reservation.lease().epoch();
        drop(reservation);
        worker_registry::mark_restart_pending("s-x", newest - 1);
        assert!(!sup.take_late_restart_marker("s-x"));
        worker_registry::mark_restart_pending("s-x", newest);
        assert!(sup.take_late_restart_marker("s-x"));
        assert!(
            !sup.take_late_restart_marker("s-x"),
            "a marker authorizes one respawn"
        );
        worker_registry::mark_restart_pending("s-x", 0);
        assert!(
            !sup.take_late_restart_marker("s-x"),
            "a legacy marker is stale once a newer generation was admitted"
        );

        {
            let mut table = lock_recover(&sup.lifecycle);
            let lease = table.admit("s-att", ResumeKind::Attach).unwrap();
            table
                .install(&lease, Some(RunnerIdentity::unattested(999_999_998, 5)))
                .unwrap();
            assert!(table.release_running(&lease));
        }
        worker_registry::mark_restart_pending("s-att", 5);
        assert!(
            sup.take_late_restart_marker("s-att"),
            "a marker for the reattached runner's own generation is honored"
        );
        worker_registry::mark_restart_pending("s-legacy", 0);
        assert!(
            sup.take_late_restart_marker("s-legacy"),
            "a legacy marker for a session this daemon never generated is honored"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn reaper_skips_a_handle_replaced_since_its_snapshot() {
        let _home = isolate_home();
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let socket = worker_registry::socket_path_for("s-reap2").unwrap();
        sup.test_install_runner("s-reap2", runner_config(socket.clone()), None)
            .await;
        let candidates = sup.reap_candidates().await;
        assert_eq!(
            candidates.len(),
            1,
            "no record on disk: the handle is a candidate"
        );

        sup.test_remove_worker("s-reap2").await;
        let replacement = sup
            .test_install_runner("s-reap2", runner_config(socket), None)
            .await;
        let outcome = sup
            .reap_candidate(candidates.into_iter().next().unwrap())
            .await;
        assert_eq!(outcome, None, "a stale candidate must be skipped");
        assert_eq!(
            sup.workers
                .lock()
                .await
                .get("s-reap2")
                .map(|h| h.lease.clone()),
            Some(replacement)
        );
        assert_eq!(sup.worker_state("s-reap2").await, AcpWorkerState::Running);
        assert!(stopped_reasons(&sink, "s-reap2").is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_and_wait_refuses_unreadable_records_and_settles_absent_runners() {
        let (_home, _tmp) = isolate_home();
        for (session_id, unreadable_record, malformed_bytes) in [
            ("sw-err", true, None),
            ("sw-malformed", true, Some(b"{\"pid\":".as_slice())),
            ("sw-missing", false, None),
        ] {
            let record_path = worker_registry::record_path(session_id).unwrap();
            if unreadable_record {
                save_record(session_id, std::process::id(), 0);
                std::fs::remove_file(&record_path).unwrap();
                if let Some(bytes) = malformed_bytes {
                    std::fs::write(&record_path, bytes).unwrap();
                } else {
                    std::fs::create_dir(&record_path).unwrap();
                }
                assert!(worker_registry::load_strict(session_id).is_err());
            }
            let sup = Supervisor::new(VecSink::new());
            sup.test_insert_worker(session_id).await;
            let result = tokio::time::timeout(
                Duration::from_secs(1),
                sup.shutdown_and_wait(session_id, Duration::from_secs(2)),
            )
            .await
            .expect("registry admission must settle without waiting for an absent PID");
            if unreadable_record {
                assert!(matches!(result, Err(SupervisorError::Acp(_))));
                assert_eq!(sup.worker_state(session_id).await, AcpWorkerState::Running);
                if let Some(bytes) = malformed_bytes {
                    assert_eq!(std::fs::read(&record_path).unwrap(), bytes);
                }
            } else {
                result.unwrap();
                assert_eq!(sup.worker_state(session_id).await, AcpWorkerState::Absent);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_and_wait_preserves_replacement_control_socket() {
        let (_home, _tmp) = isolate_home();
        let session_id = "sw-replacement-socket";
        let old_pid = std::process::id() + 1;
        let socket_path = worker_registry::socket_path_for(session_id).unwrap();
        let control_path = crate::process::worker::control_socket_sibling(&socket_path);
        let mut record = worker_record(session_id, old_pid, socket_path.clone()).with_generation(1);
        record.runner_identity = Some(attested_test_birth(old_pid, record.generation));
        record.scope = worker_registry::ScopeOwnership::Unscoped;
        worker_registry::save(&record).unwrap();
        let _old_listener = std::os::unix::net::UnixListener::bind(&control_path).unwrap();
        let original_identity = RunnerIdentity::from_record(&record);
        let mut replacement =
            worker_record(session_id, std::process::id(), socket_path).with_generation(2);
        replacement.runner_identity =
            Some(attested_test_birth(replacement.pid, replacement.generation));
        replacement.scope = worker_registry::ScopeOwnership::Unscoped;
        let expected_replacement = serde_json::to_value(&replacement).unwrap();
        let replacement_listener = Arc::new(std::sync::Mutex::new(None));
        let retained_listener = Arc::clone(&replacement_listener);
        let replacement_control = control_path.clone();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(old_pid);
        control.probe_forget(move |retired| {
            assert_eq!(retired, original_identity);
            assert!(worker_registry::load_strict(session_id).unwrap().is_none());
            assert!(!replacement_control.exists());
            let listener = std::os::unix::net::UnixListener::bind(&replacement_control).unwrap();
            worker_registry::save(&replacement).unwrap();
            *retained_listener.lock().unwrap() = Some(listener);
        });
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        let (client, _events) =
            AcpClient::fake_for_test(crate::acp::state::AcpSessionId(session_id.into()));
        sup.test_install_handle(
            session_id,
            client,
            WorkerKind::Stdio,
            Some(original_identity),
        )
        .await;

        sup.shutdown_and_wait(session_id, Duration::from_millis(300))
            .await
            .unwrap();
        assert!(!control.is_alive(original_identity));
        assert_eq!(
            serde_json::to_value(worker_registry::load_strict(session_id).unwrap().unwrap())
                .unwrap(),
            expected_replacement
        );
        let _peer = std::os::unix::net::UnixStream::connect(&control_path)
            .expect("old cleanup must preserve the replacement control listener");
        drop(replacement_listener);
        worker_registry::delete_if_owned(session_id, std::process::id()).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_and_wait_retires_a_term_ignoring_runner_before_reaping() {
        use std::os::unix::process::ExitStatusExt;

        let (_home, tmp) = isolate_home();
        let ready = tmp.path().join("old-runner-ready");
        let mut old = tokio::process::Command::new("/bin/sh")
            .args([
                "-c",
                r#"trap '' TERM; printf ready > "$1"; IFS= read -r ignored"#,
                "sh",
            ])
            .arg(&ready)
            .stdin(std::process::Stdio::piped())
            .process_group(0)
            .kill_on_drop(true)
            .spawn()
            .expect("spawn old runner stand-in");
        let old_pid = old.id().expect("spawned old runner has a PID");
        let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !ready.exists() {
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "old runner stand-in did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let session_id = "sw-physical-retirement";
        let socket_path = worker_registry::socket_path_for(session_id).unwrap();
        let mut record = worker_record(session_id, old_pid, socket_path);
        record.runner_identity = Some(
            crate::process::process_birth_identity(old_pid)
                .unwrap()
                .expect("ready runner has a live process birth identity"),
        );
        worker_registry::save(&record).unwrap();
        let sup = Supervisor::new(VecSink::new());
        let (client, _events) =
            AcpClient::fake_for_test(crate::acp::state::AcpSessionId(session_id.into()));
        sup.test_install_handle(
            session_id,
            client,
            WorkerKind::Stdio,
            Some(RunnerIdentity::from_record(&record)),
        )
        .await;

        sup.shutdown_and_wait(session_id, Duration::from_millis(300))
            .await
            .unwrap();
        let status = old
            .wait()
            .await
            .expect("reap original runner after shutdown");
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(worker_registry::load_strict(session_id).unwrap().is_none());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn permanent_retirement_does_not_delete_a_replacement_native_session() {
        let (_home, tmp) = isolate_home();
        for (replace_lease, same_pid) in [(false, false), (false, true), (true, false)] {
            let control = Arc::new(FakeProcessControl::default());
            control.alive(6651).alive(6652);
            let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
            let (client, _events, deleted) = AcpClient::fake_for_test_recording(
                crate::acp::state::AcpSessionId("s-delete-owned".into()),
            );
            sup.test_install_handle(
                "s-delete-owned",
                client,
                WorkerKind::Attached,
                Some(attested_test_identity(6651, 10)),
            )
            .await;
            let old_unit = "aoe-session-s_2ddelete_2downed-old.scope";
            sup.workers
                .lock()
                .await
                .get("s-delete-owned")
                .unwrap()
                .lease
                .record_scope(worker_registry::ScopeOwnership::Owned {
                    unit: old_unit.into(),
                });
            let retirement = sup.capture_retirement("s-delete-owned").unwrap();
            let replacement_pid = if same_pid { 6651 } else { 6652 };
            let mut replacement = worker_record(
                "s-delete-owned",
                replacement_pid,
                tmp.path().join("replacement"),
            )
            .with_generation(11);
            replacement.runner_identity =
                Some(attested_test_birth(replacement_pid, replacement.generation));
            replacement.stored_acp_session_id = Some("replacement-native-session".into());
            replacement.scope = worker_registry::ScopeOwnership::Owned {
                unit: "aoe-session-s_2ddelete_2downed-replacement.scope".into(),
            };
            let replacement_identity = RunnerIdentity::from_record(&replacement);
            worker_registry::save(&replacement).unwrap();
            let replacement_deleted = if replace_lease {
                sup.test_respawn_worker("s-delete-owned").await;
                let (client, _events, deleted) = AcpClient::fake_for_test_recording(
                    crate::acp::state::AcpSessionId("replacement-native-session".into()),
                );
                sup.workers
                    .lock()
                    .await
                    .get_mut("s-delete-owned")
                    .unwrap()
                    .client = Arc::new(client);
                Some(deleted)
            } else {
                None
            };
            let result = sup
                .shutdown_with_reason("s-delete-owned", "user_stopped", true, Some(&retirement))
                .await;
            assert!(!deleted.load(std::sync::atomic::Ordering::SeqCst));
            if replace_lease {
                assert!(matches!(result, Err(SupervisorError::TeardownPending(_))));
                assert!(!replacement_deleted
                    .unwrap()
                    .load(std::sync::atomic::Ordering::SeqCst));
                assert_eq!(
                    sup.worker_state("s-delete-owned").await,
                    AcpWorkerState::Running
                );
                assert!(control.signals().is_empty());
            } else if same_pid {
                result.unwrap();
                assert_eq!(
                    sup.worker_state("s-delete-owned").await,
                    AcpWorkerState::Stopping
                );
                assert!(sup.forget_session("s-delete-owned").is_err());
                assert!(control.signals().is_empty());
            } else {
                result.unwrap();
                assert!(control.signals().contains(&(6651, "TERM")));
                assert!(!control.signals().iter().any(|(pid, _)| *pid == 6652));
            }
            assert_eq!(
                control.scope_units(),
                if replace_lease || same_pid {
                    Vec::<String>::new()
                } else {
                    vec![old_unit.to_owned()]
                },
                "a replacement epoch or same-PID/new-generation mismatch cannot sweep any scope"
            );
            assert!(control.is_alive(replacement_identity));
            let retained = worker_registry::load("s-delete-owned").unwrap().unwrap();
            assert_eq!(
                (
                    retained.pid,
                    retained.generation,
                    retained.stored_acp_session_id
                ),
                (
                    replacement_pid,
                    11,
                    Some("replacement-native-session".into())
                )
            );
            worker_registry::delete_if_owned_by("s-delete-owned", replacement_identity);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn permanent_transcript_retirement_refuses_unattested_original_ownership() {
        let (_home, tmp) = isolate_home();
        for (id, identity) in [
            ("s-delete-missing", None),
            (
                "s-delete-unattested",
                Some(RunnerIdentity::unattested(6651, 0)),
            ),
        ] {
            let sup = Supervisor::new(VecSink::new());
            let (client, _events, deleted) = AcpClient::fake_for_test_recording(
                crate::acp::state::AcpSessionId("original-native-session".into()),
            );
            let lease = sup
                .test_install_handle(id, client, WorkerKind::Stdio, identity)
                .await;
            let client = sup.workers.lock().await.get(id).unwrap().client.clone();
            lock_recover(&sup.lifecycle).begin_stop(id, "user_stopped", true);
            let mut replacement = worker_record(id, 6651, tmp.path().join(id));
            replacement.stored_acp_session_id = Some("replacement-native-session".into());
            worker_registry::save(&replacement).unwrap();
            let path = worker_registry::record_path(id).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            retire_client(&sup.lifecycle, &lease, &client, identity).await;
            assert!(
                !deleted.load(std::sync::atomic::Ordering::SeqCst),
                "an absent or unattested original identity must preserve the replacement transcript"
            );
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn only_permanent_removal_sends_session_delete() {
        use std::io::Write;
        use std::sync::atomic::Ordering;
        let (_home, tmp) = isolate_home();
        let (dead_pid, dead_birth) = {
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", "read -r release"])
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("spawn helper");
            let id = child.id();
            let birth = crate::process::process_birth_identity(id)
                .unwrap()
                .expect("blocked helper has a live process birth identity");
            child
                .stdin
                .take()
                .expect("piped helper input")
                .write_all(b"release\n")
                .unwrap();
            assert!(child.wait().expect("wait for helper").success());
            (id, birth)
        };
        let sup = Supervisor::new(VecSink::new());
        let register = |session: &'static str| {
            let mut record = worker_record(session, dead_pid, tmp.path().join(session));
            record.runner_identity = Some(dead_birth.clone());
            record.stored_acp_session_id = Some("acp-test-id".into());
            worker_registry::save(&record).unwrap();
            let (client, _tx, saw_delete) =
                AcpClient::fake_for_test_recording(crate::acp::state::AcpSessionId(session.into()));
            (client, saw_delete, RunnerIdentity::from_record(&record))
        };

        let (client, keep, identity) = register("s-keep");
        sup.test_install_handle("s-keep", client, WorkerKind::Stdio, Some(identity))
            .await;
        sup.shutdown_and_wait("s-keep", Duration::from_secs(5))
            .await
            .expect("shutdown ok");
        assert!(
            !keep.load(Ordering::SeqCst),
            "a reversible stop keeps the transcript resumable"
        );

        let (client, purge, identity) = register("s-del");
        sup.test_install_handle("s-del", client, WorkerKind::Stdio, Some(identity))
            .await;
        sup.shutdown_and_delete_and_wait("s-del", Duration::from_secs(5))
            .await
            .expect("delete ok");
        assert!(
            purge.load(Ordering::SeqCst),
            "permanent removal sends session/delete"
        );

        let (_unused_client, _, identity) = register("s-escalate");
        let (client, closed, deleted) = AcpClient::fake_for_test_held_close(
            crate::acp::state::AcpSessionId("s-escalate".into()),
        );
        sup.test_install_handle("s-escalate", client, WorkerKind::Stdio, Some(identity))
            .await;
        let stopping = sup.shutdown("s-escalate");
        tokio::pin!(stopping);
        assert!(futures_util::poll!(&mut stopping).is_pending());
        let close_response = closed
            .await
            .expect("original close entered after workers removal");
        sup.shutdown_with_reason("s-escalate", "purged", true, None)
            .await
            .unwrap();
        close_response
            .send(crate::acp::acp_client::CloseSessionOutcome::Unsupported)
            .unwrap();
        stopping.await.unwrap();
        assert_eq!(deleted.await.unwrap(), "acp-test-id");
        assert!(!sup.is_owned("s-escalate").await);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_and_wait_outlasts_a_cancelled_resume() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4343);
        let gate = Gate::default();
        let birth = attested_test_birth(4343, 1);
        let fingerprint = birth.fingerprint();
        let (launcher, deleted) =
            recording_gated_launcher(&gate, 4343, "s-wait", "native-keep-id", Some(birth));
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(launcher),
        );
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    spawn_request("s-wait"),
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        gate.entered.notified().await;
        let original = sup
            .capture_retirement("s-wait")
            .unwrap()
            .identity
            .expect("the gated producer has already recorded its birth proof");
        assert_eq!(original.pid, 4343);
        assert_eq!(original.birth_fingerprint, Some(fingerprint));
        assert_eq!(
            RunnerIdentity::from_record(&worker_registry::load_strict("s-wait").unwrap().unwrap()),
            original,
        );

        let waiter = sup.shutdown_and_wait("s-wait", Duration::from_secs(5));
        tokio::pin!(waiter);
        assert!(
            futures_util::poll!(&mut waiter).is_pending(),
            "shutdown must cancel and wait for the resume to settle"
        );
        gate.open.notify_one();
        waiter.await.expect("cancel is a soft success");
        assert_eq!(sup.worker_state("s-wait").await, AcpWorkerState::Absent);
        assert!(control.signals().contains(&(original.pid, "TERM")));
        assert!(!control.is_alive(original));
        assert!(worker_registry::load_strict("s-wait").unwrap().is_none());
        assert_eq!(stopped_reasons(&sup.sink, "s-wait"), ["user_stopped"]);
        sup.retry_pending_teardowns().await;
        assert_eq!(stopped_reasons(&sup.sink, "s-wait"), ["user_stopped"]);
        assert!(matches!(
            spawner.await.unwrap(),
            Err(SupervisorError::SpawnCancelled(_))
        ));
        assert!(
            !deleted.load(std::sync::atomic::Ordering::SeqCst),
            "a cancelled reversible stop keeps the native transcript"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_during_spawn_tears_down_the_late_runner() {
        let _home = isolate_home();
        for cancelled_before_publication in [false, true] {
            let control = Arc::new(FakeProcessControl::default());
            control.alive(4242);
            let gate = Gate::default();
            let sink = VecSink::new();
            let launcher: super::super::Launcher = if cancelled_before_publication {
                let entered = gate.entered.clone();
                Arc::new(
                    move |config: crate::acp::acp_client::SpawnConfig, _session_id| {
                        let entered = entered.clone();
                        Box::pin(async move {
                            let lease = config
                                .launch_lease
                                .expect("supervisor must retain its producer lease");
                            lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                            assert!(lease
                                .record_runner(attested_test_identity(4242, config.generation)));
                            entered.notify_one();
                            lease.cancellation().cancelled().await;
                            Err(crate::acp::acp_client::AcpError::Spawn(
                                "constructor cancelled before registry publication".into(),
                            ))
                        })
                    },
                )
            } else {
                gated_launcher(&gate, 4242)
            };
            let sup = Arc::new(
                Supervisor::new(sink.clone())
                    .with_process_control(control.clone())
                    .with_launcher(launcher),
            );
            let spawner = {
                let sup = Arc::clone(&sup);
                tokio::spawn(async move {
                    sup.spawn(
                        spawn_request("s-late"),
                        crate::process::worker_registry::ResumeAuthority::Explicit,
                    )
                    .await
                })
            };
            tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
                .await
                .expect("the late producer freezes proof before cancellation");
            assert_eq!(sup.worker_state("s-late").await, AcpWorkerState::Resuming);
            assert!(
                worker_registry::load("s-late").unwrap().is_none(),
                "producer has not published its registry record"
            );

            sup.shutdown("s-late")
                .await
                .expect("cancel is a soft success");
            gate.open.notify_one();
            let result = tokio::time::timeout(Duration::from_secs(1), spawner)
                .await
                .expect("active cancellation must settle the producer without registry publication")
                .unwrap();
            assert!(
                matches!(result, Err(SupervisorError::SpawnCancelled(_))),
                "late spawn must report the cancel, got {result:?}"
            );
            assert!(
                control.signals().contains(&(4242, "TERM")),
                "the runner the late spawn built must be signalled: {:?}",
                control.signals()
            );
            assert!(!control.is_alive(RunnerIdentity::unattested(4242, 0)));
            if !cancelled_before_publication {
                assert_eq!(
                    stopped_reasons(&sink, "s-late"),
                    ["user_stopped"],
                    "the honored stop is published so an adopted turn closes"
                );
            }
            assert!(worker_registry::load("s-late").unwrap().is_none());
            assert!(!sup.workers.lock().await.contains_key("s-late"));
            assert_eq!(sup.worker_state("s-late").await, AcpWorkerState::Absent);
            assert!(
                matches!(
                    sup.begin_resume(
                        "s-late",
                        ResumeKind::Spawn,
                        crate::process::worker_registry::ResumeAuthority::Automatic
                    )
                    .await
                    .unwrap(),
                    ResumeReservationOutcome::Reserved(_)
                ),
                "once settled the session admits a fresh resume"
            );
        }
    }

    /// A stop that lands while the launch then fails still releases the scope
    /// the launch may have created, before the settle.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_launch_failing_under_a_stop_releases_its_scope_before_the_settle() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4444);
        let gate = Gate::default();
        let (entered, open) = (Arc::clone(&gate.entered), Arc::clone(&gate.open));
        let launcher: Launcher = Arc::new(move |config: SpawnConfig, id: AcpSessionId| {
            let (entered, open) = (Arc::clone(&entered), Arc::clone(&open));
            Box::pin(async move {
                let lease = config
                    .launch_lease
                    .as_ref()
                    .expect("supervisor retains its producer lease");
                lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                assert!(lease.record_runner(attested_test_identity(4444, config.generation)));
                entered.notify_one();
                open.notified().await;
                save_attested_test_record(&id.0, 4444, config.generation);
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
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    spawn_request("s-fail"),
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified())
            .await
            .expect("the failed launch captures proof before parking");
        sup.shutdown("s-fail").await.expect("cancel");
        gate.open.notify_one();

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), spawner)
                .await
                .expect("the cancelled failed launch settles")
                .unwrap(),
            Err(SupervisorError::SpawnCancelled(_))
        ));
        assert!(control.signals().contains(&(4444, "TERM")));
        assert_eq!(
            control.releases(),
            [("s-fail".to_string(), Some(WorkerPhase::Stopping))]
        );
        assert_eq!(sup.worker_state("s-fail").await, AcpWorkerState::Absent);
    }

    /// A stop arriving after a failed launch began its own teardown gets
    /// `AlreadyStopping`; the teardown still releases the scope before the settle.
    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn a_stop_during_a_failed_launch_teardown_releases_the_scope_before_the_settle() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(4545);
        let launcher: Launcher = Arc::new(move |config: SpawnConfig, id: AcpSessionId| {
            Box::pin(async move {
                let lease = config
                    .launch_lease
                    .as_ref()
                    .expect("supervisor retains its producer lease");
                lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                assert!(lease.record_runner(attested_test_identity(4545, config.generation)));
                save_attested_test_record(&id.0, 4545, config.generation);
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
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    spawn_request("s-late-stop"),
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        // Paused time only advances once this task idles, so the teardown
        // waits out its grace until the runner exits below.
        tokio::time::timeout(Duration::from_secs(5), control.terminated())
            .await
            .expect("the failed first launch enters captured TERM teardown");
        assert_eq!(
            sup.worker_state("s-late-stop").await,
            AcpWorkerState::Stopping
        );
        sup.shutdown("s-late-stop").await.expect("already stopping");
        assert!(control.releases().is_empty(), "{:?}", control.releases());
        control.exit(4545);

        assert!(tokio::time::timeout(Duration::from_secs(5), spawner)
            .await
            .expect("the failed launch settles after its captured runner exits")
            .unwrap()
            .is_err());
        assert_eq!(
            control.releases(),
            [("s-late-stop".to_string(), Some(WorkerPhase::Stopping))]
        );
        assert_eq!(
            sup.worker_state("s-late-stop").await,
            AcpWorkerState::Absent
        );
    }

    /// A first launch that fails after creating its runner, with no stop
    /// pending, still releases the scope it created before the settle.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_first_launch_releases_its_scope_before_the_settle() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4646);
        let launcher: Launcher = Arc::new(move |config: SpawnConfig, id: AcpSessionId| {
            Box::pin(async move {
                let lease = config
                    .launch_lease
                    .as_ref()
                    .expect("supervisor retains its producer lease");
                lease.record_scope(worker_registry::ScopeOwnership::Unscoped);
                assert!(lease.record_runner(attested_test_identity(4646, config.generation)));
                save_attested_test_record(&id.0, 4646, config.generation);
                Err(AcpError::Spawn("handshake failed".into()))
            })
        });
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(launcher),
        );
        let lifecycle = Arc::clone(&sup.lifecycle);
        control.probe_phase(move |id| lock_recover(&lifecycle).phase(id));

        assert!(matches!(
            sup.spawn(
                spawn_request("s-nostop"),
                crate::process::worker_registry::ResumeAuthority::Explicit
            )
            .await,
            Err(SupervisorError::Acp(_))
        ));
        assert!(control.signals().contains(&(4646, "TERM")));
        assert_eq!(
            control.releases(),
            [("s-nostop".to_string(), Some(WorkerPhase::Stopping))]
        );
        assert_eq!(sup.worker_state("s-nostop").await, AcpWorkerState::Absent);
    }

    /// A launch whose lease went stale retires only its own runner: a release
    /// would stop every scope of the session, including the newer start's.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_stale_launch_tears_down_its_runner_without_releasing_the_scope() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4242);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4242)),
        );
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.spawn(
                    spawn_request("s-stale"),
                    crate::process::worker_registry::ResumeAuthority::Explicit,
                )
                .await
            })
        };
        gate.entered.notified().await;
        sup.forget_session("s-stale")
            .expect_err("an active producer cannot be forgotten");
        // Force a stale lease for this internal refusal test, not through the public guard.
        lock_recover(&sup.lifecycle).clear();
        let newer = reserve(
            sup.begin_resume(
                "s-stale",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );

        gate.open.notify_one();
        assert!(matches!(
            spawner.await.unwrap(),
            Err(SupervisorError::SpawnCancelled(_))
        ));
        assert!(control.signals().contains(&(4242, "TERM")));
        assert!(control.releases().is_empty(), "{:?}", control.releases());
        assert_eq!(sup.worker_state("s-stale").await, AcpWorkerState::Resuming);
        drop(newer);
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn teardown_retry_holds_the_session_until_the_runner_exits() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(7777);
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        save_attested_test_record("s-imm", 7777, 3);
        let socket = worker_registry::socket_path_for("s-imm").unwrap();
        sup.test_install_runner(
            "s-imm",
            runner_config(socket),
            Some(attested_test_identity(7777, 3)),
        )
        .await;

        sup.shutdown("s-imm").await.expect("shutdown returns");
        assert_eq!(control.signals(), vec![(7777, "TERM"), (7777, "KILL")]);
        assert_eq!(sup.worker_state("s-imm").await, AcpWorkerState::Stopping);
        assert!(!sup.is_running("s-imm").await);
        assert!(sup.is_owned("s-imm").await);
        assert!(
            matches!(
                sup.begin_resume(
                    "s-imm",
                    ResumeKind::Spawn,
                    crate::process::worker_registry::ResumeAuthority::Automatic
                )
                .await,
                Err(SupervisorError::TeardownPending(_))
            ),
            "nothing resumes beside a runner that is not proven dead"
        );
        assert!(worker_registry::load("s-imm").unwrap().is_some());

        sup.retry_pending_teardowns().await;
        assert_eq!(sup.worker_state("s-imm").await, AcpWorkerState::Stopping);
        assert_eq!(control.signals().len(), 3, "each retry signals again");

        control.exit(7777);
        sup.retry_pending_teardowns().await;
        assert_eq!(sup.worker_state("s-imm").await, AcpWorkerState::Absent);
        assert!(worker_registry::load("s-imm").unwrap().is_none());
        assert!(matches!(
            sup.begin_resume(
                "s-imm",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic
            )
            .await
            .unwrap(),
            ResumeReservationOutcome::Reserved(_)
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_stop_during_a_failed_resume_refuses_the_fallback_spawn_once() {
        let _home = isolate_home();
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let reservation = reserve(
            sup.begin_resume(
                "s-lost",
                ResumeKind::Attach,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        sup.shutdown("s-lost")
            .await
            .expect("a stop on a starting lease is a cancel");
        drop(reservation);
        assert!(
            matches!(
                sup.begin_resume(
                    "s-lost",
                    ResumeKind::Spawn,
                    crate::process::worker_registry::ResumeAuthority::Automatic
                )
                .await,
                Err(SupervisorError::TeardownPending(_))
            ),
            "completion debt fences the fallback until the retry pass publishes it"
        );
        sup.retry_pending_teardowns().await;

        assert!(
            matches!(
                sup.begin_resume(
                    "s-lost",
                    ResumeKind::Spawn,
                    crate::process::worker_registry::ResumeAuthority::Automatic
                )
                .await,
                Err(SupervisorError::SpawnCancelled(_))
            ),
            "the fallback spawn must honor the stop"
        );
        assert_eq!(stopped_reasons(&sink, "s-lost"), ["user_stopped"]);
        assert!(
            matches!(
                sup.begin_resume(
                    "s-lost",
                    ResumeKind::Spawn,
                    crate::process::worker_registry::ResumeAuthority::Automatic
                )
                .await,
                Ok(ResumeReservationOutcome::Reserved(_))
            ),
            "a later resume proceeds"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn disk_only_and_orphaned_teardowns_are_finished() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(5858);
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        let lifecycle = Arc::clone(&sup.lifecycle);
        control.probe_phase(move |id| lock_recover(&lifecycle).phase(id));

        save_attested_test_record("s-gone", 6060, 1);
        sup.shutdown("s-gone")
            .await
            .expect("disk-only runner is stoppable");
        assert_eq!(
            control.signals(),
            vec![(6060, "TERM")],
            "a dead leader still gets the group signal"
        );
        assert_eq!(sup.worker_state("s-gone").await, AcpWorkerState::Absent);
        assert!(worker_registry::load("s-gone").unwrap().is_none());
        assert_eq!(
            control.releases(),
            [("s-gone".to_string(), Some(WorkerPhase::Stopping))]
        );

        save_attested_test_record("s-orphan", 5858, 2);
        {
            let mut table = lock_recover(&sup.lifecycle);
            let lease = table.admit("s-orphan", ResumeKind::Spawn).unwrap();
            let identity = attested_test_identity(5858, 2);
            let record = worker_registry::load_strict("s-orphan").unwrap().unwrap();
            lease.record_prime(PrimeOwnership::from_record(&record));
            lease.record_scope(record.scope.clone());
            table.install(&lease, Some(identity)).unwrap();
            // The stop began, then its driver went away before settling.
            assert!(matches!(
                table.begin_stop("s-orphan", "user_stopped", false),
                StopDecision::TearDown { .. }
            ));
        }
        sup.retry_pending_teardowns().await;
        assert_eq!(
            sup.worker_state("s-orphan").await,
            AcpWorkerState::Stopping,
            "a fresh teardown is left to its driver"
        );
        assert_eq!(control.releases().len(), 1, "{:?}", control.releases());
        lock_recover(&sup.lifecycle).age_stopping("s-orphan", TEARDOWN_ORPHAN_GRACE);
        sup.retry_pending_teardowns().await;
        assert_eq!(sup.worker_state("s-orphan").await, AcpWorkerState::Absent);
        assert!(control.signals().contains(&(5858, "TERM")));
        assert!(worker_registry::load("s-orphan").unwrap().is_none());
        assert_eq!(
            control.releases()[1..],
            [("s-orphan".to_string(), Some(WorkerPhase::Stopping))],
            "the reclaimed teardown sweeps the stopped worker's scope before the settle"
        );
    }

    /// A disk-only stop whose driver went away after adopting the runner is
    /// finished by the retry pass, which still owes the scope sweep.
    #[tokio::test]
    #[serial_test::serial]
    async fn an_orphaned_disk_only_stop_still_releases_the_scope() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(5959);
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        let lifecycle = Arc::clone(&sup.lifecycle);
        control.probe_phase(move |id| lock_recover(&lifecycle).phase(id));
        save_attested_test_record("s-adopted", 5959, 3);
        let target = sup.capture_retirement("s-adopted").unwrap();
        let lease = lock_recover(&sup.lifecycle)
            .adopt_for_stop("s-adopted")
            .unwrap();
        lease.record_runner(target.identity.unwrap());
        lease.record_prime(target.prime);
        for scope in target.scopes {
            lease.record_scope(scope);
        }

        lock_recover(&sup.lifecycle).age_stopping("s-adopted", TEARDOWN_ORPHAN_GRACE);
        sup.retry_pending_teardowns().await;
        assert_eq!(sup.worker_state("s-adopted").await, AcpWorkerState::Absent);
        assert!(control.signals().contains(&(5959, "TERM")));
        assert_eq!(
            control.releases(),
            [("s-adopted".to_string(), Some(WorkerPhase::Stopping))]
        );
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn an_unreadable_record_retains_ownership_past_the_retry_cap() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(5757);
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        save_record("s-stuck", 5757, 4);

        sup.shutdown("s-stuck").await.expect("stop is accepted");
        assert_eq!(sup.worker_state("s-stuck").await, AcpWorkerState::Stopping);
        control.exit(5757);
        let record = worker_registry::record_path("s-stuck").unwrap();
        std::fs::remove_file(&record).unwrap();
        std::fs::create_dir_all(&record).unwrap();

        // The stop itself was attempt one; the cap counts retries after it.
        for _ in 1..TEARDOWN_RETRY_CAP {
            sup.retry_pending_teardowns().await;
            assert_eq!(sup.worker_state("s-stuck").await, AcpWorkerState::Stopping);
        }
        sup.retry_pending_teardowns().await;
        assert_eq!(
            sup.worker_state("s-stuck").await,
            AcpWorkerState::Stopping,
            "unreadable ownership evidence cannot prove native settlement"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn captured_prebirth_stop_preserves_unrelated_replacement_observation() {
        use crate::acp::runner_lifecycle::ProcessControl as _;

        let (_home, tmp) = isolate_home();
        let id = "captured-prebirth-replacement";
        let control = Arc::new(ObservedProcessControl::default());
        let supervisor = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        let construction = reserve(
            supervisor
                .begin_resume(
                    id,
                    ResumeKind::SpawnFirst,
                    worker_registry::ResumeAuthority::Automatic,
                )
                .await,
        );
        let target = supervisor.capture_retirement(id).unwrap();
        // Dropping before any launch crosses birth exclusion proves no original child.
        drop(construction);
        let replacement = HeldAttachRunner::new(id, tmp.path(), 18, None);
        let replacement_bytes = std::fs::read(worker_registry::record_path(id).unwrap()).unwrap();
        let observer = reserve(
            supervisor
                .begin_resume(
                    id,
                    ResumeKind::Attach,
                    worker_registry::ResumeAuthority::Automatic,
                )
                .await,
        );
        supervisor
            .shutdown_captured_and_wait(id, &target, Duration::from_secs(5))
            .await
            .unwrap();
        drop(observer);
        assert!(control.signals().is_empty());
        assert!(control.scopes().is_empty());
        assert!(control.is_alive(replacement.identity));
        assert_eq!(
            std::fs::read(worker_registry::record_path(id).unwrap()).unwrap(),
            replacement_bytes
        );
        assert!(worker_registry::is_record_live(&replacement.record));
    }
}
