//! Owning the background `SessionPoller` attached to a running session.

use super::*;

impl Instance {
    /// Whether this session should run a session-id poller: the agent has a
    /// resume strategy to capture for, and its conversation is not already
    /// known.
    ///
    /// Pi polls its sidecar or nothing: the pane publishes its own
    /// conversation, and a store keyed by cwd cannot say which pane owns what.
    /// Reads memory only: this runs per session on every TUI refresh.
    pub fn supports_session_poller(&self) -> bool {
        let Some(agent) = self.resolved_agent() else {
            return false;
        };
        // Pi polls only what names a pane. Without the extension there is
        // nothing attributable to observe, so it does not poll at all.
        if agent.name == "pi" && !self.uses_pi_session_sidecar() {
            return false;
        }
        !matches!(
            agent.resume_strategy,
            crate::agents::ResumeStrategy::Unsupported
        )
    }

    pub fn maybe_start_poller(&mut self) {
        self.maybe_start_poller_since(None);
    }

    pub(super) fn maybe_start_poller_since(&mut self, omp_metadata: Option<OmpCaptureMetadata>) {
        if !self.supports_session_poller() {
            return;
        }
        let Some(tool) = self.capture_agent_name() else {
            return;
        };

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
                // Sidecar or nothing. Pi's store is keyed by cwd and names no
                // pane, so a scan of it can only guess, and a guess is what
                // #3576 cost. A binary that cannot load the extension gets no
                // poller and, absent a pin, no resume.
                if !self.uses_pi_session_sidecar() {
                    return;
                }
                // No source means the pane cannot be attributed; it does not
                // fall back to the host sidecar.
                let Some(source) = self.pi_sidecar_source() else {
                    return;
                };
                let inner = crate::session::capture::pi_sidecar_poll_fn(self.id.clone(), source);
                let poll_fn: crate::session::poller::SessionIdPollFn = Box::new(move |_| inner());
                let cb_instance_id = self.id.clone();
                let on_change: Box<dyn Fn(&str) + Send + 'static> = Box::new(
                    move |new_id: &str| {
                        tracing::info!(target: "session.store", "Session ID observed for {}: {}", cb_instance_id, new_id);
                    },
                );
                let initial = initial_known
                    .clone()
                    .map(crate::session::poller::SessionIdObservation::instance_sidecar);
                if poller.start_observations(instance_id.clone(), poll_fn, on_change, initial) {
                    self.session_id_poller = Some(Arc::new(Mutex::new(poller)));
                }
                return;
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
    use crate::session::instance::test_helpers::install_aliases;
    use crate::session::Instance;

    // Restart, stop, and the sid_persist path all tear down through this
    // helper, so flushing here covers each of them. Restart is the one that
    // was missed when only `stop` flushed.
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

    /// A custom agent resolves its poller through `agent_detect_as`. Keying
    /// the gate off `tool` raw missed on every wrapper, so none of them ever
    /// observed a conversation id (#3638).
    #[test]
    fn custom_agents_poll_through_their_detect_as_base() {
        const PROFILE: &str = "custom-agent-poller-test";
        let _registry = install_aliases(
            PROFILE,
            &[
                ("claude-personal", "claude"),
                ("cursor-personal", "cursor"),
                ("pi-personal", "pi"),
            ],
        );

        let mut wrapper = Instance::new("wrapper", "/tmp/custom-agent-poller");
        wrapper.source_profile = PROFILE.to_string();
        wrapper.tool = "claude-personal".to_string();
        wrapper.command = "claude-personal".to_string();
        assert!(wrapper.supports_session_poller());

        // The base agent's strategy still decides: an unresumable one polls
        // for nothing, and Pi polls only what names a pane.
        for tool in ["cursor-personal", "pi-personal"] {
            let mut inst = Instance::new("wrapper", "/tmp/custom-agent-poller");
            inst.source_profile = PROFILE.to_string();
            inst.tool = tool.to_string();
            inst.command = tool.to_string();
            assert!(!inst.supports_session_poller(), "{tool}");
        }

        // An unaliased custom agent names no built-in and still polls nothing.
        let mut unmapped = Instance::new("wrapper", "/tmp/custom-agent-poller");
        unmapped.source_profile = PROFILE.to_string();
        unmapped.tool = "unmapped-agent".to_string();
        assert!(!unmapped.supports_session_poller());
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
}
