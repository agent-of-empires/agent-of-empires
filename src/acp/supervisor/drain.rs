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
    before_session_env, overlay_env, publish_rejection, refresh_spawn_model_effort,
    resolve_mcp_servers,
};
use super::teardown::{settle_lease, tear_down_replacement, tear_down_runner, wait_for_exit};
use super::{
    lock_recover, next_seq, BroadcastSink, Launcher, ResumeReservation, SeqMap, SharedSet,
    Supervisor, WorkerKind, Workers, MAX_RESPAWNS_IN_WINDOW, RESPAWN_BACKOFF, RESTART_WINDOW,
};
use crate::acp::acp_client::{AcpError, SpawnConfig};
use crate::acp::runner_lifecycle::{
    InstallError, Lease, LifecycleTable, ProcessControl, RunnerIdentity,
};
use crate::acp::state::{AcpSessionId, Event};
use crate::process::worker_registry;

impl<S: BroadcastSink> Supervisor<S> {
    pub(super) fn start_drain_task(
        &self,
        session_id: String,
        lease: Lease,
        inbound: mpsc::Receiver<Event>,
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
    process_control: Arc<dyn ProcessControl>,
    launcher: Launcher,
    notify: Arc<tokio::sync::Notify>,
    startup_failures: SharedSet,
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
    /// The registry entry was deleted under a live handle (`aoe acp stop|kill`).
    UserStopped,
}

impl<S: BroadcastSink> Drain<S> {
    fn publish(&self, event: Event) {
        let seq = next_seq(&self.next_seqs, &self.session_id);
        self.sink.publish(&self.session_id, seq, &event);
    }

    async fn run(self, mut lease: Lease, mut inbound: mpsc::Receiver<Event>) {
        loop {
            let end = self.pump(&mut inbound, lease.epoch()).await;
            warn!(
                target: "acp.supervisor",
                session = %self.session_id,
                agent_unresponsive = end.agent_unresponsive,
                "drain channel closed (agent connection task ended); evaluating respawn"
            );
            if end.agent_unresponsive {
                kill_wedged_runner(&*self.process_control, &self.session_id).await;
            }
            if end.rate_limited {
                info!(
                    target: "acp.supervisor",
                    session = %self.session_id,
                    "rate-limited; dropping worker handle without respawn"
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
                lock_recover(&self.startup_failures).insert(self.session_id.clone());
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

    /// Publish events until the worker's channel closes.
    async fn pump(&self, inbound: &mut mpsc::Receiver<Event>, generation: u64) -> StreamEnd {
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
                    established = true;
                    self.with_cached_config(|config| {
                        info!(
                            target: "acp.supervisor",
                            session = %self.session_id,
                            acp_session_id = %acp_session_id,
                            "caching agent-assigned id for future respawn"
                        );
                        config.stored_acp_session_id = Some(acp_session_id.clone());
                        config.seed_history_replay = false;
                    })
                    .await;
                }
                Event::SessionContextReset { reason } => {
                    self.with_cached_config(|config| {
                        info!(
                            target: "acp.supervisor",
                            session = %self.session_id,
                            %reason,
                            "clearing cached id and any pending fork after a context reset"
                        );
                        config.stored_acp_session_id = None;
                        config.fork_from = None;
                    })
                    .await;
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

    async fn with_cached_config(&self, f: impl FnOnce(&mut SpawnConfig)) {
        let mut guard = self.workers.lock().await;
        if let Some(WorkerKind::Runner { spawn_config }) =
            guard.get_mut(&self.session_id).map(|h| &mut h.kind)
        {
            f(spawn_config);
        }
    }

    /// Remove this epoch's handle; a no-op once a newer epoch replaced it.
    /// Every caller is a terminal arm with no respawn behind it, so this
    /// worker's background-agent tailers die here and nothing will report
    /// their outcome: detach them, or a park leaves the panel showing them
    /// running and holds the sidebar dot lit for its length (#4001). Gated on
    /// the release so a newer epoch's sub-agents, which that epoch's own sweep
    /// owns, are left alone; the detach runs off the `workers` guard because
    /// it reads the store.
    async fn drop_handle(&self, lease: &Lease) {
        let dropped = {
            let mut guard = self.workers.lock().await;
            let dropped = lock_recover(&self.lifecycle).release_running(lease);
            if dropped {
                guard.remove(&self.session_id);
            }
            dropped
        };
        if dropped {
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
            RestartDecision::UserStopped => {
                info!(
                    target: "acp.supervisor",
                    session = %session_id,
                    "worker registry deleted by user (`aoe acp stop|kill`); \
                     dropping WorkerHandle without respawn"
                );
                self.publish(Event::Stopped {
                    reason: "user_stopped".into(),
                });
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
        let begun = lock_recover(&self.lifecycle).begin_respawn(lease);
        let Ok((respawn_lease, previous)) = begun else {
            debug!(
                target: "acp.supervisor",
                session = %session_id,
                "respawn skipped; the session's lease moved on"
            );
            return None;
        };
        let reservation = ResumeReservation {
            lease: respawn_lease.clone(),
            lifecycle: Arc::clone(&self.lifecycle),
            notify: Arc::clone(&self.notify),
        };
        config.generation = respawn_lease.epoch();

        tokio::time::sleep(RESPAWN_BACKOFF).await;
        let cancelled = lock_recover(&self.lifecycle).cancel_requested(&respawn_lease);
        if let Some(reason) = cancelled {
            if lock_recover(&self.lifecycle).convert_to_stopping(&respawn_lease) {
                self.finish_cancelled(&respawn_lease, previous, reason, None)
                    .await;
            }
            return None;
        }

        self.refresh_launch_env(&mut config).await;
        if let Some((wrapper, base)) = &config.wrapper_substitution {
            log_wrapper_substitution(session_id, &config.tool, wrapper, base);
        }
        let launched = (self.launcher)(config.clone(), AcpSessionId(session_id.clone())).await;
        let mut client = match launched {
            Ok(client) => client,
            Err(e) => {
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
            self.publish(Event::AgentStartupError {
                message: "respawned ACP client had no inbound channel".into(),
            });
            self.workers.lock().await.remove(session_id);
            return None;
        };
        let identity = client.runner_pid().map(|pid| RunnerIdentity {
            pid,
            generation: respawn_lease.epoch(),
        });
        let client = Arc::new(client);

        let refused = {
            let mut guard = self.workers.lock().await;
            match lock_recover(&self.lifecycle).install(&respawn_lease, identity) {
                Ok(()) => match guard.get_mut(session_id) {
                    Some(handle) => {
                        handle.client = Arc::clone(&client);
                        handle.lease = respawn_lease.clone();
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
                tear_down_runner(&*self.process_control, session_id, identity).await;
                lock_recover(&self.lifecycle).release_running(&respawn_lease);
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
    async fn refresh_launch_env(&self, config: &mut SpawnConfig) {
        let session_id = &self.session_id;
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

        if config.sandbox_info.is_none() {
            let minted = before_session_env(
                session_id,
                &config.tool,
                config.source_profile.clone().unwrap_or_default(),
                config.cwd.clone(),
            )
            .await;
            let error = match minted {
                Ok(Ok(pairs)) => {
                    overlay_env(&mut config.host_environment, pairs);
                    None
                }
                Ok(Err(e)) => Some(("before_session hook failed", e.to_string())),
                Err(e) => Some(("before_session hook task failed", e.to_string())),
            };
            if let Some((what, error)) = error {
                warn!(
                    target: "acp.supervisor",
                    session = %session_id,
                    error = %error,
                    "{what} on respawn; reusing the environment from the prior launch"
                );
            }
        }

        config.mcp_servers = resolve_mcp_servers(
            &config.agent_key,
            session_id,
            config.source_profile.clone(),
            config.cwd.clone(),
            config.host_environment.clone(),
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
        self.workers.lock().await.remove(session_id);
        let launched = worker_registry::load(session_id)
            .ok()
            .flatten()
            .filter(|r| r.generation == respawn_lease.epoch())
            .map(|r| RunnerIdentity {
                pid: r.pid,
                generation: r.generation,
            });
        // Under a pending stop the replaced runner is retired as well.
        let retire_previous = cancelled.is_some();
        let converted = (launched.is_some() || retire_previous)
            && lock_recover(&self.lifecycle).convert_to_stopping(respawn_lease);
        if converted {
            let settlement = tear_down_replacement(
                &*self.process_control,
                session_id,
                launched,
                previous.filter(|_| retire_previous),
            )
            .await;
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
        let settlement =
            tear_down_replacement(&*self.process_control, &self.session_id, launched, previous)
                .await;
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
    let runner_managed = matches!(
        handle.kind,
        WorkerKind::Runner { .. } | WorkerKind::Attached
    );
    if runner_managed && matches!(worker_registry::load(session_id), Ok(None)) {
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

/// Kill a runner a watchdog declared wedged, so the respawn cannot load against it.
async fn kill_wedged_runner(control: &dyn ProcessControl, session_id: &str) {
    let old_pid = worker_registry::load(session_id)
        .ok()
        .flatten()
        .map(|r| r.pid);
    if let Some(pid) = old_pid {
        if control.is_alive(pid) {
            info!(
                target: "acp.supervisor",
                session = %session_id,
                pid,
                "SIGTERM wedged runner process group before respawn (agent_unresponsive)"
            );
            control.terminate_group(pid);
            wait_for_exit(control, pid, Duration::from_secs(3)).await;
        }
        if control.is_alive(pid) {
            warn!(
                target: "acp.supervisor",
                session = %session_id,
                pid,
                "wedged runner survived SIGTERM grace; escalating to SIGKILL"
            );
            control.kill_group(pid);
            wait_for_exit(control, pid, Duration::from_millis(200)).await;
        }
    }
    if let Ok(socket_path) = worker_registry::socket_path_for(session_id) {
        if socket_path.exists() {
            let _ = std::fs::remove_file(&socket_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::daemon::AcpWorkerState;

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
            let drain = sup.start_drain_task(id.into(), lease, inbound_rx);
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
        let drain = sup.start_drain_task(id.into(), lease, inbound_rx);
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
            let drain = sup.start_drain_task(id.into(), lease, inbound_rx);
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
        save_record("s-resp", 4242, 0);
        let socket = worker_registry::socket_path_for("s-resp").unwrap();
        let lease = sup
            .test_install_runner(
                "s-resp",
                runner_config(socket),
                Some(RunnerIdentity {
                    pid: 4242,
                    generation: 0,
                }),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let drain = sup.start_drain_task("s-resp".into(), lease, inbound_rx);
        drop(inbound_tx);

        gate.entered.notified().await;
        assert_eq!(sup.worker_state("s-resp").await, AcpWorkerState::Resuming);
        sup.shutdown_idle("s-resp").await.expect("cancel");
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
                Some(RunnerIdentity {
                    pid: 4242,
                    generation: 0,
                }),
            )
            .await;
        let (inbound_tx, inbound_rx) = mpsc::channel::<Event>(4);
        let _drain = sup.start_drain_task("s-crash".into(), lease, inbound_rx);
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
        sup.shutdown_idle("s-crash").await.expect("shutdown");
    }
}
