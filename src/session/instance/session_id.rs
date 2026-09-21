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

impl Instance {
    /// Returns `(session_id, is_existing)`. Explicit intents win; default intent
    /// keeps a stored id unless the pane's own backend proves it rotated, and
    /// otherwise captures only from a live pane through the declared backend.
    pub fn acquire_session_id(&mut self) -> (Option<String>, bool) {
        // Decided here so the config read and binary probe stay off other launches.
        let preassign = self.resolved_capture_backend() == Some(SessionCaptureBackend::OpenCode)
            && self.opencode_preassign_enabled();
        let pin_pi = self.pi_session_id_pinnable();
        let preassign_environment = preassign.then(|| self.resolved_host_environment());
        self.acquire_session_id_with(&|path| {
            if pin_pi {
                return Some(generate_session_uuid());
            }
            preassign_environment.as_deref().and_then(|environment| {
                crate::session::capture::preassign_opencode_session_id(path, environment)
            })
        })
    }

    /// `mint_fresh_id` is the pre-mint seam for OpenCode preassignment and Pi pins.
    pub(super) fn acquire_session_id_with(
        &mut self,
        mint_fresh_id: &dyn Fn(&str) -> Option<String>,
    ) -> (Option<String>, bool) {
        match self.resume_intent.clone() {
            ResumeIntent::Use(sid) => {
                self.agent_session_id = Some(sid.clone());
                return (Some(sid), true);
            }
            ResumeIntent::Cleared => {
                self.agent_session_id = None;
                self.resume_probe_failed_sid = None;
                self.pi_session_path = None;
                let session_id = self.fresh_launch_session_id(mint_fresh_id);
                if session_id.is_some() {
                    self.agent_session_id = session_id.clone();
                }
                return (session_id, false);
            }
            // The pre-pinned child id; a fork starts a new session.
            ResumeIntent::Fork { .. } => return (self.agent_session_id.clone(), false),
            ResumeIntent::Default => {}
        }

        match self.attributable_prime_root() {
            Some(Some(id)) => {
                self.agent_session_id = Some(id.clone());
                return (Some(id), true);
            }
            Some(None) => {
                self.agent_session_id = None;
                self.resume_probe_failed_sid = None;
                return (None, false);
            }
            None => {}
        }

        if let Some(stored) = self.agent_session_id.clone() {
            let stored = match self.capture_freshest_session_id() {
                Some(fresh) => {
                    tracing::info!(
                        target: "session.store",
                        stale = %stored,
                        fresh = %fresh,
                        tool = %self.tool,
                        "Replacing stored session id with fresher live observation"
                    );
                    self.agent_session_id = Some(fresh.clone());
                    fresh
                }
                None => stored,
            };
            // A host Claude sid with no transcript was never written, so `--resume`
            // is certain to fail; relaunch it pinned with `--session-id` instead.
            if self.resolved_capture_backend() == Some(SessionCaptureBackend::Claude)
                && !self.is_sandboxed()
                && crate::session::capture::claude_host_transcript_confirmed_absent(
                    &self.project_path,
                    &stored,
                    &self.resolved_host_environment(),
                    self.declared_agent_config_dir_for(&self.tool).as_deref(),
                )
            {
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

        if self.tmux_session().is_ok_and(|s| s.exists()) {
            if let Some(id) = self.try_retroactive_capture() {
                tracing::info!(target: "session.store",
                    "Retroactive capture found session ID for {}: {}", self.tool, id);
                self.agent_session_id = Some(id);
                return (self.agent_session_id.clone(), true);
            }
        }

        let session_id = self.fresh_launch_session_id(mint_fresh_id);
        if let Some(ref id) = session_id {
            tracing::debug!(target: "session.store", "Session ID for {}: {}", self.tool, id);
            self.agent_session_id = session_id.clone();
        }
        (session_id, false)
    }

    /// Claude pins a UUID; OpenCode and Pi use the mint seam, whose failure
    /// returns no id rather than a guess. Other backends capture after launch.
    fn fresh_launch_session_id(
        &self,
        mint_fresh_id: &dyn Fn(&str) -> Option<String>,
    ) -> Option<String> {
        match self.resolved_capture_backend()? {
            SessionCaptureBackend::Claude => Some(generate_session_uuid()),
            SessionCaptureBackend::OpenCode | SessionCaptureBackend::Pi => {
                mint_fresh_id(&self.project_path)
            }
            _ => None,
        }
    }

    /// Opt-in, host-only, and only for a launch the ephemeral `opencode serve`
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
        let captured = self.try_retroactive_capture();
        let applied = captured.as_ref().is_some_and(|captured| {
            self.resume_probe_failed_sid.as_deref() != Some(captured.as_str())
                && persist_session_to_storage(profile, &self.id, captured, None, &file_watch)
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
            self.agent_session_id = captured;
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

    /// A newly observed native id, only from a source that attributes it to this
    /// pane: the Pi or Prime publication, the hook sidecar, or the managed store.
    pub(crate) fn capture_freshest_session_id(&self) -> Option<String> {
        let authoritative = match self.resolved_capture_backend()? {
            SessionCaptureBackend::Pi => self.pi_published_session_id(false)?,
            SessionCaptureBackend::PrimeAgent => match self.prime_root_publication()? {
                PrimeRootPublication::Ready(id) => id,
                PrimeRootPublication::Pending(_) => return None,
            },
            SessionCaptureBackend::Claude | SessionCaptureBackend::HookSidecar => {
                crate::hooks::read_hook_session_id_any_age(&self.id)?
            }
            _ => {
                let live = self.try_retroactive_capture()?;
                return override_if_distinct(self.agent_session_id.as_deref(), live);
            }
        };
        if self.retroactive_capture_excludes.contains(&authoritative) {
            return None;
        }
        override_if_distinct(self.agent_session_id.as_deref(), authoritative)
    }

    /// Whether to emit the `existing` resume arm. A Pi id AoE minted takes the
    /// pinning arm instead: `--session-id` recreates a never-prompted conversation
    /// that `--session` would exit 1 on. User pins keep `--session`.
    pub(super) fn resume_flag_arm_is_existing(
        &self,
        is_existing: bool,
        pi_pinnable: bool,
        session_id: Option<&str>,
        explicitly_pinned: bool,
    ) -> bool {
        let takes_pinning_arm = self.resolved_capture_backend() == Some(SessionCaptureBackend::Pi)
            && pi_pinnable
            && !explicitly_pinned
            && session_id.is_some_and(|sid| Uuid::parse_str(sid).is_ok());
        is_existing && !takes_pinning_arm
    }

    /// Why this row can never resume; mirrors what `apply_session_flags` refuses.
    fn terminal_resume_static_unavailable(&self) -> Option<ResumeStaticUnavailable> {
        let Some(agent) = self
            .resolved_agent()
            .filter(|agent| agent.session_support.is_some())
        else {
            return Some(ResumeStaticUnavailable::Agent);
        };
        if !self.launch_can_carry_resume_selector(agent) {
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
        let agent = self.resolved_agent()?;
        let strategy = agent.session_support.as_ref()?.resume;
        if words.first().map(String::as_str) != Some(agent.binary) {
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
    pub(super) fn apply_session_flags(&mut self, cmd: &mut String, context: &str) -> Result<bool> {
        let Some(parsed_command) = parse_launch_command(cmd) else {
            return Ok(false);
        };
        if let Some(selector) = self.existing_session_selector(&parsed_command.words) {
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
        if !self.supports_native_resume() {
            return Ok(false);
        }
        let resume_tool = self
            .resolved_agent()
            .map_or(self.tool.clone(), |agent| agent.name.to_string());
        if let ResumeIntent::Fork { from } = self.resume_intent.clone() {
            if let Some(child_id) = self.agent_session_id.as_deref() {
                let fork_part = build_fork_flags(&resume_tool, &from, child_id);
                if !fork_part.is_empty() {
                    // Codex forks with a subcommand that must follow the binary.
                    let is_subcommand = matches!(
                        self.resolved_agent().map(|agent| &agent.fork_strategy),
                        Some(crate::agents::ForkStrategy::CodexFork)
                    );
                    splice_subcommand_or_append(
                        cmd,
                        &fork_part,
                        is_subcommand.then_some(parsed_command.executable_end),
                    );
                }
            }
            return Ok(false);
        }
        let explicitly_pinned = matches!(self.resume_intent, ResumeIntent::Use(_));
        self.absorb_published_pi_session();
        let (mut session_id, is_existing) = self.acquire_session_id();
        let flag_arm_is_existing = self.resume_flag_arm_is_existing(
            is_existing,
            self.pi_session_id_pinnable(),
            session_id.as_deref(),
            explicitly_pinned,
        );
        // `build_resume_flags` already refuses unsupported agents and invalid ids.
        match self.terminal_resume_static_unavailable() {
            Some(ResumeStaticUnavailable::Command) => {
                tracing::warn!(target: "session.store",
                    tool = %self.tool,
                    command = %self.command,
                    "resume selectors need the agent's own argv and this command hides it behind a launcher; starting fresh"
                );
                session_id = None;
            }
            Some(ResumeStaticUnavailable::Sandbox) => session_id = None,
            _ => {}
        }
        // A published transcript path resolves wherever the conversation started,
        // surviving a worktree move; never over an explicit pin.
        if is_existing && !explicitly_pinned && session_id.is_some() {
            if let Some(path) = self.pi_resumable_transcript() {
                let flags = format!("--session {}", shell_escape(&path));
                splice_subcommand_or_append(cmd, &flags, None);
                tracing::debug!(target: "session.store", "Added resume flags to {} command: {}", context, flags);
                return Ok(true);
            }
        }
        // Pi's `--session` exits 1 on a conversation with no transcript, and the
        // dead pane would read as a failed resume; start fresh unless user-pinned.
        if flag_arm_is_existing
            && !explicitly_pinned
            && session_id.is_some()
            && self.resolved_capture_backend() == Some(SessionCaptureBackend::Pi)
            && self.pi_recorded_transcript_missing()
        {
            tracing::info!(
                target: "session.store",
                instance = %self.id,
                sid = ?session_id,
                "the conversation this Pi session owns has no transcript; \
                 starting fresh rather than failing the launch on `--session`. \
                 The pane's next conversation replaces it",
            );
            session_id = None;
        }
        let emitted = append_resume_flags(
            &resume_tool,
            session_id.as_deref(),
            flag_arm_is_existing,
            cmd,
            parsed_command.executable_end,
            context,
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
            Ok(write @ (SidWrite::Applied | SidWrite::Skipped)) => {
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
    fn stored_and_pinned_ids_are_reused() {
        for (tool, stored, intent, expected) in [
            (
                "opencode",
                Some("oc-session-42"),
                ResumeIntent::Default,
                "oc-session-42",
            ),
            ("codex", Some("sess-99"), ResumeIntent::Default, "sess-99"),
            (
                "claude",
                None,
                ResumeIntent::Use("user-pinned".into()),
                "user-pinned",
            ),
            (
                "claude",
                Some("observed"),
                ResumeIntent::Use("user-pinned".into()),
                "user-pinned",
            ),
        ] {
            let mut inst = tool_instance(tool, "/tmp/x");
            inst.agent_session_id = stored.map(str::to_string);
            inst.resume_intent = intent;
            assert_eq!(
                inst.acquire_session_id(),
                (Some(expected.to_string()), true),
                "{tool}"
            );
            assert_eq!(inst.agent_session_id.as_deref(), Some(expected));
        }
    }

    #[test]
    fn claude_resume_flags_follow_is_existing() {
        assert_eq!(
            build_resume_flags("claude", "some-id", true),
            "--resume some-id"
        );
        assert_eq!(
            build_resume_flags("claude", "some-id", false),
            "--session-id some-id"
        );
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
        assert!(!inst.apply_session_flags(&mut cmd, "test").unwrap());
        assert_eq!(
            cmd,
            format!("claude --resume {parent} --fork-session --session-id {child}")
        );
        assert_eq!(inst.agent_session_id.as_deref(), Some(child));
    }

    #[test]
    fn fresh_claude_launch_mints_a_stable_pinned_id() {
        let mut inst = tool_instance("claude", "/tmp/test");
        let (first, first_existing) = inst.acquire_session_id();
        assert!(first.is_some() && !first_existing);
        assert_eq!(inst.agent_session_id, first);
        // With no transcript on disk the same id stays fresh-pinned.
        assert_eq!(inst.acquire_session_id(), (first, false));

        let mut fresh = tool_instance("claude", "/tmp/test");
        let mut cmd = String::from("claude");
        assert!(!fresh.apply_session_flags(&mut cmd, "test").unwrap());
        fresh.resume_intent = ResumeIntent::Use("019342ab-1234-7def-8901-abcdef012345".into());
        let mut cmd = String::from("claude");
        assert!(fresh.apply_session_flags(&mut cmd, "test").unwrap());
    }

    #[test]
    fn cleared_intent_launches_fresh() {
        let mut claude = tool_instance("claude", "/tmp/x");
        claude.agent_session_id = Some("observed".to_string());
        claude.resume_intent = ResumeIntent::Cleared;
        let (sid, is_existing) = claude.acquire_session_id();
        assert!(sid.is_some() && !is_existing);
        assert_ne!(sid.as_deref(), Some("observed"));
        assert_eq!(claude.agent_session_id, sid);

        let mut opencode = tool_instance("opencode", "/tmp/x");
        opencode.agent_session_id = Some("observed".to_string());
        opencode.resume_intent = ResumeIntent::Cleared;
        assert_eq!(opencode.acquire_session_id(), (None, false));
        assert_eq!(opencode.agent_session_id, None);
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
            let result = inst.acquire_session_id_with(&|_| minted.map(str::to_string));
            assert_eq!(result, (expected.map(str::to_string), false));
            assert_eq!(inst.agent_session_id.as_deref(), expected);
        }
        let mut claude = tool_instance("claude", "/tmp/test");
        let (claude_sid, _) = claude.acquire_session_id_with(&|_| panic!("seam ran for claude"));
        assert!(claude_sid.is_some());
        let mut codex = tool_instance("codex", "/tmp/test");
        assert_eq!(
            codex.acquire_session_id_with(&|_| panic!("seam ran for codex")),
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

        let (session_id, is_existing) = reloaded.acquire_session_id();
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
                inst.apply_session_flags(&mut cmd, "test").unwrap(),
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
            .apply_session_flags(&mut automatic_cmd, "test")
            .unwrap());
        assert_eq!(automatic_cmd, "copilot");

        let mut host_prime = tool_instance("prime-agent", "/tmp/test");
        host_prime.agent_session_id = Some(sid.to_string());
        host_prime.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut cmd = "prime-agent".to_string();
        assert!(host_prime.apply_session_flags(&mut cmd, "test").unwrap());
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

    fn profiled(profile: &str, tool: &str, command: &str) -> Instance {
        let mut inst = tool_instance(tool, "/tmp/profiled");
        inst.source_profile = profile.to_string();
        inst.command = command.to_string();
        inst
    }

    /// A resume subcommand must land right after the program the pane runs; a launcher or a
    /// path-qualified script hides it, while a bare renamed wrapper is that program (#3638).
    #[test]
    fn codex_wrapper_command_never_takes_a_spliced_subcommand() {
        const PROFILE: &str = "codex-wrapper-splice-test";
        let _registry = install_aliases(PROFILE, &[("codex-remote", "codex")]);
        let sid = "11111111-2222-3333-4444-555555555555";
        for (command, context, expected) in [
            (
                "ssh -t lenovo codex",
                TerminalContextResume::CommandUnsupported,
                None,
            ),
            (
                "/opt/bin/mycodex",
                TerminalContextResume::CommandUnsupported,
                None,
            ),
            (
                "codex --model o3",
                TerminalContextResume::Available,
                Some(format!("codex resume {sid} --model o3")),
            ),
            (
                "mycodex",
                TerminalContextResume::Available,
                Some(format!("mycodex resume {sid}")),
            ),
            (
                "codex-personal",
                TerminalContextResume::Available,
                Some(format!("codex-personal resume {sid}")),
            ),
        ] {
            let mut inst = profiled(PROFILE, "codex-remote", command);
            inst.agent_session_id = Some(sid.to_string());
            inst.resume_intent = ResumeIntent::Use(sid.to_string());
            assert_eq!(inst.terminal_context_resume_cached(), context, "{command}");
            let mut cmd = command.to_string();
            assert_eq!(
                inst.apply_session_flags(&mut cmd, "test").unwrap(),
                expected.is_some()
            );
            assert_eq!(
                cmd,
                expected.unwrap_or_else(|| command.to_string()),
                "{command}"
            );
        }
    }

    /// A custom agent resolves capture and resume through its `detect_as` base (#3638).
    #[test]
    fn custom_agent_pins_and_resumes_through_its_detect_as_base() {
        const PROFILE: &str = "custom-agent-resume-test";
        let _registry = install_aliases(
            PROFILE,
            &[
                ("claude-personal", "claude"),
                ("copilot-personal", "copilot"),
                ("droid-personal", "droid"),
            ],
        );

        let mut inst = profiled(PROFILE, "claude-personal", "claude-personal");
        let mut fresh = "claude-personal".to_string();
        assert!(!inst.apply_session_flags(&mut fresh, "test").unwrap());
        let sid = inst
            .agent_session_id
            .clone()
            .expect("a wrapper launch pins its conversation");
        assert_eq!(fresh, format!("claude-personal --session-id {sid}"));

        inst.resume_intent = ResumeIntent::Use(sid.clone());
        assert_eq!(
            inst.terminal_context_resume_cached(),
            TerminalContextResume::Available
        );
        let mut restart = "claude-personal".to_string();
        assert!(inst.apply_session_flags(&mut restart, "test").unwrap());
        assert_eq!(restart, format!("claude-personal --resume {sid}"));
        assert_eq!(inst.agent_session_id.as_deref(), Some(sid.as_str()));

        let mut unsupported = profiled(PROFILE, "droid-personal", "droid-personal");
        unsupported.resume_intent = ResumeIntent::Use(sid.clone());
        // Sandboxing applies the base agent's store rule: copilot has nothing to resume there.
        let mut sandboxed = profiled(PROFILE, "copilot-personal", "copilot-personal");
        sandboxed.agent_session_id = Some(sid);
        sandboxed.sandbox_info = Some(test_sandbox("test", None));
        for (mut inst, context) in [
            (unsupported, TerminalContextResume::AgentUnsupported),
            (sandboxed, TerminalContextResume::SandboxUnsupported),
        ] {
            assert_eq!(inst.terminal_context_resume_cached(), context);
            let mut cmd = inst.command.clone();
            assert!(!inst.apply_session_flags(&mut cmd, "test").unwrap());
            assert_eq!(cmd, inst.command);
        }
    }

    #[test]
    fn unsupported_context_without_identity_neither_resumes_nor_polls() {
        let mut inst = tool_instance("codex", "/tmp/test");
        assert_eq!(inst.acquire_session_id_with(&|_| None), (None, false));
        assert_eq!(inst.agent_session_id, None);
        let mut cmd = String::from("codex");
        assert!(!inst.apply_session_flags(&mut cmd, "test").unwrap());
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
            assert!(!inst.apply_session_flags(&mut actual, "test").unwrap());
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
                    .apply_session_flags(&mut command.to_string(), "test")
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
        assert!(!external.apply_session_flags(&mut command, "test").unwrap());
        assert_eq!(command, "codex resume external");

        let sid = "11111111-2222-3333-4444-555555555555";
        let mut value_token = tool_instance("codex", "/tmp/x");
        value_token.resume_intent = ResumeIntent::Use(sid.to_string());
        let mut command = "codex --model resume".to_string();
        assert!(value_token
            .apply_session_flags(&mut command, "test")
            .unwrap());
        assert_eq!(command, format!("codex resume {sid} --model resume"));

        const PROFILE: &str = "selector-detect-as-alias";
        let _registry = install_aliases(PROFILE, &[("work-claude", "claude")]);
        let mut alias = Instance::new("alias", "/tmp/x");
        alias.source_profile = PROFILE.to_string();
        alias.tool = "work-claude".to_string();
        alias.command = "claude --resume external".to_string();
        let mut command = alias.command.clone();
        assert!(!alias.apply_session_flags(&mut command, "test").unwrap());
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
        assert!(!inst.apply_session_flags(&mut command, "test").unwrap());
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
        assert!(inst.apply_session_flags(&mut pinned, "test").unwrap());
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
                    "sandboxed claude reads host sidecar",
                    "claude",
                    true,
                    &[(A, 120), (B, 5)],
                    Some(A),
                    C,
                    A,
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
                let acquired = inst.acquire_session_id();
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
            let mut inst = tool_instance("claude", "/tmp/idle-sidecar");
            inst.agent_session_id = Some("stored-old".to_string());
            let dir = write_sidecar(&inst.id, "published-new");
            fs::File::options()
                .write(true)
                .open(dir.join("session_id"))
                .unwrap()
                .set_times(
                    fs::FileTimes::new()
                        .set_modified(SystemTime::now() - Duration::from_secs(10 * 60)),
                )
                .unwrap();
            assert_eq!(
                inst.capture_freshest_session_id().as_deref(),
                Some("published-new")
            );
            fs::remove_dir_all(dir).ok();
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
                inst.acquire_session_id()
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
            let acquired = inst.acquire_session_id_with(&|_| Some(pinned.to_string()));
            assert_eq!(acquired, (Some(pinned.to_string()), false));
            assert_eq!(inst.agent_session_id.as_deref(), Some(pinned));
            assert_eq!(
                build_resume_flags("pi", pinned, false),
                format!("--session-id {pinned}")
            );

            let mut unpinnable = tool_instance("pi", "/tmp/pi-fresh");
            assert_eq!(unpinnable.acquire_session_id_with(&|_| None), (None, false));
            assert_eq!(unpinnable.agent_session_id, None);
        }
    }
}
