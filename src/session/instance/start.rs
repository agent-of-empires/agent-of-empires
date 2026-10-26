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
///
/// Tmux/process failures propagate as `Err` so callers keep the existing
/// `Status::Error` + `last_error` path. Resume-probe death is represented
/// explicitly as `ResumeFailed` because it preserves durable state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// Session ID was set and resume succeeded; pane is alive.
    Resumed,
    /// Resume was attempted, but the pane died during the probe before AoE
    /// observed an explicit invalid-resume signal. The sid was preserved and
    /// marked so startup recovery does not retry it automatically.
    ResumeFailed { sid: String },
    /// No resume cascade ran. Either no prior sid, the agent doesn't support
    /// resume, the sid was invalid, the session is structured view-mode (no tmux
    /// pane), or the tmux session was already alive when entered (so
    /// `start_with_size_opts` was a no-op and the probe had nothing to
    /// detect). The pane is alive on return; whether a fresh launch
    /// actually occurred this call depends on the caller having killed
    /// any pre-existing pane first.
    Fresh,
}

/// What `start_with_size_opts` did with the agent's session id this call.
/// `start_with_resume_fallback` matches on `Existing` to gate the Tier-1
/// settle probe; without the gate, fresh Claude launches mislabel as
/// `StartOutcome::Resumed` because `acquire_session_id` always assigns a
/// UUID for Claude. `Fresh` carries its own probe gate for the launches that
/// pin an already-stored id (see `pinned_prior_sid`).
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchSidOutcome {
    /// `acquire_session_id` reused a prior sid: `ResumeIntent::Use(sid)`,
    /// observed `agent_session_id`, or retroactive-capture hit. The launch
    /// command embedded the agent's resume flag.
    Existing { sid: String },

    Fresh {
        /// Set when the fresh launch pinned an id the session already had
        /// stored, rather than a UUID minted for a brand-new conversation:
        /// the #2700 empty-thread downgrade (`--session-id <sid>`) and a fork
        /// (whose child id is pre-generated at creation). Both can die on the
        /// spot, for a live id or an unresolvable parent, so both are worth
        /// probing; a genuinely new session cannot and skips the probe.
        /// See #3399.
        pinned_prior_sid: Option<String>,
    },
    /// `start_with_size_opts` short-circuited before `apply_session_flags`
    /// ran: structured view-mode session, or a pre-existing tmux pane that is
    /// still alive (kill_clean cache race). `agent_session_id` was not mutated
    /// this call. A pre-existing *dead* pane is not skipped; it is torn down
    /// and relaunched (#3399).
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
        self.start_with_size_opts_in(size, skip_on_launch, &storage, false, None)
    }

    fn start_with_size_opts_in(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        storage: &dyn crate::session::SessionStore,
        require_tracked_row: bool,
        mut handoff: Option<(TerminalHandoffKey, bool)>,
    ) -> Result<LaunchSidOutcome> {
        storage.check_available()?;

        let title_lock = crate::session::storage::acquire_session_title_lock(&self.id)
            .context("failed to acquire instance launch title lock")?;
        let lifecycle_lock = storage
            .storage()
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance launch lock")?;
        if require_tracked_row {
            self.reconcile_from_store(storage)?;
        }
        if handoff.is_none() && self.terminal_handoff.is_some() {
            return Err(LifecycleReservationError::Busy(LifecycleOperation::Launch).into());
        }
        if let Some((key, _)) = handoff {
            self.validate_terminal_handoff(storage, key)?;
        }
        if self.is_structured() {
            return Ok(LaunchSidOutcome::Skipped);
        }
        // Archived and trashed rows never launch, from any entry point. The
        // durable recheck after the pre-launch hooks (which run without this
        // lock) lives in `reacquire_launch_locks_after_hooks`.
        self.ensure_startable()?;
        // A remain-on-exit corpse still owns its physical name and must be recreated.
        let session = self.tmux_session()?;
        let corpse_pane = if session.exists_live() {
            if !session.is_pane_dead() {
                if let Some((key, _)) = handoff {
                    self.complete_terminal_handoff_live_in(storage, key)?;
                }
                return Ok(LaunchSidOutcome::Skipped);
            }
            if let Some((key, _)) = handoff {
                if let Some(launch) = self
                    .validate_terminal_handoff(storage, key)?
                    .launch
                    .as_ref()
                {
                    Self::ensure_terminal_handoff_pane_matches(session.name(), key, launch)?;
                }
            }
            true
        } else {
            false
        };
        if !require_tracked_row {
            // A default-store no-op on a live pane needs no durable launch row.
            // A real launch still requires and rechecks its authoritative row.
            self.reconcile_from_store(storage)?;
            if self.is_structured() {
                return Ok(LaunchSidOutcome::Skipped);
            }
            self.ensure_startable()?;
        }
        if let Some((key, allow_retry)) = handoff {
            anyhow::ensure!(allow_retry || !self.validate_terminal_handoff(storage, key)?.launch_started,
                "previous terminal launch did not remain live; retry disable explicitly after correcting its error");
            self.prepare_terminal_handoff_retry_in(storage, key)?;
            self.validate_terminal_handoff_target(storage, key)?;
        }
        let generation = if let Some((key, allow_retry)) = handoff {
            let generation = self.acquire_terminal_handoff_launch_reservation(storage, key)?;
            handoff = Some((
                TerminalHandoffKey {
                    id: key.id,
                    generation,
                },
                allow_retry,
            ));
            generation
        } else {
            self.acquire_lifecycle_reservation(
                storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )?
        };

        // Hooks may invoke aoe; retain the token, not the flocks, across them.
        drop(lifecycle_lock);
        drop(title_lock);
        let hook_result = self.run_pre_launch_hooks(skip_on_launch, storage, None);
        let (_title_lock, _lifecycle_lock) =
            self.reacquire_launch_locks_after_hooks(storage, generation, hook_result)?;
        if let Some((key, _)) = handoff {
            self.validate_terminal_handoff_target(storage, key)?;
        }
        self.reconcile_sidecar_into_disk_in(storage)?;
        let expected = self.apply_fresh_launch_intent_in(storage);
        if let Err(error) = self
            .preflight_resume_launch_in(CaptureStorage::Scoped(storage), ResumeAttemptPolicy::Allow)
        {
            self.adopt_conversation_state(expected);
            self.fail_reserved_launch(storage, generation, &error, false);
            return Err(error);
        }

        let mut prepared =
            match self.prepare_launch_command_in(CaptureStorage::Scoped(storage), expected) {
                Ok(prepared) => prepared,
                Err(error) => {
                    self.fail_reserved_launch(storage, generation, &error, false);
                    return Err(error);
                }
            };
        let result = (|| {
            if corpse_pane {
                let publication = self.prime_root_publication(true);
                self.kill_clean_locked()?;
                prepared = self.refresh_prepared_prime_launch_after_pane_stop_in(
                    CaptureStorage::Scoped(storage),
                    prepared,
                    publication,
                )?;
            }
            let outcome = self.spawn_prepared_launch(size, storage, prepared)?;
            self.commit_lifecycle_launch(storage, generation, false)?;
            Ok(outcome)
        })();
        if let Err(error) = result {
            self.fail_reserved_launch(storage, generation, &error, handoff.is_none());
            return Err(error);
        }
        result
    }

    pub(crate) fn start_terminal_handoff_in(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        key: TerminalHandoffKey,
        driver: &crate::session::TerminalHandoffDriver,
        allow_retry: bool,
    ) -> Result<()> {
        crate::session::validate_instance_id(&self.id)?;
        anyhow::ensure!(
            driver.session_id() == self.id.as_str(),
            LifecycleReservationError::Superseded
        );
        self.start_with_size_opts_in(None, false, storage, true, Some((key, allow_retry)))
            .map(|_| ())
    }

    fn validate_terminal_handoff(
        &self,
        storage: &dyn crate::session::SessionStore,
        key: TerminalHandoffKey,
    ) -> Result<&TerminalHandoffIntent> {
        let intent = self
            .terminal_handoff
            .as_ref()
            .ok_or(LifecycleReservationError::Superseded)?;
        anyhow::ensure!(
            self.view == View::Terminal
                && self.lifecycle_generation == key.generation
                && intent.id == key.id
                && intent.generation == key.generation
                && intent.target_generation <= key.generation
                && intent.stage == TerminalHandoffStage::TerminalLaunchOwed
                && intent.profile == storage.storage().profile()
                && intent.profile_identity == storage.storage().bound_profile_identity()?,
            LifecycleReservationError::Superseded
        );
        anyhow::ensure!(
            intent.final_event_seq.is_some(),
            "terminal handoff has no completed original retirement"
        );
        intent.ensure_original_stopped(&self.id)?;
        Ok(intent)
    }

    fn normalized_handoff_launch_state(&self, launch: &TerminalHandoffLaunch) -> ConversationState {
        let mut desired = launch.launched.clone();
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
    }

    fn prepare_terminal_handoff_retry_in(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        key: TerminalHandoffKey,
    ) -> Result<()> {
        let intent = self.validate_terminal_handoff(storage, key)?.clone();
        let Some(launch) = intent.launch else {
            if matches!(intent.context, TerminalHandoffContext::Keep { .. }) {
                anyhow::ensure!(
                    matches!(self.resume_intent, ResumeIntent::Use(_))
                        && !self.force_fresh_next_launch,
                    "kept handoff requires its explicit native resume target"
                );
            }
            return Ok(());
        };
        if launch.generation < intent.target_generation {
            let target = intent
                .terminal
                .as_ref()
                .ok_or(LifecycleReservationError::Superseded)?;
            anyhow::ensure!(target.matches(self), LifecycleReservationError::Superseded);
            return Ok(());
        }
        let normalized = self.normalized_handoff_launch_state(&launch);
        let current_execution = launch.launched.active.is_some()
            && self.runs(&launch.launched.tool, launch.launched.active.as_ref());
        anyhow::ensure!(
            launch.prior.matches(self)
                || launch.launched.matches(self)
                || normalized.matches(self)
                || current_execution,
            LifecycleReservationError::Superseded
        );
        let observed = if current_execution {
            self.agent_session_id
                .as_ref()
                .zip(self.agent_session_binding.as_ref())
                .filter(|(sid, binding)| {
                    binding.session_id.as_str() == sid.as_str()
                        && binding.is_known()
                        && binding
                            .execution
                            .as_ref()
                            .zip(self.active_execution.as_ref())
                            .is_some_and(|(binding, active)| {
                                Self::execution_identity_matches(binding, &active.binding)
                            })
                })
                .map(|(sid, binding)| {
                    let mut state = self.conversation_state();
                    state.intent = ResumeIntent::Use(sid.clone());
                    state.resume_binding = Some(binding.clone());
                    state
                })
        } else {
            None
        };
        let mut target = match (observed, intent.context) {
            (Some(observed), _) => observed,

            (None, TerminalHandoffContext::Keep { .. }) => intent
                .terminal
                .ok_or_else(|| anyhow::anyhow!("kept handoff has no native target"))?,
            (None, TerminalHandoffContext::Discard) => {
                let ambiguous = "terminal launch outcome has no proven exact native target; use aoe session set-session-id SESSION ID --store /absolute/store, or aoe session set-session-id SESSION \"\" to explicitly authorize a fresh terminal launch";
                let sid = launch.launched.session_id.as_ref().context(ambiguous)?;
                let binding = launch
                    .launched
                    .binding
                    .as_ref()
                    .filter(|binding| {
                        binding.session_id.as_str() == sid.as_str()
                            && binding.is_known()
                            && binding
                                .execution
                                .as_ref()
                                .zip(launch.launched.active.as_ref())
                                .is_some_and(|(binding, active)| {
                                    Self::execution_identity_matches(binding, &active.binding)
                                })
                    })
                    .context(ambiguous)?;
                let mut exact = launch.launched.clone();
                exact.intent = ResumeIntent::Use(sid.clone());
                exact.resume_binding = Some(binding.clone());
                exact
            }
        };
        target.active = self.active_execution.clone();
        let profile = storage.storage().profile().to_owned();
        let identity = storage.storage().bound_profile_identity()?;
        storage.update(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == self.id)
                .ok_or(LifecycleReservationError::Superseded)?;
            let pending = row
                .terminal_handoff
                .as_ref()
                .ok_or(LifecycleReservationError::Superseded)?;
            anyhow::ensure!(
                row.lifecycle_generation == key.generation
                    && pending.id == key.id
                    && pending.generation == key.generation
                    && pending.profile == profile
                    && pending.profile_identity == identity
                    && pending.stage == TerminalHandoffStage::TerminalLaunchOwed
                    && (launch.prior.matches(row)
                        || launch.launched.matches(row)
                        || normalized.matches(row)
                        || (current_execution
                            && row.runs(&self.tool, self.active_execution.as_ref())
                            && self.conversation_state().matches(row))),
                LifecycleReservationError::Superseded
            );
            row.adopt_conversation_state(target.clone());
            row.terminal_handoff
                .as_mut()
                .expect("validated handoff")
                .terminal = Some(target.clone());
            Ok(())
        })?;
        self.adopt_conversation_state(target);
        Ok(())
    }

    fn acquire_terminal_handoff_launch_reservation(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        key: TerminalHandoffKey,
    ) -> Result<u64> {
        self.validate_terminal_handoff(storage, key)?;
        let id = self.id.clone();
        let profile = storage.storage().profile().to_owned();
        let identity = storage.storage().bound_profile_identity()?;
        let mut candidate = None;
        let result = storage.update(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == id)
                .ok_or(LifecycleReservationError::Superseded)?;
            let pending = row
                .terminal_handoff
                .as_ref()
                .ok_or(LifecycleReservationError::Superseded)?;
            anyhow::ensure!(
                row.lifecycle_generation == key.generation
                    && pending.id == key.id
                    && pending.generation == key.generation
                    && pending.profile == profile
                    && pending.profile_identity == identity
                    && pending.stage == TerminalHandoffStage::TerminalLaunchOwed,
                LifecycleReservationError::Superseded
            );
            row.ensure_startable()?;
            let generation = row
                .lifecycle_generation
                .checked_add(1)
                .ok_or(LifecycleReservationError::GenerationOverflow)?;
            row.lifecycle_generation = generation;
            row.lifecycle_reservation = Some(LifecycleReservation {
                op: LifecycleOperation::Launch,
                generation,
                at: Utc::now(),
            });
            row.status = Status::Starting;
            row.idle_entered_at = None;
            let pending = row.terminal_handoff.as_mut().expect("validated handoff");
            pending.generation = generation;
            pending.launch_started = true;
            candidate = Some(generation);
            Ok(())
        });
        let landed = match &result {
            Ok(()) => true,
            Err(error) if error.is::<crate::session::SessionCommitApplied>() => true,
            Err(_) => {
                crate::migrations::ensure_schema_supported().is_ok()
                    && candidate.is_some_and(|generation| {
                        storage
                            .storage()
                            .load_complete_with_groups()
                            .is_ok_and(|(rows, _)| {
                                rows.iter().any(|row| {
                                    row.id == id
                                        && row.lifecycle_reservation_is_owned(
                                            LifecycleOperation::Launch,
                                            generation,
                                        )
                                        && row.terminal_handoff.as_ref().is_some_and(|pending| {
                                            pending.id == key.id
                                                && pending.generation == generation
                                                && pending.profile == profile
                                                && pending.profile_identity == identity
                                        })
                                })
                            })
                    })
            }
        };
        if landed {
            self.lifecycle_generation =
                candidate.ok_or_else(|| anyhow::anyhow!("launch mutation not invoked"))?;
        }
        result?;
        self.reconcile_from_store(storage)?;
        Ok(self.lifecycle_generation)
    }

    pub(crate) fn ensure_terminal_handoff_pane_matches(
        session_name: &str,
        key: TerminalHandoffKey,
        launch: &TerminalHandoffLaunch,
    ) -> Result<()> {
        anyhow::ensure!(
            launch.generation <= key.generation,
            LifecycleReservationError::Superseded
        );
        let id = key.id.to_string();
        let generation = launch.generation.to_string();
        anyhow::ensure!(
            crate::tmux::env::get_env(session_name, crate::tmux::env::AOE_TERMINAL_HANDOFF_ID_KEY)
                .as_deref()
                == Some(id.as_str())
                && crate::tmux::env::get_env(
                    session_name,
                    crate::tmux::env::AOE_TERMINAL_HANDOFF_GENERATION_KEY
                )
                .as_deref()
                    == Some(generation.as_str()),
            "pane does not match the original terminal handoff attempt; replacement retained"
        );
        if let Some(metadata) = launch.omp_capture_metadata.as_ref() {
            anyhow::ensure!(
                crate::tmux::env::get_env(session_name, crate::tmux::env::AOE_OMP_LAUNCH_ID_KEY)
                    .as_deref()
                    == Some(metadata.launch_id.as_str()),
                "pane has different OMP launch identity"
            );
        }
        Ok(())
    }

    fn complete_terminal_handoff_live_in(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        key: TerminalHandoffKey,
    ) -> Result<()> {
        let intent = self.validate_terminal_handoff(storage, key)?;
        let Some(launch) = intent.launch.clone() else {
            return Ok(());
        };
        anyhow::ensure!(launch.generation >= intent.target_generation,
        "previous handoff pane is live but does not apply the explicitly selected target; pane retained");
        let session = self.tmux_session()?;
        Self::ensure_terminal_handoff_pane_matches(session.name(), key, &launch)?;
        let normalized = self.normalized_handoff_launch_state(&launch);
        if launch.prior.matches(self) || launch.launched.matches(self) {
            self.adopt_conversation_state(launch.launched.clone());
            self.capture_started_at = launch.capture_started_at;
            self.finalize_launch_with_store(
                session.name(),
                storage,
                &launch.prior,
                launch.omp_capture_metadata,
                launch.confirm_desired_conversation,
            )?;
        } else {
            anyhow::ensure!(
                normalized.matches(self)
                    || (launch.launched.active.is_some()
                        && self.runs(&launch.launched.tool, launch.launched.active.as_ref())),
                LifecycleReservationError::Superseded
            );
            self.capture_started_at = launch.capture_started_at;
            self.maybe_start_poller_since_in(CaptureStorage::Scoped(storage))?;
        }
        if self.lifecycle_reservation_is_owned(LifecycleOperation::Launch, key.generation) {
            self.commit_lifecycle_launch(storage, key.generation, false)?;
        }
        Ok(())
    }

    fn validate_terminal_handoff_target(
        &self,
        storage: &dyn crate::session::SessionStore,
        key: TerminalHandoffKey,
    ) -> Result<()> {
        let intent = self.validate_terminal_handoff(storage, key)?;
        let target = intent
            .terminal
            .as_ref()
            .ok_or(LifecycleReservationError::Superseded)?;
        anyhow::ensure!(
            target.tool == self.tool,
            LifecycleReservationError::Superseded
        );
        if matches!(intent.context, TerminalHandoffContext::Keep { .. })
            || matches!(&target.intent, ResumeIntent::Use(_))
        {
            anyhow::ensure!(
                matches!(&self.resume_intent, ResumeIntent::Use(id)
                if matches!(&target.intent, ResumeIntent::Use(target_id) if target_id == id))
                    && self.resume_binding == target.resume_binding
                    && !self.force_fresh_next_launch,
                "exact terminal handoff target changed; set an explicit native target to recover"
            );
        }
        Ok(())
    }
    pub(super) fn apply_fresh_launch_intent_in(
        &mut self,
        _storage: &dyn crate::session::SessionStore,
    ) -> ConversationState {
        let expected = self.conversation_state();
        if std::mem::take(&mut self.force_fresh_next_launch) {
            self.resume_intent = ResumeIntent::Cleared;
        }
        expected
    }

    #[cfg(test)]
    pub(super) fn apply_fresh_launch_intent(&mut self) -> ConversationState {
        let storage =
            crate::session::storage::Storage::new_unwatched(&self.effective_profile()).unwrap();
        self.apply_fresh_launch_intent_in(&storage)
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
        storage: &dyn crate::session::SessionStore,
        mut prepared: PreparedLaunch,
    ) -> Result<LaunchSidOutcome> {
        let profile = storage.storage().profile();
        let handoff_key = self
            .terminal_handoff
            .as_ref()
            .map(|intent| TerminalHandoffKey {
                id: intent.id,
                generation: self.lifecycle_generation,
            });
        let session = self.tmux_session()?;
        if session.exists_live() {
            anyhow::bail!(
                "session {} gained a tmux pane before its reserved launch",
                self.id
            );
        }

        // A takeover can replace the pane without changing the agent or the execution, and a poller
        // for either would then be watching the pane this launch is replacing.
        self.stop_poller();
        self.session_id_poller = None;
        if !self.is_sandboxed() {
            self.install_agent_status_hooks(self.status_agent());
        }
        self.report_store_override(prepared.execution.as_ref());
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

        let mut omp_capture_metadata = if let Some(plan) = prepared.omp_capture_plan.take() {
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
        let omp_generation_published = self.publish_omp_launch_generation_with_store(
            storage,
            omp_capture_metadata.as_ref(),
            prepared.expected_prior_omp_generation.as_deref(),
        )?;
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
        let handoff_launch = if let Some(key) = handoff_key {
            self.validate_terminal_handoff(storage, key)?;
            anyhow::ensure!(
                omp_capture_metadata.is_none() || omp_generation_published,
                "handoff OMP launch generation was not durably published"
            );
            self.apply_prepared_launch_state(&mut prepared, omp_capture_metadata.as_ref());
            prepared.launch_env.pane.push(tmux::PaneEnvMutation::set(
                crate::tmux::env::AOE_TERMINAL_HANDOFF_ID_KEY.to_owned(),
                key.id.to_string(),
            ));
            prepared.launch_env.pane.push(tmux::PaneEnvMutation::set(
                crate::tmux::env::AOE_TERMINAL_HANDOFF_GENERATION_KEY.to_owned(),
                key.generation.to_string(),
            ));
            Some(TerminalHandoffLaunch {
                generation: key.generation,
                prior: prepared.expected_conversation.clone(),
                launched: self.conversation_state(),
                capture_started_at: self.capture_started_at,
                omp_capture_metadata: omp_capture_metadata.clone(),
                confirm_desired_conversation: canonicalized
                    || prepared.require_conversation_publication,
            })
        } else {
            None
        };
        let scope = crate::process::systemd::scope_prefix_for(&self.id, profile);
        let recorded_scopes = self.terminal_scopes.clone();
        let resolved_scopes = match recorded_scopes.as_deref() {
            Some(scopes)
                if !scopes
                    .contains(&crate::process::worker_registry::ScopeOwnership::Unresolved) =>
            {
                None
            }
            recorded => crate::process::systemd::resolve_unknown_scopes(&self.id, recorded).ok(),
        };
        let generation = self.lifecycle_generation;
        self.terminal_scopes = Some(storage.update(|rows, _| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == self.id)
                .ok_or(LifecycleReservationError::Superseded)?;
            anyhow::ensure!(
                row.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation),
                LifecycleReservationError::Superseded
            );
            if let Some(key) = handoff_key {
                row.validate_terminal_handoff(storage, key)?;
                row.terminal_handoff
                    .as_mut()
                    .expect("validated handoff")
                    .launch = handoff_launch;
            }
            if let Some(resolved) = resolved_scopes.as_ref() {
                if row.terminal_scopes == recorded_scopes {
                    row.terminal_scopes = Some(resolved.clone());
                }
            }
            let scopes = row.terminal_scopes.get_or_insert_with(|| {
                vec![crate::process::worker_registry::ScopeOwnership::Unresolved]
            });
            if let Some(scope) = &scope {
                let owned = scope.ownership();
                if !scopes.contains(&owned) {
                    scopes.push(owned);
                }
            }
            Ok(scopes.clone())
        })?);
        session.create_with_size_env_and_container_env(
            &self.project_path,
            prepared.command.as_deref(),
            size,
            profile,
            &prepared.launch_env.pane,
            &prepared.launch_env.container,
            scope.as_ref(),
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

        if handoff_key.is_none() {
            self.apply_prepared_launch_state(&mut prepared, omp_capture_metadata.as_ref());
        }
        #[cfg(test)]
        test_support::observe(self, test_support::FinalizePhase::Before);

        self.finalize_launch_with_store(
            session.name(),
            storage,
            &prepared.expected_conversation,
            omp_capture_metadata,
            canonicalized || prepared.require_conversation_publication,
        )?;

        #[cfg(test)]
        test_support::observe(self, test_support::FinalizePhase::After);
        Ok(match launch_sid {
            Some(sid) => LaunchSidOutcome::Existing { sid },
            None => LaunchSidOutcome::Fresh { pinned_prior_sid },
        })
    }

    fn apply_prepared_launch_state(
        &mut self,
        prepared: &mut PreparedLaunch,
        omp_capture_metadata: Option<&OmpCaptureMetadata>,
    ) {
        if let Some(canonical) = prepared.canonical_conversation.take() {
            self.adopt_conversation_state(canonical);
        }
        // Before the `take` below consumes the execution.
        self.launch_executable = prepared
            .execution
            .as_ref()
            .filter(|execution| execution.inputs.container.is_none())
            .map(|execution| execution.program.clone());
        if let Some(execution) = prepared.execution.take() {
            let attested = self.attested_claude_store_route(&execution);
            self.active_execution = Some(ActiveExecution {
                launch_id: execution.inputs.launch_id,
                binding: execution.binding.clone(),
                capture: execution
                    .capture
                    .or_else(|| omp_capture_metadata.cloned().map(CaptureContext::Omp)),
                container: execution.inputs.container,
            });

            if prepared.fork_mints_child {
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
                let mut binding =
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
                if matches!(prepared.expected_conversation.intent, ResumeIntent::Default)
                    && super::execution::transcript_confirms_reserved_claude_conversation(&binding)
                {
                    binding.provenance = ConversationProvenance::Observed;
                }
                // #4127: a legacy binding adopts the route this launch
                // attested, so the marker stops being derived from the
                // configuration as it stands the moment a launch settles it.
                // Both the binding being adopted and the ones still held are
                // stamped, and the save below carries whichever persists.
                super::execution::attest_observed_default_store(&mut binding, attested.as_ref());
                self.attest_launch_default_store(attested.as_ref());
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
    }
    /// Name both stores when a recorded one outranks what a new session would
    /// use. Reported at the launch, not in the resolver, because a restart
    /// resolves twice and only the launch is one event.
    pub(super) fn report_store_override(
        &self,
        execution: Option<&super::execution::NativeExecution>,
    ) {
        if let Some((launch, new_session, source)) =
            execution.and_then(|execution| execution.store_override.as_ref())
        {
            tracing::warn!(target: "session.store",
                session = %self.id,
                launch_store = %launch.display(),
                new_session_store = %new_session.display(),
                new_session_store_source = %source,
                "the recorded Claude store overrides the store a new session would use here, so this launch runs on the account it recorded"
            );
        }
    }

    /// Post-launch setup: persist state, start pollers, and apply tmux options.
    pub(super) fn finalize_launch_with_store(
        &mut self,
        session_name: &str,
        storage: &dyn crate::session::SessionStore,
        expected: &ConversationState,
        omp_capture_metadata: Option<OmpCaptureMetadata>,
        confirm_desired_conversation: bool,
    ) -> Result<()> {
        let profile = storage.storage().profile();
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
            let _ = published;
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
        let outcome = self.persist_session_id_with_storage(storage, expected);
        if desired.is_some_and(|desired| {
            !matches!(outcome, SidPersistOutcome::Published) || !desired.matches(self)
        }) {
            self.reconcile_from_store(storage)?;
            anyhow::bail!("durable publication was not confirmed; durable reconciliation was attempted but may be unavailable, and the pane may remain if reservation verification or teardown fails");
        }

        // Skip outcomes leave AOE_CAPTURED_SESSION_ID untouched: this path
        // runs before any poller publish, so env is empty for fresh sessions.
        let publish_sid = matches!(outcome, SidPersistOutcome::Published);
        let previous_captured = if publish_sid {
            None
        } else {
            crate::tmux::env::get_hidden_env(
                session_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
            )
        };
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
        if let Some(previous) = previous_captured {
            let _ = crate::tmux::env::set_hidden_env(
                session_name,
                crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY,
                &previous,
            );
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

        // A launch re-evaluates the row, so its schedule goes. Clearing the working copy is not
        // enough: the live row keeps its own until the relaunch merge sees the stamp below.
        self.poller_repair.reset();
        self.maybe_start_poller_since_in(CaptureStorage::Scoped(storage))?;

        self.status = Status::Starting;
        self.last_start_time = Some(std::time::Instant::now());
        self.started_at = Some(Utc::now());

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
    /// The store route this launch actually applied, as a binding the stamp
    /// helper accepts (#4127).
    fn attested_claude_store_route(
        &self,
        execution: &super::execution::NativeExecution,
    ) -> Option<ExecutionBinding> {
        // A container routes to its own store; the host's Claude namespace says
        // nothing about it.
        if execution.inputs.container.is_some() {
            return None;
        }
        let home = super::hooks::host_home(&self.resolved_host_environment())?;
        let attested = attested_default_store(&execution.binding, &execution.routing, &home)?;
        Some(ExecutionBinding {
            exported_default_store: Some(attested),
            ..execution.binding.clone()
        })
    }

    #[cfg(test)]
    pub(super) fn finalize_launch(
        &mut self,
        session_name: &str,
        profile: &str,
        expected: &ConversationState,
        omp_capture_metadata: Option<OmpCaptureMetadata>,
        confirm_desired_conversation: bool,
    ) -> Result<()> {
        let storage = crate::session::storage::Storage::new_unwatched(profile)?;
        self.finalize_launch_with_store(
            session_name,
            &storage,
            expected,
            omp_capture_metadata,
            confirm_desired_conversation,
        )
    }
}
/// The store route a terminal launch attested, read off the routing table the
/// worker is about to be launched with (#4127).
///
/// That table — not the configuration as it stands — is what says whether the
/// default store was exported explicitly, so it is the only thing consulted
/// here: a launch that routed `CLAUDE_CONFIG_DIR` at Claude's default store
/// attested `true`, one that left it implicit attested `false`, and a launch
/// that attested nothing returns `None` so the binding keeps deriving its
/// route at read time until one does.
fn attested_default_store(
    binding: &ExecutionBinding,
    routing: &[(String, Option<String>)],
    home: &std::path::Path,
) -> Option<bool> {
    if binding.agent != "claude" || binding.filesystem != "host" || binding.stores.len() != 1 {
        return None;
    }
    // A launch that recorded no route attested nothing.
    binding.exported_default_store?;
    let store = binding.stores.first()?;
    let routed = routing
        .iter()
        .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
        .and_then(|(_, value)| value.as_deref())
        .and_then(|value| {
            crate::session::capture::canonicalize_allowing_missing_leaf(std::path::Path::new(value))
        })
        .zip(crate::session::capture::canonicalize_allowing_missing_leaf(
            store,
        ));
    Some(
        routed.is_some_and(|(routed, store)| routed == store)
            && crate::session::capture::is_default_claude_store(store, home),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn frozen_test_helper(
        root: &std::path::Path,
        entry: &str,
    ) -> crate::process::test_support::NativeLaunchExecutable {
        use std::os::unix::fs::PermissionsExt;
        let helper = root.join("native-helper");
        let module = module_path!().split_once("::").unwrap().1;
        std::fs::write(&helper, format!("#!/bin/sh\nAOE_FROZEN_NATIVE_TEST_CHILD=1 exec {} --exact {} --nocapture --test-threads=1\n", crate::session::environment::shell_escape(std::env::current_exe().unwrap().to_str().unwrap()), crate::session::environment::shell_escape(&format!("{module}::{entry}")))).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        crate::process::test_support::NativeLaunchExecutable::install(helper)
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn direct_start_refuses_failed_resume_before_replacing_a_dead_pane() {
        if std::env::var_os("AOE_FROZEN_NATIVE_TEST_CHILD").is_some() {
            crate::process::exec_frozen_environment().unwrap();
            return;
        }
        use crate::session::instance::test_helpers::{seed_claude_transcript, tool_instance};
        use crate::tmux::test_helpers::{
            only_pane_id, pane_field, tmux_available, wait_for_pane_command, TmuxTestSession,
        };
        if !tmux_available() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(root.path());
        let _env = crate::session::test_support::EnvGuard::unset(&[
            "TMUX",
            "TMUX_PANE",
            "CLAUDE_CONFIG_DIR",
            "AOE_DAEMON_URL",
        ])
        .and_set("AOE_TMUX_SOCKET", root.path().join("tmux.sock"));
        let _agent = crate::session::test_support::install_login_shell_path_command(
            root.path(),
            "claude",
            "#!/bin/sh\nexec /bin/sleep 60\n",
        );
        crate::session::config::update_config(|config| {
            config.systemd.enabled = false;
            config.session.smart_rename = false;
            config.session.auto_resume_on_restart = true;
        })
        .unwrap();
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let mut instance = tool_instance("claude", project.to_str().unwrap());
        instance.source_profile = "direct-failed-resume".into();
        let sid = "11111111-2222-4333-8444-555555555555";
        seed_claude_transcript(&mut instance, sid);
        instance.resume_probe_failed_sid = Some(sid.into());
        instance.omp_capture_generation = Some("outgoing-generation".into());
        assert!(instance.supports_native_resume());
        let before = instance.conversation_state();
        let store = crate::session::Storage::new_unwatched(&instance.source_profile).unwrap();
        store
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let name = instance.tmux_session().unwrap().name().to_owned();
        let _pane = TmuxTestSession::from_name(&name);
        assert!(crate::tmux::tmux_command()
            .args([
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                &name,
                "exec /bin/sleep 60",
            ])
            .output()
            .unwrap()
            .status
            .success());
        let pane = only_pane_id(&name);
        wait_for_pane_command(&pane, "sleep");
        assert!(crate::tmux::tmux_command()
            .args(["set-option", "-t", &name, "remain-on-exit", "on",])
            .output()
            .unwrap()
            .status
            .success());
        assert!(crate::tmux::tmux_command()
            .args(["respawn-pane", "-k", "-t", &pane, "exit 17",])
            .output()
            .unwrap()
            .status
            .success());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while pane_field(&pane, "#{pane_dead}") != "1" {
            assert!(
                std::time::Instant::now() < deadline,
                "fixture pane did not exit"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let pid = pane_field(&pane, "#{pane_pid}");
        let _helper = frozen_test_helper(
            root.path(),
            "direct_start_refuses_failed_resume_before_replacing_a_dead_pane",
        );
        let error = instance
            .start_with_size_opts(None, true)
            .expect_err("a direct Default start must not retry a previously failed conversation");
        assert!(error.is::<ManagedResumeRefused>(), "{error:#}");
        let public_error = instance.last_error.as_deref().unwrap();
        assert!(!public_error.contains(sid), "{public_error}");
        assert!(
            public_error.contains("aoe session set-session-id <session> \"\""),
            "{public_error}"
        );
        assert_eq!(only_pane_id(&name), pane);
        assert_eq!(pane_field(&pane, "#{pane_pid}"), pid);
        assert_eq!(pane_field(&pane, "#{pane_dead}"), "1");
        let stored = store.load().unwrap().remove(0);
        assert_eq!(stored.conversation_state(), before);
        assert_eq!(stored.resume_probe_failed_sid.as_deref(), Some(sid));
        assert_eq!(
            stored.omp_capture_generation.as_deref(),
            Some("outgoing-generation")
        );
        assert!(stored.lifecycle_reservation.is_none());
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn store_fork_requires_durable_publication_and_preserves_recovery_and_peer_pins() {
        if std::env::var_os("AOE_FROZEN_NATIVE_TEST_CHILD").is_some() {
            crate::process::exec_frozen_environment().unwrap();
            return;
        }
        let Ok(python) = which::which("python3") else {
            return;
        };
        if which::which("tmux").is_err() {
            return;
        }
        use crate::session::instance::test_helpers::{seed_disk_for_sidecar_test, tool_instance};
        const PARENT: &str = "ses_parent00000000000000000000000";
        const CHILD: &str = "ses_child000000000000000000000000";
        const SEED: &str = "ses_seed0000000000000000000000000";
        for (fail_adoption, fail_final, peer_pin) in [
            (true, false, false),
            (false, true, false),
            (true, true, false),
            (false, false, true),
        ] {
            let logs = crate::session::test_support::LogCapture::start();
            let directory = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_home(directory.path());
            let project = directory.path().join("project");
            std::fs::create_dir(&project).unwrap();
            let project = project.canonicalize().unwrap();
            let database = directory.path().join("opencode.db");
            let connection = rusqlite::Connection::open(&database).unwrap();
            connection.execute_batch("CREATE TABLE session_v2(id TEXT PRIMARY KEY, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT, fork_session_id TEXT, fork_boundary TEXT, slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT, version TEXT NOT NULL, share_url TEXT, metadata TEXT, cost REAL, agent TEXT, model TEXT, time_created INTEGER, time_updated INTEGER)").unwrap();
            connection.execute("INSERT INTO session_v2(id,project_id,directory,slug,version,time_created,time_updated) VALUES (?1,'project',?2,'parent','2.0.24',0,0)", rusqlite::params![PARENT, project.to_str().unwrap()]).unwrap();
            let record = directory.path().join("launch.json");
            let forks = directory.path().join("forks");
            let native = format!(
                r#"#!{}
import json, os, socket, sqlite3, sys
if sys.argv[1:] == ['--help']: print('--auto --session'); sys.exit(0)
if sys.argv[1:] == ['--version']: print('2.0.24+31bdd8e'); sys.exit(0)
if sys.argv[1] == 'serve':
    server = socket.socket()
    server.bind(('127.0.0.1', int(sys.argv[sys.argv.index('--port') + 1])))
    server.listen()
    server.settimeout(15)
    while True:
        stream, _ = server.accept()
        with stream:
            stream.settimeout(2)
            request = b''
            while b'\r\n\r\n' not in request:
                part = stream.recv(8192)
                if not part: raise RuntimeError("incomplete request headers")
                request += part
            headers, payload = request.split(b'\r\n\r\n', 1)
            length = next((int(line.split(b':', 1)[1]) for line in headers.split(b'\r\n') if line.lower().startswith(b'content-length:')), 0)
            while len(payload) < length:
                part = stream.recv(length - len(payload))
                if not part: raise RuntimeError("incomplete request body")
                payload += part
            if request.startswith(b'POST '):
                with sqlite3.connect(os.environ['OPENCODE_DB']) as db:
                    db.execute("INSERT INTO session_v2(id,project_id,directory,slug,version,time_created,time_updated) SELECT 'ses_child000000000000000000000000',project_id,directory,'child',version,0,0 FROM session_v2 WHERE id='ses_parent00000000000000000000000'")
                with open(os.environ['FORK_COUNT'], 'a') as log: log.write('fork\n')
            body = b'{{"data":{{"id":"ses_child000000000000000000000000"}}}}'
            stream.sendall(b'HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: ' + str(len(body)).encode() + b'\r\n\r\n' + body)
else:
    with open(os.environ['FORK_RECORD'] + '.tmp', 'w') as log: json.dump(sys.argv[1:], log)
    os.replace(os.environ['FORK_RECORD'] + '.tmp', os.environ['FORK_RECORD'])
    sys.stdin.read()
"#,
                python.display()
            );
            let _path = crate::session::test_support::install_login_shell_path_command(
                directory.path(),
                "opencode",
                &native,
            );
            let profile = "fork-publication";
            let config =
                crate::session::config::profile_config::get_profile_config_path(profile).unwrap();
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            let environment = vec![
                format!("OPENCODE_DB={}", database.display()),
                format!("FORK_RECORD={}", record.display()),
                format!("FORK_COUNT={}", forks.display()),
            ];
            std::fs::write(config, format!("environment={}\n[session]\nagent_status_hooks=false\nsmart_rename=false\nname_agent_session=false\nopencode_preassign_session_id=false\n", serde_json::to_string(&environment).unwrap())).unwrap();
            let mut instance = tool_instance("opencode", project.to_str().unwrap());
            instance.source_profile = profile.into();
            let parent = instance.asserted_resume_binding(PARENT, None).unwrap();
            instance.set_agent_conversation(Some(SEED.into()), None, None);
            instance.resume_intent = ResumeIntent::Fork {
                from: PARENT.into(),
            };
            instance.resume_binding = Some(parent.clone());
            let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
                instance.tmux_session().unwrap().name(),
            );
            seed_disk_for_sidecar_test(profile, &instance);
            let _helper = frozen_test_helper(
                directory.path(),
                "store_fork_requires_durable_publication_and_preserves_recovery_and_peer_pins",
            );
            let reached = std::rc::Rc::new(std::cell::Cell::new(false));
            let observed = reached.clone();
            let launch_record = record.clone();
            let observer = test_support::FinalizeObserver::install(
                instance.id.clone(),
                move |instance, phase| {
                    if !matches!(phase, test_support::FinalizePhase::Before) {
                        return;
                    }
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
                    let argv: Vec<String> = loop {
                        if let Ok(bytes) = std::fs::read(&launch_record) {
                            if let Ok(argv) = serde_json::from_slice(&bytes) {
                                break argv;
                            }
                        }
                        assert!(
                            std::time::Instant::now() < deadline,
                            "native child did not launch"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    };
                    assert!(argv.windows(2).any(|pair| pair == ["--session", CHILD]));
                    if fail_adoption {
                        assert_eq!(instance.agent_session_id.as_deref(), Some(CHILD));
                        assert!(matches!(instance.resume_intent, ResumeIntent::Fork { .. }));
                        assert_eq!(
                            instance
                                .resume_binding
                                .as_ref()
                                .map(|binding| binding.session_id.as_str()),
                            Some(PARENT)
                        );
                    }
                    observed.set(true);
                    if peer_pin {
                        crate::session::storage::Storage::new_unwatched(profile)
                            .unwrap()
                            .update(|rows, _| {
                                let row =
                                    rows.iter_mut().find(|row| row.id == instance.id).unwrap();
                                row.set_agent_conversation(
                                    Some(PARENT.into()),
                                    Some(parent.clone()),
                                    None,
                                );
                                row.resume_intent = ResumeIntent::Use(PARENT.into());
                                row.resume_binding = Some(parent.clone());
                                Ok(())
                            })
                            .unwrap();
                    }
                },
            );
            let faults = super::super::sid_persist::test_support::WriteFailures::install(
                instance.id.clone(),
                fail_adoption,
                fail_final,
            );
            let result = if fail_adoption {
                instance.restart_with_size_opts(None, false).map(|_| ())
            } else {
                instance.start_with_size_opts(None, false).map(|_| ())
            };
            assert!(
                reached.get(),
                "case ({fail_adoption}, {fail_final}, {peer_pin}): {result:?}\n{}",
                logs.contents()
            );
            assert_eq!(result.is_ok(), !fail_final && !peer_pin, "{result:?}");
            let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
            let disk = storage.load().unwrap().remove(0);
            if fail_final || peer_pin {
                assert!(!instance.tmux_session().unwrap().exists());
                assert_eq!(disk.status, Status::Error);
                if peer_pin {
                    assert_eq!(disk.agent_session_id.as_deref(), Some(PARENT));
                    assert_eq!(disk.resume_intent, ResumeIntent::Use(PARENT.into()));
                } else if fail_adoption {
                    assert_eq!(disk.agent_session_id.as_deref(), Some(SEED));
                    assert_eq!(
                        disk.resume_intent,
                        ResumeIntent::Fork {
                            from: PARENT.into()
                        }
                    );
                } else {
                    assert_eq!(disk.agent_session_id.as_deref(), Some(CHILD));
                    assert_eq!(disk.resume_intent, ResumeIntent::Default);
                    drop(faults);
                    drop(observer);
                    std::fs::remove_file(&record).unwrap();
                    let resumed = instance.start_with_size_opts(None, false).unwrap();
                    assert!(matches!(resumed, LaunchSidOutcome::Existing { sid } if sid == CHILD));
                    assert_eq!(
                        storage.load().unwrap()[0].agent_session_id.as_deref(),
                        Some(CHILD)
                    );
                }
            } else {
                assert_eq!(disk.agent_session_id.as_deref(), Some(CHILD));
                assert_eq!(disk.resume_intent, ResumeIntent::Default);
                assert_eq!(disk.active_execution, instance.active_execution);
            }
            assert_eq!(std::fs::read_to_string(forks).unwrap(), "fork\n");
        }
    }
    #[test]
    fn start_with_size_opts_returns_skipped_for_structured() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.view = View::Structured;
        let outcome = inst.start_with_size_opts(None, false).unwrap();
        assert_eq!(outcome, LaunchSidOutcome::Skipped);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_start_requires_a_tracked_row_even_for_a_preexisting_live_pane(
    ) -> anyhow::Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir()?;
        let mut row = Instance::new(
            "native tracked-row admission",
            project.path().to_str().unwrap(),
        );
        row.source_profile = "default".into();
        let storage = crate::session::Storage::new_unwatched(&row.source_profile)?;
        assert!(storage.load()?.is_empty());
        let name = crate::tmux::Session::generate_name(&row.id, &row.title);
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(name.clone());
        row.tmux_session()?.create(
            project.path().to_str().unwrap(),
            Some("exec cat"),
            "default",
        )?;
        let before = crate::tmux::test_helpers::pane_field(&name, "#{pane_pid}");
        assert!(!before.is_empty());
        let state = crate::server::test_support::build_test_app_state(vec![row.clone()]);
        crate::server::test_support::refresh_canonical_metadata_for_test(&state).await;
        let result = tokio::task::spawn_blocking(move || {
            let native = crate::server::session_store::NativeSessionStore::open(
                state,
                &row.source_profile,
                Some(row.id.clone()),
            )?;
            row.start_with_size_opts_in(None, false, &native, true, None)
                .map(|_| ())
        })
        .await?;
        let error = result.unwrap_err();
        assert!(
            matches!(
                error.downcast_ref::<LifecycleReservationError>(),
                Some(LifecycleReservationError::Superseded)
            ),
            "{error:#}"
        );
        assert_eq!(
            crate::tmux::test_helpers::pane_field(&name, "#{pane_pid}"),
            before,
            "native admission refusal does not respawn the live pane"
        );
        assert!(storage.load()?.is_empty());
        Ok(())
    }

    /// #4116: every launch funnel rereads the row and refuses an archived or
    /// trashed session, even when the caller holds a stale live copy.
    #[test]
    #[serial_test::serial]
    fn launch_funnels_refuse_archived_and_trashed_sessions() {
        type Launch = fn(&mut Instance) -> Option<StartBlocked>;
        let launches: &[(&str, Launch)] = &[
            ("start", |i| i.start().err()?.downcast_ref().copied()),
            ("restart", |i| {
                i.restart_with_resume_policy(None, false, ResumeAttemptPolicy::Allow)
                    .err()?
                    .downcast_ref()
                    .copied()
            }),
            ("ensure_pane_ready", |i| match i.ensure_pane_ready() {
                Err(EnsureReadyError::Blocked(blocked)) => Some(blocked),
                _ => None,
            }),
        ];
        let shelves: &[(fn(&mut Instance), StartBlocked, &str)] = &[
            (
                Instance::archive,
                StartBlocked::Archived,
                "session is archived; unarchive it first",
            ),
            (
                Instance::trash,
                StartBlocked::Trashed,
                "session is in trash; restore it first",
            ),
        ];
        for (shelve, want, message) in shelves {
            for (label, launch) in launches {
                let temp = tempfile::tempdir().unwrap();
                let _home = crate::session::test_support::isolate_home(temp.path());
                let profile = "start-blocked";
                let mut inst = Instance::new(label, "/tmp/x");
                inst.source_profile = profile.to_string();
                crate::session::instance::test_helpers::seed_disk_for_sidecar_test(profile, &inst);
                crate::session::storage::Storage::new_unwatched(profile)
                    .unwrap()
                    .update(|rows, _| {
                        shelve(&mut rows[0]);
                        Ok(())
                    })
                    .unwrap();

                assert_eq!(launch(&mut inst), Some(*want), "{label}");
                assert_eq!(want.to_string(), *message);
                assert!(!inst.tmux_session().unwrap().exists(), "{label}");
            }
        }
    }

    /// #4118: a row that a peer archived or trashed must not reach the
    /// pre-launch hooks, even when the caller still holds a live in-memory
    /// copy. The hooks start containers and can have side effects, so this
    /// watches the hook actually running rather than only the error code.
    #[test]
    #[serial_test::serial]
    fn start_refuses_a_stored_shelved_row_before_the_pre_launch_hooks() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .expect("acknowledge the agent hook paths");
        let witness = temp.path().join("before-session-ran");
        crate::session::config::update_config(|global| {
            global.host_hooks.before_session = vec![format!(": > '{}'", witness.display())];
        })
        .expect("install before_session hook");

        let profile = "start-shelved-stored-row";
        let mut inst = Instance::new("start", "/tmp/x");
        inst.source_profile = profile.to_string();
        crate::session::instance::test_helpers::seed_disk_for_sidecar_test(profile, &inst);
        // Only the stored row is shelved: the instance keeps a live copy, which
        // is the state a daemon cache holds when a peer shelves behind it.
        crate::session::storage::Storage::new_unwatched(profile)
            .unwrap()
            .update(|rows, _| {
                crate::session::instance::Instance::archive(&mut rows[0]);
                Ok(())
            })
            .unwrap();

        let blocked = inst
            .start_with_size_opts(None, false)
            .err()
            .and_then(|error| error.downcast_ref::<StartBlocked>().copied());
        assert_eq!(blocked, Some(StartBlocked::Archived));
        assert!(
            !witness.exists(),
            "the pre-launch hook ran for a row already shelved in the store"
        );
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

    /// #4127: the terminal launch attests the route it applied, and nothing
    /// else. A binding that resolved no route, and a launch that is not a
    /// single-store host Claude one, attest nothing at all.
    #[test]
    fn attested_default_store_reads_the_route_the_launch_applied() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let default = home.join(".claude");
        let custom = home.join("custom-claude");
        std::fs::create_dir_all(&default).unwrap();
        let binding =
            |store: &std::path::Path, agent: &str, marker: Option<bool>| ExecutionBinding {
                agent: agent.into(),
                stores: vec![store.to_path_buf()],
                configuration: Vec::new(),
                cwd: home.clone(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: marker,
            };
        let routing = |dir: Option<&std::path::Path>| {
            vec![
                ("HOME".to_string(), Some(home.display().to_string())),
                (
                    "CLAUDE_CONFIG_DIR".to_string(),
                    dir.map(|dir| dir.display().to_string()),
                ),
            ]
        };

        // Routed at the default store: the launch exported it, so the marker
        // says so rather than leaving the legacy row to guess.
        assert_eq!(
            attested_default_store(
                &binding(&default, "claude", Some(true)),
                &routing(Some(&default)),
                &home
            ),
            Some(true)
        );
        // Left implicit, which the launch observed just as surely.
        assert_eq!(
            attested_default_store(
                &binding(&default, "claude", Some(false)),
                &routing(None),
                &home
            ),
            Some(false)
        );
        // A store that is not the default one, however it was routed.
        assert_eq!(
            attested_default_store(
                &binding(&custom, "claude", Some(false)),
                &routing(Some(&custom)),
                &home
            ),
            Some(false)
        );
        // No route resolved, and not this agent's host namespace.
        assert_eq!(
            attested_default_store(
                &binding(&default, "claude", None),
                &routing(Some(&default)),
                &home
            ),
            None
        );
        // Another agent's host namespace, and a launch that is not on the host
        // at all, attest nothing.
        assert_eq!(
            attested_default_store(
                &binding(&default, "codex", Some(false)),
                &routing(Some(&default)),
                &home
            ),
            None
        );
        let mut container = binding(&default, "claude", Some(false));
        container.filesystem = "container:session".into();
        assert_eq!(
            attested_default_store(&container, &routing(Some(&default)), &home),
            None
        );
    }

    /// #4127: the route the launch attests is the one the agent will actually
    /// see. A containerised launch routes to the container's own store, so the
    /// host's Claude namespace says nothing about it and the launch attests
    /// nothing at all — even though the very same binding, launched on the
    /// host, is stamped.
    #[test]
    fn attested_claude_store_route_refuses_a_container_launch() {
        use crate::containers::{ContainerExecutionSnapshot, RuntimeExecutionSnapshot};
        use crate::session::instance::execution::{NativeExecution, NativeLaunchInputs};
        use crate::session::ContainerRuntimeName;

        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        let store = home.join(".claude");
        std::fs::create_dir_all(&store).unwrap();
        let mut instance = Instance::new("attested-route", temp.path().to_str().unwrap());
        // The launched agent's HOME, so the route is read off a known home
        // rather than the one running the tests.
        instance.pending_host_env = vec![("HOME".into(), home.display().to_string())];

        let container = ContainerExecutionSnapshot {
            runtime: RuntimeExecutionSnapshot {
                kind: ContainerRuntimeName::Docker,
                program: std::path::PathBuf::from("/usr/bin/docker"),
                cwd: temp.path().to_path_buf(),
                endpoint: "local://docker".into(),
                local_mounts: true,
                routing: Vec::new(),
                global_arguments: Vec::new(),
            },
            name: "aoe-attested-route".into(),
            id: "0123456789ab".into(),
            mounts: Vec::new(),
            shadow_mounts: Vec::new(),
        };
        let launch = |inputs_container: Option<ContainerExecutionSnapshot>| NativeExecution {
            agent: crate::agents::get_agent("claude").expect("claude is a known agent"),
            binding: ExecutionBinding {
                agent: "claude".into(),
                stores: vec![store.clone()],
                configuration: Vec::new(),
                cwd: home.clone(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: Some(false),
            },
            routing: vec![("CLAUDE_CONFIG_DIR".to_string(), None)],
            case_insensitive_routing: &[],
            omp: None,
            inputs: NativeLaunchInputs {
                raw_environment: Vec::new(),
                launch_id: "launch-attested-route".into(),
                environment: Default::default(),
                cwd: temp.path().to_path_buf(),
                profile: "default".into(),
                container: inputs_container,
                docker_env: None,
                pane_env: Vec::new(),
                identity_extension: None,
            },
            program: std::path::PathBuf::new(),
            capture: None,
            pi_transcript_path: None,
            namespace_arguments: Vec::new(),
            target_session_id: None,
            resolved_target_session_id: None,
            pi_pinnable: false,
            opencode_preassign: false,
            opencode_schema: None,
            store_override: None,
        };

        // The host launch stamps the binding it actually applied...
        let host = launch(None);
        let attested = instance
            .attested_claude_store_route(&host)
            .expect("a host launch attests the route it applied");
        assert_eq!(attested.exported_default_store, Some(false));
        // ...and stamps nothing else about it.
        assert_eq!(attested.agent, host.binding.agent);
        assert_eq!(attested.stores, host.binding.stores);
        assert_eq!(attested.cwd, host.binding.cwd);

        // The containerised launch attests no route at all, so the caller
        // keeps the unstamped binding rather than one that claims the host's
        // Claude namespace described the container's store.
        let container_launch = launch(Some(container));
        assert_eq!(
            instance.attested_claude_store_route(&container_launch),
            None
        );
    }
}
