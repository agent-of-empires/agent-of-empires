//! Acquiring the agent session id a launch resumes from.

use super::*;
use crate::agents::SessionCaptureBackend;

/// Native selectors besides the resume strategy's own flags that make Claude
/// pick its conversation itself.
const CLAUDE_SELECTORS: &[&str] = &[
    "-c",
    "--continue",
    "-r",
    "--from-pr",
    "--teleport",
    "--cloud",
    "--remote",
    "--fork-session",
];

pub(super) fn read_session_settings(
    path: &Path,
) -> anyhow::Result<Option<serde_json::Map<String, serde_json::Value>>> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("settings path has no parent"))?;
    let leaf = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("settings path has no file name"))?;
    let root = crate::session::AnchoredDir::open(parent)?;
    let Some(bytes) = root.read_regular(Path::new(leaf), 64 * 1024)? else {
        anyhow::bail!("settings are not a bounded regular file");
    };
    Ok(serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&bytes).ok())
}

impl Instance {
    /// Returns `(session_id, is_existing)`. Explicit intents win; default intent
    /// keeps a stored id unless the pane's own backend proves it rotated, and
    /// otherwise captures only from a live pane through the declared backend.
    fn acquire_session_id(
        &mut self,
        execution: Option<&super::execution::NativeExecution>,
    ) -> (Option<String>, bool) {
        let backend = execution.map_or_else(
            || self.default_selector_backend(),
            |execution| {
                execution
                    .agent
                    .session_support
                    .as_ref()
                    .and_then(|support| support.capture.as_ref())
                    .map(|capture| capture.backend)
            },
        );
        let preassign = backend == Some(crate::agents::SessionCaptureBackend::OpenCode)
            && execution.map_or_else(
                || self.opencode_preassign_enabled(),
                |execution| execution.opencode_preassign,
            );
        let pin_pi = execution.map_or_else(
            || self.pi_session_id_pinnable(),
            |execution| execution.pi_pinnable,
        );
        let environment =
            (preassign && execution.is_none()).then(|| self.resolved_host_environment());
        let native_created = std::cell::Cell::new(false);
        let result = self.acquire_session_id_with(execution, &|path| {
            if pin_pi {
                return Some(crate::session::capture::generate_session_uuid());
            }
            if !preassign {
                return None;
            }
            let mut command = std::process::Command::new(
                execution.map_or(std::path::Path::new("opencode"), |execution| {
                    execution.program.as_path()
                }),
            );
            let cwd = if let Some(execution) = execution {
                command.env_clear().envs(&execution.inputs.environment);
                for (key, value) in &execution.routing {
                    if let Some(value) = value {
                        command.env(key, value);
                    } else {
                        command.env_remove(key);
                    }
                }
                execution.inputs.cwd.to_str()?
            } else {
                command.envs(crate::session::environment::resolve_host_environment_pairs(
                    environment.as_deref()?,
                ));
                path
            };
            let sid = crate::session::capture::preassign_opencode_session_id(cwd, command);
            native_created.set(sid.is_some());
            sid
        });
        if native_created.get() {
            if let (Some(execution), Some(sid)) = (execution, result.0.as_ref()) {
                self.set_agent_conversation(
                    Some(sid.clone()),
                    Some(ConversationBinding {
                        session_id: sid.clone(),
                        execution: Some(execution.binding.clone()),
                        provenance: ConversationProvenance::Observed,
                        transcript_path: None,
                    }),
                    None,
                );
            }
        }
        result
    }

    /// Session-id acquisition with the pre-mint step injected as a seam, so
    /// tests can drive the fresh-launch arms without a real opencode binary,
    /// network, or installed pi. Production wraps this with the live preassign
    /// helper and the Pi pin.
    pub(super) fn acquire_session_id_with(
        &mut self,
        execution: Option<&super::execution::NativeExecution>,
        mint_fresh_id: &dyn Fn(&str) -> Option<String>,
    ) -> (Option<String>, bool) {
        let backend = execution.map_or_else(
            || self.default_selector_backend(),
            |execution| {
                execution
                    .agent
                    .session_support
                    .as_ref()
                    .and_then(|support| support.capture.as_ref())
                    .map(|capture| capture.backend)
            },
        );
        match self.resume_intent.clone() {
            ResumeIntent::Use(sid) => {
                let path = (self.agent_session_id.as_ref() == Some(&sid))
                    .then(|| self.pi_session_path.clone())
                    .flatten();
                self.set_agent_conversation(Some(sid.clone()), self.resume_binding.clone(), path);
                return (Some(sid), true);
            }
            ResumeIntent::Cleared => {
                self.set_agent_conversation(None, None, None);
                self.resume_probe_failed_sid = None;
                let session_id = self.fresh_launch_session_id(backend, mint_fresh_id);
                self.set_agent_conversation(session_id.clone(), None, None);
                return (session_id, false);
            }
            ResumeIntent::Fork { .. } => {
                // The child id was pre-generated and stored in
                // agent_session_id at creation. acquire returns it as the
                // session this instance owns; the actual fork flags
                // (--resume <parent> --fork-session --session-id <child>) are
                // emitted by apply_session_flags, which reads the parent off
                // the Fork intent. Report `false` (not an in-place resume): a
                // fork starts a new session.
                return (self.agent_session_id.clone(), false);
            }
            ResumeIntent::Default => {}
        }

        match self.prime_root_publication() {
            Some(PrimeRootPublication::Ready(id))
                if !self.is_capture_excluded(
                    &id,
                    self.active_execution.as_ref().map(|active| &active.binding),
                ) =>
            {
                let binding = self
                    .prime_root_observation(id.clone())
                    .conversation_binding();
                self.set_agent_conversation(Some(id.clone()), binding, None);
                return (Some(id), true);
            }
            Some(PrimeRootPublication::Pending(id))
                if !self.is_capture_excluded(
                    &id,
                    self.active_execution.as_ref().map(|active| &active.binding),
                ) =>
            {
                self.set_agent_conversation(None, None, None);
                self.resume_probe_failed_sid = None;
                return (None, false);
            }
            _ => {}
        }

        if let Some(stored) = self.agent_session_id.clone() {
            let stored = match self.capture_freshest_conversation() {
                Some(observation) => {
                    tracing::info!(target: "session.store", stale = %stored, fresh = %observation.sid, tool = %self.tool,
                        "Replacing stored conversation with fresher live observation");
                    self.apply_conversation_observation(&observation);
                    observation.sid
                }
                None => stored,
            };
            // An unwritten Claude pin can be created under the same ID. Uncertain reads preserve resume.
            let absent = backend == Some(crate::agents::SessionCaptureBackend::Claude)
                && execution.map_or_else(
                    || !self.is_sandboxed(),
                    |execution| execution.inputs.container.is_none(),
                )
                && !self.agent_session_binding.as_ref().is_some_and(|binding| {
                    binding.session_id == stored
                        && binding.is_known()
                        && execution.is_some_and(|execution| {
                            binding.execution.as_ref().is_some_and(|bound| {
                                bound.agent == execution.binding.agent
                                    && !Self::execution_identity_matches(bound, &execution.binding)
                            })
                        })
                })
                && match execution {
                    Some(execution) => execution.binding.stores.first().is_some_and(|root| {
                        crate::session::capture::claude_host_transcript_confirmed_absent(
                            execution.inputs.cwd.to_str().unwrap_or(&self.project_path),
                            &stored,
                            &[],
                            Some(root),
                        )
                    }),
                    None => crate::session::capture::claude_host_transcript_confirmed_absent(
                        &self.project_path,
                        &stored,
                        &self.resolved_host_environment(),
                        self.declared_agent_config_dir_for(&self.tool).as_deref(),
                    ),
                };
            if absent {
                tracing::info!(
                    target: "session.store",
                    sid = %stored,
                    "stored Claude sid has no transcript on disk; launching fresh \
                     with --session-id instead of --resume to avoid a certain \
                     resume failure"
                );
                return (Some(stored), false);
            }
            return (Some(stored), true);
        }

        let tmux_exists = self.tmux_session().is_ok_and(|s| s.exists());
        if tmux_exists {
            if let Some(observation) = self.try_retroactive_capture() {
                tracing::info!(target: "session.store",
                    "Retroactive capture found session ID for {}: {}",
                    self.tool,
                    observation.sid
                );
                self.apply_conversation_observation(&observation);
                return (self.agent_session_id.clone(), true);
            }
        }

        let session_id = self.fresh_launch_session_id(backend, mint_fresh_id);

        if let Some(ref id) = session_id {
            tracing::debug!(target: "session.store", "Session ID for {}: {}", self.tool, id);
            self.set_agent_conversation(session_id.clone(), None, None);
        }

        (session_id, false)
    }

    /// Mint the session id for a brand-new launch. Claude and eligible Pi
    /// launches pin a UUID. Direct host OpenCode launches pre-create a session
    /// automatically. A preassign failure returns no id rather than guessing
    /// from the shared store. Other supported backends capture after launch.
    fn fresh_launch_session_id(
        &self,
        backend: Option<crate::agents::SessionCaptureBackend>,
        mint_fresh_id: &dyn Fn(&str) -> Option<String>,
    ) -> Option<String> {
        match backend? {
            crate::agents::SessionCaptureBackend::Claude => Some(generate_session_uuid()),
            crate::agents::SessionCaptureBackend::OpenCode
            | crate::agents::SessionCaptureBackend::Pi => mint_fresh_id(&self.project_path),
            _ => None,
        }
    }

    /// mirrors with the same binary.
    fn opencode_preassign_enabled(&self) -> bool {
        !self.is_sandboxed()
            && crate::session::config::profile_config::resolve_config_or_warn(
                &self.effective_profile(),
            )
            .session
            .opencode_preassign_session_id
            && self.opencode_launch_mirrorable_by_ambient_serve()
    }

    fn opencode_launch_mirrorable_by_ambient_serve(&self) -> bool {
        self.resolved_agent().is_some_and(|agent| {
            agent.name == "opencode" && self.launch_invokes_resolved_agent_directly(agent)
        })
    }

    fn self_heal_row_is_eligible(&self, contended: &HashSet<(String, String)>) -> bool {
        self.agent_session_id.is_none()
            && self.resume_intent.is_default()
            && !matches!(self.status, Status::Deleting | Status::Creating)
            && self.effective_bucket() == SessionBucket::Active
            && !contended.contains(&self.contended_capture_key())
    }

    /// Best-effort backfill of a missing `agent_session_id` from a read-only CLI
    /// command, under a capture lifecycle reservation. Any miss is a silent no-op.
    pub(crate) fn self_heal_session_id(
        &mut self,
        profile: &str,
        contended: &HashSet<(String, String)>,
    ) {
        if !self.self_heal_row_is_eligible(contended) || !self.tmux_alive_cached() {
            return;
        }
        let file_watch = self.resolve_file_watch();
        let ownership: Result<_> = (|| {
            let storage = crate::session::storage::Storage::new(profile, file_watch.clone())?;
            let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&self.id)?;
            let generation = storage.update(|instances, _groups| {
                let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id)
                else {
                    anyhow::bail!("session disappeared before capture");
                };
                if stored.agent_session_id.is_some()
                    || !stored.resume_intent.is_default()
                    || matches!(stored.status, Status::Deleting | Status::Creating)
                    || stored.effective_bucket() != SessionBucket::Active
                {
                    anyhow::bail!("session is no longer eligible for capture");
                }
                stored
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Capture,
                        Self::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )
                    .map_err(|error| anyhow::anyhow!("capture blocked: {error}"))
            })?;
            Ok((storage, lifecycle_lock, generation))
        })();
        let Ok((storage, _lifecycle_lock, generation)) = ownership else {
            return;
        };
        let expected = self.conversation_state();
        let captured = self.try_retroactive_capture();
        let applied = captured.as_ref().is_some_and(|captured| {
            self.resume_probe_failed_sid.as_deref() != Some(captured.sid.as_str())
                && persist_session_to_storage(profile, &self.id, captured, &expected, &file_watch)
                    == SidWrite::Applied
        });
        let released = storage.update(|instances, _groups| {
            let Some(stored) = instances.iter_mut().find(|instance| instance.id == self.id) else {
                return Ok(false);
            };
            Ok(stored
                .release_lifecycle_reservation_if_owned(LifecycleOperation::Capture, generation))
        });
        if !matches!(released, Ok(true)) {
            tracing::warn!(
                target: "session.sync",
                instance = %self.id,
                "self-heal capture lost its lifecycle reservation before release",
            );
            return;
        }
        self.lifecycle_generation = generation;
        self.lifecycle_reservation = None;
        if applied {
            if let Some(observation) = captured {
                self.apply_conversation_observation(&observation);
            }
            self.resume_probe_failed_sid = None;
            tracing::info!(
                target: "session.store",
                instance = %self.id,
                tool = %self.tool,
                "backfilled agent_session_id from a read-only CLI command; \
                 resume is now available without a TUI or daemon",
            );
        }
    }

    /// A newly observed native conversation attributed to this execution.
    pub(crate) fn capture_freshest_conversation(
        &self,
    ) -> Option<crate::session::poller::SessionIdObservation> {
        if self.is_sandboxed()
            && !crate::migrations::v033_isolate_sandbox_content::instance_ready(self).ok()?
        {
            return None;
        }
        let observation = match self.source_capture_backend()? {
            SessionCaptureBackend::Pi => self.pi_published_conversation(false)?,
            SessionCaptureBackend::PrimeAgent => self.prime_published_conversation()?,
            SessionCaptureBackend::Claude | SessionCaptureBackend::HookSidecar => {
                super::execution::hook_session_observation(
                    &self.id,
                    self.active_execution.as_ref(),
                    None,
                )?
            }
            _ => self.try_retroactive_capture()?,
        };
        if self.is_capture_excluded(&observation.sid, observation.source.as_ref()) {
            return None;
        }
        if self.agent_session_id.as_ref() == Some(&observation.sid)
            && self.agent_session_binding == observation.conversation_binding()
            && self.pi_session_path == observation.pi_session_path
        {
            return None;
        }
        Some(observation)
    }

    /// Whether to emit the `existing` resume arm. A Pi id AoE minted takes the
    /// pinning arm instead: `--session-id` recreates a never-prompted conversation
    /// that `--session` would exit 1 on. User pins keep `--session`.
    pub(super) fn resume_flag_arm_is_existing(
        &self,
        execution: Option<&super::execution::NativeExecution>,
        is_existing: bool,
        pi_pinnable: bool,
        session_id: Option<&str>,
        explicitly_pinned: bool,
    ) -> bool {
        let takes_pinning_arm = execution.map_or_else(
            || self.resolved_capture_backend() == Some(SessionCaptureBackend::Pi),
            |execution| execution.agent.name == "pi",
        ) && pi_pinnable
            && !explicitly_pinned
            && session_id.is_some_and(|sid| Uuid::parse_str(sid).is_ok());
        is_existing && !takes_pinning_arm
    }

    /// Why this row can never resume; mirrors what `apply_session_flags` refuses.
    fn terminal_resume_static_unavailable(&self) -> Option<ResumeStaticUnavailable> {
        let Some(agent) = self
            .default_selector_agent()
            .filter(|agent| agent.session_support.is_some())
        else {
            return Some(ResumeStaticUnavailable::Agent);
        };
        if !self.launch_can_carry_resume_selector(agent)
            && !self.can_attempt_default_resume(agent)
            && self.legacy_default_selector_agent().is_none()
        {
            return Some(ResumeStaticUnavailable::Command);
        }
        // Copilot publishes no identity, so only an explicit pin names a sandbox conversation.
        if agent.name == "copilot"
            && self.is_sandboxed()
            && !matches!(self.resume_intent, ResumeIntent::Use(_))
        {
            return Some(ResumeStaticUnavailable::Sandbox);
        }
        None
    }

    pub(crate) fn terminal_context_resume_cached(&self) -> TerminalContextResume {
        self.terminal_context_resume_with_runtime_source(|| {
            self.tmux_session()
                .map(|session| crate::tmux::cached_session_existence(session.name()))
                .unwrap_or(crate::tmux::SessionExistence::Unknown)
        })
    }

    fn terminal_context_resume_with_runtime_source(
        &self,
        runtime_source: impl FnOnce() -> crate::tmux::SessionExistence,
    ) -> TerminalContextResume {
        match (
            self.terminal_resume_static_unavailable(),
            &self.resume_intent,
        ) {
            (Some(ResumeStaticUnavailable::Agent), _) => TerminalContextResume::AgentUnsupported,
            (Some(ResumeStaticUnavailable::Sandbox), _) => {
                TerminalContextResume::SandboxUnsupported
            }
            (Some(ResumeStaticUnavailable::Command), _) => {
                TerminalContextResume::CommandUnsupported
            }
            (None, ResumeIntent::Cleared) => TerminalContextResume::ForcedFresh,
            (None, ResumeIntent::Fork { .. }) => TerminalContextResume::ForkPending,
            (None, ResumeIntent::Use(target)) if !is_valid_session_id(target) => {
                TerminalContextResume::InvalidTarget
            }
            (None, ResumeIntent::Use(_)) => TerminalContextResume::Available,
            (None, ResumeIntent::Default) => {
                if self.agent_session_id.is_some()
                    && self.agent_session_id == self.resume_probe_failed_sid
                {
                    TerminalContextResume::PreviousFailure
                } else if self.agent_session_id.is_some()
                    || !matches!(runtime_source(), crate::tmux::SessionExistence::Absent)
                {
                    TerminalContextResume::RuntimeCheckRequired
                } else {
                    TerminalContextResume::NoTarget
                }
            }
        }
    }

    /// A native session selector the command already carries, if any.
    fn existing_session_selector(&self, words: &[String]) -> Option<String> {
        let agent = self.default_selector_agent()?;
        let strategy = agent.session_support.as_ref()?.resume;
        if words.first() != parse_launch_command(self.get_tool_command())?.words.first() {
            return None;
        }
        let flag_present = |flag: &str| {
            words.iter().skip(1).any(|word| {
                word == flag
                    || word
                        .strip_prefix(flag)
                        .is_some_and(|rest| rest.starts_with('='))
            })
        };
        let claude_selectors = if agent.name == "claude" {
            CLAUDE_SELECTORS
        } else {
            &[]
        };
        if let Some(selector) = claude_selectors.iter().find(|flag| flag_present(flag)) {
            return Some(selector.to_string());
        }
        match strategy {
            crate::agents::ResumeStrategy::Flag(flag) => {
                flag_present(flag).then(|| flag.to_string())
            }
            crate::agents::ResumeStrategy::FlagPair {
                existing,
                new_session,
            } => [existing, new_session]
                .into_iter()
                .find(|flag| flag_present(flag))
                .map(str::to_string),
            crate::agents::ResumeStrategy::Subcommand(subcommand) => words
                .get(1)
                .is_some_and(|word| word == subcommand)
                .then(|| subcommand.to_string()),
        }
    }

    /// Splice resume or fork flags into `cmd`; returns whether this launch resumes.
    pub(super) fn apply_session_flags(
        &mut self,
        cmd: &mut String,
        context: &str,
        agent: Option<&'static crate::agents::AgentDef>,
        execution: Option<&super::execution::NativeExecution>,
    ) -> Result<bool> {
        let agent = execution
            .map(|execution| execution.agent)
            .or(agent)
            .or_else(|| self.default_selector_agent());
        let Some(parsed_command) = parse_launch_command(cmd) else {
            return Ok(false);
        };
        if let Some(selector) = execution
            .is_none()
            .then(|| self.existing_session_selector(&parsed_command.words))
            .flatten()
        {
            let aoe_has_state = self.agent_session_id.is_some()
                || matches!(
                    self.resume_intent,
                    ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
                );
            if aoe_has_state {
                anyhow::bail!(
                    "{context} command already contains native session selector {selector}; remove it or clear the AoE-managed resume state before launching"
                );
            }
            tracing::info!(target: "session.store", %context, %selector,
                "command supplies its own native session selector; skipping AoE session injection");
            return Ok(false);
        }
        if execution.is_none() && !self.supports_native_resume() {
            anyhow::ensure!(
                !matches!(
                    self.resume_intent,
                    ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
                ),
                "explicit conversation operation is unsupported by this launch"
            );
            return Ok(false);
        }
        if let Some(execution) = execution {
            if !execution.namespace_arguments.is_empty() {
                cmd.push(' ');
                cmd.push_str(&shell_words::join(&execution.namespace_arguments));
            }
        }
        if let ResumeIntent::Fork { from } = self.resume_intent.clone() {
            let agent = agent.context("fork execution adapter is unavailable")?;
            let child_id = self
                .agent_session_id
                .as_deref()
                .context("fork child seed is missing")?;
            let fork_part = build_fork_flags(agent.name, &from, child_id);
            anyhow::ensure!(
                !fork_part.is_empty(),
                "native agent cannot execute this fork"
            );
            let is_subcommand =
                matches!(agent.fork_strategy, crate::agents::ForkStrategy::CodexFork);
            splice_subcommand_or_append(
                cmd,
                &fork_part,
                is_subcommand.then_some(parsed_command.executable_end),
            );
            return Ok(false);
        }
        let explicitly_pinned = matches!(self.resume_intent, ResumeIntent::Use(_));
        let (mut session_id, is_existing) = self.acquire_session_id(execution);
        if let Some(path) = execution.and_then(|execution| execution.pi_transcript_path.as_ref()) {
            self.set_agent_conversation(
                session_id.clone(),
                self.agent_session_binding.clone(),
                Some(path.clone()),
            );
            let flags = format!("--session {}", shell_escape(path));
            splice_subcommand_or_append(cmd, &flags, None);
            return Ok(true);
        }
        let flag_arm_is_existing = self.resume_flag_arm_is_existing(
            execution,
            is_existing,
            execution.map_or_else(
                || self.pi_session_id_pinnable(),
                |execution| execution.pi_pinnable,
            ),
            session_id.as_deref(),
            explicitly_pinned,
        );
        let static_unavailable = execution
            .is_none()
            .then(|| self.terminal_resume_static_unavailable())
            .flatten();
        if matches!(static_unavailable, Some(ResumeStaticUnavailable::Command)) {
            tracing::warn!(target: "session.store",
                tool = self.tool.as_str(),
                command = self.command.as_str(),
                "resume selectors need the agent's own argv and this command hides it behind a launcher; starting fresh"
            );
        }
        if matches!(
            static_unavailable,
            Some(ResumeStaticUnavailable::Sandbox | ResumeStaticUnavailable::Command)
        ) {
            session_id = None;
        }
        if execution.is_none() && is_existing && !explicitly_pinned && session_id.is_some() {
            if let Some(path) = self.pi_resumable_transcript() {
                let flags = format!("--session {}", shell_escape(&path));
                splice_subcommand_or_append(cmd, &flags, None);
                tracing::debug!(target: "session.store", "Added resume flags to {} command: {}", context, flags);
                return Ok(true);
            }
        }
        if flag_arm_is_existing
            && !explicitly_pinned
            && session_id.is_some()
            && execution.map_or_else(
                || {
                    self.resolved_capture_backend()
                        == Some(crate::agents::SessionCaptureBackend::Pi)
                        && self.pi_recorded_transcript_missing()
                },
                |execution| execution.agent.name == "pi" && execution.pi_transcript_path.is_none(),
            )
        {
            tracing::info!(
                target: "session.store",
                instance = self.id.as_str(),
                sid = ?session_id,
                "the conversation this Pi session owns has no transcript; \
                 starting fresh rather than failing the launch on `--session`. \
                 The pane's next conversation replaces it",
            );
            session_id = None;
        }
        let resume_tool = agent.map_or(self.tool.as_str(), |agent| agent.name);
        let emitted = append_resume_flags(
            resume_tool,
            session_id.as_deref(),
            flag_arm_is_existing,
            cmd,
            parsed_command.executable_end,
            context,
        );
        anyhow::ensure!(
            !matches!(self.resume_intent, ResumeIntent::Use(_)) || emitted,
            "explicit resume did not produce a native resume selector"
        );
        Ok(is_existing && emitted)
    }

    /// Persist an ambiguous resume-probe failure without clearing the durable
    /// sid; the CAS guard keeps peer sid changes authoritative.
    pub(super) fn mark_resume_probe_failed(&mut self, profile: &str, sid: &str) -> SidWrite {
        let storage =
            match crate::session::storage::Storage::new(profile, self.resolve_file_watch()) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(target: "session.store",
                        "Failed to create storage for resume-probe failure marker for {}: {}",
                        self.id,
                        e
                    );
                    return SidWrite::Failed;
                }
            };

        let outcome = storage.update(|instances, _groups| {
            let Some(inst) = instances.iter_mut().find(|i| i.id == self.id) else {
                return Ok(SidWrite::Failed);
            };
            if inst.agent_session_id.as_deref() != Some(sid) {
                tracing::warn!(target: "session.store",
                    instance_id = %self.id,
                    expected_sid = %sid,
                    disk_sid = ?inst.agent_session_id,
                    "sid CAS mismatch in resume-probe failure marker; skipping write"
                );
                return Ok(SidWrite::Skipped);
            }
            inst.resume_probe_failed_sid = Some(sid.to_string());
            Ok(SidWrite::Applied)
        });

        match outcome {
            Ok(write @ (SidWrite::Applied | SidWrite::Skipped | SidWrite::PinnedForeign)) => {
                if let Some(disk) = storage
                    .load()
                    .ok()
                    .and_then(|insts| insts.into_iter().find(|i| i.id == self.id))
                {
                    self.agent_session_id = disk.agent_session_id;
                    self.resume_intent = disk.resume_intent;
                    self.resume_probe_failed_sid = disk.resume_probe_failed_sid;
                }
                write
            }
            Ok(SidWrite::Failed) => {
                tracing::warn!(target: "session.store",
                    "Resume-probe failure marker found no instance row for {}", self.id);
                SidWrite::Failed
            }
            Err(e) => {
                tracing::warn!(target: "session.store",
                    "Failed to mark resume-probe failure for {}: {}", self.id, e);
                SidWrite::Failed
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::launch_command::build_resume_flags;
    use crate::session::instance::test_helpers::*;
    use crate::session::test_support::EnvGuard;
    use serial_test::serial;
    use tempfile::tempdir;

    #[test]
    fn acquire_reuses_stored_and_pinned_ids_unless_cleared() {
        // (tool, stored, intent, reused id, or whether a cleared launch mints a fresh one)
        for (tool, stored, intent, reused, mints) in [
            (
                "opencode",
                Some("oc-session-42"),
                ResumeIntent::Default,
                Some("oc-session-42"),
                false,
            ),
            (
                "codex",
                Some("sess-99"),
                ResumeIntent::Default,
                Some("sess-99"),
                false,
            ),
            (
                "claude",
                None,
                ResumeIntent::Use("user-pinned".into()),
                Some("user-pinned"),
                false,
            ),
            (
                "claude",
                Some("observed"),
                ResumeIntent::Use("user-pinned".into()),
                Some("user-pinned"),
                false,
            ),
            (
                "claude",
                Some("observed"),
                ResumeIntent::Cleared,
                None,
                true,
            ),
            (
                "opencode",
                Some("observed"),
                ResumeIntent::Cleared,
                None,
                false,
            ),
        ] {
            let mut inst = tool_instance(tool, "/tmp/x");
            inst.agent_session_id = stored.map(str::to_string);
            inst.resume_intent = intent;
            let (sid, existing) = inst.acquire_session_id(None);
            match reused {
                Some(id) => assert_eq!((sid.as_deref(), existing), (Some(id), true), "{tool}"),
                None if mints => {
                    assert!(
                        sid.is_some() && sid.as_deref() != stored && !existing,
                        "{tool}"
                    )
                }
                None => assert_eq!((sid.as_deref(), existing), (None, false), "{tool}"),
            }
            assert_eq!(inst.agent_session_id, sid, "{tool}");
        }
    }

    #[test]
    fn fork_intent_emits_resume_fork_session_and_pins_child() {
        let parent = "parent-1111-2222-3333-444444444444";
        let child = "child-5555-6666-7777-888888888888";
        let mut inst = tool_instance("claude", "/tmp/x");
        inst.agent_session_id = Some(child.to_string());
        inst.resume_intent = ResumeIntent::Fork {
            from: parent.to_string(),
        };
        let mut cmd = "claude".to_string();
        assert!(!inst
            .apply_session_flags(&mut cmd, "test", crate::agents::get_agent("claude"), None,)
            .unwrap());
        assert_eq!(
            cmd,
            format!("claude --resume {parent} --fork-session --session-id {child}")
        );
        assert_eq!(inst.agent_session_id.as_deref(), Some(child));
    }

    #[test]
    fn fresh_claude_launch_mints_a_stable_pinned_id() {
        let mut inst = tool_instance("claude", "/tmp/test");
        let (first, first_existing) = inst.acquire_session_id(None);
        assert!(first.is_some() && !first_existing);
        assert_eq!(inst.agent_session_id, first);
        // With no transcript on disk the same id stays fresh-pinned.
        assert_eq!(inst.acquire_session_id(None), (first, false));

        let mut fresh = tool_instance("claude", "/tmp/test");
        let mut cmd = String::from("claude");
        assert!(!fresh
            .apply_session_flags(&mut cmd, "test", None, None)
            .unwrap());
        fresh.resume_intent = ResumeIntent::Use("019342ab-1234-7def-8901-abcdef012345".into());
        let mut cmd = String::from("claude");
        assert!(fresh
            .apply_session_flags(&mut cmd, "test", None, None)
            .unwrap());
    }

    #[test]
    #[serial]
    fn default_bare_alias_pins_and_resumes_without_execution_attestation() {
        const PROFILE: &str = "legacy-default-claude-wrapper";
        let root = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&root.path().join("app"));
        let profile_path =
            crate::session::config::profile_config::get_profile_config_path(PROFILE).unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::write(
            profile_path,
            "[session.agent_detect_as]\nwork-claude = \"claude\"\n",
        )
        .unwrap();
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(PROFILE);
        let _home = EnvGuard::set(&[
            ("HOME", root.path().to_path_buf()),
            ("CLAUDE_CONFIG_DIR", root.path().join(".claude")),
        ]);
        let mut inst = tool_instance("work-claude", root.path().to_str().unwrap());
        inst.source_profile = PROFILE.into();
        inst.command = "work-claude".into();
        assert!(inst.execution_agent().is_err());
        let mut first = inst.command.clone();
        assert!(!inst
            .apply_session_flags(&mut first, "test", None, None)
            .unwrap());
        let sid = inst.agent_session_id.clone().expect("fresh Claude pin");
        assert_eq!(first, format!("work-claude --session-id {sid}"));

        let canonical_project = std::fs::canonicalize(&inst.project_path).unwrap();
        let encoded = crate::session::capture::encode_claude_project_path(
            &canonical_project.to_string_lossy(),
        );
        let project = root.path().join(".claude/projects").join(encoded);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{sid}.jsonl")), "{}\n").unwrap();
        let mut resumed = inst.command.clone();
        assert!(inst
            .apply_session_flags(&mut resumed, "test", None, None)
            .unwrap());
        assert_eq!(resumed, format!("work-claude --resume {sid}"));
        for intent in [
            ResumeIntent::Use(sid.clone()),
            ResumeIntent::Fork { from: sid.clone() },
        ] {
            inst.resume_intent = intent;
            let mut command = inst.command.clone();
            assert!(inst
                .apply_session_flags(&mut command, "test", None, None)
                .is_err());
            assert_eq!(command, "work-claude");
        }
        inst.resume_intent = ResumeIntent::Default;
        for unsafe_command in [
            "ssh -t host claude",
            "/opt/work-claude",
            "work-claude | cat",
            "work-claude --",
        ] {
            inst.command = unsafe_command.into();
            assert!(!inst.supports_native_resume(), "{unsafe_command}");
            let mut command = unsafe_command.to_string();
            assert!(!inst
                .apply_session_flags(&mut command, "test", None, None)
                .unwrap());
            assert_eq!(command, unsafe_command);
        }
        inst.command = "codex".into();
        assert!(inst.legacy_default_selector_agent().is_none());
        inst.command = "work-claude".into();
        inst.extra_args = "--resume external".into();
        let mut conflict = "work-claude --resume external".to_string();
        assert!(inst
            .apply_session_flags(&mut conflict, "test", None, None)
            .is_err());
        let mut launched = tool_instance("work-claude", root.path().to_str().unwrap());
        launched.source_profile = PROFILE.into();
        launched.command = "work-claude".into();
        let fresh = launched
            .prepare_launch_command(launched.conversation_state())
            .unwrap();
        let launched_sid = launched.agent_session_id.clone().unwrap();
        assert!(!fresh.is_existing);
        assert!(fresh
            .command
            .unwrap()
            .contains(&format!("work-claude --session-id {launched_sid}")));
        std::fs::write(project.join(format!("{launched_sid}.jsonl")), "{}\n").unwrap();
        let restart = launched
            .prepare_launch_command(launched.conversation_state())
            .unwrap();
        assert!(restart.is_existing);
        assert!(restart
            .command
            .unwrap()
            .contains(&format!("work-claude --resume {launched_sid}")));
    }

    #[test]
    #[serial(hook_base)]
    fn default_detect_as_wrappers_capture_published_conversations() {
        const PROFILE: &str = "default-wrapper-capture";
        const SID: &str = "11111111-2222-4333-8444-555555555555";
        let root = tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&root.path().join("app"));
        let profile_path =
            crate::session::config::profile_config::get_profile_config_path(PROFILE).unwrap();
        std::fs::create_dir_all(profile_path.parent().unwrap()).unwrap();
        std::fs::write(
            profile_path,
            r#"[session.agent_detect_as]
work-claude = "claude"
work-cursor = "cursor"
work-pi = "pi"
work-opencode = "opencode"
"#,
        )
        .unwrap();
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(PROFILE);
        let _home = EnvGuard::set(&[
            ("HOME", root.path().to_path_buf()),
            ("CLAUDE_CONFIG_DIR", root.path().join(".claude")),
        ]);
        let (_hooks, _base, _hook_temp) = crate::hooks::test_support::BaseGuard::ready();
        let project = std::fs::canonicalize(root.path()).unwrap();
        let claude_project = root.path().join(".claude/projects").join(
            crate::session::capture::encode_claude_project_path(&project.to_string_lossy()),
        );
        std::fs::create_dir_all(&claude_project).unwrap();
        std::fs::write(claude_project.join(format!("{SID}.jsonl")), "{}\n").unwrap();
        let pi_path = root
            .path()
            .join("pi")
            .join(format!("2026-01-01T00-00-00-000Z_{SID}.jsonl"));
        std::fs::create_dir_all(pi_path.parent().unwrap()).unwrap();

        for tool in ["work-claude", "work-cursor", "work-pi"] {
            let mut inst = tool_instance(tool, root.path().to_str().unwrap());
            inst.source_profile = PROFILE.into();
            inst.command = tool.into();
            inst.agent_session_id = Some("aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa".into());
            assert!(inst.execution_agent().is_err(), "{tool}");
            let sidecar = write_sidecar(&inst.id, SID);
            if tool == "work-pi" {
                std::fs::write(sidecar.join("session_path"), pi_path.to_str().unwrap()).unwrap();
            }
            assert!(inst.supports_session_poller(), "{tool} lost its publisher");
            assert_eq!(
                inst.acquire_session_id(None),
                (Some(SID.into()), true),
                "{tool}"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(SID), "{tool}");
            if tool == "work-claude" {
                let mut command = tool.to_string();
                assert!(inst
                    .apply_session_flags(&mut command, "test", None, None)
                    .unwrap());
                assert_eq!(command, format!("{tool} --resume {SID}"));
            }
            if tool == "work-pi" && crate::agents::pi_supports_session_id_flag() {
                let mut fresh = tool_instance(tool, root.path().to_str().unwrap());
                fresh.source_profile = PROFILE.into();
                fresh.command = tool.into();
                let mut command = tool.to_string();
                assert!(!fresh
                    .apply_session_flags(&mut command, "test", None, None)
                    .unwrap());
                assert!(
                    command.starts_with(&format!("{tool} --session-id ")),
                    "{command}"
                );
            }
        }

        let mut explicit = tool_instance("work-claude", root.path().to_str().unwrap());
        explicit.source_profile = PROFILE.into();
        explicit.command = "work-claude".into();
        for intent in [
            ResumeIntent::Use(SID.into()),
            ResumeIntent::Fork { from: SID.into() },
        ] {
            explicit.resume_intent = intent;
            assert!(
                !explicit.supports_session_poller(),
                "Use/Fork cannot trust a status alias"
            );
        }
        explicit.resume_intent = ResumeIntent::Default;
        explicit.extra_args = "--setting-sources project".into();
        assert!(!explicit.hook_session_publisher_allowed_by_argv());

        let mut shared = tool_instance("work-opencode", root.path().to_str().unwrap());
        shared.source_profile = PROFILE.into();
        shared.command = "work-opencode".into();
        assert!(shared.execution_agent().is_err());
        assert!(
            shared.resolved_session_support().is_none(),
            "an unscoped store cannot be claimed through detect_as"
        );
    }

    #[test]
    fn fresh_launch_mint_seam_is_used_only_by_opencode_and_pi() {
        for (tool, intent, minted, expected) in [
            (
                "opencode",
                ResumeIntent::Default,
                Some("ses_preassigned"),
                Some("ses_preassigned"),
            ),
            ("opencode", ResumeIntent::Default, None, None),
            (
                "opencode",
                ResumeIntent::Cleared,
                Some("ses_cleared"),
                Some("ses_cleared"),
            ),
        ] {
            let mut inst = tool_instance(tool, "/tmp/test");
            inst.resume_intent = intent;
            let result = inst.acquire_session_id_with(None, &|_| minted.map(str::to_string));
            assert_eq!(result, (expected.map(str::to_string), false));
            assert_eq!(inst.agent_session_id.as_deref(), expected);
        }
        let mut claude = tool_instance("claude", "/tmp/test");
        let (claude_sid, _) =
            claude.acquire_session_id_with(None, &|_| panic!("seam ran for claude"));
        assert!(claude_sid.is_some());
        let mut codex = tool_instance("codex", "/tmp/test");
        assert_eq!(
            codex.acquire_session_id_with(None, &|_| panic!("seam ran for codex")),
            (None, false)
        );
    }

    #[test]
    fn opencode_preassign_requires_a_mirrorable_launch() {
        let mut inst = tool_instance("opencode", "/tmp/test");
        assert!(inst.opencode_launch_mirrorable_by_ambient_serve());
        inst.command = "opencode-wrapper".to_string();
        assert!(!inst.opencode_launch_mirrorable_by_ambient_serve());
    }

    #[test]
    fn self_heal_eligibility_rejects_owned_and_inactive_rows() {
        let base = Instance::new("self-heal", "/tmp/self-heal");
        let empty = HashSet::new();
        assert!(base.self_heal_row_is_eligible(&empty));

        let mut with_id = base.clone();
        with_id.agent_session_id = Some("native-id".to_string());
        let mut cleared = base.clone();
        cleared.resume_intent = ResumeIntent::Cleared;
        let mut deleting = base.clone();
        deleting.status = Status::Deleting;
        let mut creating = base.clone();
        creating.status = Status::Creating;
        let mut archived = base.clone();
        archived.archived_at = Some(Utc::now());

        for (reason, instance) in [
            ("stored identity", with_id),
            ("cleared intent", cleared),
            ("deleting", deleting),
            ("creating", creating),
            ("archived", archived),
        ] {
            assert!(
                !instance.self_heal_row_is_eligible(&empty),
                "self-heal accepted {reason}"
            );
        }

        let contended = HashSet::from([base.contended_capture_key()]);
        assert!(!base.self_heal_row_is_eligible(&contended));
    }

    #[test]
    #[serial_test::serial]
    fn persisted_native_id_roundtrip_resumes_the_same_conversation() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let claude_home = temp.path().join(".claude");
        let _claude = crate::session::test_support::EnvGuard::set(&[(
            "CLAUDE_CONFIG_DIR",
            claude_home.clone(),
        )]);
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let project = std::fs::canonicalize(project).unwrap();
        let native_id = "11111111-2222-4333-8444-555555555555";
        let transcript = claude_home
            .join("projects")
            .join(crate::session::capture::encode_claude_project_path(
                &project.to_string_lossy(),
            ))
            .join(format!("{native_id}.jsonl"));
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(transcript, "conversation").unwrap();

        let mut inst = tool_instance("claude", &project.to_string_lossy());
        inst.agent_session_id = Some(native_id.to_string());
        let json = serde_json::to_string(&inst).unwrap();
        let mut reloaded: Instance = serde_json::from_str(&json).unwrap();

        let (session_id, is_existing) = reloaded.acquire_session_id(None);
        assert_eq!(session_id.as_deref(), Some(native_id));
        assert!(is_existing);
        assert_eq!(
            crate::session::instance::launch_command::build_resume_flags(
                "claude",
                session_id.as_deref().unwrap(),
                is_existing,
            ),
            format!("--resume {native_id}")
        );
    }

    #[test]
    fn sandbox_resume_flags_follow_capture_context_support() {
        let sid = "11111111-2222-3333-4444-555555555555";
        for (tool, expected, resumed) in [
            ("copilot", format!("copilot --session-id {sid}"), true),
            ("kimi", format!("kimi --session {sid}"), true),
            ("prime-agent", format!("prime-agent --resume {sid}"), true),
        ] {
            let mut inst = Instance::new("test", "/tmp/test");
            inst.tool = tool.to_string();
            inst.agent_session_id = Some(sid.to_string());
            inst.resume_intent = ResumeIntent::Use(sid.to_string());
            inst.sandbox_info = Some(test_sandbox("test", None));
            let mut cmd = tool.to_string();
            assert_eq!(
                inst.apply_session_flags(&mut cmd, "test", crate::agents::get_agent(tool), None,)
                    .unwrap(),
                resumed,
                "{tool}"
            );
            assert_eq!(cmd, expected, "{tool}");
            assert_eq!(inst.agent_session_id.as_deref(), Some(sid));
        }

        let mut automatic_copilot = tool_instance("copilot", "/tmp/test");
        automatic_copilot.agent_session_id = Some(sid.to_string());
        automatic_copilot.sandbox_info = Some(test_sandbox("test", None));
        let mut automatic_cmd = "copilot".to_string();
        assert!(!automatic_copilot
            .apply_session_flags(&mut automatic_cmd, "test", None, None)
            .unwrap());
        assert_eq!(automatic_cmd, "copilot");

        let mut host_prime = tool_instance("prime-agent", "/tmp/test");
        host_prime.agent_session_id = Some(sid.to_string());
        host_prime.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut cmd = "prime-agent".to_string();
        assert!(host_prime
            .apply_session_flags(&mut cmd, "test", None, None)
            .unwrap());
        assert_eq!(cmd, format!("prime-agent --resume {sid}"));
    }

    #[test]
    #[serial]
    fn opencode_preassign_requires_profile_opt_in() {
        let temp = tempdir().unwrap();
        let _home = crate::session::test_support::isolate_home(temp.path());
        let cases = [
            ("opencode-preassign-off", false),
            ("opencode-preassign-on", true),
        ];

        for (profile, enabled) in cases {
            let config_path = crate::session::get_profile_dir_path(profile)
                .unwrap()
                .join("config.toml");
            std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            std::fs::write(
                config_path,
                format!(
                    "environment = [\"OPENCODE_CONFIG_DIR=/tmp/opencode-test\"]\n[session]\nopencode_preassign_session_id = {enabled}\n"
                ),
            )
            .unwrap();

            let mut inst = Instance::new("Test", "/tmp/test");
            inst.source_profile = profile.to_string();
            inst.tool = "opencode".to_string();
            assert_eq!(inst.opencode_preassign_enabled(), enabled);
        }
    }

    /// A resume subcommand must land right after the program the pane runs; a launcher or a
    /// path-qualified script hides it, while a bare renamed wrapper is that program (#3638).
    #[test]
    fn codex_wrapper_command_never_takes_a_spliced_subcommand() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        const PROFILE: &str = "codex-wrapper-splice-test";
        let _registry = install_aliases(PROFILE, &[("codex-remote", "codex")]);
        crate::session::instance::test_helpers::declare_execution_aliases(
            PROFILE,
            &[("codex-remote", "codex")],
            home.path(),
        );
        let sid = "11111111-2222-3333-4444-555555555555";

        // The documented custom-agent shape: a multi-token launcher.
        let mut wrapped = Instance::new("wrapper", "/tmp/codex-splice");
        wrapped.source_profile = PROFILE.to_string();
        wrapped.tool = "codex-remote".to_string();
        wrapped.command = "ssh -t lenovo codex".to_string();
        wrapped.agent_session_id = Some(sid.to_string());
        wrapped.resume_intent = ResumeIntent::Use(sid.to_string());
        assert_eq!(
            wrapped.terminal_context_resume_cached(),
            TerminalContextResume::CommandUnsupported
        );
        let mut cmd = wrapped.command.clone();
        assert!(wrapped
            .apply_session_flags(&mut cmd, "test", wrapped.resolved_agent(), None)
            .is_err());
        assert_eq!(
            cmd, "ssh -t lenovo codex",
            "the resume token must not be spliced onto the launcher"
        );

        // An override that does open with the binary keeps its resume.
        let mut direct = Instance::new("direct", "/tmp/codex-splice");
        direct.source_profile = PROFILE.to_string();
        direct.tool = "codex-remote".to_string();
        direct.command = "codex --model o3".to_string();
        direct.agent_session_id = Some(sid.to_string());
        direct.resume_intent = ResumeIntent::Use(sid.to_string());
        assert_eq!(
            direct.terminal_context_resume_cached(),
            TerminalContextResume::Available
        );
        let mut direct_cmd = direct.command.clone();
        assert!(direct
            .apply_session_flags(&mut direct_cmd, "test", direct.resolved_agent(), None)
            .unwrap());
        assert_eq!(direct_cmd, format!("codex resume {sid} --model o3"));

        let mut qualified = Instance::new("qualified", "/tmp/codex-splice");
        qualified.source_profile = "codex-wrapper-unproven".into();
        qualified.tool = "codex-remote".to_string();
        qualified.command = "/opt/bin/mycodex".to_string();
        qualified.agent_session_id = Some(sid.to_string());
        qualified.resume_intent = ResumeIntent::Use(sid.to_string());
        assert_eq!(
            qualified.terminal_context_resume_cached(),
            TerminalContextResume::AgentUnsupported
        );
        let mut qualified_cmd = qualified.command.clone();
        assert!(qualified
            .apply_session_flags(&mut qualified_cmd, "test", qualified.resolved_agent(), None)
            .is_err());
        assert_eq!(qualified_cmd, "/opt/bin/mycodex");

        for command in ["mycodex", "codex-personal"] {
            let mut bare = Instance::new("bare", "/tmp/codex-splice");
            bare.source_profile = PROFILE.to_string();
            bare.tool = "codex-remote".to_string();
            bare.command = command.to_string();
            bare.agent_session_id = Some(sid.to_string());
            bare.resume_intent = ResumeIntent::Use(sid.to_string());
            assert_eq!(
                bare.terminal_context_resume_cached(),
                TerminalContextResume::Available,
                "{command}"
            );
            let mut bare_cmd = bare.command.clone();
            assert!(
                bare.apply_session_flags(&mut bare_cmd, "test", bare.resolved_agent(), None)
                    .unwrap(),
                "{command}"
            );
            assert_eq!(bare_cmd, format!("{command} resume {sid}"));
        }
    }

    #[test]
    fn unsupported_context_without_identity_neither_resumes_nor_polls() {
        let mut inst = tool_instance("codex", "/tmp/test");
        assert_eq!(inst.acquire_session_id_with(None, &|_| None), (None, false));
        assert_eq!(inst.agent_session_id, None);
        let mut cmd = String::from("codex");
        assert!(!inst
            .apply_session_flags(&mut cmd, "test", None, None)
            .unwrap());
        assert_eq!(cmd, "codex");
        inst.capture_started_at = Some(std::time::SystemTime::now());
        inst.maybe_start_poller_since(None);
        assert!(inst.session_id_poller.is_none());
    }

    /// A command's own session selector skips AoE injection, and conflicts with AoE state.
    #[test]
    fn external_session_selectors_skip_injection_or_conflict_with_managed_state() {
        let sid = "11111111-2222-3333-4444-555555555555";
        for command in [
            "claude --resume external",
            "claude --resume=external",
            "claude --session-id=external",
            "claude -c",
            "claude --continue",
            "claude -r external",
            "claude --from-pr=3678",
            "claude --from-pr 3678",
            "claude --teleport external",
            "claude --cloud external",
            "claude --remote external",
            "claude --fork-session",
        ] {
            let mut inst = tool_instance("claude", "/tmp/x");
            let mut actual = command.to_string();
            assert!(!inst
                .apply_session_flags(&mut actual, "test", None, None)
                .unwrap());
            assert_eq!(actual, command);
            assert!(inst.agent_session_id.is_none());
            for (stored, intent) in [
                (Some(sid.to_string()), ResumeIntent::Default),
                (None, ResumeIntent::Use(sid.to_string())),
                (
                    Some("child-session".to_string()),
                    ResumeIntent::Fork {
                        from: sid.to_string(),
                    },
                ),
            ] {
                let mut managed = tool_instance("claude", "/tmp/x");
                managed.agent_session_id = stored;
                managed.resume_intent = intent;
                let error = managed
                    .apply_session_flags(&mut command.to_string(), "test", None, None)
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains("already contains native session selector"),
                    "{command}: {error}"
                );
                assert!(error.contains("clear the AoE-managed resume state"));
            }
        }
    }

    #[test]
    fn codex_selector_requires_subcommand_position_and_resolves_alias() {
        let mut external = tool_instance("codex", "/tmp/x");
        let mut command = "codex resume external".to_string();
        assert!(!external
            .apply_session_flags(&mut command, "test", None, None)
            .unwrap());
        assert_eq!(command, "codex resume external");

        let sid = "11111111-2222-3333-4444-555555555555";
        let mut value_token = tool_instance("codex", "/tmp/x");
        value_token.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut command = "codex --model resume".to_string();
        assert!(value_token
            .apply_session_flags(&mut command, "test", value_token.resolved_agent(), None)
            .unwrap());
        assert_eq!(command, format!("codex resume {sid} --model resume"));

        const PROFILE: &str = "selector-detect-as-alias";
        let _registry = install_aliases(PROFILE, &[("work-claude", "claude")]);
        let mut alias = Instance::new("alias", "/tmp/x");
        alias.source_profile = PROFILE.to_string();
        alias.tool = "work-claude".to_string();
        alias.command = "claude --resume external".to_string();
        let mut command = alias.command.clone();
        assert!(!alias
            .apply_session_flags(&mut command, "test", alias.resolved_agent(), None)
            .unwrap());
        assert!(alias.agent_session_id.is_none());
    }

    #[test]
    fn terminal_context_resume_matches_emission_constraints() {
        let sid = "44444444-4444-4444-8444-444444444444".to_string();
        let mut inst = tool_instance("claude", "/tmp/context");
        assert_eq!(
            inst.terminal_context_resume_with_runtime_source(|| {
                crate::tmux::SessionExistence::Absent
            }),
            TerminalContextResume::NoTarget
        );
        assert_eq!(
            inst.terminal_context_resume_with_runtime_source(|| {
                crate::tmux::SessionExistence::Present
            }),
            TerminalContextResume::RuntimeCheckRequired
        );
        assert_eq!(
            inst.terminal_context_resume_with_runtime_source(|| {
                crate::tmux::SessionExistence::Unknown
            }),
            TerminalContextResume::RuntimeCheckRequired
        );
        inst.agent_session_id = Some(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::RuntimeCheckRequired
        );
        inst.resume_intent = ResumeIntent::Use(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::Available
        );
        inst.resume_probe_failed_sid = Some(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::Available
        );
        inst.resume_intent = ResumeIntent::Default;
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::PreviousFailure
        );
        inst.resume_intent = ResumeIntent::Use("bad target".to_string());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::InvalidTarget
        );
        let mut command = "claude".to_string();
        assert!(inst
            .apply_session_flags(&mut command, "test", inst.resolved_agent(), None)
            .is_err());
        assert_eq!(command, "claude");
        inst.resume_intent = ResumeIntent::Cleared;
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::ForcedFresh
        );
        inst.resume_intent = ResumeIntent::Fork { from: sid.clone() };
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::ForkPending
        );

        inst.tool = "droid".to_string();
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::AgentUnsupported
        );

        let sandbox = Some(test_sandbox("test", None));

        inst.tool = "copilot".to_string();
        inst.sandbox_info = sandbox.clone();
        inst.resume_intent = ResumeIntent::Default;
        for target in [None, Some(sid.clone())] {
            inst.agent_session_id = target;
            assert_eq!(
                inst.terminal_context_resume_cached(),
                TerminalContextResume::SandboxUnsupported,
                "an automatic copilot id must start fresh in a sandbox"
            );
        }
        inst.resume_intent = ResumeIntent::Use(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::Available,
            "a pinned copilot conversation must still be attempted"
        );
        let mut pinned = "copilot".to_string();
        assert!(inst
            .apply_session_flags(&mut pinned, "test", None, None)
            .unwrap());
        assert_eq!(pinned, format!("copilot --session-id {sid}"));

        for tool in ["kimi", "prime-agent"] {
            inst.tool = tool.to_string();
            inst.sandbox_info = sandbox.clone();
            inst.resume_intent = ResumeIntent::Use(sid.clone());
            inst.agent_session_id = Some(sid.clone());
            assert_eq!(
                inst.terminal_context_resume_cached(),
                TerminalContextResume::Available,
                "{tool} has a private sandbox store to resume from"
            );
        }
    }

    mod verify_on_resume {
        use super::*;
        use crate::session::capture::encode_claude_project_path;
        use std::fs;
        use std::path::{Path, PathBuf};
        use std::time::{Duration, SystemTime};
        use tempfile::{tempdir, TempDir};

        fn claude_home_guard(temp: &TempDir) -> EnvGuard {
            let mut pairs: Vec<(&'static str, PathBuf)> = vec![("HOME", temp.path().to_path_buf())];
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            pairs.push(("XDG_CONFIG_HOME", temp.path().join(".config")));
            pairs.push(("CLAUDE_CONFIG_DIR", temp.path().join(".claude")));
            EnvGuard::set(&pairs)
        }

        fn write_transcript(claude_home: &Path, project_path: &str, sid: &str, age_secs: u64) {
            let dir = claude_home
                .join("projects")
                .join(encode_claude_project_path(project_path));
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{sid}.jsonl"));
            fs::write(&path, "").unwrap();
            fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(SystemTime::now() - Duration::from_secs(age_secs)),
                )
                .unwrap();
        }

        /// Default intent: the pane's sidecar outranks the stored id and peer transcripts, and a
        /// host Claude id with no transcript relaunches pinned instead of a doomed `--resume`.
        #[test]
        #[serial]
        fn acquire_reconciles_stored_id_with_sidecar_and_transcript() {
            const A: &str = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
            const B: &str = "bbbbbbbb-2222-4222-8222-bbbbbbbbbbbb";
            const C: &str = "cccccccc-3333-4333-8333-cccccccccccc";
            type Case<'a> = (
                &'a str,
                &'a str,
                bool,
                &'a [(&'a str, u64)],
                Option<&'a str>,
                &'a str,
                &'a str,
                bool,
            );
            let cases: [Case; 7] = [
                (
                    "sidecar rotation supersedes stored",
                    "claude",
                    false,
                    &[(B, 0)],
                    Some(B),
                    A,
                    B,
                    true,
                ),
                (
                    "observed sid without transcript",
                    "claude",
                    false,
                    &[(A, 120)],
                    Some(B),
                    A,
                    B,
                    false,
                ),
                (
                    "stored sid without transcript",
                    "claude",
                    false,
                    &[],
                    None,
                    C,
                    C,
                    false,
                ),
                (
                    "idle transcript still resumes",
                    "claude",
                    false,
                    &[(C, 3600)],
                    None,
                    C,
                    C,
                    true,
                ),
                (
                    "unsupported tool keeps stored id",
                    "cursor",
                    false,
                    &[],
                    None,
                    "stored-cursor-sid",
                    "stored-cursor-sid",
                    true,
                ),
                (
                    "sidecar wins over fresher peer",
                    "claude",
                    false,
                    &[(A, 120), (B, 5)],
                    Some(A),
                    C,
                    A,
                    true,
                ),
                (
                    "sandboxed claude ignores unproven host sidecar",
                    "claude",
                    true,
                    &[(A, 120), (B, 5)],
                    Some(A),
                    C,
                    C,
                    true,
                ),
            ];
            for (label, tool, sandboxed, transcripts, sidecar, stored, want_sid, want_existing) in
                cases
            {
                let temp = tempdir().unwrap();
                let _home = claude_home_guard(&temp);
                let (_hooks, _base, _hook_temp) = crate::hooks::test_support::BaseGuard::ready();
                let project_path = "/tmp/aoe-test-verify-on-resume";
                for (sid, age) in transcripts {
                    write_transcript(&temp.path().join(".claude"), project_path, sid, *age);
                }
                let mut inst = tool_instance(tool, project_path);
                inst.agent_session_id = Some(stored.to_string());
                if sandboxed {
                    inst.sandbox_info = Some(test_sandbox("verify-sandbox", None));
                }
                let dir = sidecar.map(|sid| write_sidecar(&inst.id, sid));
                let acquired = inst.acquire_session_id(None);
                if let Some(dir) = dir {
                    fs::remove_dir_all(dir).ok();
                }
                assert_eq!(
                    acquired,
                    (Some(want_sid.to_string()), want_existing),
                    "{label}"
                );
                assert_eq!(inst.agent_session_id.as_deref(), Some(want_sid), "{label}");
            }
        }

        #[test]
        #[serial]
        fn idle_sidecar_still_overrides_stored_identity() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);
            let mut inst = Instance::new("idle-sidecar", "/tmp/idle-sidecar");
            inst.tool = "claude".to_string();
            inst.agent_session_id = Some("stored-old".to_string());
            inst.resume_intent = ResumeIntent::Default;

            let dir = super::write_sidecar(&inst.id, "published-new");
            let stale = SystemTime::now() - Duration::from_secs(10 * 60);
            std::fs::File::options()
                .write(true)
                .open(dir.join("session_id"))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(stale))
                .unwrap();

            assert_eq!(
                inst.capture_freshest_conversation()
                    .map(|observation| observation.sid)
                    .as_deref(),
                Some("published-new")
            );
            std::fs::remove_dir_all(dir).ok();
        }

        /// Same-cwd sessions in profiles with different `CLAUDE_CONFIG_DIR`s each resume their
        /// own conversation, and a `before_session`-minted value wins over the profile's (#3399).
        #[test]
        #[serial]
        fn same_cwd_sessions_resume_their_own_profile_scoped_conversation() {
            let temp = tempdir().unwrap();
            let _guard = claude_home_guard(&temp);
            let project_path = "/tmp/aoe-test-3399-shared-cwd";
            let cases = [
                ("aoe-3399-personal", "11111111-1111-4111-8111-111111111111"),
                ("aoe-3399-work", "22222222-2222-4222-8222-222222222222"),
            ];
            let home_for = |profile: &str| temp.path().join(format!(".claude-{profile}"));
            for (profile, sid) in cases {
                write_transcript(&home_for(profile), project_path, sid, 3600);
                let config_path = crate::session::get_profile_dir_path(profile)
                    .unwrap()
                    .join("config.toml");
                fs::create_dir_all(config_path.parent().unwrap()).unwrap();
                let config = format!(
                    "environment = [\"CLAUDE_CONFIG_DIR={}\"]\n",
                    home_for(profile).display()
                );
                fs::write(&config_path, config).unwrap();
            }
            let acquire = |profile: &str, sid: &str, minted: Option<&str>| {
                let mut inst = tool_instance("claude", project_path);
                inst.source_profile = profile.to_string();
                inst.agent_session_id = Some(sid.to_string());
                inst.pending_host_env = minted
                    .map(|p| {
                        vec![(
                            "CLAUDE_CONFIG_DIR".to_string(),
                            home_for(p).to_string_lossy().into_owned(),
                        )]
                    })
                    .unwrap_or_default();
                inst.acquire_session_id(None)
            };
            for (profile, sid) in cases {
                assert_eq!(
                    acquire(profile, sid, None),
                    (Some(sid.to_string()), true),
                    "{profile}"
                );
            }
            let (shadowed, (minted, other_sid)) = (cases[0].0, cases[1]);
            assert_eq!(
                acquire(shadowed, other_sid, Some(minted)),
                (Some(other_sid.to_string()), true)
            );
        }

        #[test]
        fn pi_fresh_launch_pins_the_minted_id() {
            let pinned = "aaaaaaaa-1111-4111-8111-aaaaaaaaaaaa";
            let mut inst = tool_instance("pi", "/tmp/pi-fresh");
            let acquired = inst.acquire_session_id_with(None, &|_| Some(pinned.to_string()));
            assert_eq!(acquired, (Some(pinned.to_string()), false));
            assert_eq!(inst.agent_session_id.as_deref(), Some(pinned));
            assert_eq!(
                build_resume_flags("pi", pinned, false),
                format!("--session-id {pinned}")
            );

            let mut unpinnable = tool_instance("pi", "/tmp/pi-fresh");
            assert_eq!(
                unpinnable.acquire_session_id_with(None, &|_| None),
                (None, false)
            );
            assert_eq!(unpinnable.agent_session_id, None);
        }
    }
}
