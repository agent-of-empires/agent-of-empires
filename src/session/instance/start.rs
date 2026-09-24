//! The launch path: reserving, spawning, and finalizing a tmux session.

use super::*;

#[cfg(test)]
pub(super) mod test_support {
    use super::Instance;

    #[derive(Clone, Copy)]
    pub(in crate::session::instance) enum FinalizePhase {
        Before,
        After,
    }

    type Callback = Box<dyn FnMut(&Instance, FinalizePhase)>;
    thread_local! {
        static OBSERVER: std::cell::RefCell<Option<(String, Callback)>> = const { std::cell::RefCell::new(None) };
    }

    pub(in crate::session::instance) struct FinalizeObserver;

    impl FinalizeObserver {
        pub(in crate::session::instance) fn install(
            id: String,
            callback: impl FnMut(&Instance, FinalizePhase) + 'static,
        ) -> Self {
            OBSERVER.with(|slot| {
                assert!(
                    slot.borrow().is_none(),
                    "one launch observer per test thread"
                );
                *slot.borrow_mut() = Some((id, Box::new(callback)));
            });
            Self
        }
    }

    impl Drop for FinalizeObserver {
        fn drop(&mut self) {
            OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    pub(super) fn observe(instance: &Instance, phase: FinalizePhase) {
        OBSERVER.with(|slot| {
            if let Some((id, callback)) = slot.borrow_mut().as_mut() {
                if *id == instance.id {
                    callback(instance, phase);
                }
            }
        });
    }
}

/// Outcome of `start_with_resume_fallback`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Session ID was set and resume succeeded; pane is alive.
    Resumed,
    /// Resume was attempted, but the pane died during the probe before AoE observed an explicit
    /// invalid-resume signal.
    ResumeFailed { sid: String },
    /// No resume cascade ran.
    Fresh,
    /// A resume was skipped, and the session started fresh instead, because `sid` already failed a
    /// resume probe once before.
    FreshAfterFailedResume { sid: String },
}

/// What `start_with_size_opts` did with the agent's session id this call.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSidOutcome {
    /// `acquire_session_id` reused a prior sid: `ResumeIntent::Use(sid)`, observed
    /// `agent_session_id`, or retroactive-capture hit.
    Existing { sid: String },
    Fresh {
        /// Set when the fresh launch pinned an id the session already had stored, rather than a
        /// UUID minted for a brand-new conversation.
        pinned_prior_sid: Option<String>,
    },
    /// `start_with_size_opts` short-circuited before `apply_session_flags` ran: structured
    /// view-mode session, or a pre-existing tmux pane that is still alive (kill_clean cache race).
    Skipped,
}

impl Instance {
    pub fn start(&mut self) -> Result<()> {
        self.start_with_size(None)
    }

    pub fn start_with_size(&mut self, size: Option<(u16, u16)>) -> Result<()> {
        self.start_with_size_opts(size, false).map(|_| ())
    }

    /// Start the session, optionally skipping on_launch hooks (e.g. when they
    /// already ran in the background creation poller).
    pub fn start_with_size_opts(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) -> Result<LaunchSidOutcome> {
        crate::session::validate_instance_id(&self.id)
            .context("refusing to launch: AOE_INSTANCE_ID failed validation")?;
        if self.is_structured() {
            return Ok(LaunchSidOutcome::Skipped);
        }
        let profile = self.effective_profile();
        let storage = crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;

        let title_lock = crate::session::storage::acquire_session_title_lock(&self.id)
            .context("failed to acquire instance launch title lock")?;
        let lifecycle_lock = storage
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance launch lock")?;
        self.reconcile_from_disk();
        if self.is_structured() {
            return Ok(LaunchSidOutcome::Skipped);
        }
        let session = self.tmux_session()?;
        let corpse_pane = if session.exists() {
            if !session.is_pane_dead() {
                return Ok(LaunchSidOutcome::Skipped);
            }
            true
        } else {
            false
        };
        self.acquire_lifecycle_reservation(
            &storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
        )?;

        // The durable reservation excludes peer launches while user hooks run. Both flocks must be
        // absent because a hook may invoke aoe for this same session.
        drop(lifecycle_lock);
        drop(title_lock);
        let hook_result = self.run_pre_launch_hooks(skip_on_launch, &profile);
        let (_title_lock, _lifecycle_lock) =
            self.reacquire_launch_locks_after_hooks(&storage, hook_result)?;
        self.reconcile_sidecar_into_disk();
        let expected = self.apply_fresh_launch_intent();

        let mut prepared = match self.prepare_launch_command(expected) {
            Ok(prepared) => prepared,
            Err(error) => {
                self.fail_reserved_launch(&storage, &error, false);
                return Err(error);
            }
        };
        let result = (|| {
            if corpse_pane {
                self.kill_clean_locked()?;
                prepared = self.refresh_prepared_prime_launch_after_pane_stop(prepared)?;
            }
            let outcome = self.spawn_prepared_launch(size, &profile, prepared)?;
            self.commit_lifecycle_launch(&storage, false)?;
            Ok(outcome)
        })();
        if let Err(error) = result {
            self.fail_reserved_launch(&storage, &error, true);
            return Err(error);
        }
        result
    }

    pub(super) fn apply_fresh_launch_intent(&mut self) -> ConversationState {
        let expected = self.conversation_state();
        if std::mem::take(&mut self.force_fresh_next_launch) {
            self.resume_intent = ResumeIntent::Cleared;
        }
        expected
    }

    /// The conversation a fresh launch abandons, for the capture-exclusion log.
    ///
    /// A launch that re-emits the id it started from, preallocated or observed
    /// under this launch's execution, is running that conversation: nothing is
    /// abandoned, and a stale exclusion for it must be dropped instead. Only an
    /// id the launch did not keep is recorded as abandoned.
    fn abandoned_prior_conversation(
        &self,
        expected: &ConversationState,
        prior_sid: &str,
    ) -> Option<ConversationBinding> {
        let kept = self.agent_session_id.as_deref() == Some(prior_sid)
            && self.agent_session_binding.as_ref().is_some_and(|binding| {
                binding.session_id == prior_sid
                    && self.active_execution.as_ref().is_some_and(|execution| {
                        binding.execution.as_ref() == Some(&execution.binding)
                    })
            });
        if kept {
            return None;
        }
        Some(
            expected
                .binding
                .as_ref()
                .filter(|binding| binding.session_id == prior_sid)
                .cloned()
                .unwrap_or_else(|| ConversationBinding::unknown(prior_sid.to_string())),
        )
    }

    pub(super) fn spawn_prepared_launch(
        &mut self,
        size: Option<(u16, u16)>,
        profile: &str,
        mut prepared: PreparedLaunch,
    ) -> Result<LaunchSidOutcome> {
        let session = self.tmux_session()?;
        if session.exists() {
            anyhow::bail!(
                "session {} gained a tmux pane before its reserved launch",
                self.id
            );
        }
        if !self.is_sandboxed() {
            self.install_agent_status_hooks(self.status_agent(), prepared.execution.as_ref());
        }
        let canonicalized = prepared.canonical_conversation.is_some();
        let launch_sid = if prepared.is_existing {
            Some(
                prepared
                    .canonical_conversation
                    .as_ref()
                    .and_then(|state| state.session_id.clone())
                    .or_else(|| self.agent_session_id.clone())
                    .expect("existing launch command carries agent_session_id"),
            )
        } else {
            None
        };
        // Read before `finalize_launch`, which may replace `agent_session_id`.
        let pinned_prior_sid = self.agent_session_id.clone().filter(|sid| {
            prepared.expected_conversation.session_id.as_deref() == Some(sid.as_str())
        });

        tracing::debug!(
            target: "session.store",
            sandboxed = self.is_sandboxed(),
            has_command = prepared.command.is_some(),
            "agent launch command prepared"
        );

        self.clear_pane_identity_sidecar();

        let mut omp_capture_metadata = if let Some(plan) = prepared.omp_capture_plan {
            let launched_at_ms = SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .context("system clock predates UNIX_EPOCH during OMP launch")
                .and_then(|elapsed| {
                    u64::try_from(elapsed.as_millis())
                        .context("OMP launch timestamp does not fit in u64")
                })?;
            Some(OmpCaptureMetadata {
                layout: plan.layout,
                launched_at_ms,
                launch_id: plan.launch_id,
                launch_marker: plan.launch_marker,
                routing_fingerprint: plan.routing_fingerprint,
                container_runtime: plan.container_runtime,
            })
        } else {
            None
        };
        let omp_generation_published = self.publish_omp_launch_generation(
            profile,
            omp_capture_metadata.as_ref(),
            prepared.expected_prior_omp_generation.as_deref(),
        );
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            // The launch preamble (`wrap_omp_launch`) rewrites OMP's breadcrumb and writes the
            // capture marker only if the store's terminal-sessions directory already exists.
            if !self.is_sandboxed() {
                if let Err(error) = std::fs::create_dir_all(&metadata.layout.terminal_sessions) {
                    tracing::warn!(
                        target: "session.store",
                        instance = %self.id,
                        "OMP capture may no-op: could not ensure terminal-sessions dir: {error}"
                    );
                }
            }
            prepared.launch_env.pane.push(tmux::PaneEnvMutation::set(
                crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY.to_string(),
                metadata.launch_id.clone(),
            ));
        }
        if let (Some((reason, _)), Some(command)) = (
            prepared.sandbox_context_reset.as_ref(),
            prepared.command.as_mut(),
        ) {
            *command = format!(
                "{}\n{command}",
                crate::session::environment::native_context_notice_command(reason)
            );
        }
        self.capture_started_at = Some(SystemTime::now());
        session.create_with_size_env_and_container_env(
            &self.project_path,
            prepared.command.as_deref(),
            size,
            profile,
            &prepared.launch_env.pane,
            &prepared.launch_env.container,
        )?;
        if let Some((_, transactions)) = prepared.sandbox_context_reset.as_ref() {
            crate::migrations::v033_isolate_sandbox_content::acknowledge_context_reset(
                profile,
                &self.id,
                crate::migrations::v033_isolate_sandbox_content::NativeContextView::Terminal,
                self.lifecycle_generation,
                transactions,
                None,
            )?;
        }
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            let pane_generation =
                crate::tmux::env::get_env(session.name(), crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY);
            if !omp_generation_published
                || pane_generation.as_deref() != Some(metadata.launch_id.as_str())
            {
                omp_capture_metadata = None;
            }
        }

        if let Some(canonical) = prepared.canonical_conversation.take() {
            self.adopt_conversation_state(canonical);
        }
        if let Some(execution) = prepared.execution.take() {
            self.active_execution = Some(ActiveExecution {
                launch_id: execution.inputs.launch_id,
                binding: execution.binding.clone(),
                capture: execution
                    .capture
                    .or_else(|| omp_capture_metadata.clone().map(CaptureContext::Omp)),
                container: execution.inputs.container,
            });
            let native_mints_child = matches!(
                prepared.expected_conversation.intent,
                ResumeIntent::Fork { .. }
            ) && !matches!(
                execution.agent.fork_strategy,
                crate::agents::ForkStrategy::ClaudeFork
            );
            if native_mints_child {
                self.set_agent_conversation(None, None, None);
            } else if let Some(sid) = self.agent_session_id.clone() {
                let existing = self.agent_session_binding.as_ref().filter(|binding| {
                    binding.session_id == sid
                        && (binding.execution.as_ref() == Some(&execution.binding)
                            || (matches!(self.resume_intent, ResumeIntent::Default)
                                && binding.is_known()
                                && binding
                                    .execution
                                    .as_ref()
                                    .is_some_and(|prior| prior.agent == execution.binding.agent)))
                });
                let binding =
                    if matches!(prepared.expected_conversation.intent, ResumeIntent::Use(_)) {
                        self.resume_binding.clone()
                    } else {
                        existing.cloned()
                    }
                    .unwrap_or(ConversationBinding {
                        session_id: sid.clone(),
                        execution: Some(execution.binding.clone()),
                        provenance: ConversationProvenance::Preallocated,
                        transcript_path: None,
                    });
                self.set_agent_conversation(Some(sid), Some(binding), self.pi_session_path.clone());
            }
        } else {
            self.active_execution = None;
            if !matches!(self.resume_intent, ResumeIntent::Default)
                || self.agent_session_binding.as_ref().is_none_or(|binding| {
                    self.agent_session_id.as_deref() != Some(binding.session_id.as_str())
                        || !binding.is_known()
                })
            {
                self.agent_session_binding = None;
            }
        }
        if !prepared.is_existing {
            if let Some(prior_sid) = prepared.expected_conversation.session_id.clone() {
                match self.abandoned_prior_conversation(&prepared.expected_conversation, &prior_sid)
                {
                    Some(abandoned) => {
                        self.retroactive_capture_excludes.insert(abandoned);
                    }
                    None => {
                        let source = self.active_execution.as_ref().map(|active| &active.binding);
                        self.retroactive_capture_excludes
                            .retain(|binding| !binding.excludes_capture(&prior_sid, source));
                    }
                }
            }
        }
        #[cfg(test)]
        test_support::observe(self, test_support::FinalizePhase::Before);

        self.finalize_launch(
            session.name(),
            profile,
            &prepared.expected_conversation,
            omp_capture_metadata,
            canonicalized || prepared.carry_relocated,
        )?;

        #[cfg(test)]
        test_support::observe(self, test_support::FinalizePhase::After);
        Ok(match launch_sid {
            Some(sid) => LaunchSidOutcome::Existing { sid },
            None => LaunchSidOutcome::Fresh { pinned_prior_sid },
        })
    }

    /// Post-launch setup: persist state, start pollers, and apply tmux options.
    pub(super) fn finalize_launch(
        &mut self,
        session_name: &str,
        profile: &str,
        expected: &ConversationState,
        mut omp_capture_metadata: Option<OmpCaptureMetadata>,
        confirm_desired_conversation: bool,
    ) -> Result<()> {
        if let Some(metadata) = omp_capture_metadata.as_ref() {
            let published = serde_json::to_string(metadata).ok().and_then(|encoded| {
                crate::tmux::env::set_hidden_env(
                    session_name,
                    crate::tmux::env::AOE_OMP_CAPTURE_META_KEY,
                    &encoded,
                )
                .and_then(|()| {
                    crate::tmux::env::set_hidden_env(
                        session_name,
                        crate::tmux::env::AOE_OMP_CAPTURE_READY_KEY,
                        &metadata.launch_id,
                    )
                })
                .ok()
            });
            if published.is_none() {
                omp_capture_metadata = None;
            }
        }

        let desired = confirm_desired_conversation.then(|| {
            let mut desired = self.conversation_state();
            // Mirror persist_session_id's promotion of one-shot launch
            // directives, or the comparison below would reject the state it
            // itself produces for Cleared, Fork and publisher-pinned Use.
            if matches!(
                desired.intent,
                ResumeIntent::Cleared | ResumeIntent::Fork { .. }
            ) || (matches!(desired.intent, ResumeIntent::Use(_))
                && self.launch_has_session_publisher())
            {
                desired.intent = ResumeIntent::Default;
                desired.resume_binding = None;
            }
            desired
        });
        let outcome = self.persist_session_id(profile, expected);
        if desired.is_some_and(|desired| {
            !matches!(outcome, SidPersistOutcome::Published) || !desired.matches(self)
        }) {
            self.reconcile_from_disk();
            anyhow::bail!("durable publication was not confirmed; durable reconciliation was attempted but may be unavailable, and the pane may remain if reservation verification or teardown fails");
        }

        // Skip outcomes leave AOE_CAPTURED_SESSION_ID untouched: this path
        // runs before any poller publish, so env is empty for fresh sessions.
        let publish_sid = matches!(outcome, SidPersistOutcome::Published);
        let captured_sid: Option<String> = if publish_sid {
            self.agent_session_id.clone()
        } else {
            None
        };

        let mut entries: Vec<(&str, &str, &str)> = vec![(
            session_name,
            crate::tmux::env::AOE_INSTANCE_ID_KEY,
            &self.id,
        )];
        if let Some(sid) = &captured_sid {
            entries.push((
                session_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                sid.as_str(),
            ));
        }
        if let Err(e) = crate::tmux::env::set_hidden_env_batch(&entries) {
            let keys: Vec<&str> = entries.iter().map(|(_, k, _)| *k).collect();
            tracing::warn!(target: "session.store",
            "Failed to set tmux env keys [{}] at finalize_launch: {}", keys.join(", "), e);
        }

        if publish_sid && self.agent_session_id.is_none() {
            if let Err(e) = crate::tmux::env::remove_hidden_env(
                session_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            ) {
                tracing::warn!(target: "session.store",
                instance = %self.id,
                "Failed to clear captured sid in tmux env: {}", e);
            }
        }

        self.maybe_start_poller_since(omp_capture_metadata);

        self.status = Status::Starting;
        self.last_start_time = Some(std::time::Instant::now());

        // Apply status bar options in a background thread to avoid blocking
        // the TUI on the multiple tmux subprocess calls they require.
        let session_name = session_name.to_string();
        let instance_id_for_log = self.id.clone();
        let title = self.title.clone();
        let branch = self.worktree_info.as_ref().map(|w| w.branch.clone());
        let sandbox = self.sandbox_display();
        let options_profile = profile.to_string();
        match std::thread::Builder::new()
            .name(format!("finalize-tmux-{}", instance_id_for_log))
            .spawn(move || {
                if let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::tmux::status_bar::apply_all_tmux_options(
                        &session_name,
                        &title,
                        branch.as_deref(),
                        sandbox.as_ref(),
                        &options_profile,
                    );
                })) {
                    tracing::error!(target: "session.store", "finalize-tmux thread panicked: {:?}", panic);
                }
            })
        {
            Ok(_handle) => {}
            Err(e) => {
                tracing::error!(target: "session.store",
                    session = %instance_id_for_log,
                    error = %e,
                    "Failed to spawn finalize-tmux thread"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_with_size_opts_returns_skipped_for_structured() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.view = View::Structured;
        let outcome = inst.start_with_size_opts(None, false).unwrap();
        assert_eq!(outcome, LaunchSidOutcome::Skipped);
    }

    fn instance_with_id(id: &str) -> Instance {
        let mut inst = Instance::new("tampered-id-test", "/tmp");
        inst.id = id.to_string();
        inst
    }

    #[test]
    fn start_with_size_opts_rejects_tampered_instance_id() {
        for poisoned in ["; rm -rf $HOME #", "../etc", ""] {
            let mut instance = instance_with_id(poisoned);
            let result = instance.start_with_size_opts(None, false);
            let err = match result {
                Ok(_) => panic!("must refuse tampered id at launch (id={poisoned:?})"),
                Err(e) => e,
            };
            assert!(
                err.to_string().contains("AOE_INSTANCE_ID"),
                "error must surface validator failure for id={poisoned:?}, got: {err}"
            );
            assert!(
                !instance.tmux_session().map(|s| s.exists()).unwrap_or(false),
                "no tmux session must exist after refusal for id={poisoned:?}"
            );
        }
    }
}
