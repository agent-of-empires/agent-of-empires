//! Owning the background `SessionPoller` attached to a running session.

use super::*;

impl Instance {
    /// Whether this agent uses a session ID poller for live tracking.
    pub fn supports_session_poller(&self) -> bool {
        crate::agents::get_agent(&self.tool).is_some_and(|a| {
            !matches!(
                a.resume_strategy,
                crate::agents::ResumeStrategy::Unsupported
            )
        })
    }

    pub fn maybe_start_poller(&mut self) {
        self.maybe_start_poller_since(None);
    }

    pub(super) fn maybe_start_poller_since(&mut self, omp_metadata: Option<OmpCaptureMetadata>) {
        if !self.supports_session_poller() {
            return;
        }
        let tool = self.tool.as_str();

        let tmux_session_name = self
            .tmux_env_session_name()
            .or_else(|| self.tmux_session().ok().map(|s| s.name().to_string()))
            .unwrap_or_default();
        let omp_metadata = if tool == "omp" {
            let options = match self.omp_capture_options() {
                Some(options) => options,
                None => return,
            };
            match omp_metadata
                .or_else(|| self.omp_capture_metadata(&tmux_session_name, &options, None))
            {
                Some(metadata) => Some(metadata),
                None => return,
            }
        } else {
            None
        };
        let mut poller = SessionPoller::new(tmux_session_name.clone());
        let instance_id = self.id.clone();
        let initial_known = self.agent_session_id.clone();
        // Snapshot persisted peer ownership and per-instance excludes at
        // poller-spawn time. This keeps storage reads off the hot polling path
        // while preventing the poller from adopting a conversation another row
        // parked during a tool swap.
        let extra_excludes = self.retroactive_capture_exclusion_set();
        if tool == "omp" {
            let Some(metadata) = omp_metadata.as_ref() else {
                return;
            };
            let poll_fn: crate::session::poller::SessionIdPollFn = if self.is_sandboxed() {
                let container_name = match self.sandbox_info.as_ref() {
                    Some(s) => s.container_name.clone(),
                    None => return,
                };
                Box::new(omp_poll_fn_sandboxed(
                    container_name,
                    self.id.clone(),
                    Some(metadata.launch_marker.clone()),
                    extra_excludes,
                ))
            } else {
                Box::new(omp_poll_fn(self.id.clone(), extra_excludes))
            };
            let cb_instance_id = self.id.clone();
            let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id: &str| {
                tracing::info!(target: "session.store", "Session ID observed for {}: {}", cb_instance_id, new_id);
            });
            let initial_known = initial_known.map(|sid| metadata.session_observation(sid));
            if poller.start_observations(instance_id.clone(), poll_fn, on_change, initial_known) {
                self.session_id_poller = Some(Arc::new(Mutex::new(poller)));
            } else {
                tracing::warn!(target: "session.store",
                    "Failed to start session poller for instance {}, poller will not be stored",
                    instance_id
                );
            }
            return;
        }

        let poll_fn: Box<dyn Fn() -> Option<String> + Send + 'static> = match tool {
            "claude" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(claude_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        initial_known.clone(),
                        instance_id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(claude_poll_fn(
                        self.project_path.clone(),
                        initial_known.clone(),
                        instance_id.clone(),
                        extra_excludes.clone(),
                        self.resolved_host_environment(),
                    ))
                }
            }
            "opencode" => {
                let launch_time_ms = crate::util::now_ms() as f64;
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(opencode_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        launch_time_ms,
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(opencode_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        launch_time_ms,
                        extra_excludes.clone(),
                    ))
                }
            }
            "vibe" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(vibe_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(vibe_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "pi" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(pi_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(pi_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "codex" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(codex_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(codex_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "gemini" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(gemini_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                } else {
                    Box::new(gemini_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes.clone(),
                    ))
                }
            }
            "hermes" => {
                if self.is_sandboxed() {
                    let container_name = match self.sandbox_info.as_ref() {
                        Some(s) => s.container_name.clone(),
                        None => return,
                    };
                    Box::new(hermes_poll_fn_sandboxed(
                        container_name,
                        self.container_workdir(),
                        self.id.clone(),
                        extra_excludes,
                    ))
                } else {
                    Box::new(hermes_poll_fn(
                        self.project_path.clone(),
                        self.id.clone(),
                        extra_excludes,
                    ))
                }
            }
            "copilot" => {
                // Host-only: the Copilot session-store SQLite db is read
                // directly on the host. Sandboxed sessions have no poller, so
                // their session id is never captured and they start fresh on
                // restart (sandbox resume is a follow-up).
                if self.is_sandboxed() {
                    return;
                }
                Box::new(copilot_poll_fn(
                    self.project_path.clone(),
                    self.id.clone(),
                    extra_excludes,
                ))
            }
            "kimi" => {
                // Host-only, mirroring Copilot: the Kimi session index is
                // read from the host store under the launched pane's
                // resolved environment. Sandboxed sessions have no poller
                // and start fresh on restart (sandbox resume is a
                // follow-up).
                if self.is_sandboxed() {
                    return;
                }
                let launch_time_ms = crate::util::now_ms() as f64;
                Box::new(kimi_poll_fn(
                    self.project_path.clone(),
                    self.id.clone(),
                    launch_time_ms,
                    extra_excludes,
                    self.resolved_host_environment(),
                ))
            }
            "prime-agent" => {
                // Host-only, mirroring Copilot and Kimi: the Prime Agent
                // sessions directory is read from the host `~/.prime/agent`.
                // Sandboxed sessions have no poller and start fresh on
                // restart (sandbox resume is a follow-up).
                if self.is_sandboxed() {
                    return;
                }
                let launch_time_ms = crate::util::now_ms() as f64;
                Box::new(prime_agent_poll_fn(
                    self.project_path.clone(),
                    self.id.clone(),
                    launch_time_ms,
                    extra_excludes,
                ))
            }
            _ => return,
        };

        let cb_instance_id = self.id.clone();

        // Log-only: the poller's raw observation must NOT be published to the
        // tmux hidden env here. This callback fires before any of the drain
        // guards in `sync.rs` run, and `build_exclusion_set` treats
        // AOE_CAPTURED_SESSION_ID as ownership truth — so a single transient
        // misobservation (e.g. a peer's fresher jsonl in a shared cwd, or the
        // `.claude.json` lastSessionId fallback) would instantly "claim" the
        // peer's sid, make the real owner exclude its own id, abandon its
        // anchor, and adopt a third session's conversation in a cascade
        // (#2858). `drain_and_persist_session_ids` publishes the env for
        // every touched instance after the guards and the CAS have settled.
        let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(move |new_id: &str| {
            tracing::info!(target: "session.store", "Session ID observed for {}: {}", cb_instance_id, new_id);
        });

        if poller.start(instance_id.clone(), poll_fn, on_change, initial_known) {
            self.session_id_poller = Some(Arc::new(Mutex::new(poller)));
        } else {
            tracing::warn!(target: "session.store",
                "Failed to start session poller for instance {}, poller will not be stored",
                instance_id
            );
        }
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
            || !self.has_live_tmux_pane_in(snapshot)
        {
            return false;
        }
        self.session_id_poller = None;
        self.maybe_start_poller();
        self.session_id_poller_is_running()
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
