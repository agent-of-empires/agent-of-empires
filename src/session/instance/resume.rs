//! Restart and resume: attest targets before teardown, probe resumed panes,
//! and start fresh on the next attempt after a failed resume.

use super::*;

macro_rules! recovery_message {
    ($reason:literal) => {
        concat!(
            $reason,
            "\nConversation preserved. Restore original context, then retry:\n",
            "aoe session set-session-id <session> <native-id>\n",
            "Use --store with the exact store/transcript where required.\n",
            "Or choose fresh (abandons this AoE association):\n",
            "aoe session set-session-id <session> \"\"\n",
            "Then start/restart; old history is not deleted or recovered."
        )
    };
}

/// Finite reasons safe to present without exposing native diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManagedResumeCause {
    #[error("wrapper execution contract is missing")]
    WrapperContractRequired,
    #[error("managed command or arguments cannot be attested")]
    UnsupportedManagedArguments,
    #[error("OpenCode session schema cannot be attested")]
    OpenCodeSchemaUnattested,
    #[error("native execution context cannot be attested")]
    ExecutionContextUnattested,
}

impl ManagedResumeCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WrapperContractRequired => "wrapper_contract_required",
            Self::UnsupportedManagedArguments => "unsupported_managed_arguments",
            Self::OpenCodeSchemaUnattested => "opencode_schema_unattested",
            Self::ExecutionContextUnattested => "execution_context_unattested",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::WrapperContractRequired => recovery_message!(
                "Managed resume refused: wrapper contract missing.\n\
                 Restore truthful session.agent_execution_as and\n\
                 session.agent_config_dir in trusted global/profile config.\n\
                 Both must match the original agent and store."
            ),
            Self::UnsupportedManagedArguments => recovery_message!(
                "Managed resume refused: unsupported command or arguments.\n\
                 Remove native selectors and unsupported context overrides."
            ),
            Self::OpenCodeSchemaUnattested => recovery_message!(
                "Managed resume refused: OpenCode schema cannot be attested.\n\
                 Restore a verified release/build and its original database."
            ),
            Self::ExecutionContextUnattested => recovery_message!(
                "Managed resume refused: execution context cannot be attested.\n\
                 Restore the original agent configuration and store."
            ),
        }
    }

    pub(super) fn from_error(error: &anyhow::Error) -> Self {
        if let Some(cause) = error.downcast_ref::<Self>() {
            *cause
        } else if error.is::<super::execution::UnattestedOpenCodeSchema>() {
            Self::OpenCodeSchemaUnattested
        } else {
            Self::ExecutionContextUnattested
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{}", .0.message())]
pub(crate) struct ManagedResumeRefused(pub ManagedResumeCause);

/// `HonorAutoResumeSetting` respects configured fresh manual restarts.
/// `Allow` permits resume for sends regardless of that setting. Both policies
/// start fresh when the current ID already failed a native resume probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeAttemptPolicy {
    HonorAutoResumeSetting,
    Allow,
}

pub(crate) struct LaunchReservation {
    pub(crate) generation: u64,
    pub(crate) title_lock: crate::session::storage::StorageFlock,
    pub(crate) lifecycle_lock: crate::session::storage::StorageFlock,
}
/// Finite launch observations safe to expose without tmux diagnostics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchIndeterminateCause {
    CommandTimedOut,
    ObservationFailed,
    StabilizationExpired,
    OwnershipUnproven,
}

impl LaunchIndeterminateCause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CommandTimedOut => "command_timed_out",
            Self::ObservationFailed => "observation_failed",
            Self::StabilizationExpired => "stabilization_expired",
            Self::OwnershipUnproven => "ownership_unproven",
        }
    }

    pub const fn message(self) -> &'static str {
        match self {
            Self::CommandTimedOut => "Agent readiness is unknown because a tmux command timed out. Runtime and conversation preserved. Inspect the existing pane and restore tmux access before retrying; no input was sent.",
            Self::ObservationFailed => "Agent readiness is unknown because tmux observation failed. Runtime and conversation preserved. Inspect the existing pane and restore tmux access before retrying; no input was sent.",
            Self::StabilizationExpired => "Agent readiness was not established within the observation window. Runtime and conversation preserved. Inspect the existing pane before retrying; no input was sent.",
            Self::OwnershipUnproven => "Agent readiness is unknown because launch ownership changed or could not be proved. Runtime and conversation preserved. Refresh and inspect the current pane before retrying; no input was sent.",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeResult {
    Alive,
    Dead,
    Indeterminate(LaunchIndeterminateCause),
}

/// Transient proof captured at physical launch, before publication can adopt peer state.
pub(super) struct LaunchProbe {
    pub(super) tmux_session: String,
    pub(super) pane: Option<LegacyToolObservation>,
    pub(super) expected: ConversationState,
}

pub(crate) struct ResumeLaunchOptions {
    pub(crate) resume_policy: ResumeAttemptPolicy,
    pub(crate) restart: bool,
    pub(crate) conversation_carry: Option<ConversationCarry>,
}

const RESUME_PROBE_MAX: std::time::Duration = std::time::Duration::from_millis(3000);

const RESUME_PROBE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// Grace window we keep observing after the pane stops running its boot
/// shell, before declaring `Alive`. Sized to cover the longest in-pane
/// boot before a native session rejection can exit:
/// opencode (bun-compiled native binary that loads JS, parses argv, and
/// hits the session-not-found path) reaches `pane_dead = true` between
/// ~900ms and ~1100ms after spawn on a warm cache, longer on cold or
/// heavy projects. Healthy resumes pay this entire window once; the pane is
/// fully attachable for the duration so the cost is purely in the synchronous
/// restart path's latency, not in agent responsiveness afterward.
const RESUME_PROBE_POST_SHELL_GRACE: std::time::Duration = std::time::Duration::from_millis(2000);

impl Instance {
    pub fn restart_with_size(&mut self, size: Option<(u16, u16)>) -> Result<StartOutcome> {
        self.restart_with_size_opts(size, false)
    }

    /// Restart the session, optionally skipping on_launch hooks (e.g. when they
    /// already ran in the background creation poller).
    pub fn restart_with_size_opts(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) -> Result<StartOutcome> {
        self.restart_with_resume_policy(
            size,
            skip_on_launch,
            ResumeAttemptPolicy::HonorAutoResumeSetting,
        )
    }

    pub(crate) fn restart_with_resume_policy(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
    ) -> Result<StartOutcome> {
        self.orchestrate_resume_launch(
            size,
            skip_on_launch,
            false,
            ResumeLaunchOptions {
                resume_policy,
                restart: true,
                conversation_carry: None,
            },
        )
    }

    /// Restart, first removing the sandbox container when `discard_sandbox_container`
    /// is set so the launch recreates it with the current tool's mounts (#3959),
    /// and carrying the conversation into the incoming account's config root
    /// when the swap changed only the account (#4030).
    /// Removal happens only once this restart owns the Launch reservation.
    pub fn restart_discarding_sandbox_container(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        discard_sandbox_container: bool,
        conversation_carry: Option<ConversationCarry>,
    ) -> Result<StartOutcome> {
        self.orchestrate_resume_launch(
            size,
            skip_on_launch,
            discard_sandbox_container,
            ResumeLaunchOptions {
                resume_policy: ResumeAttemptPolicy::HonorAutoResumeSetting,
                restart: true,
                conversation_carry,
            },
        )
    }

    /// Require fresh evidence from the exact pane born in this launch.
    /// Observation failure and stabilization expiry never authorize teardown.
    fn probe_settle(
        &self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        probe: &LaunchProbe,
        max: std::time::Duration,
        poll: std::time::Duration,
    ) -> ProbeResult {
        use crate::tmux::utils::PaneProbe;
        let Some(pane) = probe.pane.as_ref() else {
            return ProbeResult::Indeterminate(LaunchIndeterminateCause::OwnershipUnproven);
        };
        let session = crate::tmux::Session::from_name(&probe.tmux_session);
        #[cfg(test)]
        let max = tests::probe_max(max);
        let deadline = std::time::Instant::now() + max;
        let mut first_post_shell = None;
        if let Err(cause) = self.fence_launch_probe(storage, generation, probe) {
            return ProbeResult::Indeterminate(cause);
        }
        let command_deadline = crate::tmux::TmuxCommandDeadline::new();
        #[cfg(test)]
        let command_deadline = tests::probe_deadline(command_deadline);
        loop {
            let observation =
                crate::tmux::utils::probe_owned_pane(session.name(), pane, &command_deadline);
            let (state, shell) = match observation {
                Ok(Some(observation)) => observation,
                Ok(None) => {
                    return ProbeResult::Indeterminate(LaunchIndeterminateCause::OwnershipUnproven)
                }
                Err(error) => return self.indeterminate_probe(error),
            };
            match state {
                PaneProbe::Dead | PaneProbe::Missing => {
                    return match self.fence_launch_probe(storage, generation, probe) {
                        Ok(()) => ProbeResult::Dead,
                        Err(cause) => ProbeResult::Indeterminate(cause),
                    };
                }
                PaneProbe::Unknown => {
                    return ProbeResult::Indeterminate(LaunchIndeterminateCause::ObservationFailed)
                }
                PaneProbe::Alive => {}
            }
            let now = std::time::Instant::now();
            if !shell {
                #[cfg(test)]
                let entered_post_shell = first_post_shell.is_none();
                let started = *first_post_shell.get_or_insert(now);
                #[cfg(test)]
                if entered_post_shell {
                    tests::post_shell_observed(&session);
                }
                if now.duration_since(started) >= RESUME_PROBE_POST_SHELL_GRACE {
                    return match self.fence_launch_probe(storage, generation, probe) {
                        Ok(()) => ProbeResult::Alive,
                        Err(cause) => ProbeResult::Indeterminate(cause),
                    };
                }
            } else {
                first_post_shell = None;
            }
            if now >= deadline {
                return ProbeResult::Indeterminate(LaunchIndeterminateCause::StabilizationExpired);
            }
            std::thread::sleep(poll.min(deadline.saturating_duration_since(now)));
        }
    }

    fn fence_launch_probe(
        &self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        probe: &LaunchProbe,
    ) -> std::result::Result<(), LaunchIndeterminateCause> {
        let rows = storage
            .load()
            .map_err(|error| self.probe_failure_cause(error))?;
        if rows.iter().any(|row| {
            row.id == self.id
                && row.lifecycle_reservation_is_owned(LifecycleOperation::Launch, generation)
                && probe.expected.matches(row)
        }) {
            Ok(())
        } else {
            Err(LaunchIndeterminateCause::OwnershipUnproven)
        }
    }

    fn indeterminate_probe(&self, error: anyhow::Error) -> ProbeResult {
        ProbeResult::Indeterminate(self.probe_failure_cause(error))
    }

    fn probe_failure_cause(&self, error: anyhow::Error) -> LaunchIndeterminateCause {
        let cause = if error.chain().any(|source| {
            source
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::TimedOut)
        }) {
            LaunchIndeterminateCause::CommandTimedOut
        } else {
            LaunchIndeterminateCause::ObservationFailed
        };
        tracing::warn!(target: "session.store", session = %self.id, cause = cause.as_str(), error = %format_args!("{error:#}"), "launch observation is indeterminate");
        cause
    }

    /// Start the session with a one-shot resume fallback.
    ///
    /// Cascade:
    ///   1. If a valid `agent_session_id` is set and the agent supports
    ///      resume, attempt the start (which appends `--resume <sid>` or
    ///      equivalent). Probe the pane via `probe_settle`.
    ///   2. If the pane went dead within the probe window, stop the poller,
    ///      tear down the dead tmux session, preserve the sid, persist a
    ///      `resume_probe_failed_sid` loop-breaker, and return
    ///      `StartOutcome::ResumeFailed`. A dead pane is not proof that the
    ///      sid is invalid, so this path must not clear it or launch fresh.
    ///   3. A launch that pins an already-stored id without resuming it
    ///      (`--session-id <sid>`, or a fork's pre-generated child id) is
    ///      probed the same way, but a death there fails the call outright
    ///      rather than arming the resume loop-breaker: nothing was resumed,
    ///      so there is no resume to break. See `probe_pinned_fresh_launch`.
    ///
    /// `HonorAutoResumeSetting` respects an explicit fresh-launch configuration.
    /// Both policies start fresh on the next attempt after a failed probe,
    /// returning `StartOutcome::FreshAfterFailedResume` with the abandoned ID.
    ///
    /// Healthy resumed and fresh-pinned panes pay the post-shell grace once.
    /// A shell-looking pane is not death evidence, but an expired readiness
    /// window without positive evidence returns `Indeterminate`.
    ///
    /// Acp-mode sessions short-circuit (no tmux pane to probe).
    /// `StartOutcome::Fresh` is honest there: structured view's resume concept lives
    /// in `acp_session_id` and is handled by the ACP supervisor, not
    /// by this cascade.
    pub(crate) fn start_with_resume_fallback(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        resume_policy: ResumeAttemptPolicy,
    ) -> Result<StartOutcome> {
        self.orchestrate_resume_launch(
            size,
            skip_on_launch,
            false,
            ResumeLaunchOptions {
                resume_policy,
                restart: false,
                conversation_carry: None,
            },
        )
    }

    fn orchestrate_resume_launch(
        &mut self,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        discard_sandbox_container: bool,
        options: ResumeLaunchOptions,
    ) -> Result<StartOutcome> {
        let profile = self.effective_profile();
        let storage = crate::session::storage::Storage::new(&profile, self.resolve_file_watch())
            .context("failed to open lifecycle lock storage")?;
        self.orchestrate_resume_launch_in(
            &storage,
            size,
            skip_on_launch,
            discard_sandbox_container,
            options,
        )
    }

    pub(crate) fn orchestrate_resume_launch_in(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        discard_sandbox_container: bool,
        options: ResumeLaunchOptions,
    ) -> Result<StartOutcome> {
        crate::session::validate_instance_id(&self.id)
            .context("refusing to start: AOE_INSTANCE_ID failed validation")?;
        if self.is_structured() {
            return Ok(StartOutcome::Fresh);
        }
        storage.check_available()?;

        let title_lock = crate::session::storage::acquire_session_title_lock(&self.id)
            .context("failed to acquire instance start title lock")?;
        let lifecycle_lock = storage
            .storage()
            .acquire_instance_lifecycle_lock(&self.id)
            .context("failed to acquire instance start lock")?;
        self.reconcile_from_store(storage)?;
        if self.is_structured() {
            return Ok(StartOutcome::Fresh);
        }
        self.ensure_startable()?;
        if !options.restart && self.tmux_session()?.exists() {
            return Ok(StartOutcome::Fresh);
        }
        if self.status == Status::Error {
            self.status = Status::Idle;
            self.last_error = None;
            self.last_error_evidence = None;
            self.last_error_check = None;
        }
        let generation = self.acquire_lifecycle_reservation(
            storage,
            LifecycleOperation::Launch,
            Some(Status::Starting),
        )?;
        if discard_sandbox_container {
            if let Err(error) = self.discard_stale_sandbox_container() {
                self.fail_reserved_launch(storage, generation, &error, false);
                return Err(error);
            }
        }
        self.resume_reserved_launch(
            storage,
            size,
            skip_on_launch,
            LaunchReservation {
                generation,
                title_lock,
                lifecycle_lock,
            },
            options,
        )
    }

    pub(crate) fn resume_reserved_launch(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
        reservation: LaunchReservation,
        options: ResumeLaunchOptions,
    ) -> Result<StartOutcome> {
        let generation =
            self.prepare_reserved_launch_hooks(storage, options.restart, reservation)?;
        let hook_result = self.run_pre_launch_hooks(skip_on_launch, storage, None);
        self.finish_reserved_launch(storage, size, options, generation, hook_result)
    }
    pub(crate) fn capture_before_restart_in(
        &mut self,
        storage: &dyn crate::session::SessionStore,
    ) -> Result<()> {
        self.stop_and_flush_poller_lifecycle_locked(storage)?;
        self.capture_omp_before_restart(storage)
    }

    pub(crate) fn discard_reserved_restart_container(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
    ) -> Result<()> {
        if let Err(error) = self.discard_stale_sandbox_container() {
            self.fail_reserved_launch(storage, generation, &error, false);
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn prepare_reserved_launch_hooks(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        restart: bool,
        reservation: LaunchReservation,
    ) -> Result<u64> {
        let LaunchReservation {
            generation,
            title_lock,
            lifecycle_lock,
        } = reservation;
        if restart {
            let capture = self
                .stop_and_flush_poller_lifecycle_locked(storage)
                .and_then(|()| self.capture_omp_before_restart(storage));
            if let Err(error) = capture {
                self.fail_reserved_launch(storage, generation, &error, false);
                return Err(error);
            }
        }

        drop(lifecycle_lock);
        drop(title_lock);
        Ok(generation)
    }

    pub(super) fn capture_resume_launch_target_in(&mut self, stores: CaptureStorage<'_>) {
        if matches!(self.resume_intent, ResumeIntent::Default) {
            if let Some(observation) = self.capture_freshest_conversation_in(stores) {
                self.apply_conversation_observation(&observation);
            }
        }
    }

    /// Validate a requested resume independently of the cascade's retry policy.
    pub(super) fn preflight_resume_launch(&self) -> Result<()> {
        if !self.force_fresh_next_launch && self.conversation_target().is_some() {
            if let Some(execution) = self.resolve_launch_execution()? {
                let validate_target = !matches!(self.resume_intent, ResumeIntent::Default)
                    || self
                        .conversation_target()
                        .and_then(|(_, binding, _)| binding)
                        .is_some_and(ConversationBinding::is_known);
                if validate_target && !self.validate_prime_fork_handoff(&execution)? {
                    self.validate_conversation_target(
                        &execution.binding,
                        execution.target_session_id.as_deref(),
                    )?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn finish_reserved_launch(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        size: Option<(u16, u16)>,
        options: ResumeLaunchOptions,
        generation: u64,
        hook_result: Result<()>,
    ) -> Result<StartOutcome> {
        let (_title_lock, _lifecycle_lock) =
            self.reacquire_launch_locks_after_hooks(storage, generation, hook_result)?;
        let ResumeLaunchOptions {
            resume_policy,
            restart,
            conversation_carry,
        } = options;
        // Capture may refresh memory, but publication must compare against durable prior state.
        let prior = self.conversation_state();
        self.capture_resume_launch_target_in(CaptureStorage::Scoped(storage));
        let skipped_failed_resume_sid = self.apply_resume_policy(resume_policy);
        if let Err(error) = self.preflight_resume_launch() {
            self.adopt_conversation_state(prior);
            self.fail_reserved_launch(storage, generation, &error, false);
            return Err(error);
        }
        if std::mem::take(&mut self.force_fresh_next_launch) {
            self.resume_intent = ResumeIntent::Cleared;
        }
        let spawned = (|| {
            let mut prepared =
                self.stop_carry_and_prepare_in(storage, restart, conversation_carry, prior)?;
            prepared.require_conversation_publication |= skipped_failed_resume_sid.is_some();
            self.spawn_prepared_launch(size, storage, prepared)
        })();
        let (launch_outcome, probe) = match spawned {
            Ok(spawned) => spawned,
            Err(error) => {
                self.fail_reserved_launch(storage, generation, &error, true);
                return Err(error);
            }
        };
        let result = self.finish_resume_launch(
            launch_outcome,
            skipped_failed_resume_sid,
            storage,
            generation,
            &probe,
        );
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(error) => {
                // Probe cleanup addresses only the captured pane, never a new name owner.
                self.fail_reserved_launch(storage, generation, &error, false);
                return Err(error);
            }
        };
        if let StartOutcome::Indeterminate { cause } = outcome {
            self.last_error = Some(cause.message().to_owned());
            if self.lifecycle_generation == generation {
                self.record_terminal_error(
                    ErrorEvidenceSource::TmuxObservation,
                    RuntimeEvidence::Unknown,
                );
            } else {
                self.last_error_evidence = None;
            }
            self.last_error_check = Some(std::time::Instant::now());
            // A failed release cannot turn missing evidence into teardown authority.
            if let Err(error) = self.release_indeterminate_launch(storage, generation) {
                tracing::warn!(target: "session.store", session = %self.id, error = %format_args!("{error:#}"), "indeterminate launch reservation release failed; runtime retained");
            }
            return Ok(outcome);
        }
        if let Err(error) = self.commit_lifecycle_launch(storage, generation, restart) {
            self.fail_reserved_launch(storage, generation, &error, false);
            return Err(error);
        }
        Ok(outcome)
    }

    /// Tear down the outgoing pane, carry the conversation, then build the
    /// launch command, in that order.
    ///
    /// The order is load-bearing at both ends, which is why all three steps
    /// live here rather than spread through the cascade. The outgoing agent
    /// appends to its transcript for as long as it runs, so a carry before the
    /// teardown copies a file that is still growing, and the incoming
    /// account's copy is never repaired once published. And
    /// `build_launch_command` chooses `--resume <sid>` or `--session-id <sid>`
    /// from whether the incoming account's transcript exists, so a carry after
    /// it pins an id the agent then rejects as already in use, killing the pane
    /// (#3399).
    fn stop_carry_and_prepare_in(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        restart: bool,
        conversation_carry: Option<ConversationCarry>,
        expected: ConversationState,
    ) -> Result<PreparedLaunch> {
        let publication = if restart {
            self.prime_root_publication(true)
        } else {
            None
        };
        if restart {
            self.kill_clean_locked()?;
        }
        if let Some(carry) = conversation_carry {
            self.absorb_published_prime_session(publication);
            // Only the default conversation may be refreshed from a final
            // observation: under Use/Fork/Cleared the launch names a specific
            // conversation, and an outgoing sidecar must not rebind it
            // (launch_command's preparation applies the same gate).
            if matches!(self.resume_intent, ResumeIntent::Default) {
                if let Some(observation) =
                    self.capture_freshest_conversation_in(CaptureStorage::Scoped(storage))
                {
                    self.apply_conversation_observation(&observation);
                }
            }
            let original = self.agent_session_binding.clone();
            let relocated = match carry.run_for(self) {
                Ok(relocated) => relocated,
                Err(error) => {
                    self.adopt_conversation_state(expected);
                    return Err(error);
                }
            };
            let excluded = original
                .filter(|binding| self.retroactive_capture_excludes.insert(binding.clone()));
            let mut prepared =
                self.prepare_launch_command_in(CaptureStorage::Scoped(storage), expected)?;
            if let Some(binding) = excluded {
                self.retroactive_capture_excludes.remove(&binding);
            }
            prepared.require_conversation_publication |= relocated;
            return Ok(prepared);
        }
        let stores = CaptureStorage::Scoped(storage);
        let prepared = self.prepare_launch_command_in(stores, expected)?;
        if restart {
            return self.refresh_prepared_prime_launch_after_pane_stop_in(
                stores,
                prepared,
                publication,
            );
        }
        Ok(prepared)
    }

    #[cfg(test)]
    fn stop_carry_and_prepare(
        &mut self,
        restart: bool,
        conversation_carry: Option<ConversationCarry>,
        expected: ConversationState,
    ) -> Result<PreparedLaunch> {
        let storage =
            crate::session::storage::Storage::new_unwatched(&self.effective_profile()).unwrap();
        self.stop_carry_and_prepare_in(&storage, restart, conversation_carry, expected)
    }

    /// A failure fails the restart: relaunching into the old container would run the new tool
    /// against the previous tool's config store.
    fn discard_stale_sandbox_container(&self) -> Result<()> {
        if !self.is_sandboxed() {
            return Ok(());
        }
        let container = DockerContainer::from_session_id(&self.id);
        match container.discard() {
            crate::containers::Teardown::Removed => {
                tracing::info!(
                    target: "containers.runtime",
                    session = %self.id,
                    "removed sandbox container; it will be recreated on start"
                );
                Ok(())
            }
            crate::containers::Teardown::AlreadyGone => Ok(()),
            crate::containers::Teardown::Failed(e) => anyhow::bail!(
                "failed to remove sandbox container {}; remove it before restarting, or the \
                 restart reuses it: {e}",
                container.name
            ),
        }
    }

    fn apply_resume_policy(&mut self, resume_policy: ResumeAttemptPolicy) -> Option<String> {
        if self.resume_intent != ResumeIntent::Default {
            return None;
        }
        let sid = self.agent_session_id.as_deref()?;
        let resume_allowed_by_policy = match resume_policy {
            ResumeAttemptPolicy::Allow => true,
            ResumeAttemptPolicy::HonorAutoResumeSetting => {
                crate::session::config::profile_config::resolve_config_or_warn(
                    &self.effective_profile(),
                )
                .session
                .auto_resume_on_restart
            }
        };
        if is_valid_session_id(sid)
            && self.supports_native_resume()
            && self.resume_probe_failed_sid.as_deref() == Some(sid)
        {
            let sid = sid.to_owned();
            self.force_fresh_next_launch = true;
            return Some(sid);
        }
        if !resume_allowed_by_policy {
            if self.status_agent().is_some() || self.default_selector_agent().is_some() {
                self.force_fresh_next_launch = true;
            }
            return None;
        }
        None
    }

    /// A confirmed fresh-pinned death permits cleanup, but never a resume-failure marker.
    fn probe_pinned_fresh_launch(
        &mut self,
        sid: &str,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        probe: &LaunchProbe,
    ) -> Result<ProbeResult> {
        let result = self.probe_settle(
            storage,
            generation,
            probe,
            RESUME_PROBE_MAX,
            RESUME_PROBE_POLL,
        );
        if result != ProbeResult::Dead {
            return Ok(result);
        }
        let detail =
            Self::dead_pane_detail(probe.pane.as_ref().expect("confirmed probe has a pane"));
        let cleanup = self.remove_probed_pane(storage, generation, probe, None)?;
        if cleanup != ProbeResult::Dead {
            return Ok(cleanup);
        }
        self.capture_poller.clear();
        anyhow::bail!("agent exited immediately when pinned to session id {sid}{detail}")
    }

    fn remove_probed_pane(
        &mut self,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        probe: &LaunchProbe,
        failed_sid: Option<&str>,
    ) -> Result<ProbeResult> {
        let pane = probe.pane.as_ref().expect("confirmed probe has a pane");
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        #[cfg(test)]
        let deadline = tests::probe_deadline(deadline);
        let state = match crate::tmux::utils::probe_owned_pane(&probe.tmux_session, pane, &deadline)
        {
            Ok(Some((
                state @ (crate::tmux::utils::PaneProbe::Dead
                | crate::tmux::utils::PaneProbe::Missing),
                _,
            ))) => state,
            Ok(_) => {
                return Ok(ProbeResult::Indeterminate(
                    LaunchIndeterminateCause::OwnershipUnproven,
                ))
            }
            Err(error) => return Ok(self.indeterminate_probe(error)),
        };
        if let Err(cause) = self.fence_launch_probe(storage, generation, probe) {
            return Ok(ProbeResult::Indeterminate(cause));
        }
        if let Some(sid) = failed_sid {
            if let Err(error) =
                self.mark_resume_probe_failed(storage, sid, generation, &probe.expected)
            {
                return Ok(if error.is::<LifecycleReservationError>() {
                    ProbeResult::Indeterminate(LaunchIndeterminateCause::OwnershipUnproven)
                } else {
                    self.indeterminate_probe(error)
                });
            }
            if let Err(cause) = self.fence_launch_probe(storage, generation, probe) {
                return Ok(ProbeResult::Indeterminate(cause));
            }
        }
        if state == crate::tmux::utils::PaneProbe::Dead {
            if let Err(error) = crate::tmux::utils::remove_owned_dead_pane(pane) {
                return Ok(self.indeterminate_probe(error));
            }
        }
        self.release_scope()?;
        Ok(ProbeResult::Dead)
    }

    /// Last useful line from the exact dead pane, without terminal escapes.
    fn dead_pane_detail(pane: &LegacyToolObservation) -> String {
        let mut command = crate::tmux::tmux_query_command_for_socket(&pane.server.socket);
        command.args([
            "capture-pane",
            "-p",
            "-t",
            &pane.identity.pane_id,
            "-S",
            "-20",
        ]);
        crate::tmux::TmuxCommandDeadline::new()
            .run(&mut command)
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|output| {
                crate::tmux::utils::strip_ansi(&output)
                    .lines()
                    .rev()
                    .map(str::trim)
                    .find(|line| !line.is_empty() && !line.starts_with("Pane is dead"))
                    .map(|line| format!(": {line}"))
            })
            .unwrap_or_default()
    }

    fn should_probe_pinned_fresh_launch(&self, sid: Option<&str>) -> bool {
        sid.is_some_and(is_valid_session_id) && self.supports_native_resume()
    }

    fn finish_resume_launch(
        &mut self,
        launch_outcome: LaunchSidOutcome,
        skipped_failed_resume_sid: Option<String>,
        storage: &dyn crate::session::SessionStore,
        generation: u64,
        probe: &LaunchProbe,
    ) -> Result<StartOutcome> {
        let (attempted_sid, pinned_prior_sid) = match launch_outcome {
            LaunchSidOutcome::Existing { sid }
                if is_valid_session_id(&sid) && self.supports_native_resume() =>
            {
                (Some(sid), None)
            }
            LaunchSidOutcome::Fresh { pinned_prior_sid }
                if self.should_probe_pinned_fresh_launch(pinned_prior_sid.as_deref()) =>
            {
                (None, pinned_prior_sid)
            }
            _ => (None, None),
        };
        let Some(stale_sid) = attempted_sid else {
            if let Some(sid) = pinned_prior_sid {
                if let ProbeResult::Indeterminate(cause) =
                    self.probe_pinned_fresh_launch(&sid, storage, generation, probe)?
                {
                    return Ok(StartOutcome::Indeterminate { cause });
                }
            }
            return Ok(match skipped_failed_resume_sid {
                Some(sid) => StartOutcome::FreshAfterFailedResume { sid },
                None => StartOutcome::Fresh,
            });
        };

        match self.probe_settle(
            storage,
            generation,
            probe,
            RESUME_PROBE_MAX,
            RESUME_PROBE_POLL,
        ) {
            ProbeResult::Alive => return Ok(StartOutcome::Resumed),
            ProbeResult::Indeterminate(cause) => return Ok(StartOutcome::Indeterminate { cause }),
            ProbeResult::Dead => {}
        }

        tracing::warn!(
            target: "session.store",
            "start: resume with sid {} for session {} observed dead pane; \
             revalidating ownership and death before failure settlement",
            stale_sid,
            self.id,
        );
        let cleanup = self.remove_probed_pane(storage, generation, probe, Some(&stale_sid))?;
        if let ProbeResult::Indeterminate(cause) = cleanup {
            return Ok(StartOutcome::Indeterminate { cause });
        }
        self.capture_poller.clear();
        self.status = Status::Error;
        self.record_terminal_error(
            ErrorEvidenceSource::TmuxObservation,
            RuntimeEvidence::PaneExited { exit_code: None },
        );
        self.last_error = Some(format!(
            "resume failed for sid {}; preserved for explicit retry",
            stale_sid
        ));
        self.last_error_check = Some(std::time::Instant::now());
        Ok(StartOutcome::ResumeFailed { sid: stale_sid })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::instance::launch_command::build_resume_flags;
    use crate::session::instance::test_helpers::{install_aliases, seed_claude_transcript};
    use serial_test::serial;
    use tempfile::tempdir;
    #[test]
    #[serial]
    #[serial(hook_base)]
    fn fresh_publication_keeps_the_durable_cas_baseline() {
        use crate::session::instance::test_helpers::write_sidecar;
        if !crate::tmux::test_helpers::tmux_available() {
            return;
        }
        let temp = tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _env = isolate_resume_environment(temp.path());
        let (_hooks, _base, _hook_temp) = crate::hooks::test_support::BaseGuard::ready();
        let _agent = install_fake_claude(temp.path(), "#!/bin/sh\nexec sleep 60\n");
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let (old, fresh) = (
            "11111111-2222-4333-8444-555555555555",
            "66666666-7777-4888-8999-aaaaaaaaaaaa",
        );
        let mut inst = Instance::new("fresh-publication", project.to_str().unwrap());
        inst.tool = "claude".into();
        inst.command = "claude".into();
        inst.source_profile = "fresh-publication".into();
        seed_claude_transcript(&mut inst, old);
        seed_claude_transcript(&mut inst.clone(), fresh);
        let binding = inst.resolve_native_execution(None).unwrap().binding;
        let launch_id = uuid::Uuid::new_v4().to_string();
        let directory = write_sidecar(&inst.id, old);
        let leaf = crate::hooks::session_id_leaf(Some(&launch_id)).unwrap();
        inst.active_execution = Some(ActiveExecution {
            launch_id: launch_id.clone(),
            binding,
            capture: Some(CaptureContext::Hooks(directory.join(leaf.as_ref()))),
            container: None,
        });
        let storage = crate::session::Storage::new_unwatched(&inst.source_profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let generation = inst
            .acquire_lifecycle_reservation(
                &storage,
                LifecycleOperation::Launch,
                Some(Status::Starting),
            )
            .unwrap();
        crate::hooks::write_session_id_via_guard(&inst.id, fresh, Some(&launch_id)).unwrap();
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(old)
        );
        assert_eq!(inst.capture_freshest_conversation().unwrap().sid, fresh);
        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(name);
        let outcome = inst
            .finish_reserved_launch(
                &storage,
                None,
                ResumeLaunchOptions {
                    resume_policy: ResumeAttemptPolicy::Allow,
                    restart: true,
                    conversation_carry: None,
                },
                generation,
                Ok(()),
            )
            .unwrap();
        assert_eq!(outcome, StartOutcome::Resumed);
        let stored = storage.load().unwrap().remove(0);
        assert_eq!(stored.agent_session_id.as_deref(), Some(fresh));
        assert_eq!(
            stored.agent_session_binding.as_ref().unwrap().session_id,
            fresh
        );
        assert_ne!(
            stored.active_execution.as_ref().unwrap().launch_id,
            launch_id
        );
        assert_eq!(stored.active_execution, inst.active_execution);
        assert_eq!(
            inst.restart_with_resume_policy(None, true, ResumeAttemptPolicy::Allow)
                .unwrap(),
            StartOutcome::Resumed
        );
        assert_eq!(
            storage.load().unwrap()[0].agent_session_id.as_deref(),
            Some(fresh)
        );
        inst.kill_clean_in(&storage).unwrap();
    }

    #[test]
    #[serial]
    fn explicit_fresh_configuration_clears_opaque_wrapper_identity() {
        use crate::tmux::test_helpers::{
            only_pane_id, pane_field, wait_for_pane_command, TmuxTestSession,
        };
        if !crate::tmux::test_helpers::tmux_available() {
            return;
        }
        let temp = tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _env = isolate_resume_environment(temp.path());
        let ready = temp.path().join("agent-ready");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in --help|--version) exit 1 ;; esac\nprintf '%s\\n' \"$$\" > {}\nexec /bin/sleep 60\n",
            shell_escape(ready.to_str().unwrap())
        );
        let _agent = install_fake_claude(temp.path(), &script);
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let profile = "opaque-fresh-policy";
        let config =
            crate::session::config::profile_config::get_profile_config_path(profile).unwrap();
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(config, "[session]\nauto_resume_on_restart=false\n").unwrap();
        let mut inst = Instance::new("opaque-fresh-policy", project.to_str().unwrap());
        inst.tool = "claude".into();
        inst.command = "claude".into();
        inst.source_profile = profile.into();
        let sid = "11111111-2222-4333-8444-555555555555";
        seed_claude_transcript(&mut inst, sid);
        inst.command = format!(
            "/bin/sh -c 'exec {}'",
            temp.path().join("bin/claude").display()
        );
        assert!(!inst.supports_native_resume());
        let before = inst.conversation_state();
        let storage = crate::session::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _pane = TmuxTestSession::from_name(&name);
        assert!(crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", &name, "exec sleep 60"])
            .output()
            .unwrap()
            .status
            .success());
        let pane = only_pane_id(&name);
        wait_for_pane_command(&pane, "sleep");
        let process = pane_field(&pane, "#{pane_pid}");
        assert!(inst
            .restart_with_resume_policy(None, true, ResumeAttemptPolicy::Allow)
            .is_err());
        assert_eq!(only_pane_id(&name), pane);
        assert_eq!(pane_field(&pane, "#{pane_pid}"), process);
        assert_eq!(storage.load().unwrap()[0].conversation_state(), before);
        let outcome = inst
            .restart_with_resume_policy(None, true, ResumeAttemptPolicy::HonorAutoResumeSetting)
            .unwrap();
        assert_eq!(outcome, StartOutcome::Fresh);
        let stored = storage.load().unwrap().remove(0);
        assert_eq!(stored.agent_session_id, None);
        assert_eq!(stored.agent_session_binding, None);
        assert_eq!(stored.resume_intent, ResumeIntent::Default);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&ready)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "opaque wrapper did not run its agent"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(crate::process::worker::is_pid_alive(pid));
        assert_ne!(pane_field(&only_pane_id(&name), "#{pane_pid}"), process);
        inst.kill_clean_in(&storage).unwrap();
    }

    #[test]
    #[serial]
    fn managed_resume_refusal_preserves_the_live_pane_and_capture_generation() {
        use crate::tmux::test_helpers::{
            only_pane_id, pane_field, tmux_available, wait_for_pane_command, TmuxTestSession,
        };
        if !tmux_available() {
            return;
        }
        let temp = tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _env = isolate_resume_environment(temp.path());
        let _omp = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "omp",
            "#!/bin/sh\nexec sleep 60\n",
        );
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let sid = "11111111-2222-4333-8444-555555555555";
        let mut inst = Instance::new("managed-resume-refusal", project.to_str().unwrap());
        inst.tool = "omp".into();
        inst.command = "omp".into();
        inst.source_profile = "managed-resume-refusal".into();
        inst.agent_session_id = Some(sid.into());
        inst.omp_capture_generation = Some("original-generation".into());
        let storage =
            crate::session::storage::Storage::new_unwatched(&inst.source_profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _pane = TmuxTestSession::from_name(&name);
        assert!(crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", &name, "exec sleep 60"])
            .output()
            .unwrap()
            .status
            .success());
        let pane = only_pane_id(&name);
        wait_for_pane_command(&pane, "sleep");
        let process = pane_field(&pane, "#{pane_pid}");
        for intent in [ResumeIntent::Default, ResumeIntent::Use(sid.into())] {
            inst.resume_intent = intent.clone();
            let before = inst.conversation_state();
            let failed_probe = matches!(intent, ResumeIntent::Use(_));
            storage
                .update(|rows, _| {
                    rows[0].resume_intent = intent.clone();
                    rows[0].resume_probe_failed_sid = failed_probe.then(|| sid.into());
                    Ok(())
                })
                .unwrap();
            let result = inst.restart_with_resume_policy(None, true, ResumeAttemptPolicy::Allow);
            assert!(result.is_err(), "an unbound managed resume must refuse");
            assert_eq!(only_pane_id(&name), pane);
            assert_eq!(pane_field(&pane, "#{pane_pid}"), process);
            assert_eq!(pane_field(&pane, "#{pane_dead}"), "0");
            assert_eq!(inst.conversation_state(), before);
            let stored = storage.load().unwrap().remove(0);
            assert_eq!(stored.conversation_state(), before);
            assert_eq!(
                stored.omp_capture_generation.as_deref(),
                Some("original-generation")
            );
            assert_eq!(
                stored.resume_probe_failed_sid.as_deref(),
                failed_probe.then_some(sid)
            );
            assert!(stored.lifecycle_reservation.is_none());
        }
    }

    #[test]
    #[serial]
    fn cleared_bare_wrapper_launch_does_not_invent_a_managed_identity() {
        let temp = tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&temp.path().join("app"));
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("HOME", temp.path().to_path_buf()),
            ("CLAUDE_CONFIG_DIR", temp.path().join(".claude")),
        ]);
        let old = "11111111-2222-4333-8444-555555555555";
        for (label, failed_probe) in [("loop-breaker", true), ("policy", false)] {
            let profile = format!("bare-wrapper-cleared-{label}");
            let path =
                crate::session::config::profile_config::get_profile_config_path(&profile).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path,
                "[session]\nauto_resume_on_restart = false\n[session.agent_detect_as]\nwork-claude = \"claude\"\n"
            ).unwrap();
            let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
            let mut inst = Instance::new(label, temp.path().to_str().unwrap());
            inst.source_profile = profile.clone();
            inst.tool = "work-claude".into();
            inst.command = "work-claude".into();
            inst.agent_session_id = Some(old.into());
            inst.resume_probe_failed_sid = failed_probe.then(|| old.into());
            assert!(inst.execution_agent().is_err());
            let storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
            storage
                .update(|rows, _| {
                    rows.push(inst.clone());
                    Ok(())
                })
                .unwrap();

            assert_eq!(
                inst.apply_resume_policy(ResumeAttemptPolicy::HonorAutoResumeSetting),
                failed_probe.then(|| old.into())
            );
            let expected = inst.apply_fresh_launch_intent();
            let prepared = inst.prepare_launch_command(expected.clone()).unwrap();
            assert!(inst.agent_session_id.is_none());
            assert!(!prepared.is_existing);
            let command = prepared.command.unwrap();
            assert!(!command.contains("--session-id"), "{command}");
            assert!(!command.contains("--resume"), "{command}");
            assert_eq!(
                inst.persist_session_id(&profile, &expected),
                SidPersistOutcome::Published
            );
            let row = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == inst.id)
                .unwrap();
            assert!(row.agent_session_id.is_none());
            assert_eq!(row.resume_probe_failed_sid, None);
            assert_eq!(row.resume_intent, ResumeIntent::Default);
            let expected = inst.conversation_state();
            let second = inst.prepare_launch_command(expected.clone()).unwrap();
            assert!(!second.is_existing);
            assert_eq!(inst.conversation_state(), expected);
            assert!(!second.command.unwrap().contains("--session-id"));

            for intent in [
                ResumeIntent::Use(old.into()),
                ResumeIntent::Fork { from: old.into() },
            ] {
                let mut explicit = row.clone();
                explicit.resume_intent = intent;
                assert!(explicit
                    .prepare_launch_command(explicit.conversation_state())
                    .is_err());
            }
        }
    }

    /// Pins the order inside `stop_carry_and_prepare`: the launch command is
    /// built from whether the incoming account's transcript exists, so the
    /// carry has to have published it by then. Preparing first yields
    /// `--session-id <sid>` on an id the agent rejects as already in use once
    /// the carry creates the transcript (#3399, #4030).
    #[test]
    #[serial]
    fn carry_runs_before_the_launch_command_picks_its_resume_flag() {
        const SID: &str = "11111111-2222-3333-4444-555555555555";
        let _app_dir = crate::session::test_support::isolate_app_dir();
        let stub = tempdir().unwrap();
        let _claude = install_fake_claude(stub.path(), "#!/bin/sh\nexit 1\n");
        let home = dirs::home_dir().expect("home");
        let app_dir = crate::session::get_app_dir().expect("app dir");
        std::fs::create_dir_all(&app_dir).expect("app dir");
        std::fs::write(
            app_dir.join("config.toml"),
            "[session.agent_detect_as]\n\
             claude-1 = \"claude\"\n\
             claude-2 = \"claude\"\n\
             \n\
             [session.agent_config_dir]\n\
             claude-1 = \"~/dot-claude-1\"\n\
             claude-2 = \"~/dot-claude-2\"\n",
        )
        .expect("config");
        let profile = crate::session::config::effective_profile("");
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
        crate::session::config::profile_config::resolve_config_or_warn(&profile);

        let project = home.join("project");
        std::fs::create_dir_all(&project).expect("project");
        let mut inst = Instance::new("t", project.to_str().unwrap());
        inst.tool = "claude-1".to_string();
        inst.detect_as = "claude".to_string();
        // The renamed-wrapper shape these per-account tools take: a bare token
        // the launch shell resolves, which keeps native resume available.
        inst.command = "claude".to_string();
        inst.agent_session_id = Some(SID.to_string());

        // The outgoing account holds the conversation, the incoming one does not.
        let encoded = crate::session::capture::encode_claude_project_path(
            &crate::session::capture::canonicalize_or_raw(project.to_str().unwrap())
                .to_string_lossy(),
        );
        let seeded = home.join("dot-claude-1").join("projects").join(&encoded);
        std::fs::create_dir_all(&seeded).expect("seed dir");
        std::fs::write(seeded.join(format!("{SID}.jsonl")), "conversation\n").expect("seed");
        inst.agent_session_binding =
            Some(inst.asserted_resume_binding(SID, None).expect("binding"));

        let carry = match crate::session::conversation_carry::classify(&inst, &profile, "claude-2")
        {
            crate::session::conversation_carry::ToolSwap::KeepConversation(Some(carry)) => carry,
            other => panic!("expected a planned carry, got {other:?}"),
        };
        inst.swap_account("claude-2");
        let expected = inst.conversation_state();
        let prepared = inst
            .stop_carry_and_prepare(false, Some(carry), expected)
            .expect("prepare");

        assert!(
            prepared.is_existing,
            "the carried transcript must be visible when the flag is chosen"
        );
        assert!(
            prepared
                .command
                .as_deref()
                .is_some_and(|command| command.contains(&format!("--resume {SID}"))),
            "expected --resume on the carried conversation, got: {:?}",
            prepared.command
        );
        let resumed = inst
            .resolve_native_execution(inst.conversation_target())
            .expect("execution");
        assert_eq!(
            resumed.binding.stores.first(),
            Some(&home.join("dot-claude-2").canonicalize().unwrap()),
            "account swap must consume target store"
        );
    }
    type PostShellCallback = Box<dyn FnOnce(&crate::tmux::Session)>;
    thread_local! {
        static POST_SHELL_OBSERVER: std::cell::RefCell<Option<PostShellCallback>> = const { std::cell::RefCell::new(None) };
    }

    thread_local! {
        static PROBE_COMMANDS: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
        static PROBE_MAX: std::cell::Cell<Option<std::time::Duration>> = const { std::cell::Cell::new(None) };
    }

    pub(super) fn probe_deadline(
        default: crate::tmux::TmuxCommandDeadline,
    ) -> crate::tmux::TmuxCommandDeadline {
        PROBE_COMMANDS.with(|commands| {
            commands.get().map_or(
                default,
                crate::tmux::TmuxCommandDeadline::expiring_after_commands,
            )
        })
    }

    pub(super) fn probe_max(default: std::time::Duration) -> std::time::Duration {
        PROBE_MAX.with(|max| max.get().unwrap_or(default))
    }

    struct ProbeSettings;
    impl ProbeSettings {
        fn install(commands: Option<i64>, max: Option<std::time::Duration>) -> Self {
            PROBE_COMMANDS.with(|slot| slot.set(commands));
            PROBE_MAX.with(|slot| slot.set(max));
            Self
        }
    }
    impl Drop for ProbeSettings {
        fn drop(&mut self) {
            PROBE_COMMANDS.with(|slot| slot.set(None));
            PROBE_MAX.with(|slot| slot.set(None));
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    #[serial(hook_base)]
    fn outer_launch_probe_preserves_unknown_runtime() {
        if std::env::var_os("AOE_FROZEN_NATIVE_TEST_CHILD").is_some() {
            crate::process::exec_frozen_environment().unwrap();
            return;
        }
        use crate::session::instance::start::test_support::{FinalizeObserver, FinalizePhase};
        use crate::tmux::test_helpers::{only_pane_id, pane_field};
        use crate::tmux::utils::owned_probe_test_support::Observer;
        use std::cell::{Cell, RefCell};
        use std::os::unix::process::ExitStatusExt;
        use std::rc::Rc;

        struct CaseContext {
            resumed: bool,
            fault: &'static str,
        }
        impl Drop for CaseContext {
            fn drop(&mut self) {
                if std::thread::panicking() {
                    eprintln!(
                        "probe fixture flavor={} fault={}",
                        if self.resumed {
                            "resumed"
                        } else {
                            "fresh-pinned"
                        },
                        self.fault
                    );
                }
            }
        }

        if !crate::tmux::test_helpers::tmux_available() {
            return;
        }
        const SID: &str = "11111111-2222-4333-8444-555555555555";
        for resumed in [true, false] {
            for fault in [
                "lookup_timeout",
                "pane_timeout",
                "failed",
                "malformed",
                "expiry",
                "release_failure",
                "cached_absence",
                "superseded",
                "same_sid_assertion",
                "concurrent_status",
                "replacement",
                "rename_only",
                "dead_recheck_timeout",
                "dead_recheck_malformed",
                "dead_recheck_owner",
                "dead_recheck_status",
                "dead_recheck_marked",
                "dead_remove_timeout",
                "server_changed",
                "pane_pid_missing",
                "dead_pid_changed",
                "dead",
                "absence",
            ] {
                if !resumed && fault == "dead_recheck_marked" {
                    continue;
                }
                let _case = CaseContext { resumed, fault };
                let logs = crate::session::test_support::LogCapture::start();
                let dead_first = fault == "dead" || fault.starts_with("dead_");
                let root = tempdir().unwrap();
                let _app = crate::session::test_support::isolate_app_dir_at(root.path());
                let _native_helper = crate::session::instance::test_helpers::frozen_test_helper(
                    root.path(),
                    concat!(
                        module_path!(),
                        "::outer_launch_probe_preserves_unknown_runtime"
                    ),
                );
                let _env = isolate_resume_environment(root.path());
                let (_hooks, _base, _hook_temp) = crate::hooks::test_support::BaseGuard::ready();
                crate::session::config::update_config(|config| {
                    config.systemd.enabled = false;
                    config.session.smart_rename = false;
                })
                .unwrap();
                let project = root.path().join("project");
                std::fs::create_dir(&project).unwrap();
                let argv = root.path().join("argv");
                let native_ready = root.path().join("native.pid");
                let pending_ready = root.path().join("native.pid.tmp");
                let script = format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' \"$$\" > {}\nmv {} {}\n{}\n",
                    shell_escape(&argv.to_string_lossy()),
                    shell_escape(&pending_ready.to_string_lossy()),
                    shell_escape(&pending_ready.to_string_lossy()),
                    shell_escape(&native_ready.to_string_lossy()),
                    if dead_first {
                        "exit 1"
                    } else {
                        "exec sleep 60"
                    }
                );
                let _agent = install_fake_claude(root.path(), &script);
                let mut instance = Instance::new("probe safety", project.to_str().unwrap());
                instance.tool = "claude".into();
                instance.command = "claude".into();
                instance.source_profile = "probe-safety".into();
                if resumed {
                    seed_claude_transcript(&mut instance, SID);
                } else {
                    std::fs::create_dir_all(root.path().join(".claude")).unwrap();
                    instance.agent_session_id = Some(SID.into());
                }
                let storage =
                    crate::session::Storage::new_unwatched(&instance.source_profile).unwrap();
                storage
                    .update(|rows, _| {
                        rows.push(instance.clone());
                        Ok(())
                    })
                    .unwrap();
                let name = crate::tmux::Session::generate_name(&instance.id, &instance.title);
                let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(&name);
                let sentinel =
                    crate::tmux::test_helpers::TmuxTestSession::new("probe-absence-sentinel");
                if fault == "absence" {
                    crate::tmux::Session::from_name(sentinel.name())
                        .create(
                            project.to_str().unwrap(),
                            Some("exec sleep 60"),
                            &instance.source_profile,
                        )
                        .unwrap();
                }
                let snapshot = Rc::new(RefCell::new(None));
                let seen = snapshot.clone();
                let cache = Rc::new(RefCell::new(None));
                let force_cache = cache.clone();
                let old_pane = Rc::new(RefCell::new(None));
                let old_pane_guard = old_pane.clone();
                let peer = Rc::new(RefCell::new(None));
                let peer_pane = peer.clone();
                let _finalize = FinalizeObserver::install(
                    instance.id.clone(),
                    move |row, phase| {
                        if !matches!(phase, FinalizePhase::After) {
                            return;
                        }
                        let pane = only_pane_id(&name);
                        let deadline =
                            std::time::Instant::now() + std::time::Duration::from_secs(5);
                        let native_pid =
                            loop {
                                if let Some(pid) = std::fs::read_to_string(&native_ready)
                                    .ok()
                                    .and_then(|text| text.trim().parse::<u32>().ok())
                                {
                                    break pid;
                                }
                                assert!(std::time::Instant::now() < deadline,
                                "fake Agent did not publish PID/argv receipt; pane output: {:?}",
                                crate::tmux::Session::from_name(&name).capture_pane(30));
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            };
                        let native_birth = if dead_first {
                            while pane_field(&pane, "#{pane_dead}") != "1" {
                                assert!(std::time::Instant::now() < deadline,
                                    "fake Agent {native_pid} was invoked but pane stayed live; native alive={}; pane output: {:?}",
                                    crate::process::worker::is_pid_alive(native_pid),
                                    crate::tmux::Session::from_name(&name).capture_pane(30));
                                std::thread::sleep(std::time::Duration::from_millis(10));
                            }
                            None
                        } else {
                            let birth = crate::process::process_birth_identity(native_pid)
                                .unwrap()
                                .expect("invoked fake Agent has a process birth");
                            assert!(
                                crate::process::process_birth_is_live(native_pid, &birth).unwrap()
                            );
                            assert_eq!(pane_field(&pane, "#{pane_dead}"), "0");
                            Some(birth)
                        };
                        let args = std::fs::read_to_string(&argv).unwrap();
                        let args: Vec<_> = args.lines().collect();
                        let selector = if resumed { "--resume" } else { "--session-id" };
                        assert!(
                            args.windows(2).any(|pair| pair == [selector, SID]),
                            "{args:?}"
                        );
                        if fault == "cached_absence" {
                            let guard = crate::tmux::SessionCacheGuard::capture();
                            guard.force_present(&[]);
                            *force_cache.borrow_mut() = Some(guard);
                        }
                        *seen.borrow_mut() = Some((
                            row.clone(),
                            pane.clone(),
                            pane_field(&pane, "#{pane_pid}"),
                            native_pid,
                            native_birth,
                        ));
                        if fault == "absence" {
                            let output = crate::tmux::tmux_command()
                                .args(["kill-session", "-t", &name])
                                .output()
                                .unwrap();
                            assert!(output.status.success());
                        }
                        if matches!(fault, "replacement" | "rename_only") {
                            let old_name = if fault == "rename_only" {
                                crate::tmux::Session::generate_name(&row.id, "renamed probe")
                            } else {
                                format!("{name}_old")
                            };
                            *old_pane_guard.borrow_mut() = Some(
                                crate::tmux::test_helpers::TmuxTestSession::from_name(&old_name),
                            );
                            let renamed = crate::tmux::tmux_command()
                                .args(["rename-session", "-t", &name, &old_name])
                                .output()
                                .unwrap();
                            assert!(renamed.status.success());
                            if fault == "replacement" {
                                let output = crate::tmux::tmux_command()
                                    .args(["new-session", "-d", "-s", &name, "exec sleep 60"])
                                    .output()
                                    .unwrap();
                                assert!(output.status.success());
                                let replacement = only_pane_id(&name);
                                let peer_pid = pane_field(&replacement, "#{pane_pid}");
                                let peer_birth = crate::process::process_birth_identity(
                                    peer_pid.parse().unwrap(),
                                )
                                .unwrap()
                                .expect("replacement pane process has a birth");
                                assert!(crate::process::process_birth_is_live(
                                    peer_pid.parse().unwrap(),
                                    &peer_birth
                                )
                                .unwrap());
                                *peer_pane.borrow_mut() =
                                    Some((name.clone(), replacement.clone(), peer_pid, peer_birth));
                            }
                        }
                    },
                );
                let observed = Rc::new(Cell::new(0usize));
                let observed_query = observed.clone();
                let writes = Rc::new(RefCell::new(None));
                let fail_writes = writes.clone();
                let profile = instance.source_profile.clone();
                let id = instance.id.clone();
                let _observation = Observer::install(move |output| {
                    let sample = observed_query.get() + 1;
                    observed_query.set(sample);
                    match fault {
                        "pane_pid_missing" | "dead_pid_changed" => {
                            let text = String::from_utf8(output.stdout.clone()).unwrap();
                            let mut fields: Vec<_> = text.trim_end().splitn(8, '|').collect();
                            let other_pid = std::process::id().to_string();
                            fields[2] = if fault == "pane_pid_missing" {
                                ""
                            } else {
                                &other_pid
                            };
                            output.stdout = format!("{}\n", fields.join("|")).into_bytes();
                        }
                        "dead_recheck_timeout" if sample == 1 => {
                            PROBE_COMMANDS.with(|commands| commands.set(Some(0)));
                        }
                        "dead_recheck_malformed" if sample == 2 => {
                            output.stdout = b"not a pane observation\n".to_vec();
                        }
                        "dead_recheck_marked" => {
                            if sample == 1 {
                                assert!(
                                    String::from_utf8_lossy(&output.stdout).split('|').nth(5)
                                        == Some("1")
                                );
                                crate::session::Storage::new_unwatched(&profile)
                                    .unwrap()
                                    .update(|rows, _| {
                                        rows.iter_mut()
                                            .find(|row| row.id == id)
                                            .unwrap()
                                            .resume_probe_failed_sid = Some(SID.into());
                                        Ok(())
                                    })
                                    .unwrap();
                            } else if sample == 2 {
                                output.stdout = b"not a pane observation\n".to_vec();
                            }
                        }
                        "dead_recheck_owner" | "dead_recheck_status" | "dead_remove_timeout"
                            if sample == 2 =>
                        {
                            crate::session::Storage::new_unwatched(&profile)
                                .unwrap()
                                .update(|rows, _| {
                                    let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                                    row.status = Status::Running;
                                    if fault == "dead_recheck_owner" {
                                        row.lifecycle_generation += 1;
                                        row.lifecycle_reservation = None;
                                    }
                                    Ok(())
                                })
                                .unwrap();
                            if fault == "dead_recheck_status" {
                                output.stdout = b"not a pane observation\n".to_vec();
                            } else if fault == "dead_remove_timeout" {
                                crate::tmux::utils::owned_probe_test_support::expire_removal();
                            }
                        }
                        "failed" => {
                            output.status = std::process::ExitStatus::from_raw(1 << 8);
                            output.stderr = b"private tmux transport diagnostic".to_vec();
                        }
                        "malformed" => output.stdout = b"not a pane observation\n".to_vec(),
                        "server_changed" => {
                            let text = String::from_utf8(output.stdout.clone()).unwrap();
                            let mut fields: Vec<_> = text.trim_end().splitn(8, '|').collect();
                            let other_pid = std::process::id().to_string();
                            fields[4] = &other_pid;
                            output.stdout = format!("{}\n", fields.join("|")).into_bytes();
                        }
                        "expiry" => {
                            let text = String::from_utf8(output.stdout.clone()).unwrap();
                            let mut fields: Vec<_> = text.trim_end().splitn(8, '|').collect();
                            fields[6] = "sh";
                            fields[7] = "";
                            output.stdout = format!("{}\n", fields.join("|")).into_bytes();
                        }
                        "superseded" | "same_sid_assertion" | "concurrent_status" => {
                            if sample != 1 {
                                return;
                            }
                            crate::session::Storage::new_unwatched(&profile)
                                .unwrap()
                                .update(|rows, _| {
                                    let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                                    row.status = Status::Running;
                                    if fault == "superseded" {
                                        row.lifecycle_generation += 1;
                                        row.lifecycle_reservation = None;
                                        row.resume_probe_failed_sid = Some(SID.into());
                                    } else if fault == "same_sid_assertion" {
                                        row.resume_intent = ResumeIntent::Use(SID.into());
                                        row.resume_binding = row.agent_session_binding.clone();
                                        row.resume_probe_failed_sid = Some(SID.into());
                                    }
                                    Ok(())
                                })
                                .unwrap();
                            if fault == "concurrent_status" {
                                output.stdout = b"not a pane observation\n".to_vec();
                            }
                        }
                        "release_failure" => {
                            *fail_writes.borrow_mut() = Some(
                                crate::session::Storage::new_unwatched(&profile)
                                    .unwrap()
                                    .fail_writes_for_test(),
                            );
                            output.stdout = b"not a pane observation\n".to_vec();
                        }
                        _ => {}
                    }
                });
                let _settings = ProbeSettings::install(
                    match fault {
                        "lookup_timeout" => Some(0),
                        "pane_timeout" => Some(1),
                        _ => None,
                    },
                    (fault == "expiry").then_some(std::time::Duration::ZERO),
                );
                let released = Rc::new(Cell::new(0));
                let releases = released.clone();
                let _release = crate::process::systemd::release_probe::install(move |_| {
                    releases.set(releases.get() + 1)
                });
                let wake_dispatches = crate::session::restart::wake_dispatches_for_test();
                let result = if fault == "lookup_timeout" {
                    match instance.ensure_pane_ready_with_size(None) {
                        Ok(EnsureReadyOutcome::Indeterminate { cause }) => {
                            Ok(StartOutcome::Indeterminate { cause })
                        }
                        other => panic!("Ensure must refuse uncertain readiness: {other:?}"),
                    }
                } else if fault == "pane_timeout" {
                    let result = crate::session::restart::perform_restart(
                        crate::session::restart::RestartRequest {
                            session_id: instance.id.clone(),
                            instance,
                            size: None,
                            wake_message: "must-not-be-dispatched".into(),
                            skip_on_launch: true,
                            bound_hooks: false,
                            discard_sandbox_container: false,
                            conversation_carry: None,
                        },
                    );
                    instance = *result.instance;
                    result.outcome.map_err(anyhow::Error::msg)
                } else {
                    instance.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
                };
                assert_eq!(
                    crate::session::restart::wake_dispatches_for_test(),
                    wake_dispatches
                );
                let (before, pane, pid, native_pid, native_birth) = snapshot
                    .borrow_mut()
                    .take()
                    .expect("real outer finalizer ran");
                if !matches!(
                    fault,
                    "lookup_timeout" | "pane_timeout" | "replacement" | "rename_only" | "absence"
                ) {
                    assert!(
                        observed.get() > 0,
                        "fault must follow a real exact-pane observation"
                    );
                }
                if fault.starts_with("dead_recheck_") || fault == "dead_remove_timeout" {
                    assert_eq!(
                        observed.get(),
                        if fault == "dead_recheck_timeout" {
                            1
                        } else {
                            2
                        },
                        "fault must occur after the first confirmed-dead observation"
                    );
                }
                if matches!(fault, "dead" | "absence") {
                    if resumed {
                        assert_eq!(
                            result.unwrap(),
                            StartOutcome::ResumeFailed { sid: SID.into() }
                        );
                    } else {
                        assert!(result.is_err());
                    }
                    assert!(!instance.tmux_session().unwrap().exists_live());
                    let stored = storage.load().unwrap().remove(0);
                    assert_eq!(
                        stored.resume_probe_failed_sid.as_deref(),
                        resumed.then_some(SID)
                    );
                    assert_eq!(released.get(), 1);
                    continue;
                }
                let outcome = result.unwrap();
                if fault == "cached_absence" {
                    assert_eq!(
                        outcome,
                        if resumed {
                            StartOutcome::Resumed
                        } else {
                            StartOutcome::Fresh
                        }
                    );
                } else {
                    let cause = match fault {
                        "lookup_timeout"
                        | "pane_timeout"
                        | "dead_recheck_timeout"
                        | "dead_remove_timeout" => LaunchIndeterminateCause::CommandTimedOut,
                        "expiry" => LaunchIndeterminateCause::StabilizationExpired,
                        "superseded" | "same_sid_assertion" | "replacement" | "rename_only"
                        | "server_changed" | "dead_recheck_owner" | "pane_pid_missing"
                        | "dead_pid_changed" => LaunchIndeterminateCause::OwnershipUnproven,
                        _ => LaunchIndeterminateCause::ObservationFailed,
                    };
                    assert_eq!(
                        outcome,
                        StartOutcome::Indeterminate { cause },
                        "private launch diagnostics:\n{}",
                        logs.contents()
                    );
                    assert!(!crate::session::restart::launched_agent(&Ok(outcome)));
                }
                writes.borrow_mut().take();
                let stored = storage.load().unwrap().remove(0);
                assert_eq!(pane_field(&pane, "#{pane_pid}"), pid);
                if let Some(birth) = native_birth.as_ref() {
                    assert!(
                        crate::process::process_birth_is_live(native_pid, birth).unwrap(),
                        "the same invoked fake Agent must survive without replacement"
                    );
                }
                assert_eq!(
                    pane_field(&pane, "#{pane_dead}"),
                    if dead_first { "1" } else { "0" }
                );
                if fault == "same_sid_assertion" {
                    assert_eq!(stored.agent_session_id.as_deref(), Some(SID));
                    assert_eq!(stored.resume_intent, ResumeIntent::Use(SID.into()));
                    assert_eq!(stored.resume_probe_failed_sid.as_deref(), Some(SID));
                } else {
                    assert_eq!(stored.conversation_state(), before.conversation_state());
                    assert_eq!(
                        stored.resume_probe_failed_sid.as_deref(),
                        if fault == "superseded"
                            || fault == "dead_recheck_marked"
                            || (fault == "dead_remove_timeout" && resumed)
                        {
                            Some(SID)
                        } else {
                            before.resume_probe_failed_sid.as_deref()
                        }
                    );
                }
                assert_eq!(stored.active_execution, before.active_execution);
                assert_eq!(stored.terminal_scopes, before.terminal_scopes);
                assert_eq!(
                    stored.lifecycle_generation,
                    before.lifecycle_generation
                        + if matches!(fault, "superseded" | "dead_recheck_owner") {
                            1
                        } else {
                            0
                        }
                );
                assert_eq!(
                    stored.lifecycle_reservation.is_some(),
                    fault == "release_failure"
                );
                assert_eq!(instance.capture_started_at, before.capture_started_at);
                assert_eq!(instance.started_at, before.started_at);
                assert_eq!(released.get(), 0);
                let transcript = root
                    .path()
                    .join(".claude")
                    .join("projects")
                    .join(crate::session::capture::encode_claude_project_path(
                        project.canonicalize().unwrap().to_str().unwrap(),
                    ))
                    .join(format!("{SID}.jsonl"));
                if resumed {
                    assert_eq!(std::fs::read_to_string(transcript).unwrap(), "seed\n");
                } else {
                    assert!(!transcript.exists());
                }
                if matches!(
                    fault,
                    "superseded"
                        | "same_sid_assertion"
                        | "concurrent_status"
                        | "dead_recheck_owner"
                        | "dead_recheck_status"
                        | "dead_remove_timeout"
                ) {
                    assert_eq!(stored.status, Status::Running);
                }
                if let Some((name, peer_pane, peer_pid, peer_birth)) = peer.borrow().as_ref() {
                    assert_ne!(*peer_pane, pane);
                    assert_eq!(only_pane_id(name), *peer_pane);
                    assert_eq!(pane_field(peer_pane, "#{pane_pid}"), *peer_pid);
                    assert_eq!(pane_field(peer_pane, "#{pane_dead}"), "0");
                    assert!(crate::process::process_birth_is_live(
                        peer_pid.parse().unwrap(),
                        peer_birth
                    )
                    .unwrap());
                }
                assert!(std::sync::Arc::ptr_eq(
                    instance
                        .capture_poller
                        .handle()
                        .expect("capture remains installed"),
                    before
                        .capture_poller
                        .handle()
                        .expect("finalizer installed capture"),
                ));
                if !dead_first {
                    assert!(instance.capture_poller.is_running());
                }
                instance.capture_poller.clear();
            }
        }
    }

    pub(super) fn post_shell_observed(session: &crate::tmux::Session) {
        let observer = POST_SHELL_OBSERVER.with(|slot| slot.borrow_mut().take());
        if let Some(observer) = observer {
            observer(session);
        }
    }

    struct PostShellObserver;
    impl Drop for PostShellObserver {
        fn drop(&mut self) {
            POST_SHELL_OBSERVER.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    fn isolate_resume_environment(
        root: &std::path::Path,
    ) -> crate::session::test_support::EnvGuard {
        let roots = vec![
            ("HOME", root.to_path_buf()),
            ("XDG_CONFIG_HOME", root.join(".config")),
            ("CLAUDE_CONFIG_DIR", root.join(".claude")),
        ];
        let guard = crate::session::test_support::EnvGuard::set(&roots);
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
        guard
    }

    fn install_fake_claude(
        root: &std::path::Path,
        script: &str,
    ) -> crate::session::test_support::EnvGuard {
        crate::session::test_support::install_login_shell_path_command(root, "claude", script)
    }

    #[test]
    #[serial]
    fn account_restart_consumes_persisted_selected_store() {
        const SID: &str = "11111111-2222-3333-4444-555555555555";
        let temp = tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _env = isolate_resume_environment(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("config.toml"),
            "[session.agent_detect_as]\na = \"claude\"\nb = \"claude\"\n[session.agent_config_dir]\na = \"~/source\"\nb = \"~/destination\"\n").unwrap();
        let profile = crate::session::config::effective_profile("");
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(&profile);
        crate::session::config::profile_config::resolve_config_or_warn(&profile);
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let relative = std::path::Path::new("projects")
            .join(crate::session::capture::encode_claude_project_path(
                &project.canonicalize().unwrap().to_string_lossy(),
            ))
            .join(format!("{SID}.jsonl"));
        for (root, content, seconds) in [
            ("source", "unselected\n", 200),
            ("destination", "selected\n", 100),
        ] {
            let path = temp.path().join(root).join(&relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(
                    std::fs::FileTimes::new().set_modified(
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds),
                    ),
                )
                .unwrap();
        }
        let record = temp.path().join("launched");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$CLAUDE_CONFIG_DIR\" \"$@\" > {}\nexec sleep 60\n",
            shell_escape(&record.to_string_lossy()),
        );
        let _claude = install_fake_claude(temp.path(), &script);
        let destination = temp.path().join("destination");
        let mut instance = Instance::new("carry-runtime", project.to_str().unwrap());
        instance.source_profile = profile.clone();
        instance.tool = "a".into();
        instance.command = "claude".into();
        instance.detect_as = "claude".into();
        let binding = instance
            .asserted_resume_binding(SID, Some(&destination))
            .unwrap();
        instance.set_agent_conversation(Some(SID.into()), Some(binding), None);
        let storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(instance.clone());
                Ok(())
            })
            .unwrap();
        let name = crate::tmux::Session::generate_name(&instance.id, &instance.title);
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(name);
        instance
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        std::fs::remove_file(&record).unwrap();
        let crate::session::conversation_carry::ToolSwap::KeepConversation(Some(carry)) =
            crate::session::conversation_carry::classify(&instance, &profile, "b")
        else {
            panic!("expected account carry");
        };
        storage
            .update(|rows, _| {
                rows.iter_mut()
                    .find(|row| row.id == instance.id)
                    .unwrap()
                    .swap_account("b");
                Ok(())
            })
            .unwrap();
        let outcome = instance.restart_discarding_sandbox_container(None, true, false, Some(carry));
        instance.stop_and_flush_poller();
        let observed = std::fs::read_to_string(&record).unwrap();
        instance.kill_clean_in(&storage).unwrap();
        assert!(outcome.is_ok(), "{outcome:?}");
        // Line 0 is the launched CLAUDE_CONFIG_DIR; the remaining lines are
        // the argv.
        let recorded_store = observed.lines().next().unwrap_or_default().to_string();
        assert_eq!(
            crate::session::capture::canonicalize_allowing_missing_leaf(std::path::Path::new(
                &recorded_store
            ),)
            .unwrap_or_else(|| {
                crate::git::template::lexical_normalize(std::path::Path::new(&recorded_store))
            }),
            crate::session::capture::canonicalize_allowing_missing_leaf(&destination).unwrap(),
            "the launch must receive the asserted store"
        );
        let args: Vec<_> = observed.lines().skip(1).collect();
        assert!(
            args.windows(2).any(|pair| pair == ["--resume", SID]),
            "{args:?}"
        );
        assert_eq!(
            std::fs::read_to_string(destination.join(relative)).unwrap(),
            "selected\n"
        );
        let rows = storage.load().unwrap();
        let row = rows.iter().find(|row| row.id == instance.id).unwrap();
        assert_eq!(row.tool, "b");
        assert_eq!(
            row.agent_session_binding
                .as_ref()
                .unwrap()
                .execution
                .as_ref()
                .unwrap()
                .stores,
            vec![destination.canonicalize().unwrap()]
        );
    }

    #[test]
    fn resume_and_capture_capabilities_control_each_path() {
        let sid = "11111111-1111-1111-1111-111111111111";
        let cases = [
            ("cursor", true, true),
            ("qwen", false, false),
            ("kiro", false, false),
            ("copilot", true, false),
            ("vibe", true, false),
            ("claude", true, true),
            ("opencode", true, false),
        ];

        for (tool, resume_supported, poller_supported) in cases {
            let mut inst = Instance::new("resume-contract", "/tmp/test");
            inst.tool = tool.to_string();
            inst.agent_session_id = Some(sid.to_string());
            inst.resume_intent = ResumeIntent::Use(sid.to_string());

            assert_eq!(
                inst.supports_native_resume(),
                resume_supported,
                "{tool}: resume-probe decision"
            );
            assert_eq!(
                inst.supports_session_poller(),
                poller_supported,
                "{tool}: poller capability"
            );
            assert_eq!(
                crate::session::recovery::is_recovery_candidate(&inst),
                resume_supported,
                "{tool}: startup recovery eligibility"
            );

            let mut command = crate::agents::get_agent(tool)
                .unwrap()
                .launch_base_command();
            let base_command = command.clone();
            let resumed = inst.apply_session_flags(
                &mut command,
                inst.resolved_agent(),
                None,
                &super::execution::AgentLaunchContext::host(
                    inst.resolved_agent(),
                    inst.default_selector_agent()
                        .and_then(|agent| inst.host_agent_command(agent, None))
                        .as_ref(),
                ),
                &mut inst.conversation_state(),
            );
            if resume_supported {
                assert!(resumed.unwrap(), "{tool}: launch resume decision");
                assert_ne!(command, base_command, "{tool}: resume selector missing");
            } else {
                assert!(
                    resumed.is_err(),
                    "{tool}: an explicit unsupported resume must fail"
                );
                assert_eq!(command, base_command, "{tool}: rejected command changed");
            }
            assert_eq!(
                build_resume_flags(tool, sid, true).is_empty(),
                !resume_supported,
                "{tool}: direct resume flags"
            );
            if matches!(tool, "qwen" | "kiro") {
                assert_eq!(
                    inst.finish_resume_launch(
                        LaunchSidOutcome::Fresh {
                            pinned_prior_sid: Some(sid.to_string()),
                        },
                        None,
                        &crate::session::storage::Storage::new_unwatched("resume-contract")
                            .unwrap(),
                        0,
                        &LaunchProbe {
                            tmux_session: String::new(),
                            pane: None,
                            expected: inst.conversation_state()
                        },
                    )
                    .unwrap(),
                    StartOutcome::Fresh,
                    "{tool}: inert stored ID must not trigger the pinned launch probe"
                );
            }
        }
    }

    fn dead_resume_fixture(inst: &Instance) -> crate::tmux::test_helpers::TmuxTestSession {
        use crate::tmux::test_helpers::{only_pane_id, wait_for_pane_dead, TmuxTestSession};
        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let guard = TmuxTestSession::from_name(name.clone());
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                &name,
                "true",
                ";",
                "set-option",
                "-p",
                "-t",
                &name,
                "remain-on-exit",
                "on",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        wait_for_pane_dead(&only_pane_id(&name));
        crate::tmux::refresh_session_cache();
        guard
    }

    #[test]
    #[serial]
    fn a_native_launch_records_the_executable_it_ran() {
        const PROFILE: &str = "launch-executable";
        let temp = tempdir().unwrap();
        let _env = isolate_resume_environment(temp.path());
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let _claude = install_fake_claude(temp.path(), "#!/bin/sh\nsleep 30\n");
        let mut inst = Instance::new("launch-executable", project.to_str().unwrap());
        inst.tool = "claude".to_string();
        inst.command = "claude".to_string();
        inst.source_profile = PROFILE.to_string();
        let storage = crate::session::storage::Storage::new_unwatched(PROFILE).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(tmux_name);

        inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();

        let recorded = inst.launch_executable.clone().expect("recorded on the row");
        assert!(recorded.is_absolute());
        assert_eq!(recorded.file_name().unwrap(), "claude");
        let rows = storage.load().unwrap();
        let row = rows.iter().find(|row| row.id == inst.id).unwrap();
        assert_eq!(row.launch_executable.as_deref(), Some(recorded.as_path()));
    }

    #[test]
    #[serial]
    fn start_with_resume_fallback_uses_launch_sid_for_probe_decision() {
        use crate::session::instance::start::test_support::{FinalizeObserver, FinalizePhase};
        const PROFILE: &str = "launch-sid-probe";
        const LAUNCHED_SID: &str = "11111111-1111-1111-1111-111111111111";
        const PEER_SID: &str = "22222222-2222-2222-2222-222222222222";

        let temp = tempdir().unwrap();
        let _env = isolate_resume_environment(temp.path());
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let argv_path = temp.path().join("argv");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nexit 1\n",
            shell_escape(&argv_path.to_string_lossy())
        );
        let _claude = install_fake_claude(temp.path(), &script);
        let mut inst = Instance::new("launch-sid-probe", project.to_str().unwrap());
        inst.tool = "claude".to_string();
        inst.command = "claude".to_string();
        inst.source_profile = PROFILE.to_string();
        inst.agent_session_id = Some(LAUNCHED_SID.to_string());
        seed_claude_transcript(&mut inst, LAUNCHED_SID);
        let storage = crate::session::storage::Storage::new_unwatched(PROFILE).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(tmux_name.clone());
        let finalized = std::rc::Rc::new(std::cell::Cell::new(false));
        let finalized_in_observer = finalized.clone();
        let _observer = FinalizeObserver::install(inst.id.clone(), move |instance, phase| {
            match phase {
                FinalizePhase::Before => {
                    let pane = crate::tmux::test_helpers::only_pane_id(&tmux_name);
                    crate::tmux::test_helpers::wait_for_pane_dead(&pane);
                    let argv = std::fs::read_to_string(&argv_path).unwrap();
                    let args: Vec<_> = argv.lines().collect();
                    assert!(
                        args.windows(2)
                            .any(|pair| pair == ["--resume", LAUNCHED_SID]),
                        "actual agent argv: {args:?}"
                    );
                    assert_eq!(instance.agent_session_id.as_deref(), Some(LAUNCHED_SID));
                    let peer_storage =
                        crate::session::storage::Storage::new_unwatched(PROFILE).unwrap();
                    peer_storage
                        .update(|rows, _| {
                            let row = rows.iter_mut().find(|row| row.id == instance.id).unwrap();
                            row.agent_session_id = Some(PEER_SID.to_string());
                            Ok(())
                        })
                        .unwrap();
                }
                FinalizePhase::After => {
                    assert_eq!(instance.agent_session_id.as_deref(), Some(PEER_SID), "real finalize CAS skip must reload peer identity before producing the launch outcome");
                    finalized_in_observer.set(true);
                }
            }
        });
        let outcome = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert!(finalized.get(), "native launch must reach finalization");
        assert_eq!(
            outcome,
            StartOutcome::Indeterminate {
                cause: LaunchIndeterminateCause::OwnershipUnproven,
            }
        );
        let rows = storage.load().unwrap();
        let row = rows.iter().find(|row| row.id == inst.id).unwrap();
        assert_eq!(row.agent_session_id.as_deref(), Some(PEER_SID));
        assert_eq!(
            row.resume_probe_failed_sid, None,
            "failure of A must not mark the peer's B as failed"
        );
        assert!(inst.tmux_session().unwrap().exists_live());
    }

    #[test]
    #[serial]
    fn missing_probe_owner_preserves_even_a_dead_pane() {
        let temp = tempdir().unwrap();
        let _env = isolate_resume_environment(temp.path());
        let mut inst = Instance::new("missing-probe-owner", "/tmp/test");
        inst.tool = "claude".to_string();
        inst.source_profile = "missing-probe-owner".to_string();
        let launched_sid = "11111111-1111-1111-1111-111111111111";
        inst.agent_session_id = Some(launched_sid.to_string());
        let storage =
            crate::session::storage::Storage::new_unwatched(&inst.source_profile).unwrap();
        let _pane = dead_resume_fixture(&inst);
        let result = inst
            .finish_resume_launch(
                LaunchSidOutcome::Existing {
                    sid: launched_sid.to_owned(),
                },
                None,
                &storage,
                0,
                &LaunchProbe {
                    tmux_session: String::new(),
                    pane: None,
                    expected: inst.conversation_state(),
                },
            )
            .unwrap();
        assert_eq!(
            result,
            StartOutcome::Indeterminate {
                cause: LaunchIndeterminateCause::OwnershipUnproven
            }
        );
        assert_eq!(inst.resume_probe_failed_sid, None);
        assert!(inst.tmux_session().unwrap().exists());
        assert!(inst.tmux_session().unwrap().is_pane_dead());
    }

    #[test]
    #[serial]
    fn restart_outcome_for_acp_session_is_fresh() {
        let temp = tempdir().unwrap();
        let _env = isolate_resume_environment(temp.path());

        let mut inst = Instance::new("acp_test", "/tmp/x");
        inst.view = crate::session::instance::View::Structured;
        inst.agent_session_id = Some("11111111-1111-1111-1111-111111111111".to_string());
        inst.tool = "claude".to_string();

        let outcome = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(outcome, StartOutcome::Fresh);
    }

    #[test]
    #[serial]
    fn resume_fallback_marks_failed_and_preserves_sid_instead_of_launching_fresh() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        // (sid, fake agent): one dies on any launch; one would live, but only without the stale
        // sid, and the fallback must still not launch fresh.
        for (stale_sid, script) in [
            ("11111111-1111-1111-1111-111111111111", "#!/bin/sh\nexit 1\n"),
            (
                "22222222-2222-2222-2222-222222222222",
                "#!/bin/sh\ncase \"$*\" in *22222222-2222-2222-2222-222222222222*) exit 1 ;; esac\nexec sleep 30\n",
            ),
        ] {
            let temp = tempdir().unwrap();
            let project_dir = temp.path().join("project");
            std::fs::create_dir_all(&project_dir).unwrap();
            let _env = isolate_resume_environment(temp.path());
            let storage = crate::session::storage::Storage::new_unwatched("fb-test").unwrap();

            let mut inst = Instance::new("fallback_test", project_dir.to_str().unwrap());
            inst.tool = "claude".to_string();
            inst.source_profile = "fb-test".to_string();
            let _fake_claude = install_fake_claude(temp.path(), script);
            inst.command = "claude".to_string();
            inst.agent_session_id = Some(stale_sid.to_string());
            inst.status = Status::Idle;
            // Real prior conversation on disk so acquire takes the --resume path.
            seed_claude_transcript(&mut inst, stale_sid);

            let xs = vec![inst.clone()];
            storage
                .update(|i, g| {
                    *i = xs.to_vec();
                    *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                    Ok(())
                })
                .unwrap();

            let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();
            let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);
            let _ = crate::tmux::tmux_command()
                .args(["kill-session", "-t", &tmux_name])
                .output();

            assert_eq!(
                outcome.unwrap(),
                StartOutcome::ResumeFailed {
                    sid: stale_sid.to_string(),
                }
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(stale_sid));
            assert_eq!(inst.resume_probe_failed_sid.as_deref(), Some(stale_sid));
            assert_eq!(inst.status, Status::Error);
            assert_eq!(
                inst.last_error,
                Some(format!(
                    "resume failed for sid {stale_sid}; preserved for explicit retry"
                ))
            );
            assert!(inst.last_error_check.is_some());
            let loaded = storage.load().unwrap();
            let row = loaded.iter().find(|i| i.id == inst.id).expect("instance");
            assert_eq!(row.agent_session_id.as_deref(), Some(stale_sid));
            assert_eq!(row.resume_probe_failed_sid.as_deref(), Some(stale_sid));
        }
    }

    #[test]
    #[serial]
    fn moved_known_default_restart_probes_sid_without_rebinding_history() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("skipping: tmux unavailable");
            return;
        }
        let temp = tempdir().unwrap();
        let _env = isolate_resume_environment(temp.path());
        let _claude = install_fake_claude(temp.path(), "#!/bin/sh\nexit 1\n");
        let before = temp.path().join("before");
        let after = temp.path().join("after");
        std::fs::create_dir_all(&before).unwrap();
        std::fs::create_dir_all(&after).unwrap();
        let profile = "moved-known-default-restart";
        let sid = "11111111-2222-4333-8444-555555555555";
        let mut inst = Instance::new("moved-known", before.to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        inst.command = "claude".into();
        seed_claude_transcript(&mut inst, sid);
        let known = inst.agent_session_binding.clone();
        inst.project_path = after.to_str().unwrap().into();
        for intent in [
            ResumeIntent::Use(sid.into()),
            ResumeIntent::Fork { from: sid.into() },
        ] {
            let mut explicit = inst.clone();
            explicit.resume_intent = intent;
            explicit.resume_binding = known.clone();
            assert!(explicit
                .prepare_launch_command(explicit.conversation_state())
                .is_err());
        }
        let storage = crate::session::storage::Storage::new_unwatched(profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(inst.clone());
                Ok(())
            })
            .unwrap();
        let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(name);
        // The pane the failed resume discards has its scope released while
        // the launch reservation still fences a replacement.
        let held_at_release = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let seen = std::rc::Rc::clone(&held_at_release);
        let _probe = crate::process::systemd::release_probe::install(move |id| {
            let held = crate::session::storage::Storage::new_unwatched(profile)
                .unwrap()
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == id)
                .and_then(|row| row.lifecycle_reservation)
                .map(|reservation| reservation.op);
            seen.borrow_mut().push((id.to_string(), held));
        });
        let outcome = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(outcome, StartOutcome::ResumeFailed { sid: sid.into() });
        assert_eq!(
            *held_at_release.borrow(),
            [(inst.id.clone(), Some(LifecycleOperation::Launch))]
        );
        assert_eq!(inst.agent_session_binding, known);
        assert_eq!(inst.resume_probe_failed_sid.as_deref(), Some(sid));
        let saved = storage.load().unwrap();
        assert_eq!(saved[0].agent_session_binding, known);
        assert_eq!(saved[0].agent_session_id.as_deref(), Some(sid));
        assert_eq!(saved[0].resume_probe_failed_sid.as_deref(), Some(sid));
    }

    #[test]
    #[serial]
    fn restart_policy_honors_opt_out_and_prior_probe_failure() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("skipping: tmux unavailable");
            return;
        }
        let temp = tempdir().unwrap();
        let _env = isolate_resume_environment(temp.path());
        let sid = "44444444-4444-4444-4444-444444444444";
        let script = format!("#!/bin/sh\ncase \"$*\" in *{sid}*) exit 1 ;; esac\nexec sleep 30\n");
        let _claude = install_fake_claude(temp.path(), &script);
        for (policy, failed_probe, expected_resume) in [
            (ResumeAttemptPolicy::HonorAutoResumeSetting, false, false),
            (ResumeAttemptPolicy::Allow, false, true),
            (ResumeAttemptPolicy::HonorAutoResumeSetting, true, false),
            (ResumeAttemptPolicy::Allow, true, false),
        ] {
            let profile = format!("restart-policy-{policy:?}-{failed_probe}");
            let project = temp.path().join(&profile);
            std::fs::create_dir_all(&project).unwrap();
            let config =
                crate::session::config::profile_config::get_profile_config_path(&profile).unwrap();
            std::fs::create_dir_all(config.parent().unwrap()).unwrap();
            std::fs::write(config, "[session]\nauto_resume_on_restart = false\n").unwrap();
            let mut inst = Instance::new("policy", project.to_str().unwrap());
            inst.source_profile = profile.clone();
            inst.tool = "claude".into();
            inst.command = "claude".into();
            seed_claude_transcript(&mut inst, sid);
            inst.resume_probe_failed_sid = failed_probe.then(|| sid.into());
            let storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
            storage
                .update(|rows, _| {
                    rows.push(inst.clone());
                    Ok(())
                })
                .unwrap();
            let name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
            let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(name);
            let outcome = inst.start_with_resume_fallback(None, true, policy).unwrap();
            if failed_probe {
                assert_eq!(
                    outcome,
                    StartOutcome::FreshAfterFailedResume { sid: sid.into() }
                );
                assert_ne!(inst.agent_session_id.as_deref(), Some(sid));
                let stored = storage.load().unwrap().remove(0);
                assert_eq!(stored.conversation_state(), inst.conversation_state());
                assert_eq!(stored.resume_probe_failed_sid, None);
            } else if expected_resume {
                assert_eq!(outcome, StartOutcome::ResumeFailed { sid: sid.into() });
                assert_eq!(
                    storage.load().unwrap()[0].agent_session_id.as_deref(),
                    Some(sid)
                );
            } else {
                assert_eq!(outcome, StartOutcome::Fresh);
                assert_ne!(inst.agent_session_id.as_deref(), Some(sid));
            }
            inst.kill_clean_in(&storage).unwrap();
        }
    }

    #[test]
    #[serial]
    fn fresh_after_failed_resume_requires_durable_conversation_publication() {
        if !crate::tmux::test_helpers::tmux_available() {
            return;
        }
        let temp = tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _env = isolate_resume_environment(temp.path());
        let sid = "77777777-7777-4777-8777-777777777777";
        for tool in ["claude", "copilot"] {
            let _agent = crate::session::test_support::install_login_shell_path_command(
                temp.path(),
                tool,
                "#!/bin/sh\ncase \"$1\" in --help|--version) exit 1 ;; esac\nexec /bin/sleep 60\n",
            );
            let project = temp.path().join(tool);
            std::fs::create_dir(&project).unwrap();
            let profile = format!("fresh-publication-{tool}");
            let mut instance = Instance::new(tool, project.to_str().unwrap());
            instance.tool = tool.into();
            instance.command = tool.into();
            instance.source_profile = profile.clone();
            instance.agent_session_id = Some(sid.into());
            instance.resume_probe_failed_sid = Some(sid.into());
            assert!(instance.supports_native_resume());
            let before = instance.conversation_state();
            let storage = crate::session::Storage::new_unwatched(&profile).unwrap();
            storage
                .update(|rows, _| {
                    rows.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(
                instance.tmux_session().unwrap().name(),
            );
            {
                let _fault = super::super::sid_persist::test_support::WriteFailures::install(
                    instance.id.clone(),
                    false,
                    true,
                );
                let error = instance
                    .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
                    .expect_err("fresh success requires its conversation transition to commit");
                assert!(
                    error.to_string().contains("durable publication"),
                    "{tool}: {error:#}"
                );
                let stored = storage.load().unwrap().remove(0);
                assert_eq!(stored.conversation_state(), before);
                assert_eq!(stored.resume_probe_failed_sid.as_deref(), Some(sid));
                assert!(stored.lifecycle_reservation.is_none());
                assert!(!instance.tmux_session().unwrap().exists_live());
            }
            let outcome = instance
                .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
                .unwrap();
            assert_eq!(
                outcome,
                StartOutcome::FreshAfterFailedResume { sid: sid.into() }
            );
            assert_eq!(instance.agent_session_id.is_some(), tool == "claude");
            assert_ne!(instance.agent_session_id.as_deref(), Some(sid));
            let stored = storage.load().unwrap().remove(0);
            assert_eq!(stored.conversation_state(), instance.conversation_state());
            assert_eq!(stored.resume_intent, ResumeIntent::Default);
            assert_eq!(stored.resume_probe_failed_sid, None);
            assert!(stored.lifecycle_reservation.is_none());
            instance.kill_clean_in(&storage).unwrap();
        }
    }

    /// A sid whose resume probe already failed is never retried automatically (#2609).
    #[test]
    #[serial]
    fn stale_probe_failed_sid_is_not_retried_on_next_attempt() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.to_str().unwrap();
        let _env = isolate_resume_environment(temp.path());

        let storage = crate::session::storage::Storage::new_unwatched("fb-loop-break").unwrap();

        let stale_sid = "66666666-6666-6666-6666-666666666666".to_string();
        let mut inst = Instance::new("fallback_loop_break_test", project_path);
        inst.tool = "claude".to_string();
        inst.source_profile = "fb-loop-break".to_string();
        let ready = temp.path().join("fresh-agent-ready");
        let script = format!(
            "#!/bin/sh\ncase \"$1\" in --help|--version) exit 1 ;; esac\ncase \"$*\" in *{stale_sid}*) exit 1 ;; esac\nprintf '%s\\n' \"$$\" > {}\nexec /bin/sleep 60\n",
            shell_escape(ready.to_str().unwrap())
        );
        let _fake_claude = install_fake_claude(temp.path(), &script);
        inst.command = "claude".to_string();
        inst.agent_session_id = Some(stale_sid.clone());
        inst.status = Status::Idle;
        // Seed real history so the first attempt exercises native resume.
        seed_claude_transcript(&mut inst, &stale_sid);
        let transcript = temp
            .path()
            .join(".claude/projects")
            .join(crate::session::capture::encode_claude_project_path(
                &project_dir.canonicalize().unwrap().to_string_lossy(),
            ))
            .join(format!("{stale_sid}.jsonl"));
        let original_transcript = std::fs::read(&transcript).unwrap();

        let xs = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        // The first attempt preserves the conversation and marks the failure.
        let first = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(
            first,
            StartOutcome::ResumeFailed {
                sid: stale_sid.clone(),
            }
        );
        assert_eq!(
            inst.resume_probe_failed_sid.as_deref(),
            Some(stale_sid.as_str())
        );

        let second = inst
            .start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow)
            .unwrap();
        assert_eq!(
            second,
            StartOutcome::FreshAfterFailedResume {
                sid: stale_sid.clone(),
            }
        );
        assert_ne!(inst.agent_session_id.as_deref(), Some(stale_sid.as_str()));
        assert_eq!(inst.resume_probe_failed_sid, None);
        let stored = storage.load().unwrap().remove(0);
        assert_eq!(stored.conversation_state(), inst.conversation_state());
        assert_eq!(stored.resume_probe_failed_sid, None);
        assert_eq!(
            std::fs::read(&transcript).unwrap(),
            original_transcript,
            "fresh launch must preserve old history"
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&ready)
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
            {
                break pid;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "fresh native agent did not start"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(crate::process::worker::is_pid_alive(pid));
        assert!(!inst.tmux_session().unwrap().is_pane_dead());
        inst.kill_clean_in(&storage).unwrap();
    }

    #[test]
    #[serial]
    fn resume_failed_fires_when_pane_dies_inside_post_shell_grace_window() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("tmux not available; skipping");
            return;
        }
        let temp = tempdir().unwrap();
        let project_dir = temp.path().join("project");
        std::fs::create_dir_all(&project_dir).unwrap();
        let project_path = project_dir.to_str().unwrap();
        let _env = isolate_resume_environment(temp.path());

        let storage = crate::session::storage::Storage::new_unwatched("fb-test-grace").unwrap();

        let stale_sid = "33333333-3333-3333-3333-333333333333".to_string();
        let mut inst = Instance::new("fallback_grace_test", project_path);
        inst.tool = "claude".to_string();
        inst.source_profile = "fb-test-grace".to_string();
        let script = format!(
            "#!/bin/sh\ncase \"$*\" in *{stale}*) exec sleep 30 ;; esac\nexec sleep 30\n",
            stale = stale_sid,
        );
        let _fake_claude = install_fake_claude(temp.path(), &script);
        inst.command = "claude".to_string();
        inst.agent_session_id = Some(stale_sid.clone());
        inst.status = Status::Idle;
        // Real prior conversation on disk so acquire takes the --resume path.
        seed_claude_transcript(&mut inst, &stale_sid);

        let xs = vec![inst.clone()];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = crate::session::GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();

        let tmux_name = crate::tmux::Session::generate_name(&inst.id, &inst.title);
        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        let _pane = crate::tmux::test_helpers::TmuxTestSession::from_name(tmux_name.clone());
        let observed = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed_in_probe = observed.clone();
        let probe_name = tmux_name.clone();
        POST_SHELL_OBSERVER.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |session| {
                assert!(session.exists());
                assert!(!session.is_pane_dead());
                assert!(!session.is_pane_running_shell());
                observed_in_probe.set(true);
                let pane = crate::tmux::test_helpers::only_pane_id(&probe_name);
                let mut kill = crate::tmux::tmux_command();
                kill.args(["send-keys", "-t", &pane, "C-c"]);
                assert!(kill.output().unwrap().status.success());
                crate::tmux::test_helpers::wait_for_pane_dead(&pane);
            }))
        });
        let _observer = PostShellObserver;
        let outcome = inst.start_with_resume_fallback(None, true, ResumeAttemptPolicy::Allow);
        assert!(
            observed.get(),
            "the observer must enter the post-shell grace branch before death"
        );

        let _ = crate::tmux::tmux_command()
            .args(["kill-session", "-t", &tmux_name])
            .output();

        assert_eq!(
            outcome.unwrap(),
            StartOutcome::ResumeFailed {
                sid: stale_sid.clone()
            }
        );
        assert_eq!(inst.agent_session_id.as_deref(), Some(stale_sid.as_str()));
        assert_eq!(
            inst.resume_probe_failed_sid.as_deref(),
            Some(stale_sid.as_str())
        );
    }

    #[test]
    fn pinned_fresh_probe_uses_resolved_alias_capability() {
        const PROFILE: &str = "pinned-probe-alias";
        let _registry = install_aliases(PROFILE, &[("work-claude", "claude")]);
        let mut inst = Instance::new("alias", "/tmp/x");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "work-claude".to_string();
        inst.command = "claude".to_string();
        assert!(inst.should_probe_pinned_fresh_launch(Some("11111111-2222-3333-4444-555555555555")));
    }
}
