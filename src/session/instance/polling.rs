//! Owning the background `SessionPoller` attached to a running session.

use super::*;
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};

const MANAGED_CAPTURE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

#[cfg(test)]
thread_local! {
    static AFTER_FINAL_PI_DRAIN: std::cell::RefCell<
        Option<Box<dyn FnOnce(&mut Instance)>>,
    > = std::cell::RefCell::new(None);
}

#[cfg(test)]
fn take_after_final_pi_drain_hook() -> Option<Box<dyn FnOnce(&mut Instance)>> {
    AFTER_FINAL_PI_DRAIN.with(|hook| hook.borrow_mut().take())
}

#[cfg(test)]
fn clear_after_final_pi_drain_hook() {
    AFTER_FINAL_PI_DRAIN.with(|hook| {
        let _ = hook.borrow_mut().take();
    });
}

#[cfg(test)]
#[must_use = "binds the Pi drain hook cleanup to this guard's lifetime"]
pub(crate) struct FinalPiDrainHookGuard;

#[cfg(test)]
impl Drop for FinalPiDrainHookGuard {
    fn drop(&mut self) {
        clear_after_final_pi_drain_hook();
    }
}

/// Outcome of [`Instance::maybe_start_poller`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollerStart {
    /// A poller thread is running for this session (started now, or already).
    Started,
    /// Nothing to poll for this session at the moment; not a failure.
    NotApplicable,
    /// Capture prerequisites are unresolved; a retry is scheduled on
    /// `session_id_poller_retry_after`.
    Deferred,
    /// The process-wide poller-thread budget is spent.
    BudgetExhausted,
    /// The OS refused to spawn the poller thread.
    SpawnFailed,
}

/// An exclusive claim on one physical capture store, held for as long as the poller that owns it
/// runs.
#[derive(Debug)]
struct ManagedCaptureLease(std::fs::File);

impl Drop for ManagedCaptureLease {
    fn drop(&mut self) {
        // Spelled through the trait: `File`'s inherent `unlock` is newer than
        // this crate's MSRV.
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

/// Why a managed capture store is not available to poll.
#[derive(Debug, PartialEq, Eq)]
enum LeaseRefusal {
    /// Another owner holds this physical store.
    Contended,
    /// The claim could not be evaluated at all: the app dir, the store path,
    /// or the lock file did not resolve. Not evidence of another owner.
    Unresolved,
}

fn try_acquire_managed_capture_lease(
    backend: crate::agents::SessionCaptureBackend,
    store: &Path,
) -> Result<ManagedCaptureLease, LeaseRefusal> {
    fn unresolved<E>(_: E) -> LeaseRefusal {
        LeaseRefusal::Unresolved
    }
    let lock_dir = crate::session::get_app_dir()
        .map_err(unresolved)?
        .join("capture-locks");
    std::fs::create_dir_all(&lock_dir).map_err(unresolved)?;
    let store = std::fs::canonicalize(store).map_err(unresolved)?;
    let mut digest = Sha256::new();
    digest.update(format!("{backend:?}\0"));
    digest.update(store.as_os_str().as_encoded_bytes());
    let mut key = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(&mut key, "{byte:02x}").map_err(unresolved)?;
    }
    let path = lock_dir.join(format!("{key}.lock"));

    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(&path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(LeaseRefusal::Unresolved);
    }
    let lease = options.open(path).map_err(unresolved)?;
    lease
        .try_lock_exclusive()
        .map_err(|_| LeaseRefusal::Contended)?;
    Ok(ManagedCaptureLease(lease))
}

/// Use a unique live agent; paired-only or ambiguous live panes forbid fallback.
fn log_observed_session_id(instance_id: &str) -> Box<dyn Fn(&str) + Send + 'static> {
    let instance_id = instance_id.to_string();
    Box::new(move |new_id| {
        tracing::info!(target: "session.store", "Session ID observed for {}: {}", instance_id, new_id);
    })
}

fn poller_seed_name(
    live: AgentSeed,
    derived: impl FnOnce() -> Option<String>,
    session_id: &str,
) -> Option<String> {
    match live {
        AgentSeed::Agent(name) => Some(name),
        AgentSeed::NoUniqueAgent => None,
        AgentSeed::NothingLive => {
            derived().filter(|name| crate::tmux::agent_session_belongs_to(name, session_id))
        }
    }
}

impl Instance {
    /// Whether this session should run a session-id poller: the agent has a resume strategy to
    /// capture for, and its conversation is not already known.
    pub(crate) fn launch_has_session_publisher(&self) -> bool {
        let Some((capture, context)) = self.resolved_session_support() else {
            return false;
        };
        match capture.backend {
            crate::agents::SessionCaptureBackend::Pi => self.pi_extension_launched,
            crate::agents::SessionCaptureBackend::OpenCode
            | crate::agents::SessionCaptureBackend::Omp => false,
            _ if context == crate::agents::SessionCaptureContext::ManagedExclusiveStore => true,
            _ => self.identity_publisher_launched,
        }
    }

    pub fn supports_session_poller(&self) -> bool {
        let Some((capture, context)) = self.source_session_support() else {
            return false;
        };
        match capture.backend {
            crate::agents::SessionCaptureBackend::OpenCode => false,
            crate::agents::SessionCaptureBackend::Pi => self.uses_pi_session_sidecar(),
            _ => context != crate::agents::SessionCaptureContext::Preassigned,
        }
    }

    pub(super) fn managed_capture_store_is_exclusive(
        &self,
        backend: crate::agents::SessionCaptureBackend,
    ) -> bool {
        if !self.is_sandboxed() {
            return false;
        }
        let Some(current_store) = self.capture_store_dir() else {
            return false;
        };
        let Ok(current_store) = std::fs::canonicalize(current_store) else {
            return false;
        };
        let current_profile = self.effective_profile();
        let Ok(mut profiles) = crate::session::list_profiles() else {
            return false;
        };
        if !profiles.contains(&current_profile) {
            profiles.push(current_profile.clone());
        }
        for profile in profiles {
            let Ok(storage) = crate::session::storage::Storage::new_unwatched(&profile) else {
                return false;
            };
            let Ok(instances) = storage.load() else {
                return false;
            };
            for mut peer in instances {
                if peer.id == self.id && profile == current_profile {
                    continue;
                }
                peer.source_profile = profile.clone();
                if !peer.is_sandboxed()
                    || peer.archived_at.is_some()
                    || peer.trashed_at.is_some()
                    || matches!(peer.status, Status::Stopped | Status::Deleting)
                {
                    continue;
                }
                let agent = match &peer.active_execution {
                    Some(active) => crate::agents::get_agent(&active.binding.agent),
                    None => peer.resolved_agent(),
                };
                let Some(agent) = agent else {
                    return false;
                };
                let peer_backend = agent
                    .session_support
                    .as_ref()
                    .and_then(|support| support.capture.as_ref())
                    .map(|capture| capture.backend);
                if peer_backend != Some(backend) {
                    continue;
                }
                if peer.active_execution.is_none() {
                    return false;
                }
                let Some(peer_store) = peer.capture_store_dir() else {
                    return false;
                };
                let Ok(peer_store) = std::fs::canonicalize(peer_store) else {
                    return false;
                };
                if peer_store == current_store {
                    return false;
                }
            }
        }
        true
    }

    pub fn maybe_start_poller(&mut self) -> PollerStart {
        self.maybe_start_poller_since(None)
    }

    /// Store a freshly spawned poller, or say why there is none.
    fn install_poller(
        &mut self,
        poller: SessionPoller,
        spawn: crate::session::poller::PollerSpawn,
    ) -> PollerStart {
        use crate::session::poller::PollerSpawn;
        match spawn {
            PollerSpawn::Spawned => {
                self.session_id_poller = Some(Arc::new(Mutex::new(poller)));
                self.poller_repair.reset();
                PollerStart::Started
            }
            PollerSpawn::BudgetExhausted => PollerStart::BudgetExhausted,
            // A poller built on this call cannot have been started already;
            // if it says so, it is not ours to keep.
            PollerSpawn::AlreadyStarted | PollerSpawn::SpawnFailed => {
                tracing::warn!(target: "session.store",
                    "Failed to start session poller for instance {}, poller will not be stored",
                    self.id
                );
                PollerStart::SpawnFailed
            }
        }
    }

    pub(super) fn maybe_start_poller_since(
        &mut self,
        omp_metadata: Option<OmpCaptureMetadata>,
    ) -> PollerStart {
        if !crate::migrations::v033_isolate_sandbox_content::instance_ready(self).unwrap_or(false) {
            self.session_id_poller = None;
            return PollerStart::NotApplicable;
        }
        if self.session_id_poller_is_running() {
            return PollerStart::Started;
        }
        self.session_id_poller = None;
        let Some((capture, context)) = self.source_session_support() else {
            return PollerStart::NotApplicable;
        };
        let backend = capture.backend;
        if !self.supports_session_poller() {
            return PollerStart::NotApplicable;
        }
        let prime_options = if self.active_execution.is_none()
            && backend == crate::agents::SessionCaptureBackend::PrimeAgent
        {
            self.prime_agent_capture_options()
        } else {
            None
        };
        // Prime argv eligibility is in-memory; resolving its store/settings stays behind the budget gate.
        let eligible = match backend {
            crate::agents::SessionCaptureBackend::Codex
            | crate::agents::SessionCaptureBackend::Gemini
            | crate::agents::SessionCaptureBackend::Hermes
            | crate::agents::SessionCaptureBackend::Kimi => self.capture_store_dir().is_some(),
            crate::agents::SessionCaptureBackend::PrimeAgent => self
                .active_execution
                .as_ref()
                .map_or(prime_options.is_some(), |active| {
                    matches!(active.capture, Some(CaptureContext::Prime { .. }))
                }),
            crate::agents::SessionCaptureBackend::Omp => {
                self.active_execution.as_ref().map_or_else(
                    || self.omp_capture_options().is_some(),
                    |active| matches!(active.capture, Some(CaptureContext::Omp(_))),
                )
            }
            crate::agents::SessionCaptureBackend::Pi => self.pi_sidecar_source().is_some(),
            crate::agents::SessionCaptureBackend::Claude
            | crate::agents::SessionCaptureBackend::HookSidecar => true,
            crate::agents::SessionCaptureBackend::OpenCode => false,
        };
        if !eligible {
            return PollerStart::NotApplicable;
        }
        // Avoid configuration I/O, lease and profile scans when no poller can be spawned.
        if !crate::session::poller::session_id_poller_budget_available() {
            return PollerStart::BudgetExhausted;
        }
        let prime_plan = if let Some(active) = &self.active_execution {
            match &active.capture {
                Some(CaptureContext::Prime { plan, .. }) => Some(plan.clone()),
                _ => None,
            }
        } else if let Some(options) = prime_options {
            match self.prime_agent_capture_plan(options) {
                Ok(plan) => Some(plan),
                Err(error) => {
                    self.session_id_poller_retry_after =
                        Some(std::time::Instant::now() + MANAGED_CAPTURE_RETRY_BACKOFF);
                    tracing::warn!(target: "session.capture", session = %self.id,
                        reason = %format_args!("{error:#}"), retry_after_secs = MANAGED_CAPTURE_RETRY_BACKOFF.as_secs(),
                        "Prime session capture deferred because its configuration could not be resolved");
                    return PollerStart::Deferred;
                }
            }
        } else {
            None
        };
        let managed_lease = if context
            == crate::agents::SessionCaptureContext::ManagedExclusiveStore
        {
            let Some(store) = prime_plan
                .as_ref()
                .map(|plan| plan.store.clone())
                .or_else(|| self.capture_store_dir())
            else {
                return PollerStart::NotApplicable;
            };
            // Lease contention is the common multi-process loser path. Check it
            // before loading every profile to prove store exclusivity.
            let lease = match try_acquire_managed_capture_lease(backend, &store) {
                Ok(lease) => lease,
                Err(refusal) => {
                    self.session_id_poller_retry_after =
                        Some(std::time::Instant::now() + MANAGED_CAPTURE_RETRY_BACKOFF);
                    match refusal {
                        LeaseRefusal::Contended => {
                            tracing::warn!(target: "session.capture", session = %self.id, ?backend,
                            "Session capture deferred because another process owns this store");
                        }
                        LeaseRefusal::Unresolved => {
                            tracing::warn!(target: "session.capture", session = %self.id, ?backend,
                            "Session capture deferred because this store's lease could not be resolved");
                        }
                    }
                    return PollerStart::Deferred;
                }
            };
            if !self.managed_capture_store_is_exclusive(backend) {
                self.session_id_poller_retry_after =
                    Some(std::time::Instant::now() + MANAGED_CAPTURE_RETRY_BACKOFF);
                tracing::warn!(target: "session.capture", session = %self.id, ?backend,
                "Session capture deferred because store ownership is ambiguous");
                return PollerStart::Deferred;
            }
            Some(lease)
        } else {
            None
        };
        self.session_id_poller_retry_after = None;

        // Unlike the eligibility checks above, this forks `tmux list-sessions`, so it stays behind
        // the budget gate rather than joining them.
        let Some(tmux_session_name) = poller_seed_name(
            self.live_agent_seed(),
            || self.tmux_session().ok().map(|s| s.name().to_string()),
            &self.id,
        ) else {
            tracing::debug!(target: "session.create",
                "No agent tmux session resolves for {}; session-id poller not started",
                self.id);
            return PollerStart::NotApplicable;
        };
        let omp_metadata = if backend == crate::agents::SessionCaptureBackend::Omp {
            if let Some(active) = &self.active_execution {
                match &active.capture {
                    Some(CaptureContext::Omp(metadata)) => Some(metadata.clone()),
                    _ => None,
                }
            } else {
                let Some(options) = self.omp_capture_options() else {
                    return PollerStart::NotApplicable;
                };
                omp_metadata
                    .or_else(|| self.omp_capture_metadata(&tmux_session_name, &options, None))
            }
        } else {
            None
        };

        let mut poller = SessionPoller::new(tmux_session_name);
        let instance_id = self.id.clone();
        let initial_known = self.agent_session_id.clone().filter(|_| {
            self.agent_session_binding
                .as_ref()
                .is_some_and(ConversationBinding::is_known)
        });
        let extra_excludes = self.retroactive_capture_excludes.clone();

        if backend == crate::agents::SessionCaptureBackend::Omp {
            let Some(metadata) = omp_metadata.as_ref() else {
                return PollerStart::NotApplicable;
            };
            let container_name = match &self.active_execution {
                Some(active) => active
                    .container
                    .as_ref()
                    .map(|container| container.id.clone()),
                None => self
                    .sandbox_info
                    .as_ref()
                    .filter(|sandbox| sandbox.enabled)
                    .map(|sandbox| sandbox.container_name.clone()),
            };
            let poll_fn: crate::session::poller::SessionIdPollFn =
                if let Some(container_name) = container_name {
                    Box::new(omp_poll_fn_sandboxed(
                        container_name,
                        self.id.clone(),
                        Some(metadata.launch_marker.clone()),
                        extra_excludes,
                        self.active_execution.clone(),
                    ))
                } else {
                    Box::new(omp_poll_fn(
                        self.id.clone(),
                        extra_excludes,
                        self.active_execution.clone(),
                    ))
                };
            let on_change = log_observed_session_id(&self.id);
            let initial = initial_known.map(|sid| metadata.session_observation(sid));
            let spawn = poller.start_observations(instance_id, poll_fn, on_change, initial);
            return self.install_poller(poller, spawn);
        }

        if backend == crate::agents::SessionCaptureBackend::Pi {
            let Some(source) = self.pi_sidecar_source() else {
                return PollerStart::NotApplicable;
            };
            let inner = crate::session::capture::pi_sidecar_poll_fn(
                self.id.clone(),
                source,
                self.active_execution.clone(),
            );
            let poll_fn: crate::session::poller::SessionIdPollFn = Box::new(move |_| inner());
            let on_change = log_observed_session_id(&self.id);
            // Seed without a path so the first published transcript is not suppressed.
            let initial = initial_known.map(|sid| {
                crate::session::poller::SessionIdObservation::instance_sidecar(sid, None)
            });
            let spawn = poller.start_observations(instance_id, poll_fn, on_change, initial);
            return self.install_poller(poller, spawn);
        }

        if matches!(
            backend,
            crate::agents::SessionCaptureBackend::Claude
                | crate::agents::SessionCaptureBackend::HookSidecar
        ) {
            let sidecar_id = self.id.clone();
            let active = self.active_execution.clone();
            let poll_fn: crate::session::poller::SessionIdPollFn = Box::new(move |_| {
                super::execution::hook_session_observation(
                    &sidecar_id,
                    active.as_ref(),
                    Some(std::time::Duration::from_secs(300)),
                )
            });
            let on_change = log_observed_session_id(&self.id);
            let initial = initial_known.map(|sid| {
                crate::session::poller::SessionIdObservation::instance_sidecar(sid, None)
            });
            let spawn = poller.start_observations(instance_id, poll_fn, on_change, initial);
            return self.install_poller(poller, spawn);
        }

        let capture_floor = self
            .capture_started_at
            .unwrap_or_else(std::time::SystemTime::now);
        let capture_floor_ms = capture_floor
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64() * 1000.0)
            .unwrap_or(f64::MAX);
        let store_cwd = self
            .active_execution
            .as_ref()
            .and_then(|active| match &active.capture {
                Some(CaptureContext::Store { cwd, .. }) => Some(cwd.clone()),
                _ => None,
            });
        let source = self
            .active_execution
            .as_ref()
            .map(|active| active.binding.clone());
        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = match backend {
            store_backed @ (crate::agents::SessionCaptureBackend::Codex
            | crate::agents::SessionCaptureBackend::Gemini
            | crate::agents::SessionCaptureBackend::Hermes
            | crate::agents::SessionCaptureBackend::Kimi) => {
                let Some(store) = self.capture_store_dir() else {
                    return PollerStart::NotApplicable;
                };
                let workdir = store_cwd.unwrap_or_else(|| self.container_workdir());
                let id = self.id.clone();
                match store_backed {
                    crate::agents::SessionCaptureBackend::Codex => {
                        Box::new(codex_poll_fn_sandboxed_store(
                            store,
                            workdir,
                            id,
                            capture_floor,
                            extra_excludes,
                            source,
                        ))
                    }
                    crate::agents::SessionCaptureBackend::Gemini => {
                        Box::new(gemini_poll_fn_sandboxed_store(
                            store,
                            workdir,
                            id,
                            capture_floor,
                            extra_excludes,
                            source,
                        ))
                    }
                    crate::agents::SessionCaptureBackend::Hermes => {
                        Box::new(hermes_poll_fn_sandboxed_store(
                            store,
                            workdir,
                            id,
                            capture_floor,
                            extra_excludes,
                            source,
                        ))
                    }
                    _ => Box::new(kimi_poll_fn_sandboxed_store(
                        store,
                        workdir,
                        id,
                        capture_floor_ms,
                        extra_excludes,
                        source,
                    )),
                }
            }
            crate::agents::SessionCaptureBackend::PrimeAgent => {
                let Some(plan) = prime_plan else {
                    return PollerStart::NotApplicable;
                };
                let preferred_sidecar = self.prime_root_sidecar_poll_fn(plan.clone());
                Box::new(prime_agent_poll_fn_sandboxed(
                    preferred_sidecar,
                    plan,
                    self.id.clone(),
                    capture_floor_ms,
                    extra_excludes,
                    source,
                ))
            }
            crate::agents::SessionCaptureBackend::Claude
            | crate::agents::SessionCaptureBackend::HookSidecar
            | crate::agents::SessionCaptureBackend::OpenCode
            | crate::agents::SessionCaptureBackend::Pi
            | crate::agents::SessionCaptureBackend::Omp => return PollerStart::NotApplicable,
        };
        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> =
            if let Some(lease) = managed_lease {
                Box::new(move || {
                    let _lease = &lease;
                    poll_fn()
                })
            } else {
                poll_fn
            };

        let on_change = log_observed_session_id(&self.id);
        let active = self.active_execution.clone().filter(|active| {
            matches!(
                active.capture,
                Some(CaptureContext::Store { .. } | CaptureContext::Prime { .. })
            )
        });
        let poll_fn: crate::session::poller::SessionIdPollFn = Box::new(move |_| {
            let mut observation =
                crate::session::poller::SessionIdObservation::instance_sidecar(poll_fn()?, None);
            if let Some(active) = &active {
                observation.source = Some(active.binding.clone());
                observation.execution = Some(active.clone());
            }
            Some(observation)
        });
        let initial = initial_known
            .map(|sid| crate::session::poller::SessionIdObservation::instance_sidecar(sid, None));
        let spawn = poller.start_observations(instance_id, poll_fn, on_change, initial);
        self.install_poller(poller, spawn)
    }

    pub(crate) fn session_id_poller_is_running(&self) -> bool {
        self.session_id_poller.as_ref().is_some_and(|poller| {
            poller
                .lock()
                .map(|guard| guard.is_running())
                .unwrap_or_else(|poisoned| poisoned.into_inner().is_running())
        })
    }

    /// Replace a missing or finished poller once its tmux pane is live.
    pub(crate) fn repair_session_id_poller_if_needed(
        &mut self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> bool {
        let now = std::time::Instant::now();
        // Keep cheap eligibility checks ahead of support resolution. A parked row without a
        // live agent falls out at the pane check, while a no-kill parked row remains repairable.
        if self.is_structured()
            || self.session_id_poller_is_running()
            || self
                .session_id_poller_retry_after
                .is_some_and(|deadline| now < deadline)
            || !self.poller_repair.due(now)
            // Agent pane, not any pane: a terminal outliving the agent is not
            // something a session-id poller can follow.
            || !self.has_live_agent_pane_in(snapshot)
        {
            return false;
        }
        if !self.supports_session_poller() {
            return false;
        }
        let repair_now = std::time::Instant::now();
        self.session_id_poller = None;
        match self.maybe_start_poller() {
            // `install_poller` cleared the schedule.
            PollerStart::Started => true,
            // Nothing failed: the session has nothing to poll right now, or the managed store's own
            // retry deadline governs.
            PollerStart::NotApplicable | PollerStart::Deferred => {
                self.poller_repair.reset();
                false
            }
            PollerStart::BudgetExhausted => {
                self.defer_poller_repair(repair_now, "budget exhausted");
                false
            }
            PollerStart::SpawnFailed => {
                self.defer_poller_repair(repair_now, "start failed");
                false
            }
        }
    }

    /// Schedule the next repair attempt and log at the backoff's cadence
    /// (first miss, each escalation, then every ~10 min at the cap).
    fn defer_poller_repair(&mut self, now: std::time::Instant, why: &str) {
        let (active, max) = crate::session::poller::session_id_poller_budget();
        if let Some(delay) = self.poller_repair.defer(now) {
            tracing::warn!(
                target: "session.create",
                "Session-id poller for {} not restarted ({why}; {active}/{max} threads); \
                 next attempt in {}s after {} deferral(s). Raise \
                 [session] session_id_poller_max_threads if the fleet outgrew the budget",
                self.id,
                delay.as_secs(),
                self.poller_repair.deferrals(),
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn set_after_final_pi_drain_hook_for_test(
        hook: impl FnOnce(&mut Instance) + 'static,
    ) -> FinalPiDrainHookGuard {
        AFTER_FINAL_PI_DRAIN.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "final Pi drain hook already installed");
            *slot = Some(Box::new(hook));
        });
        FinalPiDrainHookGuard
    }

    pub(crate) fn stop_poller(&self) {
        if let Some(ref poller_arc) = self.session_id_poller {
            match poller_arc.lock() {
                Ok(mut poller) => poller.stop(),
                Err(e) => e.into_inner().stop(),
            }
        }
    }

    /// Join the old poller and persist its final capture as a lifecycle
    /// transition.
    pub(crate) fn stop_and_flush_poller(&mut self) {
        let profile = self.effective_profile();
        let storage = match crate::session::storage::Storage::new(
            &profile,
            self.resolve_file_watch(),
        ) {
            Ok(storage) => storage,
            Err(error) => {
                tracing::warn!(target: "session.sync", session = %self.id, "capture storage failed: {error}");
                self.stop_poller();
                self.session_id_poller = None;
                return;
            }
        };
        let _lifecycle_lock = match storage.acquire_instance_lifecycle_lock(&self.id) {
            Ok(lock) => lock,
            Err(error) => {
                tracing::warn!(target: "session.sync", session = %self.id, "capture lifecycle lock failed: {error}");
                self.stop_poller();
                self.session_id_poller = None;
                return;
            }
        };
        self.stop_and_flush_poller_lifecycle_locked();
    }

    pub(super) fn stop_and_flush_poller_lifecycle_locked(&mut self) {
        // A Pi pane's last word is in its sidecar, which no poller may have read: a CLI-only pane
        // has none, and a restart tears the pane down before the next one starts.
        self.flush_published_if_present();
        // stop_poller() signals the thread but leaves the handle in place, so this is_some() means
        // "a poller existed and may have queued a final observation".
        self.stop_poller();
        if self.session_id_poller.is_some() {
            let file_watch = self.resolve_file_watch();
            let _ = crate::session::sync::drain_and_persist_session_ids_lifecycle_locked(
                std::slice::from_mut(self),
                &file_watch,
            );
            #[cfg(test)]
            if let Some(hook) = take_after_final_pi_drain_hook() {
                hook(self);
            }
            let inst = &*self;
            let retry_pi_path =
                crate::session::sync::pending_poller_observation_matches(inst, |observation| {
                    inst.observation_is_current_pi_path(observation)
                });
            if retry_pi_path {
                let _ = crate::session::sync::drain_and_persist_session_ids_lifecycle_locked(
                    std::slice::from_mut(self),
                    &file_watch,
                );
            }
        }
        self.session_id_poller = None;
    }
}

#[cfg(test)]
mod tests {
    use super::PollerStart;
    use crate::session::instance::test_helpers::*;
    use crate::session::{Instance, Status};

    fn admit_fixture_content(inst: &Instance) {
        let app = crate::session::get_app_dir().unwrap();
        for root in crate::migrations::v033_isolate_sandbox_content::instance_roots(inst).unwrap() {
            std::fs::create_dir_all(&root.path).unwrap();
            let roles: Vec<&str> = root.roles.iter().map(String::as_str).collect();
            crate::migrations::v033_isolate_sandbox_content::certify_test_content(
                &app, &inst.id, &root.path, &roles,
            )
            .unwrap();
        }
    }

    /// The 2026-09-04 fleet shape.
    #[test]
    fn repair_defers_with_backoff_while_the_poller_budget_is_spent() {
        let budget = crate::session::poller::test_support::IsolatedBudget::exhausted();
        let mut inst = Instance::new("repair-backoff", "/tmp/repair-backoff");
        let live = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![crate::tmux::Session::generate_name(
                &inst.id,
                &inst.title,
            )]),
            None,
        );
        assert!(
            inst.has_live_tmux_pane_in(&live),
            "fixture pane must read live"
        );
        assert!(inst.supports_session_poller());

        assert!(!inst.repair_session_id_poller_if_needed(&live));
        assert!(
            inst.session_id_poller.is_none(),
            "no poller stored over budget"
        );
        assert_eq!(inst.poller_repair.deferrals(), 1);
        assert_eq!(
            inst.poller_repair.current_delay(),
            Some(std::time::Duration::from_secs(5))
        );

        // The next tick lands inside the scheduled delay: no probe, no log.
        assert!(!inst.repair_session_id_poller_if_needed(&live));
        assert_eq!(
            inst.poller_repair.deferrals(),
            1,
            "tick inside the delay is a no-op"
        );

        // Once due and still over budget, the delay escalates.
        inst.poller_repair.expire();
        assert!(!inst.repair_session_id_poller_if_needed(&live));
        assert_eq!(inst.poller_repair.deferrals(), 2);
        assert_eq!(
            inst.poller_repair.current_delay(),
            Some(std::time::Duration::from_secs(10))
        );

        // Budget freed (another session stopped, or the ceiling was raised):
        // the due attempt starts the poller and clears the schedule.
        budget.set_active(0);
        inst.poller_repair.expire();
        assert!(inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller_is_running());
        assert_eq!(inst.poller_repair, Default::default());
        inst.stop_poller();
    }

    /// A direct (non-repair) start is a success too.
    #[test]
    fn direct_start_clears_a_deferred_repair_schedule() {
        let _budget = crate::session::poller::test_support::IsolatedBudget::with_ceiling(1);
        let mut inst = Instance::new("direct-start", "/tmp/direct-start");
        let now = std::time::Instant::now();
        inst.poller_repair.defer(now);
        inst.poller_repair.defer(now);
        assert!(!inst.poller_repair.due(now), "fixture: a pending schedule");

        assert_eq!(inst.maybe_start_poller(), PollerStart::Started);

        assert!(inst.session_id_poller_is_running());
        assert_eq!(
            inst.poller_repair,
            Default::default(),
            "a successful start clears the schedule whoever triggered it"
        );
        inst.stop_poller();
    }

    /// A live pane with nothing to poll right now (here: an OMP pane whose capture metadata is not
    /// resolvable) is not a failed spawn.
    #[test]
    fn repair_does_not_defer_a_session_with_nothing_to_poll() {
        let mut inst = Instance::new("omp-no-meta", "/tmp/omp-no-meta");
        inst.tool = "omp".to_string();
        inst.omp_capture_generation = Some("gen-1".to_string());
        let live = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![crate::tmux::Session::generate_name(
                &inst.id,
                &inst.title,
            )]),
            None,
        );
        assert!(inst.has_live_tmux_pane_in(&live));
        assert!(
            inst.supports_session_poller(),
            "OMP is pollable in principle, so repair walks the start path"
        );
        assert_eq!(inst.maybe_start_poller(), PollerStart::NotApplicable);

        assert!(!inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller.is_none());
        assert_eq!(
            inst.poller_repair.deferrals(),
            0,
            "nothing to poll is not a failed repair"
        );
        assert!(
            inst.poller_repair.due(std::time::Instant::now()),
            "the next tick may look again"
        );
    }

    #[test]
    #[serial_test::serial]
    fn prime_repair_distinguishes_no_plan_budget_and_store_contention() {
        let app = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(app.path());
        let budget = crate::session::poller::test_support::IsolatedBudget::exhausted();
        let mut inst = Instance::new("prime-repair", "/tmp/prime-repair");
        inst.tool = "prime-agent".to_string();
        inst.sandbox_info = Some(test_sandbox(
            "prime-repair",
            Some("/workspace/prime-repair"),
        ));
        admit_fixture_content(&inst);
        let store = inst.sandbox_capture_store_dir().unwrap();
        std::fs::create_dir_all(&store).unwrap();
        let live = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![inst.tmux_session().unwrap().name().to_string()]),
            None,
        );
        let backend = crate::agents::SessionCaptureBackend::PrimeAgent;

        inst.extra_args = "--no-session".to_string();
        assert!(inst.supports_session_poller());
        assert_eq!(inst.maybe_start_poller(), PollerStart::NotApplicable);
        assert!(!inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller_retry_after.is_none());
        assert!(inst.session_id_poller.is_none());

        inst.extra_args.clear();
        let settings = store.join("settings.json");
        std::fs::create_dir(&settings).unwrap();
        assert_eq!(inst.maybe_start_poller(), PollerStart::BudgetExhausted);
        assert!(!inst.repair_session_id_poller_if_needed(&live));
        assert!(!inst.poller_repair.due(std::time::Instant::now()));
        let lease = super::try_acquire_managed_capture_lease(backend, &store)
            .expect("budget rejection releases the store lease");

        budget.set_active(0);
        inst.poller_repair.expire();
        assert_eq!(inst.maybe_start_poller(), PollerStart::Deferred);
        assert!(inst.session_id_poller_retry_after.is_some());
        std::fs::remove_dir(&settings).unwrap();
        assert!(!inst.repair_session_id_poller_if_needed(&live));
        inst.session_id_poller_retry_after = None;
        assert_eq!(inst.maybe_start_poller(), PollerStart::Deferred);
        assert!(inst.session_id_poller_retry_after.is_some());
        drop(lease);
        assert!(!inst.repair_session_id_poller_if_needed(&live));
        inst.session_id_poller_retry_after = None;

        assert!(inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller_is_running());
        assert_eq!(
            super::try_acquire_managed_capture_lease(backend, &store).unwrap_err(),
            super::LeaseRefusal::Contended
        );
        inst.stop_poller();
        super::try_acquire_managed_capture_lease(backend, &store)
            .expect("stopping the poller releases the store lease");
    }

    // Every teardown path must flush the last published conversation.
    #[test]
    #[serial_test::serial]
    fn teardown_flushes_the_published_pi_conversation() {
        let (_guard, _base, _tmp) = crate::hooks::test_support::BaseGuard::ready();
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_app_dir_at(home.path());

        let profile = "pi-teardown-flush";
        let mut inst = Instance::new("pi-teardown", "/tmp/pi-teardown");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.agent_session_id = Some("d38740e4-bd1f-43d7-8727-485652e4678e".to_string());
        inst.mark_pi_extension_launched_for_test();

        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let seed = inst.clone();
        storage
            .update(|instances, _| {
                *instances = vec![seed.clone()];
                Ok(())
            })
            .unwrap();

        let published = "01a053b6-c470-78de-9d8f-bc00ef05332a";
        super::super::test_helpers::publish_host_pi_transcript(&inst.id, published, home.path());

        inst.stop_and_flush_poller_lifecycle_locked();

        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(published),
            "a teardown must keep what the pane last published"
        );
        assert_eq!(
            inst.agent_session_id.as_deref(),
            Some(published),
            "and the in-memory row a restart reads moments later"
        );
    }

    #[test]
    #[serial_test::serial]
    fn a_live_pi_poller_records_a_transcript_path_published_after_its_id() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_app_dir_at(home.path());
        let _budget = crate::session::poller::test_support::IsolatedBudget::with_ceiling(1);

        // A `--session-id` launch: the row holds the id before the pane publishes anything.
        // Sandboxed, because the poller thread cannot see a test's host hook dir override.
        let profile = "pi-late-path";
        let sid = "01a05234-8889-72e2-a7c9-7ebc27b25b78";
        let mut inst = Instance::new("pilatepath000001", "/tmp/pi-late-path");
        inst.source_profile = profile.to_string();
        inst.tool = "pi".to_string();
        inst.sandbox_info = Some(test_sandbox("aoe-pi-late-path", None));
        inst.agent_session_id = Some(sid.to_string());
        inst.mark_pi_extension_launched_for_test();
        admit_fixture_content(&inst);
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        let seed = inst.clone();
        storage
            .update(|instances, _| {
                *instances = vec![seed.clone()];
                Ok(())
            })
            .unwrap();
        let Some(crate::session::instance::SessionSidecarSource::SandboxDir(dir)) =
            inst.pi_sidecar_source()
        else {
            panic!("a sandboxed pane publishes under its bind");
        };
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("session_id"), format!("{sid}\n")).unwrap();
        assert_eq!(inst.maybe_start_poller(), PollerStart::Started);

        let published =
            format!("/root/.pi/agent/sessions/--proj--/2026-01-01T00-00-00-000Z_{sid}.jsonl");
        std::fs::create_dir_all(
            dir.parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("agent/sessions/--proj--"),
        )
        .unwrap();
        std::fs::write(dir.join("session_path"), format!("{published}\n")).unwrap();

        let file_watch = crate::file_watch::FileWatchService::noop();
        let mut instances = [inst];
        let stored = || storage.load().unwrap()[0].pi_session_path.clone();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while stored().is_none() && std::time::Instant::now() < deadline {
            crate::session::sync::drain_and_persist_session_ids(&mut instances, &file_watch);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        instances[0].stop_poller();
        assert_eq!(
            stored(),
            Some(published),
            "the path must be durable while the pane lives, not only at teardown"
        );
    }

    #[test]
    #[serial_test::serial]
    fn sandboxed_pi_polls_the_bind_backed_sidecar() {
        // A container publishes under its own bind, not the host hook dir.
        // Reading the wrong one is silent: the poller simply never observes.
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());

        let mut host = Instance::new("pi-host-poll", "/tmp/pi-poll");
        host.tool = "pi".to_string();
        assert_eq!(
            host.pi_sidecar_source().and_then(|s| match s {
                crate::session::instance::SessionSidecarSource::SandboxDir(d) => Some(d),
                _ => None,
            }),
            None,
            "a host pane reads the hook dir"
        );

        let mut sandboxed = Instance::new("pisandboxpoll001", "/tmp/pi-poll");
        sandboxed.tool = "pi".to_string();
        sandboxed.sandbox_info = Some(test_sandbox("aoe-pi-poll", None));
        admit_fixture_content(&sandboxed);
        let dir = sandboxed
            .pi_sidecar_source()
            .and_then(|s| match s {
                crate::session::instance::SessionSidecarSource::SandboxDir(d) => Some(d),
                _ => None,
            })
            .expect("a sandboxed pane reads its bind");
        assert!(
            dir.ends_with(format!("aoe-session/{}", sandboxed.id)),
            "got {dir:?}"
        );

        // And the closure built from it observes what the pane publishes.
        std::fs::create_dir_all(&dir).unwrap();
        let published = "99999999-9999-4999-8999-999999999999";
        std::fs::write(dir.join("session_id"), format!("{published}\n")).unwrap();
        let transcript = dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("agent/sessions")
            .join(format!("time_{published}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(
            &transcript,
            format!("{{\"type\":\"session\",\"id\":\"{published}\"}}\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("session_path"),
            format!("/root/.pi/agent/sessions/time_{published}.jsonl"),
        )
        .unwrap();
        let poll = crate::session::capture::pi_sidecar_poll_fn(
            sandboxed.id.clone(),
            sandboxed
                .pi_sidecar_source()
                .expect("a resolvable sandbox source"),
            None,
        );
        assert_eq!(poll().map(|o| o.sid).as_deref(), Some(published));
    }

    #[test]
    fn pi_polls_only_what_names_a_pane() {
        // Without the extension there is nothing attributable to observe, and
        // the store is not an answer, so the pane does not poll at all.
        let mut inst = Instance::new("pi-poll", "/tmp/pi-poll");
        inst.tool = "pi".to_string();
        assert!(!inst.supports_session_poller());

        inst.mark_pi_extension_launched_for_test();
        assert!(inst.supports_session_poller());

        // A known id is no reason to stop: `/new` is still this pane's.
        inst.agent_session_id = Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa".to_string());
        assert!(inst.supports_session_poller());

        let mut claude = Instance::new("claude-poll", "/tmp/pi-poll");
        claude.tool = "claude".to_string();
        assert!(claude.supports_session_poller());
    }
    #[test]
    #[serial_test::serial]
    fn managed_capture_repair_honors_contention_backoff() {
        let app = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(app.path());
        let mut inst = tool_instance("gemini", "/tmp/gemini-backoff");
        inst.sandbox_info = Some(test_sandbox("test", Some("/workspace/gemini-backoff")));
        let name = inst.tmux_session().unwrap().name().to_string();
        let live = crate::tmux::LiveSessionSnapshot::from_parts(Some(vec![name]), None);
        admit_fixture_content(&inst);
        inst.session_id_poller_retry_after =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(60));

        assert!(!inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller.is_none());

        inst.session_id_poller_retry_after = None;
        std::fs::create_dir_all(inst.sandbox_capture_store_dir().unwrap()).unwrap();
        assert!(inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller_is_running());
        inst.stop_poller();
    }

    fn sandboxed_gemini(title: &str, project_path: &str, workdir: &str) -> Instance {
        let mut inst = Instance::new(title, project_path);
        inst.tool = "gemini".to_string();
        inst.status = Status::Running;
        inst.sandbox_info = Some(test_sandbox(&format!("test-{}", inst.id), Some(workdir)));
        inst
    }

    #[test]
    #[serial_test::serial]
    fn managed_capture_exclusivity_is_store_based_across_profiles() {
        let app = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(app.path());
        let backend = crate::agents::SessionCaptureBackend::Gemini;
        let current_storage = crate::session::Storage::new_unwatched("capture-owner-a").unwrap();
        let peer_storage = crate::session::Storage::new_unwatched("capture-owner-b").unwrap();
        let mut current = sandboxed_gemini("current", "/repos/current", "/workspace/current");
        current.source_profile = "capture-owner-a".into();
        let mut peer = sandboxed_gemini("peer", "/repos/peer", "/workspace/peer");
        peer.source_profile = "capture-owner-b".into();
        let shared_store = app.path().join("shared");
        std::fs::create_dir_all(&shared_store).unwrap();
        admit_fixture_content(&peer);
        std::fs::create_dir_all(peer.sandbox_capture_store_dir().unwrap()).unwrap();
        let bind = |instance: &mut Instance, store: &std::path::Path| {
            instance.active_execution = Some(super::ActiveExecution {
                launch_id: uuid::Uuid::new_v4().to_string(),
                binding: crate::session::ExecutionBinding {
                    agent: "gemini".into(),
                    stores: vec![store.to_path_buf()],
                    configuration: Vec::new(),
                    exported_default_store: false,
                    cwd: "/workspace".into(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                },
                capture: Some(super::CaptureContext::Store {
                    root: store.to_path_buf(),
                    cwd: "/workspace".into(),
                }),
                container: None,
            });
        };
        bind(&mut current, &shared_store);
        bind(&mut peer, &shared_store);
        current_storage
            .update(|instances, _| {
                *instances = vec![current.clone()];
                Ok(())
            })
            .unwrap();
        peer_storage
            .update(|instances, _| {
                *instances = vec![peer.clone()];
                Ok(())
            })
            .unwrap();
        assert!(
            !current.managed_capture_store_is_exclusive(backend),
            "inspected mounts override predicted private stores"
        );

        peer.tool = "claude".into();
        peer_storage
            .update(|instances, _| {
                *instances = vec![peer.clone()];
                Ok(())
            })
            .unwrap();
        assert!(
            !current.managed_capture_store_is_exclusive(backend),
            "configuration changes do not change the running writer"
        );

        let peer_store = app.path().join("distinct");
        std::fs::create_dir_all(&peer_store).unwrap();
        bind(&mut peer, &peer_store);
        peer_storage
            .update(|instances, _| {
                *instances = vec![peer.clone()];
                Ok(())
            })
            .unwrap();
        assert!(
            current.managed_capture_store_is_exclusive(backend),
            "distinct physical stores do not conflict"
        );

        peer.active_execution = None;
        peer_storage
            .update(|instances, _| {
                *instances = vec![peer.clone()];
                Ok(())
            })
            .unwrap();
        assert!(
            current.managed_capture_store_is_exclusive(backend),
            "an unlocated Claude peer must not block Gemini capture"
        );
        peer.tool = "gemini".into();
        peer_storage
            .update(|instances, _| {
                *instances = vec![peer.clone()];
                Ok(())
            })
            .unwrap();
        assert!(
            !current.managed_capture_store_is_exclusive(backend),
            "an unlocated peer of the same backend cannot prove exclusivity"
        );
    }

    #[test]
    #[serial_test::serial]
    fn managed_capture_lease_serializes_the_physical_store() {
        let app = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(app.path());
        let store = tempfile::tempdir().unwrap();
        let other_store = tempfile::tempdir().unwrap();
        let backend = crate::agents::SessionCaptureBackend::Gemini;
        let first =
            super::try_acquire_managed_capture_lease(backend, store.path()).expect("first owner");
        assert_eq!(
            super::try_acquire_managed_capture_lease(backend, store.path()).unwrap_err(),
            super::LeaseRefusal::Contended,
            "another row using the same store must contend regardless of workspace"
        );
        #[cfg(unix)]
        {
            let alias = app.path().join("store-alias");
            std::os::unix::fs::symlink(store.path(), &alias).unwrap();
            assert_eq!(
                super::try_acquire_managed_capture_lease(backend, &alias).unwrap_err(),
                super::LeaseRefusal::Contended,
                "a symlink to the same physical store must contend"
            );
        }
        assert_eq!(
            super::try_acquire_managed_capture_lease(backend, &app.path().join("missing"))
                .unwrap_err(),
            super::LeaseRefusal::Unresolved,
            "an unresolved store identity fails closed without claiming another owner"
        );
        let distinct = super::try_acquire_managed_capture_lease(backend, other_store.path())
            .expect("a distinct store has a distinct lease");

        drop(first);
        super::try_acquire_managed_capture_lease(backend, store.path())
            .expect("the released store is claimable again");
        drop(distinct);
    }

    /// Repair declines when no live agent pane exists for the row: a terminal outliving the agent
    /// is not something a session-id poller can follow.
    #[test]
    fn repair_declines_without_a_live_agent_pane() {
        let mut inst = Instance::new("repair-no-pane", "/tmp/repair-no-pane");
        inst.tool = "claude".to_string();
        assert!(
            inst.supports_session_poller(),
            "a host claude row resolves support, so the pane lookup is what declines"
        );
        let snapshot = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![]),
            Some(std::collections::HashMap::new()),
        );
        // Present but not running, so the running check does not
        // short-circuit and the handle stays observable.
        inst.session_id_poller = Some(std::sync::Arc::new(std::sync::Mutex::new(
            crate::session::poller::SessionPoller::new("unstarted".to_string()),
        )));

        assert!(!inst.repair_session_id_poller_if_needed(&snapshot));
        assert!(
            inst.session_id_poller.is_some(),
            "decline must happen before the handle is cleared"
        );
    }
    #[test]
    #[serial_test::serial]
    fn repair_preserves_live_parked_rows_for_no_kill_transitions() {
        let app = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(app.path());
        let _budget = crate::session::poller::test_support::IsolatedBudget::with_ceiling(1);

        for (label, parked) in [("trashed", 1_u8), ("archived", 2), ("both", 3)] {
            let mut inst =
                Instance::new(&format!("repair-{label}"), &format!("/tmp/repair-{label}"));
            inst.source_profile = "repair-parked".into();
            inst.tool = "claude".into();
            inst.status = Status::Running;
            if parked & 1 != 0 {
                inst.trash();
            }
            if parked & 2 != 0 {
                inst.archive();
            }
            // Keep the status stale to prove lifecycle metadata is not a blanket poller gate.
            inst.status = Status::Running;
            let no_live = crate::tmux::LiveSessionSnapshot::from_parts(
                Some(Vec::new()),
                Some(std::collections::HashMap::new()),
            );
            assert!(!inst.repair_session_id_poller_if_needed(&no_live));
            assert!(inst.session_id_poller.is_none());

            let live = crate::tmux::LiveSessionSnapshot::from_parts(
                Some(vec![crate::tmux::Session::generate_name(
                    &inst.id,
                    &inst.title,
                )]),
                Some(std::collections::HashMap::new()),
            );
            assert!(inst.repair_session_id_poller_if_needed(&live));
            assert!(inst.session_id_poller_is_running());
            inst.stop_poller();
        }
    }

    #[test]
    #[serial_test::serial]
    fn repair_checks_live_pane_before_resolving_support() {
        let app = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(app.path());
        let profile = "repair-guard-order";
        let config_path =
            crate::session::config::profile_config::get_profile_config_path(profile).unwrap();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        std::fs::write(
            config_path,
            "[[agents.claude.status_rules]]\nstatus = \"running\"\ncontains = \"repair-sentinel\"\n",
        )
        .unwrap();
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);

        for (label, parked) in [
            ("active", 0_u8),
            ("trashed", 1),
            ("archived", 2),
            ("both", 3),
        ] {
            assert!(!crate::tmux::status_rules::has_rules(profile, "claude"));
            let mut inst = Instance::new(
                &format!("repair-guard-order-{label}"),
                &format!("/tmp/repair-guard-order-{label}"),
            );
            inst.source_profile = profile.into();
            inst.tool = "claude".into();
            if parked & 1 != 0 {
                inst.trash();
            }
            if parked & 2 != 0 {
                inst.archive();
            }
            let live = crate::tmux::LiveSessionSnapshot::from_parts(
                Some(Vec::new()),
                Some(std::collections::HashMap::new()),
            );

            assert!(!inst.repair_session_id_poller_if_needed(&live));
            assert!(
                !crate::tmux::status_rules::has_rules(profile, "claude"),
                "repair resolved support for {label} row without a live agent pane"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn repair_keeps_live_stopped_and_errored_rows_eligible() {
        let app = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(app.path());
        let _budget = crate::session::poller::test_support::IsolatedBudget::with_ceiling(1);

        for status in [Status::Stopped, Status::Error] {
            let mut inst = Instance::new("repair-status", "/tmp/repair-status");
            inst.source_profile = "repair-status".into();
            inst.tool = "claude".into();
            inst.status = status;
            let live = crate::tmux::LiveSessionSnapshot::from_parts(
                Some(vec![crate::tmux::Session::generate_name(
                    &inst.id,
                    &inst.title,
                )]),
                Some(std::collections::HashMap::new()),
            );

            assert!(inst.repair_session_id_poller_if_needed(&live));
            assert!(inst.session_id_poller_is_running());
            inst.stop_poller();
        }
    }

    #[test]
    #[serial_test::serial]
    fn repair_keeps_live_pi_rows_eligible() {
        let app = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(app.path());
        let _budget = crate::session::poller::test_support::IsolatedBudget::with_ceiling(1);
        let mut inst = Instance::new("repair-pi", "/tmp/repair-pi");
        inst.source_profile = "repair-pi".into();
        inst.tool = "pi".into();
        inst.mark_pi_extension_launched_for_test();
        let live = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![crate::tmux::Session::generate_name(
                &inst.id,
                &inst.title,
            )]),
            Some(std::collections::HashMap::new()),
        );

        assert!(inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller_is_running());
        inst.stop_poller();
    }

    /// The live arm is decisive and the derived arm is the fallback only when the scan found
    /// nothing live at all.
    #[test]
    fn poller_seed_name_prefers_the_live_agent_and_falls_back_to_the_derived_name() {
        const ID: &str = "9f2c41d6-0000-4000-8000-000000000001";
        let agent = crate::tmux::Session::generate_name(ID, "Vikings");
        let renamed = crate::tmux::Session::generate_name(ID, "Vikings the sequel");
        // A title sanitizing under TERMINAL_PREFIX: the agent name the name shape alone refuses,
        // which the live scan now answers with because the session says what kind it is.
        let aux_shaped = crate::tmux::Session::generate_name(ID, "term rewriting");

        // (case, what the live scan says, derived name, expected seed)
        type Case<'a> = (&'a str, super::AgentSeed, Option<&'a str>, Option<&'a str>);
        let cases: Vec<Case> = vec![
            (
                "agent pane live",
                super::AgentSeed::Agent(agent.clone()),
                Some(&agent),
                Some(&agent),
            ),
            (
                "live agent under its pre-rename name",
                super::AgentSeed::Agent(agent.clone()),
                Some(&renamed),
                Some(&agent),
            ),
            (
                "live agent whose title reads as a terminal",
                super::AgentSeed::Agent(aux_shaped.clone()),
                Some(&aux_shaped),
                Some(&aux_shaped),
            ),
            (
                "only a terminal outlived the agent",
                super::AgentSeed::NoUniqueAgent,
                Some(&agent),
                None,
            ),
            (
                "nothing live yet",
                super::AgentSeed::NothingLive,
                Some(&agent),
                Some(&agent),
            ),
            (
                "nothing live and no derived name",
                super::AgentSeed::NothingLive,
                None,
                None,
            ),
            (
                "nothing live and an aux-shaped derived name",
                super::AgentSeed::NothingLive,
                Some(&aux_shaped),
                None,
            ),
        ];

        for (case, live, derived, expected) in cases {
            assert_eq!(
                super::poller_seed_name(live, || derived.map(str::to_string), ID).as_deref(),
                expected,
                "{case}"
            );
        }
    }

    /// #3888 end to end: a row titled `term rewriting` whose agent session is live and marked gets
    /// a poller, on that session.
    #[test]
    #[serial_test::serial]
    #[cfg(unix)]
    fn an_aux_shaped_title_polls_its_marked_agent_pane() {
        let _env_read = crate::session::test_support::EnvGuard::read_lock();
        use std::os::unix::fs::PermissionsExt;

        let _budget = crate::session::poller::test_support::IsolatedBudget::with_ceiling(1);
        let temp = tempfile::tempdir().unwrap();
        let mut inst = Instance::new("term rewriting", "/tmp/aux-shaped-title");
        inst.tool = "claude".to_string();
        let live_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        assert!(
            !crate::tmux::agent_session_belongs_to(&live_name, &inst.id),
            "fixture: this title's agent name is one the name shape refuses"
        );

        // A `tmux` answering every query with that one session, marked as the
        // agent, which is what the real `list-sessions -F` scan reads back.
        let shim = temp.path().join("tmux");
        std::fs::write(
            &shim,
            format!("#!/bin/sh\necho '{live_name}|1789065184|agent'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = format!(
            "{}:{}",
            temp.path().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _guard = crate::session::test_support::EnvGuard::set(&[("PATH", path)]);

        assert_eq!(inst.maybe_start_poller(), PollerStart::Started);
        assert!(inst.session_id_poller_is_running());
        inst.stop_poller();
    }

    /// The race #3880 describes: repair sees a live agent pane in its snapshot, the agent dies
    /// before `maybe_start_poller` re-queries tmux, and only the paired terminal answers.
    #[test]
    fn repair_declines_when_the_agent_pane_dies_under_the_snapshot() {
        let budget = crate::session::poller::test_support::IsolatedBudget::with_ceiling(1);
        let mut inst = Instance::new("term rewriting", "/tmp/agent-died-under-snapshot");
        inst.tool = "claude".to_string();
        let snapshot = crate::tmux::LiveSessionSnapshot::from_parts(
            Some(vec![crate::tmux::Session::generate_name(
                &inst.id, "Vikings",
            )]),
            Some(std::collections::HashMap::new()),
        );
        assert!(
            inst.has_live_agent_pane_in(&snapshot),
            "fixture: the snapshot repair gates on still shows an agent pane"
        );
        assert!(
            !crate::tmux::agent_session_belongs_to(
                &crate::tmux::Session::generate_name(&inst.id, &inst.title),
                &inst.id
            ),
            "fixture: this title's own agent name is aux-shaped, so the live \
             re-query resolves no agent name"
        );

        assert!(!inst.repair_session_id_poller_if_needed(&snapshot));

        assert!(
            inst.session_id_poller.is_none(),
            "no poller on the wrong pane"
        );
        assert_eq!(budget.active(), 0, "a declined start takes no budget slot");
        assert_eq!(
            inst.poller_repair,
            Default::default(),
            "nothing to poll is not a failed repair: the next tick looks again"
        );
    }
}
