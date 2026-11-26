//! Request-path forwarders from the API to a session's live client.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::launch::{apply_mode, set_spawn_model};
use super::{
    lock_recover, BroadcastSink, Supervisor, SupervisorError, WorkerKind, WORKER_READY_TIMEOUT,
};
use crate::acp::acp_client::{AcpClient, AcpError, ResetSessionOutcome};
use crate::acp::approvals::{ApprovalDecision, Nonce};
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::event_store::AttachmentBlob;
use crate::acp::runner_lifecycle::WorkerPhase;
use crate::acp::state::Event;

impl<S: BroadcastSink> Supervisor<S> {
    /// Wait until the session's worker is installed; false once no resume is
    /// pending or `deadline` elapses.
    pub(super) async fn wait_for_worker(&self, session_id: &str, deadline: Duration) -> bool {
        let started = Instant::now();
        loop {
            let notified = self.worker_notify.notified();
            tokio::pin!(notified);
            let has_worker = self.workers.lock().await.contains_key(session_id);
            let waiting_for_context =
                lock_recover(&self.pending_context_resets).contains(session_id);
            if has_worker && !waiting_for_context {
                return true;
            }
            if !has_worker
                && lock_recover(&self.lifecycle).phase(session_id) != WorkerPhase::Resuming
            {
                return false;
            }
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return false;
            }
            #[cfg(test)]
            let notified = {
                let mut reported = false;
                std::future::poll_fn(move |cx| {
                    let result = std::future::Future::poll(notified.as_mut(), cx);
                    if result.is_pending() && !reported {
                        reported = true;
                        let _ = self.worker_waits.send(session_id.to_owned());
                    }
                    result
                })
            };
            tokio::pin!(notified);
            if tokio::time::timeout(remaining, &mut notified)
                .await
                .is_err()
            {
                return false;
            }
        }
    }

    pub(super) async fn client_for_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        self.client_for_effect(session_id, false).await
    }

    async fn client_for_effect(
        &self,
        session_id: &str,
        user_input: bool,
    ) -> Result<Arc<AcpClient>, SupervisorError> {
        loop {
            let wait = {
                let workers = self.workers.lock().await;
                let handle = workers
                    .get(session_id)
                    .ok_or_else(|| SupervisorError::UnknownSession(session_id.into()))?;
                let lifecycle = lock_recover(&self.lifecycle);
                if lifecycle.current_lease(session_id).as_ref() != Some(&handle.lease)
                    || lifecycle.phase(session_id) != WorkerPhase::Running
                {
                    return Err(SupervisorError::SpawnCancelled(session_id.into()));
                }
                let scope = handle
                    .lease
                    .native_scope()
                    .map_err(|()| SupervisorError::NativeStoreUnavailable)?;
                match scope {
                    Some(scope) => {
                        let unresolved = std::cell::Cell::new(false);
                        match scope
                            .fence
                            .try_lock_observation_matching(|row| {
                                let established = matches!(
                                    row.acp_establishment,
                                    Some(
                                        crate::acp::establishment::AcpEstablishment::Applied { .. }
                                    )
                                );
                                unresolved.set(user_input && !established);
                                (!user_input || established)
                                    && scope
                                        .choice
                                        .as_ref()
                                        .is_some_and(|choice| choice.matches(row))
                            })
                            .map_err(|_| {
                                if unresolved.get() {
                                    SupervisorError::Establishment(
                                        crate::acp::establishment::EstablishmentRefusal::Unresolved,
                                    )
                                } else {
                                    SupervisorError::SpawnCancelled(session_id.into())
                                }
                            })? {
                            Some(_native) => {
                                if user_input {
                                    handle.client.ensure_established()?;
                                } else {
                                    handle.client.ensure_available()?;
                                }
                                return Ok(Arc::clone(&handle.client));
                            }
                            None => Arc::clone(&scope.fence),
                        }
                    }
                    None => {
                        if self.native_admission_required {
                            return Err(SupervisorError::NativeStoreUnavailable);
                        }
                        if user_input {
                            handle.client.ensure_established()?;
                        } else {
                            handle.client.ensure_available()?;
                        }
                        return Ok(Arc::clone(&handle.client));
                    }
                }
            };
            tokio::task::spawn_blocking(move || wait.lock_observation().map(drop))
                .await
                .map_err(|_| SupervisorError::NativeStoreUnavailable)?
                .map_err(|_| SupervisorError::SpawnCancelled(session_id.into()))?;
            // Reacquire and recheck the actual current actor after contention.
        }
    }

    /// Capture the actual actor without physical admission or historical events.
    /// Availability is qualified later under the frozen scope's native fence.
    pub(crate) async fn current_native_availability(
        &self,
        session_id: &str,
    ) -> Option<crate::acp::state::CurrentNativeAvailability> {
        let workers = self.workers.lock().await;
        let handle = workers.get(session_id)?;
        let scope = handle.lease.native_scope().ok()??;
        Some(crate::acp::state::CurrentNativeAvailability {
            scope,
            owner: handle.client.connected_runner(),
            unavailable: None,
            ready: false,
            client: Arc::clone(&handle.client),
            lifecycle: Arc::clone(&self.lifecycle),
            lease: handle.lease.clone(),
        })
    }

    pub(crate) async fn prepare_initial_prompt(
        &self,
        session_id: &str,
        text: &str,
        attachments: &[crate::acp::event_store::AttachmentBlob],
    ) -> Result<Option<crate::acp::acp_client::PreparedInitialPrompt>, SupervisorError> {
        use crate::acp::establishment::EstablishmentRefusal;
        use crate::acp::state::NativeObservationWait;
        let unconfirmed = || AcpError::Establishment(EstablishmentRefusal::Unconfirmed);
        let original = self
            .current_native_availability(session_id)
            .await
            .ok_or_else(|| SupervisorError::UnknownSession(session_id.to_owned()))?;
        let fence = Arc::clone(&current.scope.fence);
        let expected = {
            let observed = fence
                .lock_observation_project(
                    |row, storage| {
                        let expected = current.initial_prompt_attempt(row, storage)?;
                        if expected.is_some()
                            && !(current.ready && row.require_resolved_acp_selection().is_ok())
                        {
                            return Ok(expected);
                        }
                        row.require_resolved_acp_selection()?;
                        let selected = current
                            .scope
                            .choice
                            .as_ref()
                            .is_some_and(|choice| choice.matches(row));
                        if !current
                            .refresh(session_id, selected)
                            .map_err(|_| EstablishmentRefusal::Unconfirmed)?
                            || !current.ready
                            || !current
                                .qualify_owner(session_id)
                                .map_err(|_| EstablishmentRefusal::Unconfirmed)?
                        {
                            anyhow::bail!(EstablishmentRefusal::Unconfirmed);
                        }
                        Ok(None)
                    },
                    true,
                        return Err(AcpError::Establishment(
                            refusal.unwrap_or(EstablishmentRefusal::Unconfirmed),
                        )
                        .into());
                    }
                },
            };
            expected
        };
        let Some(expected) = expected else {
            return Ok(None);
        };
        let client = Arc::clone(&current.client);
        let cancellation = current.lease.cancellation().child_token();
        drop(current);
        let agent_key = self.agent_key_for_session(session_id).await;
        if crate::acp::agent_profiles::resolve(&agent_key).is_clear_command(text) {
            return Err(SupervisorError::Acp(AcpError::Establishment(
                EstablishmentRefusal::Unresolved,
            )));
        }
        client
            .prepare_initial_prompt(expected, cancellation, text, attachments)
            .await
            .map(Some)
            .map_err(SupervisorError::Acp)
    }

    /// The session's client, after waiting out a mid-resume worker.
    async fn ready_client(&self, session_id: &str) -> Result<Arc<AcpClient>, SupervisorError> {
        self.wait_for_worker(session_id, WORKER_READY_TIMEOUT).await;
        if lock_recover(&self.pending_context_resets).contains(session_id) {
            return Err(AcpError::Spawn(
                "isolated native context initialization is not complete".into(),
            )
            .into());
        }
        let client = self.client_for_effect(session_id, true).await?;
        if crate::process::worker_registry::stop_requested(session_id) {
            return Err(SupervisorError::SpawnCancelled(session_id.into()));
        }
        Ok(client)
    }

    /// Observe establishment and SDK usability before final physical admission.
    pub async fn wait_until_ready(&self, session_id: &str) -> Result<(), SupervisorError> {
        let deadline = tokio::time::Instant::now() + WORKER_READY_TIMEOUT;
        self.wait_for_worker(session_id, WORKER_READY_TIMEOUT).await;
        let (client, lease) = {
            let workers = tokio::time::timeout_at(deadline, self.workers.lock())
                .await
                .map_err(|_| crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?;
            let handle = workers
                .get(session_id)
                .ok_or_else(|| SupervisorError::UnknownSession(session_id.into()))?;
            let lifecycle = lock_recover(&self.lifecycle);
            if lifecycle.current_lease(session_id).as_ref() != Some(&handle.lease)
                || lifecycle.phase(session_id) != WorkerPhase::Running
            {
                return Err(SupervisorError::SpawnCancelled(session_id.into()));
            }
            (Arc::clone(&handle.client), handle.lease.clone())
        };
        let expected = if let Some(projection) = client.establishment_projection() {
            use crate::acp::event_sender::EstablishmentResolution;
            let attempt_id = if let Some(attempt) = projection.attempt() {
                attempt.id
            } else {
                let workers = tokio::time::timeout_at(deadline, self.workers.lock())
                    .await
                    .map_err(|_| crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?;
                let handle = workers
                    .get(session_id)
                    .ok_or_else(|| SupervisorError::UnknownSession(session_id.into()))?;
                if !Arc::ptr_eq(&client, &handle.client) {
                    return Err(SupervisorError::Superseded(session_id.into()));
                }
                handle
                    .lease
                    .native_scope()
                    .map_err(|()| SupervisorError::NativeStoreUnavailable)?
                    .and_then(|scope| scope.choice.as_ref().and_then(|choice| choice.authority))
                    .map(|authority| authority.id)
                    .ok_or(crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?
            };
            let settled = tokio::time::timeout_at(deadline, projection.settled())
                .await
                .map_err(|_| crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?;
            let applied = match settled {
                EstablishmentResolution::Applied(postimage) => postimage,
                EstablishmentResolution::Blocked => {
                    if lease.defers_rate_limit_rejection() {
                        if let Some(rejection) = client.original_establishment_rejection(attempt_id)
                        {
                            {
                                let mut workers = self.workers.lock().await;
                                let mut lifecycle = lock_recover(&self.lifecycle);
                                if lifecycle.current_lease(session_id).as_ref() != Some(&lease)
                                    || !workers.get(session_id).is_some_and(|handle| {
                                        handle.lease == lease
                                            && Arc::ptr_eq(&handle.client, &client)
                                    })
                                {
                                    return Err(SupervisorError::Superseded(session_id.into()));
                                }
                                if !matches!(
                                    lifecycle.begin_connection_retirement(&lease),
                                    crate::acp::runner_lifecycle::StopDecision::TearDown { .. }
                                ) {
                                    return Err(SupervisorError::SpawnCancelled(session_id.into()));
                                }
                                let handle = workers
                                    .remove(session_id)
                                    .expect("the checked failed original worker is current");
                                lease.retirement().record_worker(handle);
                            }
                            self.reap_failed_launch(&lease).await;
                            return Err(SupervisorError::Acp(rejection));
                        }
                    }
                    return Err(crate::acp::establishment::EstablishmentRefusal::Unresolved.into());
                }
                _ => {
                    return Err(crate::acp::establishment::EstablishmentRefusal::Unconfirmed.into())
                }
            };
            if applied.attempt_id != attempt_id {
                return Err(SupervisorError::Superseded(session_id.into()));
            }
            tokio::time::timeout_at(deadline, client.wait_until_usable())
                .await
                .map_err(|_| crate::acp::establishment::EstablishmentRefusal::Unconfirmed)??;
            let cut = projection.emission_cut();
            if !projection.processed_through(cut, deadline).await {
                return Err(crate::acp::establishment::EstablishmentRefusal::Unconfirmed.into());
            }
            Some(applied)
        } else {
            None
        };
        let current = tokio::time::timeout_at(deadline, self.ready_client(session_id))
            .await
            .map_err(|_| crate::acp::establishment::EstablishmentRefusal::Unconfirmed)??;
        if !Arc::ptr_eq(&client, &current) {
            return Err(SupervisorError::Superseded(session_id.into()));
        }
        if let Some(expected) = expected {
            if current
                .establishment_projection()
                .and_then(|projection| projection.applied())
                .as_deref()
                != Some(expected.as_ref())
            {
                return Err(SupervisorError::Superseded(session_id.into()));
            }
        }
        Ok(())
    }

    pub async fn send_prompt(
        &self,
        session_id: &str,
        text: &str,
        attachments: &[AttachmentBlob],
    ) -> Result<(), SupervisorError> {
        let client = self.ready_client(session_id).await?;
        client.send_prompt(text, attachments).await?;
        Ok(())
    }

    /// Drive a conversation reset (a fresh `session/new`) for a clear command,
    /// then re-assert the session's mode.
    pub async fn reset_session_context(
        &self,
        session_id: &str,
        text: &str,
        acp_mode_id: Option<&str>,
        yolo_mode: bool,
    ) -> Result<(), SupervisorError> {
        let started = tokio::time::Instant::now();
        let deadline = started + crate::acp::acp_client::SESSION_RESET_IN_TASK_TIMEOUT;
        let client = tokio::time::timeout_at(deadline, self.ready_client(session_id))
            .await
            .map_err(|_| crate::acp::establishment::EstablishmentRefusal::Unconfirmed)??;
        let agent_key = self.agent_key_for_session(session_id).await;
        self.reset_client_context(
            &client,
            session_id,
            &agent_key,
            text,
            (acp_mode_id, yolo_mode),
            started,
        )
        .await
    }

    pub(crate) async fn reset_client_context(
        &self,
        client: &Arc<AcpClient>,
        session_id: &str,
        agent_key: &str,
        text: &str,
        mode: (Option<&str>, bool),
        started: tokio::time::Instant,
    ) -> Result<(), SupervisorError> {
        let deadline = started + crate::acp::acp_client::SESSION_RESET_IN_TASK_TIMEOUT;
        let outer_deadline = started + crate::acp::acp_client::SESSION_RESET_TIMEOUT;
        let proposal = tokio::time::timeout_at(
            deadline,
            self.prepare_live_fresh(session_id, client, deadline),
        )
        .await
        .map_err(|_| crate::acp::establishment::EstablishmentRefusal::Unconfirmed)?
        .map_err(|error| {
            warn!(target: "acp.establishment", %error, "live Fresh proposal refused");
            error
                .downcast_ref::<crate::acp::establishment::EstablishmentRefusal>()
                .copied()
                .unwrap_or(crate::acp::establishment::EstablishmentRefusal::Unconfirmed)
        })?;
        match client
            .reset_session(text, proposal, deadline, outer_deadline)
            .await?
        {
            ResetSessionOutcome::Reset { new_acp_session_id } => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    new_acp_session_id = %new_acp_session_id,
                    "conversation reset: fresh session/new swapped the ACP session id"
                );
            }
            ResetSessionOutcome::Failed { message } => {
                return Err(SupervisorError::Acp(AcpError::ResetFailed(message)));
            }
        }
        // An explicit persisted mode wins over the yolo bool, as on spawn.
        let mode_id = match mode {
            (Some(id), _) => Some(id.to_string()),
            (None, true) => crate::acp::agent_profiles::resolve(agent_key)
                .yolo_mode_id
                .map(str::to_string),
            (None, false) => None,
        };
        let _ = tokio::time::timeout_at(
            outer_deadline,
            apply_mode(client, session_id, mode_id.as_deref(), "conversation reset"),
        )
        .await;
        Ok(())
    }

    /// Cancel the current turn; an already-exited agent has no turn to cancel.
    pub async fn cancel_prompt(
        &self,
        session_id: &str,
        host_prompt_intent: Option<uuid::Uuid>,
    ) -> Result<(), SupervisorError> {
        debug!(target: "acp.supervisor", session = %session_id, "turn cancel requested");
        if let Some(intent) = host_prompt_intent {
            use crate::acp::acp_client::InitialPromptCancellation;
            let current = self
                .current_native_availability(session_id)
                .await
                .ok_or_else(|| SupervisorError::Superseded(session_id.to_owned()))?;
            let epoch = {
                let lifecycle = lock_recover(&self.lifecycle);
                if lifecycle.phase(session_id) != crate::acp::runner_lifecycle::WorkerPhase::Running
                    || lifecycle.current_lease(session_id).as_ref() != Some(&current.lease)
                    || current.lease.cancellation().is_cancelled()
                {
                    return Err(SupervisorError::Superseded(session_id.to_owned()));
                }
                current.lease.epoch()
            };
            let client = Arc::clone(&current.client);
            drop(current);
            return match client.cancel_initial_prompt(intent).await? {
                InitialPromptCancellation::Missing
                | InitialPromptCancellation::BeforePublication => {
                    Err(SupervisorError::Superseded(session_id.to_owned()))
                }
                InitialPromptCancellation::Pending { seq } => {
                    self.publish_initial_stopped(
                        session_id,
                        Arc::downgrade(&client),
                        epoch,
                        intent,
                        seq,
                    )
                    .await
                }
                InitialPromptCancellation::ConsumerWillSettle
                | InitialPromptCancellation::Issued => Ok(()),
            };
        }
        let client = match self.ready_client(session_id).await {
            Ok(client) => client,
            Err(SupervisorError::Acp(AcpError::AgentExited | AcpError::NotRunning)) => {
                return Ok(())
            }
            Err(error) => return Err(error),
        };
        match client.cancel_prompt().await {
            Ok(()) | Err(AcpError::AgentExited | AcpError::NotRunning) => Ok(()),
            Err(e) => Err(SupervisorError::Acp(e)),
        }
    }

    /// Cancel one subagent of the session; the root turn keeps running. False
    /// when the child is unknown or did not advertise `cancel`.
    pub async fn cancel_subagent(
        &self,
        session_id: &str,
        child_id: &str,
    ) -> Result<bool, SupervisorError> {
        debug!(target: "acp.supervisor", session = %session_id, child = %child_id, "subagent cancel requested");
        let client = self.ready_client(session_id).await?;
        match client.cancel_child(child_id).await {
            Ok(sent) => Ok(sent),
            Err(AcpError::AgentExited) => Ok(false),
            Err(e) => Err(SupervisorError::Acp(e)),
        }
    }

    /// User-initiated "Force stop".
    pub async fn force_end_turn(&self, session_id: &str) {
        debug!(target: "acp.supervisor", session = %session_id, "force stop requested");
        let delivered = match self.client_for_session(session_id).await {
            Ok(client) => client.force_cancel().await.map_err(SupervisorError::Acp),
            Err(error) => Err(error),
        };
        if let Err(error) = delivered {
            warn!(target: "acp.supervisor", session = %session_id, %error, "force stop: cancel not delivered");
        }
        self.publish_next(
            session_id,
            Event::Stopped {
                reason: "user_forced".into(),
            },
        );
    }

    pub async fn set_mode(
        &self,
        session_id: &str,
        mode_id: &str,
        request_nonce: Option<uuid::Uuid>,
    ) -> Result<crate::acp::establishment::NativeOperationCorrelation, SupervisorError> {
        let client = self.ready_client(session_id).await?;
        Ok(client.set_mode(mode_id, request_nonce).await?)
    }

    pub(crate) async fn set_config_option(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
        remember: bool,
        request_nonce: Option<uuid::Uuid>,
    ) -> Result<
        (
            crate::acp::establishment::NativeOperationCorrelation,
            crate::acp::runner_lifecycle::Lease,
        ),
        SupervisorError,
    > {
        let client = self.ready_client(session_id).await?;
        let lease = {
            let workers = self.workers.lock().await;
            let handle = workers
                .get(session_id)
                .ok_or_else(|| SupervisorError::UnknownSession(session_id.into()))?;
            if !Arc::ptr_eq(&client, &handle.client) {
                return Err(SupervisorError::Superseded(session_id.into()));
            }
            handle.lease.clone()
        };
        let operation = client
            .set_config_option(config_id, value, remember, request_nonce)
            .await?;
        Ok((operation, lease))
    }

    /// A watchdog respawn clones the cached spawn config, which the daemon's
    /// persisted model pick never reaches.
    pub async fn refresh_cached_model(&self, session_id: &str, model: &str) {
        if let Some(WorkerKind::Runner { spawn_config }) = self
            .workers
            .lock()
            .await
            .get_mut(session_id)
            .map(|h| &mut h.kind)
        {
            set_spawn_model(spawn_config, Some(model.to_string()));
        }
    }

    /// A watchdog respawn clones the cached spawn config, which a live
    /// `model_config` pick never reaches.
    pub async fn refresh_cached_model_config(
        &self,
        session_id: &str,
        config_id: &str,
        value: &str,
    ) {
        if let Some(WorkerKind::Runner { spawn_config }) = self
            .workers
            .lock()
            .await
            .get_mut(session_id)
            .map(|h| &mut h.kind)
        {
            spawn_config
                .default_model_config
                .insert(config_id.to_string(), value.to_string());
        }
    }

    pub async fn resolve_permission(
        &self,
        session_id: &str,
        nonce: Nonce,
        decision: ApprovalDecision,
        option_id: Option<String>,
    ) -> Result<(), SupervisorError> {
        let client = self.client_for_session(session_id).await?;
        client
            .resolve_permission(nonce, decision, option_id)
            .await?;
        Ok(())
    }

    /// The user dismissed the approval card without answering it.
    pub async fn cancel_permission(
        &self,
        session_id: &str,
        nonce: Nonce,
    ) -> Result<(), SupervisorError> {
        let client = self.client_for_session(session_id).await?;
        client.cancel_permission(nonce).await?;
        Ok(())
    }

    pub async fn resolve_elicitation(
        &self,
        session_id: &str,
        nonce: Nonce,
        resolution: ElicitationResolution,
    ) -> Result<(), SupervisorError> {
        let client = self.client_for_session(session_id).await?;
        client.resolve_elicitation(nonce, resolution).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{ResumeKind, WorkerKind};
    use super::*;
    use crate::acp::state::AcpSessionId;
    use crate::daemon::AcpWorkerState;

    #[tokio::test(start_paused = true)]
    async fn wait_for_worker_blocks_on_a_reservation_until_it_drops() {
        let sup = Arc::new(Supervisor::new(VecSink::new()));
        assert!(
            !sup.wait_for_worker("s-1748", Duration::from_secs(60)).await,
            "with no reservation, wait_for_worker fails fast"
        );

        let reservation = reserve(
            sup.begin_resume(
                "s-1748",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic,
            )
            .await,
        );
        assert!(
            sup.is_running("s-1748").await,
            "a reservation counts as running so the reconciler skips it"
        );
        assert_eq!(sup.worker_state("s-1748").await, AcpWorkerState::Resuming);
        assert!(sup
            .begin_resume(
                "s-1748",
                ResumeKind::Spawn,
                crate::process::worker_registry::ResumeAuthority::Automatic
            )
            .await
            .unwrap()
            .is_none());

        let mut entered = sup.watch_worker_waits();
        let waiter = {
            let sup = Arc::clone(&sup);
            tokio::spawn(
                async move { sup.wait_for_worker("s-1748", Duration::from_secs(60)).await },
            )
        };
        assert_eq!(entered.recv().await.unwrap(), "s-1748");
        assert!(!waiter.is_finished());
        let before = tokio::time::Instant::now();
        drop(reservation);
        let woke = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("reservation drop must notify the waiter")
            .unwrap();
        assert!(!woke);
        assert_eq!(
            tokio::time::Instant::now(),
            before,
            "the wakeup must not depend on a poll timer"
        );
        assert_eq!(sup.worker_state("s-1748").await, AcpWorkerState::Absent);
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_worker_blocks_until_native_context_is_durable() {
        let sup = Arc::new(Supervisor::new(VecSink::new()));
        sup.test_install_handle(
            "s-isolated",
            AcpClient::fake_for_test_dead_connection(AcpSessionId("acp-isolated".into())),
            WorkerKind::Stdio,
            None,
        )
        .await;
        lock_recover(&sup.pending_context_resets).insert("s-isolated".into());

        let mut entered = sup.watch_worker_waits();
        let waiter = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move {
                sup.wait_for_worker("s-isolated", Duration::from_secs(60))
                    .await
            })
        };
        assert_eq!(entered.recv().await.unwrap(), "s-isolated");
        assert!(!waiter.is_finished());

        lock_recover(&sup.pending_context_resets).remove("s-isolated");
        sup.worker_notify.notify_waiters();
        assert!(tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("context acknowledgement must wake the waiter")
            .unwrap());
        sup.test_remove_worker("s-isolated").await;
    }

    #[tokio::test]
    async fn requests_racing_a_force_stop_teardown_are_not_faults() {
        let sup = Supervisor::new(VecSink::new());
        sup.test_install_handle(
            "s-3401",
            AcpClient::fake_for_test_dead_connection(AcpSessionId("acp-3401".into())),
            WorkerKind::Stdio,
            None,
        )
        .await;

        assert!(
            sup.cancel_prompt("s-3401", None).await.is_ok(),
            "the turn a cancel would have ended is already over"
        );
        assert!(
            matches!(
                sup.send_prompt("s-3401", "hi", &[]).await,
                Err(SupervisorError::Acp(AcpError::NotRunning))
            ),
            "a prompt that did not land keeps its typed reason"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reset_session_context_resets_then_reasserts_mode_without_clearing_on_failure() {
        managed::with_managed(|scope| {
            Box::pin(async move {
                let directory = scope.tempdir();
                let (prompt_seen, prompt_received) = tokio::sync::oneshot::channel();
                let mut prompt_seen = Some(prompt_seen);
                let mut generation = 0;
                let (config, native) =
                    scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
                        let id = request.get("id")?;
                        let result = match request["method"].as_str().unwrap() {
                            "initialize" => serde_json::json!({
                                "protocolVersion":1,"agentCapabilities":{}
                            }),
                            "session/new" => {
                                generation += 1;
                                serde_json::json!({
                                    "sessionId":format!("reset-native-{generation}"),
                                    "modes":{
                                        "currentModeId":"code",
                                        "availableModes":[
                                            {"id":"code","name":"Code"},
                                            {"id":"plan","name":"Plan"}
                                        ]
                                    }
                                })
                            }
                            "session/set_mode" => {
                                assert_eq!(request["params"]["modeId"], "plan");
                                serde_json::json!({})
                            }
                            "session/prompt" => {
                                prompt_seen.take().unwrap().send(()).unwrap();
                                return None;
                            }
                            "session/cancel" => return None,
                            other => panic!("unexpected reset operation: {other}"),
                        };
                        Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":result}))
                    });
                let client = scope.spawn(config, "reset-mode").await.unwrap();
                client.ready().await;
                let state = client.app_state();
                let supervisor = &state.acp_supervisor;
                supervisor
                    .reset_session_context(&client.id, "/new", None, false)
                    .await
                    .expect("genuine reset");
                assert_eq!(
                    native.requests(),
                    ["initialize", "session/new", "session/new"],
                    "the clear alias issues New, never Prompt"
                );
                supervisor
                    .reset_session_context(&client.id, "/new", Some("plan"), false)
                    .await
                    .expect("genuine reset with mode");
                tokio::time::timeout(Duration::from_secs(10), async {
                    while !native
                        .requests()
                        .iter()
                        .any(|method| method == "session/set_mode")
                    {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("explicit persisted mode reaches the actual Native after New");
                assert_eq!(
                    native.requests(),
                    [
                        "initialize",
                        "session/new",
                        "session/new",
                        "session/new",
                        "session/set_mode"
                    ]
                );
                client
                    .send_prompt("keep this turn pending", &[])
                    .await
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(10), prompt_received)
                    .await
                    .unwrap()
                    .unwrap();
                let before = state.acp_event_store.highest_seq(&client.id).unwrap();
                let error = supervisor
                    .reset_session_context(&client.id, "/new", None, false)
                    .await
                    .expect_err("busy reset must be rejected");
                assert!(
                    matches!(
                        &error,
                        SupervisorError::Acp(AcpError::ResetFailed(message))
                            if message.contains("turn is in flight")
                    ),
                    "busy classification must remain visible: {error:?}"
                );
                assert!(!state
                    .acp_event_store
                    .replay_from(&client.id, before)
                    .unwrap()
                    .iter()
                    .any(|stored| matches!(stored.event.as_ref(), Event::SessionCleared)));
                assert_eq!(
                    native
                        .requests()
                        .iter()
                        .filter(|method| method.as_str() == "session/new")
                        .count(),
                    3,
                    "busy reset cannot issue another New"
                );
                client.shutdown().await.unwrap();
            })
        })
        .await;
    }
}
