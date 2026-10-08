//! Stopping workers: shutdown, reaping user stops, and proving runners dead.

use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::{lock_recover, BroadcastSink, Supervisor, SupervisorError, WorkerHandle, WorkerKind};
use crate::acp::acp_client::{AcpClient, DeleteSessionOutcome};
use crate::acp::runner_lifecycle::{
    Lease, LifecycleTable, RunnerIdentity, Settlement, StopDecision,
};
use crate::acp::state::{BackgroundAgentStatus, Event};
#[cfg(test)]
use crate::daemon::AcpWorkerState;
use crate::process::worker_registry;

/// A teardown claimed this long without settling lost its driver.
const TEARDOWN_ORPHAN_GRACE: Duration = Duration::from_secs(15);

impl<S: BroadcastSink> Supervisor<S> {
    /// Stop the immutable original whose caller already owns its lifecycle receipt.
    pub(crate) fn shutdown(
        &self,
        stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
    ) -> impl std::future::Future<Output = Result<(), SupervisorError>> + Send + 'static {
        self.shutdown_owned(stop, "user_stopped", false, false)
    }

    pub(crate) fn shutdown_idle(
        &self,
        stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
    ) -> impl std::future::Future<Output = Result<(), SupervisorError>> + Send + 'static {
        self.shutdown_owned(stop, "idle_auto_stop", false, false)
    }

    /// The outer purge driver retains its actual non-Clone transaction through
    /// this request, the one session/delete RPC, and all issued-job retirement.
    pub(crate) fn shutdown_and_delete(
        &self,
        stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
    ) -> impl std::future::Future<Output = Result<(), SupervisorError>> + Send + 'static {
        self.shutdown_owned(stop, "user_stopped", true, true)
    }

    pub(crate) fn shutdown_and_require_dead(
        &self,
        stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
    ) -> impl std::future::Future<Output = Result<(), SupervisorError>> + Send + 'static {
        self.shutdown_owned(stop, "user_stopped", false, true)
    }

    pub(crate) async fn shutdown_and_wait(
        &self,
        stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
        deadline: Duration,
    ) -> Result<(), SupervisorError> {
        match tokio::time::timeout(deadline, self.shutdown_and_require_dead(stop.clone())).await {
            Ok(outcome) => outcome,
            Err(_) => Err(SupervisorError::TeardownPending(
                stop.session_id().to_owned(),
            )),
        }
    }

    fn shutdown_owned(
        &self,
        stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
        reason: &'static str,
        delete_adapter_state: bool,
        require_dead: bool,
    ) -> impl std::future::Future<Output = Result<(), SupervisorError>> + Send + 'static {
        let supervisor = self.clone();
        let driver = tokio::spawn(async move {
            supervisor
                .shutdown_with_reason(stop, reason, delete_adapter_state, require_dead)
                .await
        });
        async move {
            driver.await.map_err(|error| {
                SupervisorError::Acp(crate::acp::acp_client::AcpError::Spawn(format!(
                    "owned stop driver: {error}"
                )))
            })?
        }
    }

    async fn shutdown_with_reason(
        &self,
        stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
        stop_reason: &str,
        delete_adapter_state: bool,
        require_dead: bool,
    ) -> Result<(), SupervisorError> {
        let session_id = stop.session_id();
        let check = stop.clone();
        tokio::task::spawn_blocking(move || check.with_scope(|_| Ok(())))
            .await
            .map_err(|_| SupervisorError::TeardownPending(session_id.to_owned()))?
            .map_err(|_| SupervisorError::TeardownPending(session_id.to_owned()))?;
        // Same lock order as begin_resume: no handle can install between the
        // lifecycle decision and removal of the connection we actually own.
        let mut workers = self.workers.lock().await;
        let decision = lock_recover(&self.lifecycle).begin_owned_stop(&stop, stop_reason);
        let (lease, identity, handle) = match decision {
            StopDecision::TearDown { lease, identity } => {
                (Some(lease), identity, workers.remove(session_id))
            }
            // Disk-only executions are stopped from their authoritative journal,
            // never adopted from a mutable registry identity.
            StopDecision::CancelRequested
            | StopDecision::AlreadyStopping
            | StopDecision::NotOwned => (None, None, None),
        };
        drop(workers);
        let clear = stop.clone();
        let _ = tokio::task::spawn_blocking(move || {
            clear.with_scope(|_| {
                worker_registry::clear_restart_marker(clear.session_id());
                Ok(())
            })
        })
        .await;
        if let Some(handle) = &handle {
            if delete_adapter_state {
                try_session_delete(
                    &handle.client,
                    stop.clone(),
                    session_id,
                    identity,
                    handle.native_session_id.as_deref(),
                )
                .await;
            }
            // Owned stdio Child cleanup is valid. Detached runner termination is
            // exclusively the journal's authenticated per-execution stop protocol.
            let _ = handle.client.shutdown().await;
            handle.drain_task.abort();
        }
        // Queued native jobs retain this receipt even if the HTTP observer disappears.
        loop {
            let notified = self.worker_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !lock_recover(&self.lifecycle).owned_stop_has_jobs(&stop) {
                break;
            }
            notified.await;
        }
        let outcome = crate::session::runner_journal::settle(stop.clone()).await;
        let settlement = if outcome.is_ok() {
            Settlement::Proven
        } else {
            Settlement::Unproven(identity)
        };
        let lease = lease.or_else(|| {
            outcome
                .is_ok()
                .then(|| {
                    lock_recover(&self.lifecycle)
                        .claim_owned_stop_retry(&stop)
                        .map(|claim| claim.lease)
                })
                .flatten()
        });
        if let Some(lease) = lease {
            self.settle(&lease, settlement);
        }
        if handle
            .as_ref()
            .is_some_and(|handle| !is_test_worker(handle))
            && stop.with_scope(|_| Ok(())).is_ok()
        {
            for agent_id in self.sink.unresolved_background_agent_ids(session_id) {
                self.publish_next(
                    session_id,
                    &Event::BackgroundAgentCompleted {
                        agent_id,
                        status: BackgroundAgentStatus::Detached,
                        tools: Vec::new(),
                        result: None,
                        warning: None,
                        ended_at: chrono::Utc::now(),
                    },
                );
            }
            self.publish_next(
                session_id,
                &Event::Stopped {
                    reason: stop_reason.into(),
                },
            );
        }
        if require_dead && lock_recover(&self.lifecycle).is_owned(session_id) {
            return Err(SupervisorError::TeardownPending(session_id.to_string()));
        }
        outcome.map_err(|error| {
            warn!(target: "acp.supervisor", session = %session_id, %error,
                "runner journal cannot prove all executions dead; retaining session state");
            SupervisorError::TeardownPending(session_id.to_string())
        })
    }

    /// Drain issued producers and their canonical preparation ACKs without stopping residents.
    pub fn close_admissions(
        &self,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send + 'static {
        let issued = lock_recover(&self.lifecycle).close_admissions();
        async move {
            let mut first_error = None;
            for admission in issued {
                if let Err(error) = admission.drain().await {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    /// Drop every worker handle without killing the runners (daemon restart).
    pub async fn detach_all(&self) {
        let drained: Vec<(String, WorkerHandle)> = self.workers.lock().await.drain().collect();
        info!(
            target: "acp.supervisor",
            count = drained.len(),
            "detaching structured view workers; they continue running. \
             Use `aoe acp stop` to terminate."
        );
        for (id, handle) in drained {
            debug!(target: "acp.supervisor", session = %id, "detaching");
            let _ = handle.client.shutdown().await;
            handle.drain_task.abort();
        }
    }

    pub(super) fn settle(&self, lease: &Lease, settlement: Settlement) {
        settle_lease(&self.lifecycle, &self.worker_notify, lease, settlement);
    }

    /// Retry only the captured execution; absence of identity is never proof.
    pub async fn retry_pending_teardowns(&self, mut on_retired: impl FnMut(&Lease)) {
        let ids = lock_recover(&self.lifecycle).retry_ids_after(TEARDOWN_ORPHAN_GRACE);
        for id in ids {
            let claim = lock_recover(&self.lifecycle).claim_retry(&id, TEARDOWN_ORPHAN_GRACE);
            let Some(claim) = claim else {
                continue;
            };
            let stop = lock_recover(&self.lifecycle).retry_stop(&claim.lease);
            let settlement = if let Some(stop) = stop {
                if crate::session::runner_journal::settle(stop).await.is_ok() {
                    Settlement::Proven
                } else {
                    Settlement::Unproven(claim.identity)
                }
            } else {
                tear_down_runner(&id, claim.identity).await
            };
            self.settle(&claim.lease, settlement);
            if settlement == Settlement::Proven {
                on_retired(&claim.lease);
            }
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
        let (handle, stop_lease) = {
            let mut workers = self.workers.lock().await;
            let mut table = lock_recover(&self.lifecycle);
            if table
                .running(&id)
                .is_none_or(|(running, _)| running != lease)
            {
                return None;
            }
            let StopDecision::TearDown {
                lease: stop_lease, ..
            } = table.begin_lease_stop(&lease, "user_stopped")
            else {
                return None;
            };
            let handle = workers.remove(&id)?;
            // Stop the old drain before settling its captured execution; a
            // registry replacement never changes the nonce this path owns.
            handle.drain_task.abort();
            (handle, stop_lease)
        };
        // Abort only requests cancellation, so joining is what establishes the
        // drain stopped working before this path touches the registry again.
        let WorkerHandle {
            client,
            drain_task,
            restart_history,
            kind,
            lease: _,
            native_session_id,
        } = handle;
        let _ = drain_task.await;
        // A marker authorizes a restart only of the generation that was stopped.
        let generation = identity.map_or(0, |i| i.generation);
        let is_restart = worker_registry::take_restart_marker(&id, generation);
        let reason = if is_restart {
            "restart_pending"
        } else {
            "user_stopped"
        };
        info!(
            target: "acp.supervisor",
            session = %id,
            reason,
            "registry entry gone while worker handle live; tearing down"
        );
        self.publish_next(
            &id,
            &Event::Stopped {
                reason: reason.to_string(),
            },
        );
        let _ = client.shutdown().await;
        let settlement = tear_down_runner(&id, identity).await;
        self.settle(&stop_lease, settlement);
        // `restart_history`, `kind` and `native_session_id` are read through the
        // map entry that is now gone; binding them keeps the destructure
        // exhaustive if a field is added to the handle.
        let _ = (restart_history, kind, native_session_id);
        Some(is_restart)
    }
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
        Ok(Some(record)) => {
            identity.is_some_and(|identity| identity.proves_different_record(&record))
        }
        Err(_) => false,
    }
}

/// Fire the experimental `session/delete` for the session's stored ACP id.
/// Outcomes are non-fatal; journal settlement still gates destructive callers.
async fn try_session_delete(
    client: &AcpClient,
    stop: std::sync::Arc<crate::session::runner_journal::OwnedStop>,
    session_id: &str,
    identity: Option<RunnerIdentity>,
    native_session_id: Option<&str>,
) {
    let Some(identity) = identity else {
        return;
    };
    if identity.launch_nonce.is_none() {
        return;
    }
    let Some(native_session_id) = native_session_id else {
        return;
    };
    if client.session_id.0 != session_id || client.launch_nonce() != identity.launch_nonce {
        warn!(target: "acp.protocol", session = %session_id,
            "skipping session/delete: current client execution ownership is not established");
        return;
    }
    let id = session_id.to_string();
    let loaded = tokio::task::spawn_blocking(move || {
        stop.with_scope(|row| {
            let record = worker_registry::load_strict(&id)?;
            if let Some(record) = &record {
                anyhow::ensure!(
                    identity.matches_record(record) && row.runner_journal.owns_record(record),
                    "session/delete registry file is not the original published witness"
                );
            }
            Ok(record)
        })
    })
    .await;
    let record = match loaded {
        Ok(Ok(Some(record)))
            if identity.matches_record(&record)
                && record.stored_acp_session_id.as_deref() == Some(native_session_id) =>
        {
            record
        }
        _ => {
            warn!(target: "acp.protocol", session = %session_id,
                "skipping session/delete: registry does not name this client execution");
            return;
        }
    };
    let (acp_id, adapter) = (record.stored_acp_session_id, record.agent_key);
    let Some(acp_id) = acp_id else {
        debug!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter,
            "skipping session/delete: no stored ACP session id (pre-handshake or never assigned)"
        );
        return;
    };
    let started = Instant::now();
    let outcome = client.delete_session(acp_id.clone()).await;
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
            "adapter does not support session/delete; proceeding to journal settlement"
        ),
        DeleteSessionOutcome::TimedOut => warn!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter,
            acp_session_id = %acp_id,
            elapsed_ms,
            "session/delete RPC timed out; proceeding to journal settlement"
        ),
        DeleteSessionOutcome::Failed(msg) => warn!(
            target: "acp.protocol",
            session = %session_id,
            adapter = %adapter,
            acp_session_id = %acp_id,
            elapsed_ms,
            "session/delete RPC failed: {msg}; proceeding to journal settlement"
        ),
    }
}

/// Settle a captured nonce, or observe whole-journal quiescence without one.
pub(super) async fn tear_down_runner(
    session_id: &str,
    identity: Option<RunnerIdentity>,
) -> Settlement {
    let outcome = match identity {
        Some(identity) => {
            crate::session::runner_journal::settle_captured_ticket(session_id, identity, false)
                .await
        }
        None => Err(anyhow::anyhow!(
            "no captured native birth; mutable ID discovery cannot authorize teardown"
        )),
    };
    match outcome {
        Ok(()) => Settlement::Proven,
        Err(error) => {
            warn!(target: "acp.supervisor", session = %session_id,
                nonce = ?identity.and_then(|runner| runner.launch_nonce), %error,
                "execution is not proven settled; retaining teardown");
            Settlement::Unproven(identity)
        }
    }
}

/// Retire each captured execution without touching an unrelated replacement.
pub(super) async fn tear_down_replacement(
    session_id: &str,
    launched: Option<RunnerIdentity>,
    previous: Option<RunnerIdentity>,
) -> Settlement {
    let settlement = tear_down_runner(session_id, launched).await;
    let Some(previous) = previous.filter(|p| Some(*p) != launched) else {
        return settlement;
    };
    match tear_down_runner(session_id, Some(previous)).await {
        Settlement::Unproven(_) if settlement == Settlement::Proven => {
            Settlement::Unproven(Some(previous))
        }
        _ => settlement,
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

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{ResumeKind, ResumeReservationOutcome};
    use super::*;
    fn store_session(id: &str, directory: &std::path::Path) -> crate::session::Storage {
        let storage = crate::session::Storage::new_unwatched("teardown").unwrap();
        let mut row = crate::session::Instance::new(id, directory.to_str().unwrap());
        row.id = id.into();
        row.source_profile = "teardown".into();
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        storage
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
        store_session("s-detach", tmp.path());
        let sink = VecSink::new();
        *sink.stale_background_agent_ids.lock().unwrap() = vec!["bg-1".into(), "bg-2".into()];
        let sup = Supervisor::new(sink.clone());
        sup.test_install_runner(
            "s-detach",
            runner_config(tmp.path().join("dummy.sock")),
            None,
        )
        .await;

        sup.shutdown(crate::acp::supervisor::test_support::stop_receipt(
            "s-detach",
        ))
        .await
        .expect("shutdown");

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
        store_session("s-clean", tmp.path());
        store_session("s-resuming", tmp.path());
        // Nothing outstanding: only the Stopped teardown always publishes.
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        sup.test_install_runner(
            "s-clean",
            runner_config(tmp.path().join("dummy.sock")),
            None,
        )
        .await;
        sup.shutdown(crate::acp::supervisor::test_support::stop_receipt(
            "s-clean",
        ))
        .await
        .expect("shutdown");
        let clean = sink.frames.lock().unwrap().len();
        assert_eq!(clean, 1, "only the Stopped, no synthetic completion");

        // `CancelRequested`: a resume is still in flight, no handle installed.
        let sink = VecSink::new();
        *sink.stale_background_agent_ids.lock().unwrap() = vec!["bg-1".into()];
        let sup = Supervisor::new(sink.clone());
        let reservation = reserve(
            sup.begin_resume(
                "s-resuming",
                crate::acp::runner_lifecycle::NativeResume::Spawn,
                stored_origin("s-resuming"),
                false,
            )
            .await,
        );
        let stop = stop_receipt("s-resuming");
        let pending = sup.shutdown(stop);
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pending.as_mut())
                .await
                .is_err()
        );
        drop(reservation);
        tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .expect("real admission retirement releases the stop");
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
        store_session("s-real-teardown", tmp.path());
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

        sup.shutdown(crate::acp::supervisor::test_support::stop_receipt(
            "s-real-teardown",
        ))
        .await
        .expect("shutdown");

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
            store_session(id, _home.1.path());
            let socket = worker_registry::socket_path_for(id).unwrap();
            let identity = generation.map(|generation| RunnerIdentity {
                pid: 999_999_999,
                generation,
                launch_nonce: Some(uuid::Uuid::new_v4()),
                incarnation: None,
                profile_identity: None,
                boot: None,
            });
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

    #[tokio::test]
    #[serial_test::serial]
    async fn strict_stop_refuses_empty_journal_while_admission_driver_is_active() {
        let (_home, temp) = isolate_home();
        store_session("s-admitted", temp.path());
        let sup = Supervisor::new(VecSink::new());
        let reservation = reserve(
            sup.begin_resume(
                "s-admitted",
                crate::acp::runner_lifecycle::NativeResume::Spawn,
                crate::acp::supervisor::test_support::stored_origin("s-admitted"),
                false,
            )
            .await,
        );
        let stop = stop_receipt("s-admitted");
        let pending = sup.shutdown_and_require_dead(stop.clone());
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pending.as_mut())
                .await
                .is_err(),
            "a live admission cannot produce a death proof"
        );
        assert_eq!(
            sup.worker_state("s-admitted").await,
            AcpWorkerState::Resuming
        );
        drop(reservation);
        tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_stop_during_a_failed_resume_requires_explicit_original_override() {
        let _home = isolate_home();
        store_session("s-lost", _home.1.path());
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let reservation = reserve(
            crate::acp::supervisor::test_support::memory_resume(&sup, "s-lost", ResumeKind::Attach)
                .await,
        );
        let stop = stop_receipt("s-lost");
        let pending = sup.shutdown(stop.clone());
        tokio::pin!(pending);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pending.as_mut())
                .await
                .is_err()
        );
        drop(reservation);
        tokio::time::timeout(Duration::from_secs(5), pending)
            .await
            .unwrap()
            .unwrap();
        crate::session::runner_journal::release_owned_stop(&stop).unwrap();

        assert!(
            matches!(
                sup.begin_resume(
                    "s-lost",
                    crate::acp::runner_lifecycle::NativeResume::Spawn,
                    stored_origin("s-lost"),
                    false
                )
                .await,
                Err(SupervisorError::SpawnCancelled(_))
            ),
            "the fallback spawn must honor the stop"
        );
        assert!(
            matches!(
                sup.begin_resume(
                    "s-lost",
                    crate::acp::runner_lifecycle::NativeResume::Spawn,
                    stored_origin("s-lost"),
                    true
                )
                .await,
                Ok(ResumeReservationOutcome::Reserved(_))
            ),
            "a later resume proceeds"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn unknown_history_blocks_destructive_stops_without_a_daemon_handle() {
        let (_home, tmp) = isolate_home();
        let storage = store_session("s-unknown", tmp.path());
        storage
            .update(|rows, _| {
                rows.iter_mut()
                    .find(|row| row.id == "s-unknown")
                    .unwrap()
                    .runner_journal = Default::default();
                Ok(())
            })
            .unwrap();
        let sup = Supervisor::new(VecSink::new());
        let stop = stop_receipt("s-unknown");
        assert!(matches!(
            sup.shutdown_and_delete(stop.clone()).await,
            Err(SupervisorError::TeardownPending(_))
        ));
        assert!(matches!(
            sup.shutdown_and_require_dead(stop.clone()).await,
            Err(SupervisorError::TeardownPending(_))
        ));
        assert_eq!(
            tear_down_runner("s-unknown", None).await,
            Settlement::Unproven(None)
        );
        assert_eq!(storage.load().unwrap()[0].id, "s-unknown");
    }
}
