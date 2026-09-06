//! Owning the background `SessionPoller` attached to a running session.

use super::*;
use fs2::FileExt as _;
use sha2::{Digest as _, Sha256};

const MANAGED_CAPTURE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// Outcome of [`Instance::maybe_start_poller`].
///
/// Only the last two variants are failures a caller should retry with
/// backoff or warn about. A session can legitimately have nothing to poll
/// right now — its capture metadata is not resolvable yet, its sandbox
/// store is not mounted, another process holds the store lease — and
/// treating those as failed spawns misreports a healthy fleet as over
/// budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollerStart {
    /// A poller thread is running for this session (started now, or already).
    Started,
    /// Nothing to poll for this session at the moment; not a failure.
    NotApplicable,
    /// Another process owns the managed capture store; a retry is scheduled
    /// on `session_id_poller_retry_after`.
    Deferred,
    /// The process-wide poller-thread budget is spent.
    BudgetExhausted,
    /// The OS refused to spawn the poller thread.
    SpawnFailed,
}

fn try_acquire_managed_capture_lease(
    backend: crate::agents::SessionCaptureBackend,
    store: &Path,
) -> Option<std::fs::File> {
    let lock_dir = crate::session::get_app_dir().ok()?.join("capture-locks");
    std::fs::create_dir_all(&lock_dir).ok()?;
    let store = std::fs::canonicalize(store).ok()?;
    let mut digest = Sha256::new();
    digest.update(format!("{backend:?}\0"));
    digest.update(store.as_os_str().as_encoded_bytes());
    let mut key = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(&mut key, "{byte:02x}").ok()?;
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
        return None;
    }
    let lease = options.open(path).ok()?;
    lease.try_lock_exclusive().ok()?;
    Some(lease)
}

impl Instance {
    /// Whether this session should run a session-id poller: the agent has a
    /// resume strategy to capture for, and its conversation is not already
    /// known.
    ///
    /// Pi polls its sidecar or nothing: the pane publishes its own
    /// conversation, and a store keyed by cwd cannot say which pane owns what.
    /// Reads memory only: this runs per session on every TUI refresh.
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
        let Some((capture, context)) = self.resolved_session_support() else {
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
        let Some(current_store) = self.sandbox_capture_store_dir() else {
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
                    || peer.resolved_capture_backend() != Some(backend)
                {
                    continue;
                }
                let Some(peer_store) = peer.sandbox_capture_store_dir() else {
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
    ///
    /// The one place a start succeeds, so it is also the one place the
    /// repair schedule is cleared: a poller started directly (session
    /// create, restart, resume) must not leave a stale backoff behind for
    /// the next repair to wait out.
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
        if self.session_id_poller_is_running() {
            return PollerStart::Started;
        }
        self.session_id_poller = None;
        let Some((capture, context)) = self.resolved_session_support() else {
            return PollerStart::NotApplicable;
        };
        let backend = capture.backend;
        if !self.supports_session_poller() {
            return PollerStart::NotApplicable;
        }
        let managed_lease =
            if context == crate::agents::SessionCaptureContext::ManagedExclusiveStore {
                let Some(store) = self.sandbox_capture_store_dir() else {
                    return PollerStart::NotApplicable;
                };
                // Lease contention is the common multi-process loser path. Check it
                // before loading every profile to prove store exclusivity.
                let Some(lease) = try_acquire_managed_capture_lease(backend, &store) else {
                    self.session_id_poller_retry_after =
                        Some(std::time::Instant::now() + MANAGED_CAPTURE_RETRY_BACKOFF);
                    tracing::warn!(target: "session.capture", session = %self.id, ?backend,
                    "Session capture deferred because another process owns this store");
                    return PollerStart::Deferred;
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

        let tmux_session_name = self
            .tmux_env_session_name()
            .or_else(|| {
                self.tmux_session()
                    .ok()
                    .map(|session| session.name().to_string())
            })
            .unwrap_or_default();
        let omp_metadata = if backend == crate::agents::SessionCaptureBackend::Omp {
            let Some(options) = self.omp_capture_options() else {
                return PollerStart::NotApplicable;
            };
            omp_metadata.or_else(|| self.omp_capture_metadata(&tmux_session_name, &options, None))
        } else {
            None
        };

        let mut poller = SessionPoller::new(tmux_session_name);
        let instance_id = self.id.clone();
        let initial_known = self.agent_session_id.clone();
        let extra_excludes = self.retroactive_capture_exclusion_set();

        if backend == crate::agents::SessionCaptureBackend::Omp {
            let Some(metadata) = omp_metadata.as_ref() else {
                return PollerStart::NotApplicable;
            };
            let poll_fn: crate::session::poller::SessionIdPollFn = if self.is_sandboxed() {
                let Some(sandbox) = self.sandbox_info.as_ref() else {
                    return PollerStart::NotApplicable;
                };
                Box::new(omp_poll_fn_sandboxed(
                    sandbox.container_name.clone(),
                    self.id.clone(),
                    Some(metadata.launch_marker.clone()),
                    extra_excludes,
                ))
            } else {
                Box::new(omp_poll_fn(self.id.clone(), extra_excludes))
            };
            let log_id = self.id.clone();
            let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id| {
                tracing::info!(target: "session.store", "Session ID observed for {}: {}", log_id, new_id);
            });
            let initial = initial_known.map(|sid| metadata.session_observation(sid));
            let spawn = poller.start_observations(instance_id, poll_fn, on_change, initial);
            return self.install_poller(poller, spawn);
        }

        if backend == crate::agents::SessionCaptureBackend::Pi {
            let Some(source) = self.pi_sidecar_source() else {
                return PollerStart::NotApplicable;
            };
            let inner = crate::session::capture::pi_sidecar_poll_fn(self.id.clone(), source);
            let poll_fn: crate::session::poller::SessionIdPollFn = Box::new(move |_| inner());
            let log_id = self.id.clone();
            let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id| {
                tracing::info!(target: "session.store", "Session ID observed for {}: {}", log_id, new_id);
            });
            let initial =
                initial_known.map(crate::session::poller::SessionIdObservation::instance_sidecar);
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
        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = match backend {
            crate::agents::SessionCaptureBackend::Claude
            | crate::agents::SessionCaptureBackend::HookSidecar => {
                let sidecar_id = self.id.clone();
                Box::new(move || crate::hooks::read_hook_session_id(&sidecar_id))
            }
            crate::agents::SessionCaptureBackend::Codex => {
                let Some(store) = self.sandbox_capture_store_dir() else {
                    return PollerStart::NotApplicable;
                };
                Box::new(codex_poll_fn_sandboxed_store(
                    store,
                    self.container_workdir(),
                    self.id.clone(),
                    capture_floor,
                    extra_excludes,
                ))
            }
            crate::agents::SessionCaptureBackend::Gemini => {
                let Some(store) = self.sandbox_capture_store_dir() else {
                    return PollerStart::NotApplicable;
                };
                Box::new(gemini_poll_fn_sandboxed_store(
                    store,
                    self.container_workdir(),
                    self.id.clone(),
                    capture_floor,
                    extra_excludes,
                ))
            }
            crate::agents::SessionCaptureBackend::Hermes => {
                let Some(store) = self.sandbox_capture_store_dir() else {
                    return PollerStart::NotApplicable;
                };
                Box::new(hermes_poll_fn_sandboxed_store(
                    store,
                    self.container_workdir(),
                    self.id.clone(),
                    capture_floor,
                    extra_excludes,
                ))
            }
            crate::agents::SessionCaptureBackend::Kimi => {
                let Some(store) = self.sandbox_capture_store_dir() else {
                    return PollerStart::NotApplicable;
                };
                Box::new(kimi_poll_fn_sandboxed_store(
                    store,
                    self.container_workdir(),
                    self.id.clone(),
                    capture_floor_ms,
                    extra_excludes,
                ))
            }
            crate::agents::SessionCaptureBackend::PrimeAgent => {
                let Some(store) = self.sandbox_capture_store_dir() else {
                    return PollerStart::NotApplicable;
                };
                Box::new(prime_agent_poll_fn_sandboxed_store(
                    store,
                    self.container_workdir(),
                    self.id.clone(),
                    capture_floor_ms,
                    extra_excludes,
                ))
            }
            crate::agents::SessionCaptureBackend::OpenCode
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

        let log_id = self.id.clone();
        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id| {
            tracing::info!(target: "session.store", "Session ID observed for {}: {}", log_id, new_id);
        });
        let spawn = poller.start(instance_id, poll_fn, on_change, initial_known);
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
    ///
    /// OMP pollers reload pane metadata on every tick, so a replacement binds
    /// to the durable generation that won any concurrent restart race.
    pub(crate) fn repair_session_id_poller_if_needed(
        &mut self,
        snapshot: &crate::tmux::LiveSessionSnapshot,
    ) -> bool {
        // Structured sessions have ACP workers rather than tmux panes. Their
        // lifecycle is reconciled by the daemon, so probing tmux here can only
        // fail and is especially costly from the native TUI's refresh loop.
        if self.is_structured()
            || !self.supports_session_poller()
            || self.session_id_poller_is_running()
            || self
                .session_id_poller_retry_after
                .is_some_and(|deadline| std::time::Instant::now() < deadline)
            || !self.has_live_tmux_pane_in(snapshot)
        {
            return false;
        }
        let now = std::time::Instant::now();
        // A failed attempt schedules the next one (5 s doubling to 60 s), so
        // an over-budget fleet is not re-probed — and re-warned — for every
        // session on every 2 s tick.
        if !self.poller_repair.due(now) {
            return false;
        }
        self.session_id_poller = None;
        match self.maybe_start_poller() {
            // `install_poller` cleared the schedule.
            PollerStart::Started => true,
            // Nothing failed: the session has nothing to poll right now, or
            // the managed store's own retry deadline governs. Neither is a
            // reason to back off or to blame the thread budget.
            PollerStart::NotApplicable | PollerStart::Deferred => {
                self.poller_repair.reset();
                false
            }
            PollerStart::BudgetExhausted => {
                self.defer_poller_repair(now, "budget exhausted");
                false
            }
            PollerStart::SpawnFailed => {
                self.defer_poller_repair(now, "start failed");
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

    pub(super) fn stop_poller(&self) {
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
        // A Pi pane's last word is in its sidecar, which no poller may have
        // read: a CLI-only pane has none, and a restart tears the pane down
        // before the next one starts. Every teardown reaches here, so this is
        // where the flush belongs rather than at one call site.
        self.flush_pi_sidecar_if_published();
        // stop_poller() signals the thread but leaves the handle in place, so
        // this is_some() means "a poller existed and may have queued a final
        // observation": drain it before dropping the handle below.
        self.stop_poller();
        if self.session_id_poller.is_some() {
            let file_watch = self.resolve_file_watch();
            let _ = crate::session::sync::drain_and_persist_session_ids_lifecycle_locked(
                std::slice::from_mut(self),
                &file_watch,
            );
        }
        self.session_id_poller = None;
    }
}

#[cfg(test)]
mod tests {
    use super::PollerStart;
    use crate::session::{Instance, SandboxInfo, Status};

    /// The 2026-09-04 fleet shape: two sessions past the poller budget were
    /// re-probed by the daemon every 2 s tick, each attempt logging a
    /// "budget exhausted" + "Failed to start session poller" pair (~2.5
    /// lines/s). Repair must schedule its retry instead.
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

    /// A direct (non-repair) start is a success too: it must clear any
    /// schedule left by earlier deferrals, or a poller that later dies waits
    /// out a stale 60 s delay before repair even looks at it.
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

    /// A live pane with nothing to poll right now (here: an OMP pane whose
    /// capture metadata is not resolvable) is not a failed spawn. Repair
    /// must not schedule a backoff for it, and must not warn that the thread
    /// budget is too small.
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

    // Restart, stop, standalone attach, and sid_persist all tear down through
    // this helper. Restart was missed when only `stop` flushed.
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
        crate::hooks::write_session_id_via_guard(&inst.id, published).unwrap();

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
    fn sandboxed_pi_polls_the_bind_backed_sidecar() {
        // A container publishes under its own bind, not the host hook dir.
        // Reading the wrong one is silent: the poller simply never observes.
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::EnvGuard::set(&[("HOME", temp.path())]);

        let mut host = Instance::new("pi-host-poll", "/tmp/pi-poll");
        host.tool = "pi".to_string();
        assert_eq!(
            host.pi_sidecar_source().and_then(|s| match s {
                crate::session::instance::PiSidecarSource::SandboxDir(d) => Some(d),
                _ => None,
            }),
            None,
            "a host pane reads the hook dir"
        );

        let mut sandboxed = Instance::new("pisandboxpoll001", "/tmp/pi-poll");
        sandboxed.tool = "pi".to_string();
        sandboxed.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "aoe-pi-poll".to_string(),
            extra_env: None,
            custom_instruction: None,
            container_workdir: None,
            before_start_env: Vec::new(),
        });
        let dir = sandboxed
            .pi_sidecar_source()
            .and_then(|s| match s {
                crate::session::instance::PiSidecarSource::SandboxDir(d) => Some(d),
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
        let poll = crate::session::capture::pi_sidecar_poll_fn(
            sandboxed.id.clone(),
            sandboxed
                .pi_sidecar_source()
                .expect("a resolvable sandbox source"),
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
        let mut inst = Instance::new("gemini", "/tmp/gemini-backoff");
        inst.tool = "gemini".to_string();
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: Some("/workspace/gemini-backoff".to_string()),
        });
        let name = inst.tmux_session().unwrap().name().to_string();
        let live = crate::tmux::LiveSessionSnapshot::from_parts(Some(vec![name]), None);
        inst.session_id_poller_retry_after =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(60));

        assert!(!inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller.is_none());

        inst.session_id_poller_retry_after = None;
        std::fs::create_dir_all(inst.sandbox_capture_store_dir().unwrap()).unwrap();
        assert!(inst.repair_session_id_poller_if_needed(&live));
        assert!(inst.session_id_poller_is_running());
    }

    fn sandboxed_gemini(title: &str, project_path: &str, workdir: &str) -> Instance {
        let mut inst = Instance::new(title, project_path);
        inst.tool = "gemini".to_string();
        inst.status = Status::Running;
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: format!("test-{}", inst.id),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: Some(workdir.to_string()),
        });
        inst
    }

    #[test]
    #[serial_test::serial]
    fn managed_capture_exclusivity_is_store_based_across_profiles() {
        let app = tempfile::tempdir().unwrap();
        let _app_guard = crate::session::test_support::isolate_app_dir_at(app.path());
        let backend = crate::agents::SessionCaptureBackend::Gemini;
        let current_profile = "capture-owner-a";
        let peer_profile = "capture-owner-b";
        let current_storage = crate::session::Storage::new_unwatched(current_profile).unwrap();
        let peer_storage = crate::session::Storage::new_unwatched(peer_profile).unwrap();

        let mut current = sandboxed_gemini("current", "/repos/current", "/workspace/current");
        current.source_profile = current_profile.to_string();
        current.sandbox_store_generation = 1;
        let mut peer = sandboxed_gemini("peer", "/repos/peer", "/workspace/peer");
        peer.source_profile = peer_profile.to_string();
        peer.sandbox_store_generation = 1;
        let shared_store = current.sandbox_capture_store_dir().unwrap();
        assert_eq!(peer.sandbox_capture_store_dir().unwrap(), shared_store);
        std::fs::create_dir_all(&shared_store).unwrap();

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
            "different rows and workdirs sharing one store are not exclusive"
        );

        peer.sandbox_store_generation =
            crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION;
        let peer_store = peer.sandbox_capture_store_dir().unwrap();
        assert_ne!(peer_store, shared_store);
        std::fs::create_dir_all(&peer_store).unwrap();
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
        assert!(
            super::try_acquire_managed_capture_lease(backend, store.path()).is_none(),
            "another row using the same store must contend regardless of workspace"
        );
        #[cfg(unix)]
        {
            let alias = app.path().join("store-alias");
            std::os::unix::fs::symlink(store.path(), &alias).unwrap();
            assert!(
                super::try_acquire_managed_capture_lease(backend, &alias).is_none(),
                "a symlink to the same physical store must contend"
            );
        }
        assert!(
            super::try_acquire_managed_capture_lease(backend, &app.path().join("missing"))
                .is_none(),
            "an unresolved store identity must fail closed"
        );
        let distinct = super::try_acquire_managed_capture_lease(backend, other_store.path())
            .expect("a distinct store has a distinct lease");

        drop(first);
        assert!(super::try_acquire_managed_capture_lease(backend, store.path()).is_some());
        drop(distinct);
    }
}
