//! Stopping workers: shutdown, reaping user stops, and proving runners dead.

use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

use super::{lock_recover, BroadcastSink, Supervisor, SupervisorError, WorkerHandle, WorkerKind};
use crate::acp::acp_client::{AcpClient, DeleteSessionOutcome};
use crate::acp::runner_lifecycle::{
    Lease, LifecycleTable, ProcessControl, RunnerIdentity, Settlement, StopDecision,
};
use crate::acp::state::{BackgroundAgentStatus, Event};
use crate::daemon::AcpWorkerState;
use crate::process::worker_registry;

/// Grace after SIGTERM before SIGKILL, and after SIGKILL before parking the teardown.
const TEARDOWN_TERM_GRACE: Duration = Duration::from_secs(2);
const TEARDOWN_KILL_GRACE: Duration = Duration::from_millis(500);
const TEARDOWN_POLL: Duration = Duration::from_millis(50);
/// Retries after which a dead runner whose record cannot be settled is released anyway.
const TEARDOWN_RETRY_CAP: u32 = 30;
/// A teardown claimed this long without settling lost its driver; the retry pass takes over.
const TEARDOWN_ORPHAN_GRACE: Duration = Duration::from_secs(15);

impl<S: BroadcastSink> Supervisor<S> {
    /// Stop a worker, keeping its agent-side transcript resumable.
    pub async fn shutdown(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.shutdown_with_reason(session_id, "user_stopped", false)
            .await
    }

    /// Stop a worker reclaimed for inactivity.
    pub async fn shutdown_idle(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.shutdown_with_reason(session_id, "idle_auto_stop", false)
            .await
    }

    /// Stop a worker being permanently discarded, releasing agent-side state
    /// via `session/delete`. Reversible stops must not use this.
    pub async fn shutdown_and_delete(&self, session_id: &str) -> Result<(), SupervisorError> {
        self.shutdown_with_reason(session_id, "user_stopped", true)
            .await
    }

    /// `shutdown`, then wait for the resume/teardown to settle and the runner
    /// process to exit so a following spawn cannot collide on its socket.
    pub async fn shutdown_and_wait(
        &self,
        session_id: &str,
        deadline: Duration,
    ) -> Result<(), SupervisorError> {
        let pid_before = worker_registry::pid_source_for(session_id);
        let start = Instant::now();
        match self.shutdown(session_id).await {
            Ok(()) => {}
            Err(SupervisorError::UnknownSession(_)) => return Ok(()),
            Err(e) => return Err(e),
        }
        loop {
            let notified = self.worker_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if !matches!(
                self.worker_state(session_id).await,
                AcpWorkerState::Resuming | AcpWorkerState::Stopping
            ) {
                break;
            }
            let remaining = deadline.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                break;
            }
            let _ = tokio::time::timeout(remaining, notified).await;
        }
        #[cfg(unix)]
        if let Some(pid) = pid_before {
            let start = Instant::now();
            while start.elapsed() < deadline && worker_registry::is_pid_alive(pid) {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = pid_before;
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                "shutdown_and_wait called on non-Unix host; PID wait is unimplemented for this platform"
            );
        }
        Ok(())
    }

    async fn shutdown_with_reason(
        &self,
        session_id: &str,
        stop_reason: &str,
        delete_adapter_state: bool,
    ) -> Result<(), SupervisorError> {
        // Same lock order as `begin_resume`, so a resume cannot slip between
        // the decision and the handle removal.
        let mut workers = self.workers.lock().await;
        let decision = lock_recover(&self.lifecycle).begin_stop(session_id, stop_reason);
        match decision {
            StopDecision::TearDown { lease, identity } => {
                let handle = workers.remove(session_id);
                drop(workers);
                worker_registry::clear_restart_marker(session_id);
                let Some(handle) = handle else {
                    self.settle(&lease, Settlement::Proven);
                    return Ok(());
                };
                if delete_adapter_state {
                    try_session_delete(&handle.client, session_id).await;
                }
                let _ = handle.client.shutdown().await;
                handle.drain_task.abort();
                let settlement =
                    tear_down_runner(&*self.process_control, session_id, identity).await;
                self.settle(&lease, settlement);
                // Publish now so the UI clears its thinking state before the next reap tick.
                if !is_test_worker(&handle) {
                    // The worker's tailer died with it, so a sub-agent still
                    // running on disk will never get its own terminal event
                    // and `has_active_background_agent` would stay true
                    // forever (#4001). Ahead of the `Stopped`, so a reader
                    // folding the log in order sees it cleared no later than
                    // the turn-end event.
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
                Ok(())
            }
            StopDecision::CancelRequested => {
                drop(workers);
                worker_registry::clear_restart_marker(session_id);
                debug!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "shutdown: resume in flight; it will tear down what it builds"
                );
                Ok(())
            }
            StopDecision::AlreadyStopping => Ok(()),
            StopDecision::NotOwned => {
                // A runner from a previous daemon may still be on disk.
                let Some(record) = worker_registry::load(session_id).ok().flatten() else {
                    drop(workers);
                    return Err(SupervisorError::UnknownSession(session_id.into()));
                };
                let lease = {
                    let mut table = lock_recover(&self.lifecycle);
                    table.note_generation(session_id, record.generation);
                    table.adopt_for_stop(session_id)
                };
                drop(workers);
                worker_registry::clear_restart_marker(session_id);
                let identity = RunnerIdentity {
                    pid: record.pid,
                    generation: record.generation,
                };
                let settlement =
                    tear_down_runner(&*self.process_control, session_id, Some(identity)).await;
                if let Some(lease) = lease {
                    self.settle(&lease, settlement);
                }
                Ok(())
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

    /// Drive every parked teardown once more.
    pub async fn retry_pending_teardowns(&self) {
        let ids = lock_recover(&self.lifecycle).retry_ids_after(TEARDOWN_ORPHAN_GRACE);
        for id in ids {
            let claim = lock_recover(&self.lifecycle).claim_retry(&id, TEARDOWN_ORPHAN_GRACE);
            let Some(claim) = claim else {
                continue;
            };
            let pid = claim.identity.map(|i| i.pid);
            if claim.attempts > TEARDOWN_RETRY_CAP
                && pid.is_none_or(|pid| !self.process_control.is_alive(pid))
            {
                warn!(
                    target: "acp.supervisor",
                    session = %id,
                    pid,
                    attempts = claim.attempts,
                    "runner is dead but its registry record could not be settled; releasing the session"
                );
                self.settle(&claim.lease, Settlement::Proven);
                continue;
            }
            match claim.identity {
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
                    "teardown lost its driver; finishing it from the registry"
                ),
            }
            let killed_before = claim.identity.is_some();
            let settlement =
                tear_down_runner_from(&*self.process_control, &id, claim.identity, killed_before)
                    .await;
            self.settle(&claim.lease, settlement);
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
        let handle = {
            let mut workers = self.workers.lock().await;
            if !lock_recover(&self.lifecycle).release_running(&lease) {
                return None;
            }
            workers.remove(&id)?
        };
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
        let _ = handle.client.shutdown().await;
        handle.drain_task.abort();
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
    match worker_registry::load(session_id) {
        Ok(None) => true,
        Ok(Some(record)) => {
            identity.is_some_and(|i| !i.matches_record(record.pid, record.generation))
        }
        Err(_) => false,
    }
}

/// Fire the experimental `session/delete` for the session's stored ACP id.
/// Every outcome is non-fatal; the caller proceeds to SIGTERM.
async fn try_session_delete(client: &AcpClient, session_id: &str) {
    let id = session_id.to_string();
    let loaded = tokio::task::spawn_blocking(move || worker_registry::load(&id))
        .await
        .map_err(|e| format!("registry load task join failed: {e}"))
        .and_then(|r| r.map_err(|e| format!("worker_registry load failed: {e}")));
    let record = match loaded {
        Ok(record) => record,
        Err(e) => {
            warn!(
                target: "acp.protocol",
                session = %session_id,
                "skipping session/delete: {e}"
            );
            return;
        }
    };
    let (acp_id, adapter) = record.map_or((None, String::new()), |rec| {
        (rec.stored_acp_session_id, rec.agent_key)
    });
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
}

/// Signal the runner behind `identity` (or the registry record) and prove it
/// gone: SIGTERM, then SIGKILL, each with a bounded wait, then settle its record.
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
    let identity = identity.or_else(|| {
        worker_registry::load(session_id)
            .ok()
            .flatten()
            .map(|r| RunnerIdentity {
                pid: r.pid,
                generation: r.generation,
            })
    });
    let Some(identity) = identity else {
        return Settlement::Proven;
    };
    let pid = identity.pid;
    if !killed_before {
        // Sent even to a dead leader: its group's descendants still need the signal.
        control.terminate_group(pid);
        wait_for_exit(control, pid, TEARDOWN_TERM_GRACE).await;
    }
    if control.is_alive(pid) {
        if !killed_before {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                pid,
                "runner ignored SIGTERM; escalating to SIGKILL"
            );
        }
        control.kill_group(pid);
        wait_for_exit(control, pid, TEARDOWN_KILL_GRACE).await;
    }
    if control.is_alive(pid) {
        warn!(
            target: "acp.supervisor",
            session = %session_id,
            pid,
            "runner survived SIGKILL; holding the session until it exits"
        );
        return Settlement::Unproven(identity);
    }
    if !worker_registry::delete_if_owned_by(session_id, pid, identity.generation) {
        warn!(
            target: "acp.supervisor",
            session = %session_id,
            pid,
            "runner exited but its registry record could not be read; retrying settlement"
        );
        return Settlement::Unproven(identity);
    }
    Settlement::Proven
}

/// Retire a respawn's launched runner and, when given, the runner it replaced.
pub(super) async fn tear_down_replacement(
    control: &dyn ProcessControl,
    session_id: &str,
    launched: Option<RunnerIdentity>,
    previous: Option<RunnerIdentity>,
) -> Settlement {
    let settlement = tear_down_runner(control, session_id, launched).await;
    let Some(previous) = previous.filter(|p| Some(*p) != launched) else {
        return settlement;
    };
    match tear_down_runner(control, session_id, Some(previous)).await {
        Settlement::Unproven(_) if settlement == Settlement::Proven => {
            Settlement::Unproven(previous)
        }
        _ => settlement,
    }
}

/// Polls on the tokio clock so paused-time tests advance through the grace.
pub(super) async fn wait_for_exit(control: &dyn ProcessControl, pid: u32, grace: Duration) {
    let deadline = tokio::time::Instant::now() + grace;
    while control.is_alive(pid) && tokio::time::Instant::now() < deadline {
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

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::{ResumeKind, ResumeReservationOutcome};
    use super::*;
    use crate::acp::runner_lifecycle::test_support::FakeProcessControl;
    use std::sync::Arc;

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
        let _reservation = reserve(sup.begin_resume("s-resuming", ResumeKind::Spawn).await);
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
            let identity = generation.map(|generation| RunnerIdentity {
                pid: 999_999_999,
                generation,
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
    async fn late_restart_markers_authorize_only_the_newest_generation_once() {
        let _home = isolate_home();
        let sup = Supervisor::new(VecSink::new());
        let reservation = reserve(sup.begin_resume("s-x", ResumeKind::Spawn).await);
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
                .install(
                    &lease,
                    Some(RunnerIdentity {
                        pid: 999_999_998,
                        generation: 5,
                    }),
                )
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
    async fn shutdown_and_wait_returns_promptly_without_a_pid_source() {
        let (_home, _tmp) = isolate_home();
        // (session, whether an unreadable registry record is planted)
        for (session_id, unreadable_record) in [("sw-err", true), ("sw-missing", false)] {
            if unreadable_record {
                save_record(session_id, std::process::id(), 0);
                let record_path = worker_registry::record_path(session_id).unwrap();
                std::fs::remove_file(&record_path).unwrap();
                std::fs::create_dir(&record_path).unwrap();
                assert!(worker_registry::load(session_id).is_err());
            }
            let sup = Supervisor::new(VecSink::new());
            sup.test_insert_worker(session_id).await;
            let shutdown = sup.shutdown_and_wait(session_id, Duration::from_secs(2));
            tokio::pin!(shutdown);
            assert!(
                matches!(
                    futures_util::poll!(&mut shutdown),
                    std::task::Poll::Ready(Ok(()))
                ),
                "{session_id}: without a PID source shutdown must not enter a poll wait"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_and_wait_preserves_replacement_control_socket() {
        use std::os::unix::process::CommandExt;

        let (_home, tmp) = isolate_home();
        let ready = tmp.path().join("old-runner-ready");
        let mut old = std::process::Command::new("/bin/sh")
            .args([
                "-c",
                r#"trap '' TERM; printf ready > "$1"; exec sleep 60"#,
                "sh",
            ])
            .arg(&ready)
            .process_group(0)
            .spawn()
            .expect("spawn old runner stand-in");
        let old_pid = old.id();
        std::thread::spawn(move || {
            let _ = old.wait();
        });
        let ready_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !ready.exists() {
            assert!(
                tokio::time::Instant::now() < ready_deadline,
                "old runner stand-in did not become ready"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let session_id = "sw-replacement-socket";
        let socket_path = worker_registry::socket_path_for(session_id).unwrap();
        let control_path = crate::process::worker::control_socket_sibling(&socket_path);
        worker_registry::save(&worker_record(session_id, old_pid, socket_path.clone())).unwrap();

        let sup = Supervisor::new(VecSink::new());
        sup.test_insert_worker(session_id).await;
        let replacement_control = control_path.clone();
        let replacement = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now()
                + TEARDOWN_TERM_GRACE
                + TEARDOWN_KILL_GRACE
                + Duration::from_secs(1);
            while worker_registry::load(session_id).unwrap().is_some() {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "old registry record was not removed"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let listener = tokio::net::UnixListener::bind(&replacement_control).unwrap();
            worker_registry::save(&worker_record(session_id, std::process::id(), socket_path))
                .unwrap();
            listener
        });

        sup.shutdown_and_wait(session_id, Duration::from_millis(300))
            .await
            .unwrap();
        let listener = tokio::time::timeout(Duration::from_secs(1), replacement)
            .await
            .expect("replacement must publish during shutdown wait")
            .unwrap();
        assert!(
            control_path.exists(),
            "old runner cleanup removed the replacement control socket"
        );
        drop(listener);
        worker_registry::delete_if_owned(session_id, std::process::id()).unwrap();
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn only_permanent_removal_sends_session_delete() {
        use std::sync::atomic::Ordering;
        let (_home, tmp) = isolate_home();
        let dead_pid = {
            let mut child = std::process::Command::new("/bin/sh")
                .args(["-c", "exit 0"])
                .spawn()
                .expect("spawn helper");
            let id = child.id();
            let _ = child.wait();
            id
        };
        let sup = Supervisor::new(VecSink::new());
        let register = |session: &'static str| {
            let mut record = worker_record(session, dead_pid, tmp.path().join(session));
            record.stored_acp_session_id = Some("acp-test-id".into());
            worker_registry::save(&record).unwrap();
            let (client, _tx, saw_delete) =
                AcpClient::fake_for_test_recording(crate::acp::state::AcpSessionId(session.into()));
            (client, saw_delete)
        };

        let (client, keep) = register("s-keep");
        sup.test_install_handle("s-keep", client, WorkerKind::Stdio, None)
            .await;
        sup.shutdown("s-keep").await.expect("shutdown ok");
        assert!(
            !keep.load(Ordering::SeqCst),
            "a reversible stop keeps the transcript resumable"
        );

        let (client, purge) = register("s-del");
        sup.test_install_handle("s-del", client, WorkerKind::Stdio, None)
            .await;
        sup.shutdown_and_delete("s-del").await.expect("delete ok");
        assert!(
            purge.load(Ordering::SeqCst),
            "permanent removal sends session/delete"
        );
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_and_wait_outlasts_a_cancelled_resume() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4343);
        let gate = Gate::default();
        let sup = Arc::new(
            Supervisor::new(VecSink::new())
                .with_process_control(control)
                .with_launcher(gated_launcher(&gate, 4343)),
        );
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move { sup.spawn(spawn_request("s-wait")).await })
        };
        gate.entered.notified().await;

        let waiter = sup.shutdown_and_wait("s-wait", Duration::from_secs(5));
        tokio::pin!(waiter);
        assert!(
            futures_util::poll!(&mut waiter).is_pending(),
            "shutdown must cancel and wait for the resume to settle"
        );
        gate.open.notify_one();
        waiter.await.expect("cancel is a soft success");
        assert_eq!(sup.worker_state("s-wait").await, AcpWorkerState::Absent);
        assert!(matches!(
            spawner.await.unwrap(),
            Err(SupervisorError::SpawnCancelled(_))
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn shutdown_during_spawn_tears_down_the_late_runner() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.alive(4242);
        let gate = Gate::default();
        let sink = VecSink::new();
        let sup = Arc::new(
            Supervisor::new(sink.clone())
                .with_process_control(control.clone())
                .with_launcher(gated_launcher(&gate, 4242)),
        );
        let spawner = {
            let sup = Arc::clone(&sup);
            tokio::spawn(async move { sup.spawn(spawn_request("s-late")).await })
        };
        gate.entered.notified().await;
        assert_eq!(sup.worker_state("s-late").await, AcpWorkerState::Resuming);

        sup.shutdown("s-late")
            .await
            .expect("cancel is a soft success");
        gate.open.notify_one();
        let result = spawner.await.unwrap();
        assert!(
            matches!(result, Err(SupervisorError::SpawnCancelled(_))),
            "late spawn must report the cancel, got {result:?}"
        );
        assert!(
            control.signals().contains(&(4242, "TERM")),
            "the runner the late spawn built must be signalled: {:?}",
            control.signals()
        );
        assert!(!control.is_alive(4242));
        assert_eq!(
            stopped_reasons(&sink, "s-late"),
            ["user_stopped"],
            "the honored stop is published so an adopted turn closes"
        );
        assert!(worker_registry::load("s-late").unwrap().is_none());
        assert!(!sup.workers.lock().await.contains_key("s-late"));
        assert_eq!(sup.worker_state("s-late").await, AcpWorkerState::Absent);
        assert!(
            matches!(
                sup.begin_resume("s-late", ResumeKind::Spawn).await.unwrap(),
                ResumeReservationOutcome::Reserved(_)
            ),
            "once settled the session admits a fresh resume"
        );
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn teardown_retry_holds_the_session_until_the_runner_exits() {
        let _home = isolate_home();
        let control = Arc::new(FakeProcessControl::default());
        control.immortal(7777);
        let sup = Supervisor::new(VecSink::new()).with_process_control(control.clone());
        save_record("s-imm", 7777, 3);
        let socket = worker_registry::socket_path_for("s-imm").unwrap();
        sup.test_install_runner(
            "s-imm",
            runner_config(socket),
            Some(RunnerIdentity {
                pid: 7777,
                generation: 3,
            }),
        )
        .await;

        sup.shutdown("s-imm").await.expect("shutdown returns");
        assert_eq!(control.signals(), vec![(7777, "TERM"), (7777, "KILL")]);
        assert_eq!(sup.worker_state("s-imm").await, AcpWorkerState::Stopping);
        assert!(!sup.is_running("s-imm").await);
        assert!(sup.is_owned("s-imm").await);
        assert!(
            matches!(
                sup.begin_resume("s-imm", ResumeKind::Spawn).await,
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
            sup.begin_resume("s-imm", ResumeKind::Spawn).await.unwrap(),
            ResumeReservationOutcome::Reserved(_)
        ));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_stop_during_a_failed_resume_refuses_the_fallback_spawn_once() {
        let _home = isolate_home();
        let sink = VecSink::new();
        let sup = Supervisor::new(sink.clone());
        let reservation = reserve(sup.begin_resume("s-lost", ResumeKind::Attach).await);
        sup.shutdown("s-lost")
            .await
            .expect("a stop on a starting lease is a cancel");
        drop(reservation);

        assert!(
            matches!(
                sup.begin_resume("s-lost", ResumeKind::Spawn).await,
                Err(SupervisorError::SpawnCancelled(_))
            ),
            "the fallback spawn must honor the stop"
        );
        assert_eq!(stopped_reasons(&sink, "s-lost"), ["user_stopped"]);
        assert!(
            matches!(
                sup.begin_resume("s-lost", ResumeKind::Spawn).await,
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

        save_record("s-gone", 6060, 1);
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

        save_record("s-orphan", 5858, 2);
        {
            let mut table = lock_recover(&sup.lifecycle);
            let lease = table.admit("s-orphan", ResumeKind::Spawn).unwrap();
            let identity = RunnerIdentity {
                pid: 5858,
                generation: 2,
            };
            table.install(&lease, Some(identity)).unwrap();
            // The stop began, then its driver went away before settling.
            assert!(matches!(
                table.begin_stop("s-orphan", "user_stopped"),
                StopDecision::TearDown { .. }
            ));
        }
        sup.retry_pending_teardowns().await;
        assert_eq!(
            sup.worker_state("s-orphan").await,
            AcpWorkerState::Stopping,
            "a fresh teardown is left to its driver"
        );
        lock_recover(&sup.lifecycle).age_stopping("s-orphan", TEARDOWN_ORPHAN_GRACE);
        sup.retry_pending_teardowns().await;
        assert_eq!(sup.worker_state("s-orphan").await, AcpWorkerState::Absent);
        assert!(control.signals().contains(&(5858, "TERM")));
        assert!(worker_registry::load("s-orphan").unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    #[serial_test::serial]
    async fn a_dead_runner_with_an_unreadable_record_is_released_after_the_retry_cap() {
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
            AcpWorkerState::Absent,
            "past the cap a dead runner's session is released"
        );
    }
}
