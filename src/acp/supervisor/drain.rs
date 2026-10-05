//! The drain task: pumps a worker's events into the sink and respawns the
//! worker when its connection ends, within the restart budget.

use std::collections::HashMap;
use std::sync::Arc;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use super::agents::log_wrapper_substitution;
use super::launch::{
    apply_claude_store_pin, before_session_env, overlay_env, publish_rejection,
    refresh_spawn_model_effort, resolve_mcp_servers,
};
use super::teardown::{settle_lease, tear_down_replacement, tear_down_runner};
use super::{
    lock_recover, next_seq, BroadcastSink, Launcher, PendingContextReset, ResumeReservation,
    SeqMap, SharedSet, Supervisor, WorkerKind, Workers, MAX_RESPAWNS_IN_WINDOW, RESPAWN_BACKOFF,
    RESTART_WINDOW,
};
use crate::acp::acp_client::{AcpError, SpawnConfig};
use crate::acp::runner_lifecycle::{
    InstallError, Lease, LifecycleTable, RunnerIdentity, Settlement,
};
use crate::acp::state::{AcpSessionId, Event};

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
            launcher: Arc::clone(&self.launcher),
            notify: Arc::clone(&self.worker_notify),
            startup_failures: Arc::clone(&self.startup_failures),
            pending_context_resets: Arc::clone(&self.pending_context_resets),
            context_reset,
            respawned_in_place: Arc::clone(&self.respawned_in_place),
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
    launcher: Launcher,
    notify: Arc<tokio::sync::Notify>,
    startup_failures: SharedSet,
    pending_context_resets: SharedSet,
    context_reset: Option<PendingContextReset>,
    respawned_in_place: SharedSet,
}

/// What a worker's event stream said before it closed.
#[derive(Default)]
struct StreamEnd {
    agent_unresponsive: bool,
    rate_limited: bool,
    startup_failed: bool,
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
}

impl<S: BroadcastSink> Drain<S> {
    fn publish(&self, event: Event) {
        let seq = next_seq(&self.next_seqs, &self.session_id);
        self.sink.publish(&self.session_id, seq, &event);
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
                let settlement = tear_down_runner(&self.session_id, identity).await;
                if settlement != Settlement::Proven {
                    warn!(target: "acp.supervisor", session = %self.session_id, "wedged execution remains protected; refusing respawn");
                    self.drop_handle(&lease, Some(settlement)).await;
                    return;
                }
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
            if end.startup_failed {
                info!(
                    target: "acp.supervisor",
                    session = %self.session_id,
                    "startup failed before a session was established; leaving the retry to the reconciler"
                );
                lock_recover(&self.startup_failures).insert(self.session_id.clone());
                self.drop_handle(&lease, None).await;
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

    /// Publish events until the worker's channel closes.
    async fn pump(&mut self, inbound: &mut mpsc::Receiver<Event>, generation: u64) -> StreamEnd {
        let mut end = StreamEnd::default();
        let mut established = false;
        while let Some(event) = inbound.recv().await {
            match &event {
                Event::Stopped { reason } => match reason.as_str() {
                    "agent_unresponsive" | "prompt_orphaned" | "user_forced" => {
                        end.agent_unresponsive = true
                    }
                    "rate_limited" => end.rate_limited = true,
                    "stored_session_rejected" => end.startup_failed = true,
                    _ => {}
                },
                Event::AgentStartupError { .. } if !established => end.startup_failed = true,
                Event::AcpSessionAssigned { acp_session_id } => {
                    if let Some(pending) = self.context_reset.take() {
                        self.sink.publish_from_worker(
                            &self.session_id,
                            next_seq(&self.next_seqs, &self.session_id),
                            &Event::SessionContextReset {
                                reason: pending.reason,
                            },
                            generation,
                        );
                        let id = self.session_id.clone();
                        let assigned = acp_session_id.clone();
                        let acknowledged = tokio::task::spawn_blocking(move || {
                            crate::migrations::v033_isolate_sandbox_content::acknowledge_context_reset(
                                &pending.profile,
                                &id,
                                crate::migrations::v033_isolate_sandbox_content::NativeContextView::Structured,
                                pending.generation,
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
                            self.sink.publish_from_worker(
                                &self.session_id,
                                next_seq(&self.next_seqs, &self.session_id),
                                &Event::AgentStartupError {
                                    message: format!(
                                        "Could not commit the isolated native context: {error}"
                                    ),
                                },
                                generation,
                            );
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
            let seq = next_seq(&self.next_seqs, &self.session_id);
            // Tagged so a frame queued by a replaced worker cannot mutate runtime state.
            self.sink
                .publish_from_worker(&self.session_id, seq, &event, generation);
        }
        end
    }

    fn clear_pending_context_reset(&self) -> bool {
        lock_recover(&self.pending_context_resets).remove(&self.session_id)
    }

    /// Remove only this epoch's handle, retaining its lease until the captured execution is proven retired.
    /// Background tracking stops with the handle even when execution retirement remains pending.
    async fn drop_handle(&self, lease: &Lease, settled: Option<Settlement>) {
        let Some((_, identity)) = lock_recover(&self.lifecycle)
            .running(&self.session_id)
            .filter(|(current, _)| current == lease)
        else {
            return;
        };
        let settlement = match settled {
            Some(settlement) => settlement,
            None => tear_down_runner(&self.session_id, identity).await,
        };
        let dropped = {
            let mut guard = self.workers.lock().await;
            let mut table = lock_recover(&self.lifecycle);
            let dropped = if settlement == Settlement::Proven {
                table.release_running(lease)
            } else if table
                .running(&self.session_id)
                .is_some_and(|(current, _)| current == *lease)
            {
                if let crate::acp::runner_lifecycle::StopDecision::TearDown { lease, .. } =
                    table.begin_lease_stop(lease, "drain_closed")
                {
                    table.settle(&lease, settlement);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if dropped {
                guard.remove(&self.session_id);
            }
            dropped
        };
        if dropped {
            self.clear_pending_context_reset();
            self.notify.notify_waiters();
            super::publish::detach_orphaned_background_agents_on(
                &*self.sink,
                &self.next_seqs,
                &self.session_id,
                "the worker that was tracking this sub-agent stopped; tracking stopped",
            );
        }
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
        let previous_admission = config.execution_admission.as_ref()?.clone();
        let original = previous_admission.origin()?;
        let previous_retirement = previous_admission.preparation_retirement();
        let begun = lock_recover(&self.lifecycle).begin_respawn(lease);
        let Ok((respawn_lease, previous)) = begun else {
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                "respawn skipped; the session's lease moved on"
            );
            return None;
        };
        let issued = lock_recover(&self.lifecycle).execution_admission(&respawn_lease);
        issued.set_origin(original.clone()).ok()?;
        let mut reservation = ResumeReservation {
            lease: respawn_lease.clone(),
            lifecycle: Arc::clone(&self.lifecycle),
            notify: Arc::clone(&self.notify),
            execution: previous,
            custody: Some(issued.begin_job()),
            retirement_required: true,
            issued,
        };
        let _body_custody = reservation.issued.begin_job();
        if let Some(retirement) = previous_retirement {
            crate::session::runner_journal::PreparationCustody::await_retired(retirement)
                .await
                .ok()?;
        }

        tokio::time::sleep(RESPAWN_BACKOFF).await;
        let cancelled = lock_recover(&self.lifecycle).cancel_requested(&respawn_lease);
        if let Some(reason) = cancelled {
            if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease, previous) {
                self.finish_cancelled(&respawn_lease, previous, reason, None)
                    .await;
            }
            return None;
        }
        if let Some(previous) = previous {
            let settlement = tear_down_runner(session_id, Some(previous)).await;
            if settlement != Settlement::Proven {
                if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease, Some(previous))
                {
                    settle_lease(&self.lifecycle, &self.notify, &respawn_lease, settlement);
                }
                return None;
            }
        }
        reservation.execution = None;
        let admission = reservation.issued.clone();
        let custody = admission.begin_job();
        let lifecycle = self.lifecycle.clone();
        let preparation_lease = respawn_lease.clone();
        let prepared = tokio::task::spawn_blocking(move || {
            let _custody = custody;
            let (prepared, preparation) = original.prepare(
                &crate::acp::runner_lifecycle::NativeResume::Spawn,
                &admission,
                |commit| {
                    crate::acp::runner_lifecycle::PreparationAuthorization::acquire(
                        lock_recover(&lifecycle),
                        &preparation_lease,
                        &original,
                        false,
                        commit,
                    )
                },
            )?;
            admission.set_prepared_origin(prepared, preparation)?;
            anyhow::Ok(())
        })
        .await;
        if !matches!(prepared, Ok(Ok(()))) {
            warn!(session = %session_id, ?prepared, "respawn preparation refused");
            return None;
        }
        config.generation = reservation.issued.origin()?.generation();
        config.execution_admission = Some(reservation.issued.clone());

        if let Err(error) = super::launch::validate_launch_origin(&reservation.issued).await {
            self.fail_launch(
                &respawn_lease,
                None,
                &config,
                AcpError::Spawn(error.to_string()),
                None,
            )
            .await;
            return None;
        }
        Self::refresh_launch_env(&self.session_id, &mut config).await;
        let cancelled = lock_recover(&self.lifecycle).cancel_requested(&respawn_lease);
        if let Some(reason) = cancelled {
            let converted = lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease, None);
            if converted {
                self.finish_cancelled(&respawn_lease, None, reason, None)
                    .await;
            }
            return None;
        }
        if let Some((wrapper, base)) = &config.wrapper_substitution {
            log_wrapper_substitution(session_id, &config.tool, wrapper, base);
        }
        let mut launch_config = config.clone();
        launch_config.execution_admission = Some(reservation.issued.clone());
        let launched = (self.launcher)(launch_config, AcpSessionId(session_id.clone())).await;
        let mut client = match launched {
            Ok(client) => client,
            Err(e) => {
                self.fail_launch(
                    &respawn_lease,
                    previous,
                    &config,
                    e,
                    reservation.execution(),
                )
                .await;
                return None;
            }
        };
        let identity = reservation.execution().or_else(|| client.runner_identity());
        reservation.execution = identity;
        let Some(inbound) = client.take_inbound() else {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                "respawned client missing inbound receiver; parking",
            );
            self.publish(Event::AgentStartupError {
                message: "respawned ACP client had no inbound channel".into(),
            });

            let _ = client.shutdown().await;
            if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease, identity) {
                let settlement = tear_down_runner(session_id, identity).await;
                settle_lease(&self.lifecycle, &self.notify, &respawn_lease, settlement);
            }
            self.workers.lock().await.remove(session_id);
            return None;
        };

        if let Err(error) = super::launch::validate_launch_origin(&reservation.issued).await {
            let _ = client.shutdown().await;
            self.fail_launch(
                &respawn_lease,
                None,
                &config,
                AcpError::Spawn(error.to_string()),
                identity,
            )
            .await;
            return None;
        }
        let client = Arc::new(client);

        let refused = {
            let mut guard = self.workers.lock().await;
            match lock_recover(&self.lifecycle).install(&respawn_lease, identity) {
                Ok(()) => match guard.get_mut(session_id) {
                    Some(handle) => {
                        handle.client = Arc::clone(&client);
                        handle.lease = respawn_lease.clone();
                        handle.native_session_id = None;
                        if let WorkerKind::Runner { spawn_config } = &mut handle.kind {
                            spawn_config.generation = config.generation;
                            spawn_config.execution_admission = config.execution_admission.take();
                        }
                        None
                    }
                    None => Some(InstallError::Stale),
                },
                Err(refusal) => Some(refusal),
            }
        };
        match refused {
            None => {}
            Some(InstallError::Cancelled { reason }) => {
                let _ = client.shutdown().await;
                self.finish_cancelled(&respawn_lease, previous, reason, identity)
                    .await;
                return None;
            }
            Some(InstallError::Stale) => {
                warn!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "respawn completed under a stale lease; tearing the runner down"
                );
                let _ = client.shutdown().await;
                let settlement = tear_down_runner(session_id, identity).await;
                let mut table = lock_recover(&self.lifecycle);
                if settlement == Settlement::Proven {
                    table.release_running(&respawn_lease);
                } else if table
                    .running(session_id)
                    .is_some_and(|(current, _)| current == respawn_lease)
                {
                    if let crate::acp::runner_lifecycle::StopDecision::TearDown { lease, .. } =
                        table.begin_lease_stop(&respawn_lease, "refused_install")
                    {
                        table.settle(&lease, settlement);
                    }
                }
                return None;
            }
        }
        reservation.installed();
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
        let cwd = config.cwd.clone();
        let defaults = tokio::task::spawn_blocking(move || {
            crate::session::config::repo_config::resolve_config_with_repo_or_warn(&profile, &cwd)
                .acp
                .acp_defaults_for(&agent)
                .cloned()
        })
        .await;
        match defaults {
            Ok(defaults) => refresh_spawn_model_effort(config, defaults.as_ref()),
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
                config.execution_admission.clone(),
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
                apply_claude_store_pin(&mut host_environment, config.claude_store_pin.as_ref());
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

    /// Report a failed relaunch and retire whatever runner this epoch left.
    async fn fail_launch(
        &self,
        respawn_lease: &Lease,
        previous: Option<RunnerIdentity>,
        config: &SpawnConfig,
        e: AcpError,
        captured: Option<RunnerIdentity>,
    ) {
        let session_id = &self.session_id;
        let cancelled = lock_recover(&self.lifecycle).cancel_requested(respawn_lease);
        if let Some(reason) = cancelled.clone() {
            info!(
                target: "acp.supervisor",
                session = %session_id,
                "respawn launch failed under a pending stop: {e}"
            );
            self.publish(Event::Stopped { reason });
        } else {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                "respawn failed: {e}"
            );
            if matches!(e.underlying(), AcpError::IncompatibleAgent(_)) {
                lock_recover(&self.incompatible_binaries)
                    .insert(session_id.clone(), config.spec.command.clone());
            }
            if !publish_rejection(&e, |event| self.publish(event)) {
                self.publish(Event::AgentStartupError {
                    message: format!("ACP agent respawn failed: {e}"),
                });
            }
        }
        let reported = e.issued_execution();
        let issued = captured
            .map(|identity| (Some(identity), reported == Some((Some(identity), true))))
            .or(reported);
        let stopping = (issued.is_some() || cancelled.is_some())
            && lock_recover(&self.lifecycle).convert_to_stopping(
                respawn_lease,
                issued.map(|(identity, _)| identity).unwrap_or(previous),
            );
        self.workers.lock().await.remove(session_id);
        if stopping {
            let mut settlement = match issued {
                Some((_, true)) => Settlement::Proven,
                Some((identity, false)) => tear_down_runner(session_id, identity).await,
                None => Settlement::Proven,
            };
            if cancelled.is_some() && previous.is_some() {
                let old = tear_down_runner(session_id, previous).await;
                if old != Settlement::Proven {
                    settlement = old;
                }
            }
            settle_lease(&self.lifecycle, &self.notify, respawn_lease, settlement);
        }
    }

    /// Honor a stop that raced the respawn: retire the replacement and the runner it replaced.
    async fn finish_cancelled(
        &self,
        respawn_lease: &Lease,
        previous: Option<RunnerIdentity>,
        reason: String,
        launched: Option<RunnerIdentity>,
    ) {
        self.workers.lock().await.remove(&self.session_id);
        let settlement = match launched {
            Some(_) => tear_down_replacement(&self.session_id, launched, previous).await,
            None => tear_down_runner(&self.session_id, previous).await,
        };
        settle_lease(&self.lifecycle, &self.notify, respawn_lease, settlement);
        self.publish(Event::Stopped { reason });
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

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::daemon::AcpWorkerState;
    use crate::process::worker_registry;

    #[tokio::test]
    #[serial_test::serial]
    async fn restart_decision_burns_the_budget() {
        let (_home, tmp) = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let socket = tmp.path().join("budget.sock");
        sup.test_install_runner("s-1", runner_config(socket.clone()), None)
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
    }

    /// A captured journal ticket permits retirement even if its registry row disappears.
    #[tokio::test]
    #[serial_test::serial]
    async fn drain_retires_attached_crash_without_trusting_registry_presence() {
        if super::super::test_support::run_execution_fixture_child().await {
            return;
        }
        for (id, registry_saved) in [("s-attach-rearm", true), ("s-attach-no-row", false)] {
            let (_home, _tmp) = isolate_home();
            let sink = VecSink::new();
            let sup = Supervisor::new(sink.clone());
            let profile = crate::session::Storage::new_unwatched("default")
                .unwrap()
                .profile()
                .to_owned();
            let execution = published_execution(id, &profile, None, false);
            let identity = execution.identity;
            if !registry_saved {
                let record = worker_registry::load_strict(id).unwrap().unwrap();
                assert!(worker_registry::delete_if_owned_by(&record));
            }
            let (mut client, _client_tx) = crate::acp::acp_client::AcpClient::fake_for_test(
                crate::acp::state::AcpSessionId(id.into()),
            );
            client.capture_runner(execution.identity);
            let lease = sup
                .test_install_handle(id, client, WorkerKind::Attached, Some(identity))
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
            tokio::time::timeout(Duration::from_secs(5), drain)
                .await
                .expect("captured execution should retire")
                .unwrap();
            assert!(!sup.workers.lock().await.contains_key(id));
            assert!(!crate::process::worker::is_process_group_alive(
                execution.pid
            ));
            assert!(!lock_recover(&sup.lifecycle).is_owned(id));
            assert_eq!(sup.take_startup_failures(), vec![id.to_string()]);
            assert!(stopped_reasons(&sink, id).is_empty());
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
        for (_, event) in store.replay_from(id, 0) {
            state.apply_event(event).unwrap();
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
    #[serial_test::serial]
    async fn drain_drops_the_handle_without_respawn_on_terminal_signals() {
        let (_home, _tmp) = isolate_home();
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
            ("s-rl", rate_limited, false),
            ("s-startup", vec![startup_error.clone()], true),
            ("s-crash", vec![established, startup_error], false),
        ];
        for (id, events, startup_failure) in cases {
            let sink = VecSink::new();
            let sup = Supervisor::new(sink.clone());
            let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(16);
            let profile = crate::session::Storage::new_unwatched("default")
                .unwrap()
                .profile()
                .to_owned();
            let execution = published_execution(id, &profile, None, false);
            let identity = execution.identity;
            let (mut client, _client_tx) = crate::acp::acp_client::AcpClient::fake_for_test(
                crate::acp::state::AcpSessionId(id.into()),
            );
            client.capture_runner(execution.identity);
            let lease = sup
                .test_install_handle(id, client, WorkerKind::Stdio, Some(identity))
                .await;
            let drain = sup.start_drain_task(id.into(), lease, inbound_rx, None);
            for event in events {
                inbound_tx.send(event).await.unwrap();
            }
            drop(inbound_tx);
            tokio::time::timeout(Duration::from_secs(5), drain)
                .await
                .expect("drain task should retire the captured execution within 5s")
                .unwrap();

            assert!(!sup.workers.lock().await.contains_key(id), "{id}");
            assert_eq!(sup.worker_state(id).await, AcpWorkerState::Absent, "{id}");
            let expected_failures: Vec<String> = if startup_failure {
                vec![id.to_string()]
            } else {
                Vec::new()
            };
            assert_eq!(sup.take_startup_failures(), expected_failures, "{id}");
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn context_reset_ack_uses_its_claimed_generation() {
        let (_home, _temp) = isolate_home();
        let id = "reset-generation";
        let canonical_generation = 7;
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut row = crate::session::Instance::new("Reset", "/tmp");
        row.id = id.into();
        row.lifecycle_generation = canonical_generation;
        row.sandbox_content_resets.push(
            serde_json::from_value(serde_json::json!({
                "slot": "reset-slot", "transaction": "reset-transaction", "tool": row.tool,
                "agent": "claude", "roots": [], "recovery": [],
                "terminal": {"pending": false, "generation": null},
                "structured": {"pending": true, "generation": canonical_generation},
                "retired_terminal": null, "retired_structured": [], "retired_import": false
            }))
            .unwrap(),
        );
        storage
            .update(|rows, _| {
                rows.push(row);
                Ok(())
            })
            .unwrap();
        let sink = VecSink::new();
        let supervisor = Supervisor::new(sink.clone());
        let lease = supervisor.test_install_stdio(id).await;
        assert_ne!(lease.epoch(), canonical_generation);
        let (sender, inbound) = mpsc::channel(4);
        let drain = supervisor.start_drain_task(
            id.into(),
            lease,
            inbound,
            Some(PendingContextReset {
                profile: "default".into(),
                generation: canonical_generation,
                reason: "native content changed".into(),
                transactions: vec!["reset-slot".into()],
            }),
        );
        sender
            .send(Event::AcpSessionAssigned {
                acp_session_id: "fresh-context".into(),
            })
            .await
            .unwrap();
        sender
            .send(Event::Stopped {
                reason: "fixture-complete".into(),
            })
            .await
            .unwrap();
        drop(sender);
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .unwrap()
            .unwrap();
        let row = storage.load().unwrap().remove(0);
        assert_eq!(row.acp_session_id.as_deref(), Some("fresh-context"));
        let lane = serde_json::to_value(&row.sandbox_content_resets[0]).unwrap();
        assert_eq!(lane["structured"]["pending"], false);
        assert!(supervisor.take_startup_failures().is_empty());
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn consecutive_crashes_resume_under_current_admission() {
        let (_home, temp) = isolate_home();
        let id = "two-crashes";
        let (launched, mut launches) = mpsc::unbounded_channel();
        let launcher: Launcher = Arc::new(move |config, session| {
            let launched = launched.clone();
            Box::pin(async move {
                let (client, sender) = crate::acp::acp_client::AcpClient::fake_for_test(session);
                launched.send(sender).unwrap();
                drop(config);
                Ok(client)
            })
        });
        let sink = VecSink::new();
        let supervisor = Supervisor::new(sink.clone()).with_launcher(launcher);
        let lease = supervisor
            .test_install_runner(id, runner_config(temp.path().join("runner.sock")), None)
            .await;
        let (sender, inbound) = mpsc::channel(4);
        let mut drain = supervisor.start_drain_task(id.into(), lease, inbound, None);
        sender
            .send(Event::AcpSessionAssigned {
                acp_session_id: "initial".into(),
            })
            .await
            .unwrap();
        drop(sender);
        for native_id in ["first-replacement", "second-replacement"] {
            let sender = tokio::select! {
                sender = launches.recv() => sender.expect("launcher must remain available"),
                result = &mut drain => { result.unwrap(); panic!("drain exited before {native_id}"); },
                _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("no launch for {native_id}"),
            };
            sender
                .send(Event::AcpSessionAssigned {
                    acp_session_id: native_id.into(),
                })
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let assigned = sink.frames.lock().unwrap().iter().any(|(_, _, event)| matches!(event, Event::AcpSessionAssigned { acp_session_id } if acp_session_id == native_id));
                    if assigned { break; }
                    tokio::task::yield_now().await;
                }
            }).await.unwrap();
            assert_eq!(supervisor.worker_state(id).await, AcpWorkerState::Running);
            if native_id == "second-replacement" {
                let storage = crate::session::Storage::open_unwatched("default").unwrap();
                assert_eq!(storage.load().unwrap()[0].lifecycle_generation, 2);
                drain.abort();
                assert!(tokio::time::timeout(Duration::from_secs(5), &mut drain)
                    .await
                    .unwrap()
                    .unwrap_err()
                    .is_cancelled());
            }
            drop(sender);
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
                generation: 0,
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
            assert!(!events
                .iter()
                .any(|(_, _, event)| matches!(event, Event::AcpSessionAssigned { .. })));
        }
        assert_eq!(supervisor.take_startup_failures(), vec![id.to_string()]);
        assert!(!supervisor.workers.lock().await.contains_key(id));
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

        let (config_tx, mut config_rx) = mpsc::unbounded_channel();
        let held_senders: Arc<std::sync::Mutex<Vec<mpsc::Sender<Event>>>> = Default::default();
        let launcher_senders = Arc::clone(&held_senders);
        let executions: Arc<std::sync::Mutex<Vec<PublishedExecution>>> = Default::default();
        let launcher: Launcher = Arc::new(move |config, session_id| {
            let config_tx = config_tx.clone();
            let senders = Arc::clone(&launcher_senders);
            let executions = Arc::clone(&executions);
            Box::pin(async move {
                let profile = config
                    .managed_profile
                    .as_deref()
                    .expect("respawn launch requires an explicit stored owner");
                let execution = published_execution(
                    &session_id.0,
                    profile,
                    config.execution_admission.as_ref(),
                    false,
                );
                let identity = execution.identity;
                executions.lock().unwrap().push(execution);
                config_tx.send(config).unwrap();
                let (mut client, tx) = crate::acp::acp_client::AcpClient::fake_for_test(session_id);
                client.capture_runner(identity);
                senders.lock().unwrap().push(tx);
                Ok(client)
            })
        });
        let sup = Arc::new(Supervisor::new(VecSink::new()).with_launcher(launcher));
        let profile = crate::session::Storage::new_unwatched("default")
            .unwrap()
            .profile()
            .to_owned();
        let previous = published_execution("s-store", &profile, None, false);
        let socket = worker_registry::socket_path_for("s-store").unwrap();
        let mut config = runner_config(socket);
        config.managed_profile = Some(profile.clone());
        config.source_profile = Some(profile);
        config.claude_store_pin = Some(crate::session::capture::ClaudeStorePin {
            store: selected.clone(),
            exported_default_store: None,
        });
        config.host_environment = vec![("CLAUDE_CONFIG_DIR".into(), "stale".into())];
        config.base_host_environment = vec![("HOME".into(), temp.path().display().to_string())];
        let lease = sup
            .test_install_runner("s-store", config, Some(previous.identity))
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

        let _ = sup
            .shutdown_idle(crate::acp::supervisor::test_support::stop_receipt(
                "s-store",
            ))
            .await;
        held_senders.lock().unwrap().clear();
        tokio::time::timeout(Duration::from_secs(5), drain)
            .await
            .expect("drain should stop")
            .unwrap();
    }
}
