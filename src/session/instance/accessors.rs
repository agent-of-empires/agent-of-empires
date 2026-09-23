//! Construction, identity, and the small accessors every other slice of
//! `Instance` builds on.

use super::*;

fn generate_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")[..16].to_string()
}

impl Instance {
    pub fn new(title: &str, project_path: &str) -> Self {
        Self {
            id: generate_id(),
            title: title.to_string(),
            last_auto_title: None,
            smart_rename_attempted: false,
            project_path: project_path.to_string(),
            group_path: String::new(),
            parent_session_id: None,
            command: String::new(),
            extra_args: String::new(),
            tool: "claude".to_string(),
            detect_as: String::new(),
            yolo_mode: false,
            status: Status::Idle,
            created_at: Utc::now(),
            last_accessed_at: None,
            idle_entered_at: None,
            archived_at: None,
            favorited_at: None,
            snoozed_until: None,
            unread: false,
            idle_dormant_since: None,
            pinned_at: None,
            trashed_at: None,
            pre_trash_project_path: None,
            lifecycle_reservation: None,
            plugin_meta: std::collections::BTreeMap::new(),
            created_by_plugin: None,
            plugin_create_idempotency: None,
            pending_initial_turn: None,
            queued_prompts: Vec::new(),
            queued_prompt_next_seq: 0,
            acp_mode_id: None,
            prior_tool_session_ids: HashMap::new(),
            scratch: false,
            worktree_info: None,
            workspace_info: None,
            sandbox_info: None,
            sandbox_store_generation:
                crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION,
            sandbox_store_transition_paths: Vec::new(),
            terminal_info: None,
            agent_session_id: None,
            agent_session_binding: None,
            resume_binding: None,
            active_execution: None,
            omp_capture_generation: None,
            lifecycle_generation: 0,
            resume_probe_failed_sid: None,
            resume_intent: ResumeIntent::Default,
            force_fresh_next_launch: false,
            source_profile: String::new(),
            notify_on_waiting: None,
            notify_on_idle: None,
            notify_on_error: None,
            callback_url: None,
            idempotency_key: None,
            base_branch_override: None,
            color: None,
            view: View::Terminal,
            agent_name: None,
            agent_model: None,
            acp_effort: None,
            acp_session_id: None,
            import_pending: None,
            fork_pending: None,
            acp_load_session_capable: None,
            last_error_check: None,
            last_start_time: None,
            live_status_baseline: None,
            ever_confirmed_present: false,
            unknown_since: None,
            detection: DetectionState::default(),
            pending_host_env: Vec::new(),
            capture_started_at: None,
            pi_extension_launched: false,
            identity_publisher_launched: false,
            pi_session_path: None,
            last_error: None,
            session_id_poller: None,
            poller_repair: Default::default(),
            session_id_poller_retry_after: None,
            retroactive_capture_excludes: HashSet::new(),
            pane_dead_observed: false,
            file_watch: None,
        }
    }

    /// Inject the live FileWatchService Arc into this Instance for in-process Local fast-path
    /// notifications during subsequent storage mutations.
    pub(crate) fn set_file_watch(
        &mut self,
        fw: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) {
        self.file_watch = Some(fw);
    }

    /// Resolve the live `Arc<FileWatchService>` for this Instance, falling back to a noop service
    /// when none was injected (ad-hoc construction or pre-injection state).
    pub(super) fn resolve_file_watch(&self) -> std::sync::Arc<crate::file_watch::FileWatchService> {
        self.file_watch
            .clone()
            .unwrap_or_else(crate::file_watch::FileWatchService::noop)
    }

    /// Whether a title rename should also move the worktree directory leaf, given the resolved
    /// `session.tie_workdir_to_name` setting.
    pub fn tie_workdir_applies(&self, tie_setting: bool) -> bool {
        tie_setting
            && self
                .worktree_info
                .as_ref()
                .is_some_and(|w| w.managed_by_aoe)
    }

    /// Whether deleting this session has aoe-managed worktree state to clean up, covering BOTH
    /// single-repo and multi-repo (workspace) sessions.
    pub fn has_managed_worktree_or_workspace(&self) -> bool {
        self.worktree_info
            .as_ref()
            .is_some_and(|w| w.managed_by_aoe)
            || self
                .workspace_info
                .as_ref()
                .is_some_and(|ws| ws.cleanup_on_delete)
    }

    /// Every repo this session works in, empty for a single-repo session.
    pub fn all_repos(&self) -> &[WorkspaceRepo] {
        self.workspace_info
            .as_ref()
            .map(|ws| ws.repos.as_slice())
            .unwrap_or(&[])
    }

    /// Return the profile that should drive config resolution for this instance, falling back to
    /// the user's globally configured default when `source_profile` was never populated (e.g.
    /// legacy callers).
    pub fn effective_profile(&self) -> String {
        crate::session::config::effective_profile(&self.source_profile)
    }

    /// The `agent_detect_as` alias that actually applies to this session.
    pub(super) fn effective_detect_as(&self) -> std::borrow::Cow<'_, str> {
        tmux::status_rules::effective_detect_as(&self.source_profile, &self.tool, &self.detect_as)
    }

    /// Native execution identity; explicit targets never trust status aliases.
    pub(crate) fn resolved_agent(&self) -> Option<&'static crate::agents::AgentDef> {
        self.execution_agent().ok()
    }

    pub(crate) fn status_agent(&self) -> Option<&'static crate::agents::AgentDef> {
        self.resolved_agent()
            .or_else(|| crate::agents::get_agent(&self.tool))
            .or_else(|| crate::agents::get_agent(&self.effective_detect_as()))
    }

    /// The built-in identity used to compare capture stores and aliases.
    pub(crate) fn capture_agent_name(&self) -> Option<&'static str> {
        self.resolved_agent().map(|a| a.name)
    }
    /// Whether a launch fragment carries shell syntax the pane's shell would
    /// act on, so the agent is not what the command word names.
    pub(super) fn contains_active_shell_syntax(value: &str) -> bool {
        let mut quote = None;
        let mut escaped = false;
        for ch in value.chars() {
            if matches!(ch, '\n' | '\r') {
                return true;
            }
            if escaped {
                escaped = false;
                continue;
            }
            match quote {
                Some('\'') => {
                    if ch == '\'' {
                        quote = None;
                    }
                }
                Some('"') => match ch {
                    '"' => quote = None,
                    '\\' => escaped = true,
                    '$' | '`' => return true,
                    _ => {}
                },
                _ => match ch {
                    '\'' | '"' => quote = Some(ch),
                    '\\' => escaped = true,
                    '|' | '&' | ';' | '<' | '>' | '(' | ')' | '#' | '`' | '$' | '*' | '?' | '['
                    | ']' | '{' | '}' | '~' | '!' => return true,
                    _ => {}
                },
            }
        }
        false
    }

    pub(crate) fn launch_invokes_resolved_agent_directly(
        &self,
        agent: &crate::agents::AgentDef,
    ) -> bool {
        let contains_active_shell_syntax = Self::contains_active_shell_syntax;
        let raw_command = self.get_tool_command();
        let launch_extra_args = if self.command.is_empty() {
            crate::session::config::quote_model_value_in_args(&self.extra_args)
        } else {
            self.extra_args.clone()
        };
        if raw_command.trim().is_empty()
            || contains_active_shell_syntax(raw_command)
            || contains_active_shell_syntax(&launch_extra_args)
        {
            return false;
        }
        let Some(parsed_command) = parse_launch_command(raw_command) else {
            return false;
        };
        let mut words = parsed_command.words;
        if let Ok(extra) = shell_words::split(&self.extra_args) {
            words.extend(extra);
        } else {
            return false;
        }
        words
            .first()
            .is_some_and(|executable| executable == agent.binary)
            && !words.iter().any(|word| word == "--")
    }

    /// The basename of the program this launch actually runs, which is the token a live process
    /// carries in argv.
    pub(crate) fn launch_executable_token(&self) -> Option<String> {
        let words = parse_launch_command(self.get_tool_command())?.words;
        Path::new(words.first()?)
            .file_name()?
            .to_str()
            .map(str::to_owned)
    }

    /// Whether a resume selector appended to this launch reaches the agent.
    /// Only a verified direct invocation or an explicit wrapper contract carries selectors.
    pub(crate) fn launch_can_carry_resume_selector(&self, agent: &crate::agents::AgentDef) -> bool {
        self.execution_agent()
            .is_ok_and(|actual| actual.name == agent.name)
            && self.managed_user_argv(agent).is_ok()
    }

    /// A direct native launch can try its stored ID without attesting every argument.
    pub(super) fn can_attempt_default_resume(&self, agent: &crate::agents::AgentDef) -> bool {
        matches!(self.resume_intent, ResumeIntent::Default)
            && self.agent_session_id.is_some()
            && self.launch_invokes_resolved_agent_directly(agent)
    }

    /// Whether this launch shape leaves Claude user hooks enabled.
    pub(crate) fn hook_session_publisher_allowed_by_argv(&self) -> bool {
        if !self
            .default_selector_agent()
            .is_some_and(|agent| agent.name == "claude")
        {
            return true;
        }
        let Some(parsed_command) = parse_launch_command(self.get_tool_command()) else {
            return false;
        };
        let mut words = parsed_command.words;
        let Ok(extra) = shell_words::split(&self.extra_args) else {
            return false;
        };
        words.extend(extra);
        if words.iter().any(|word| {
            matches!(word.as_str(), "--safe-mode" | "--bare")
                || word.starts_with("--safe-mode=")
                || word.starts_with("--bare=")
        }) {
            return false;
        }

        let mut setting_sources: Option<Option<&str>> = None;
        let mut index = 0;
        while index < words.len() {
            let word = words[index].as_str();
            if word == "--setting-sources" {
                setting_sources = Some(
                    words
                        .get(index + 1)
                        .map(String::as_str)
                        .filter(|value| !value.starts_with('-')),
                );
                index += 2;
                continue;
            }
            if let Some(value) = word.strip_prefix("--setting-sources=") {
                setting_sources = Some(Some(value));
            }
            index += 1;
        }
        match setting_sources {
            None => true,
            Some(Some(value)) => value.split(',').any(|source| source.trim() == "user"),
            Some(None) => false,
        }
    }

    pub(super) fn resolved_session_support(
        &self,
    ) -> Option<(
        &'static crate::agents::SessionCaptureSpec,
        crate::agents::SessionCaptureContext,
    )> {
        let native = self.resolved_agent();
        let agent = native.or_else(|| self.legacy_default_selector_agent())?;
        let support = agent.session_support.as_ref()?;
        let capture = support.capture.as_ref()?;
        let context = if self.is_sandboxed() {
            capture.sandbox
        } else {
            capture.host
        };
        if context == crate::agents::SessionCaptureContext::Unsupported {
            return None;
        }
        // Pane-scoped publishers confine Default/Cleared wrapper capture to this pane.
        let self_attributing = matches!(
            capture.backend,
            crate::agents::SessionCaptureBackend::Claude
                | crate::agents::SessionCaptureBackend::HookSidecar
                | crate::agents::SessionCaptureBackend::Pi
        );
        let authorized = if self_attributing {
            native.is_none() || self.launch_can_carry_resume_selector(agent)
        } else {
            native.is_some() && self.launch_invokes_resolved_agent_directly(agent)
        };
        authorized.then_some((capture, context))
    }
    pub(super) fn source_session_support(
        &self,
    ) -> Option<(
        &'static crate::agents::SessionCaptureSpec,
        crate::agents::SessionCaptureContext,
    )> {
        let Some(active) = &self.active_execution else {
            return self.resolved_session_support();
        };
        let capture = crate::agents::get_agent(&active.binding.agent)?
            .session_support
            .as_ref()?
            .capture
            .as_ref()?;
        let context = if active.container.is_some() {
            capture.sandbox
        } else {
            capture.host
        };
        (context != crate::agents::SessionCaptureContext::Unsupported).then_some((capture, context))
    }

    pub(super) fn source_capture_backend(&self) -> Option<crate::agents::SessionCaptureBackend> {
        self.source_session_support()
            .map(|(capture, _)| capture.backend)
    }

    pub(super) fn resolved_capture_backend(&self) -> Option<crate::agents::SessionCaptureBackend> {
        self.resolved_session_support()
            .map(|(capture, _)| capture.backend)
    }

    /// Bare wrappers can receive automatic selectors, without proving execution identity.
    pub(super) fn legacy_default_selector_agent(&self) -> Option<&'static crate::agents::AgentDef> {
        if matches!(
            self.resume_intent,
            ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
        ) {
            return None;
        }
        let agent = resolved_agent_for(&self.effective_profile(), &self.tool, &self.detect_as)?;
        if crate::session::config::profile_config::resolve_config_or_warn(&self.effective_profile())
            .session
            .agent_execution_as
            .contains_key(&self.tool)
        {
            return None;
        }
        let command = self.get_tool_command();
        if Self::contains_active_shell_syntax(command)
            || Self::contains_active_shell_syntax(&self.extra_args)
        {
            return None;
        }
        let parsed = parse_launch_command(command)?;
        let [program] = parsed.words.as_slice() else {
            return None;
        };
        if program.contains('/')
            || shell_words::split(&self.extra_args)
                .ok()?
                .iter()
                .any(|arg| arg == "--")
        {
            return None;
        }
        if crate::agents::AGENTS
            .iter()
            .any(|other| other.binary == program && other.name != agent.name)
        {
            return None;
        }
        let capture = agent.session_support.as_ref()?.capture.as_ref()?;
        if (if self.is_sandboxed() {
            capture.sandbox
        } else {
            capture.host
        }) == crate::agents::SessionCaptureContext::Unsupported
        {
            return None;
        }
        Some(agent)
    }

    pub(super) fn default_selector_agent(&self) -> Option<&'static crate::agents::AgentDef> {
        self.resolved_agent()
            .or_else(|| self.legacy_default_selector_agent())
    }

    /// A selector backend is only for minting and native flags, not conversation capture.
    pub(super) fn default_selector_backend(&self) -> Option<crate::agents::SessionCaptureBackend> {
        self.resolved_capture_backend().or_else(|| {
            self.legacy_default_selector_agent()?
                .session_support
                .as_ref()?
                .capture
                .as_ref()
                .map(|capture| capture.backend)
        })
    }

    pub fn supports_native_resume(&self) -> bool {
        let native = self.resolved_agent();
        let Some(agent) = native.or_else(|| self.legacy_default_selector_agent()) else {
            return false;
        };
        if agent.session_support.is_none() {
            return false;
        }
        if native.is_none() {
            return true;
        }
        let implicit = self.can_attempt_default_resume(agent);
        if !implicit && !self.launch_can_carry_resume_selector(agent) {
            return false;
        }
        implicit
            || self.resolved_session_support().is_some()
            || matches!(
                self.resume_intent,
                ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
            )
    }

    /// The `session.agent_config_dir` entry `tool` reads, resolved against the
    /// session's host `HOME`.
    ///
    /// The declared directory wins over the agent's config-dir environment
    /// variable wherever both could answer, the precedence
    /// [`crate::hooks::trust_host_project`] applies: the setting exists for a
    /// wrapper that exports that variable itself, which happens after AoE has
    /// handed the launch its environment, so AoE never sees it.
    ///
    /// `tool` is a parameter rather than `self.tool` because a swap has to
    /// resolve the directory of the tool it is moving to as well as the one it
    /// is moving from.
    pub(crate) fn declared_agent_config_dir_for(&self, tool: &str) -> Option<std::path::PathBuf> {
        let home = super::hooks::host_home(&self.resolved_host_environment())?;
        crate::session::config::profile_config::resolve_config_or_warn(&self.effective_profile())
            .session
            .agent_config_dir_for(tool, &home)
    }

    pub(super) fn sandbox_capture_store_dir(&self) -> Option<std::path::PathBuf> {
        if !self.is_sandboxed() {
            return None;
        }
        let home = dirs::home_dir()?;
        let config = crate::session::config::profile_config::resolve_config_or_warn(
            &self.effective_profile(),
        );
        let declared = config.session.agent_config_dir_for(&self.tool, &home);
        let agent = self.resolved_agent()?;
        if self.sandbox_store_generation
            < crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION
        {
            return crate::session::config::container_config::legacy_sandbox_store_dir(
                agent.name,
                &home,
                declared.as_deref(),
                (self.sandbox_store_generation == 0).then_some(self.id.as_str()),
            );
        }
        crate::session::config::container_config::sandbox_store_dir(
            agent.name,
            &home,
            declared.as_deref(),
            &self.id,
        )
        .ok()
        .flatten()
    }
    pub fn is_sub_session(&self) -> bool {
        self.parent_session_id.is_some()
    }

    pub fn is_sandboxed(&self) -> bool {
        self.sandbox_info.as_ref().is_some_and(|s| s.enabled)
    }

    /// The repo this session groups under: the worktree's main repo when present (so all branches
    /// of a repo group together), else the project path.
    pub fn repo_path(&self) -> &str {
        self.worktree_info
            .as_ref()
            .map(|w| w.main_repo_path.as_str())
            .unwrap_or(&self.project_path)
    }

    pub fn is_yolo_mode(&self) -> bool {
        self.yolo_mode
    }

    /// True when this session renders in the structured (ACP) view. Rows damaged by pre-fix writers
    /// are healed on reload by the server's structured row repair path.
    pub fn is_structured(&self) -> bool {
        self.view == View::Structured
    }

    /// Keep only a store asserted by the user or captured from the live worker.
    pub(crate) fn switch_to_terminal_keep_context(
        &mut self,
        worker: Option<&ExecutionBinding>,
    ) -> Result<()> {
        let sid = self
            .acp_session_id
            .clone()
            .context("ACP conversation ID is unavailable")?;
        let asserted = self
            .resume_binding
            .as_ref()
            .filter(|binding| {
                matches!(&self.resume_intent, ResumeIntent::Use(target) if target == &sid)
                    && binding.session_id == sid
                    && binding.provenance == ConversationProvenance::Asserted
                    && binding
                        .execution
                        .as_ref()
                        .is_some_and(|execution| execution.agent == "claude")
            })
            .cloned();
        let binding = if asserted.is_some() {
            asserted
        } else {
            worker.map_or(Ok(None), |worker| self.resolved_handoff_binding(&sid, worker))?
        }
        .context("ACP does not prove a native conversation store; bind its current ID with aoe session set-session-id SESSION ID --store /absolute/claude-store before switching to terminal")?;
        self.adopt_conversation_state(ConversationState {
            session_id: Some(sid.clone()),
            binding: Some(binding.clone()),
            intent: ResumeIntent::Use(sid),
            resume_binding: Some(binding),
            active: None,
            pi_session_path: None,
        });
        self.acp_session_id = None;
        self.import_pending = None;
        self.acp_load_session_capable = None;
        self.view = View::Terminal;
        Ok(())
    }

    pub(crate) fn selected_claude_conversation(&self) -> Option<(&str, &ExecutionBinding)> {
        if self.is_sandboxed() || matches!(self.resume_intent, ResumeIntent::Fork { .. }) {
            return None;
        }
        let (sid, binding, _) = self.conversation_target()?;
        let binding = binding?;
        if !binding.is_known() {
            return None;
        }
        let execution = binding.execution.as_ref()?;
        (execution.agent == "claude" && execution.filesystem == "host").then_some((sid, execution))
    }

    fn resolved_handoff_binding(
        &self,
        sid: &str,
        worker: &ExecutionBinding,
    ) -> Result<Option<ConversationBinding>> {
        if worker.agent != "claude" || self.is_sandboxed() || worker.filesystem != "host" {
            return Ok(None);
        }
        let binding = ConversationBinding {
            session_id: sid.to_owned(),
            execution: Some(worker.clone()),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        };
        let execution = self.resolve_native_execution(Some((sid, Some(&binding), true)))?;
        Ok(Self::execution_identity_matches(worker, &execution.binding).then_some(binding))
    }
}

/// Resolve a built-in from the instance's stored alias or its profile registry.
/// The stored value wins; legacy rows with no value consult the live registry.
pub(crate) fn resolved_agent_for(
    profile: &str,
    tool: &str,
    detect_as: &str,
) -> Option<&'static crate::agents::AgentDef> {
    crate::agents::get_agent(tool).or_else(|| {
        crate::agents::get_agent(&tmux::status_rules::effective_detect_as(
            profile, tool, detect_as,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;

    fn worktree(main_repo_path: &str) -> WorktreeInfo {
        WorktreeInfo {
            branch: "feature/abc".to_string(),
            main_repo_path: main_repo_path.to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        }
    }

    #[test]
    fn switch_to_terminal_keep_context_carries_asserted_native_binding() {
        let mut inst = Instance::new("claude", "/tmp");
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".to_string());
        inst.import_pending = Some(true);
        inst.acp_load_session_capable = Some(true);
        let binding = ConversationBinding {
            session_id: "sid-abc".into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec!["/tmp/claude-store".into()],
                configuration: Vec::new(),
                cwd: "/tmp".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
            }),
            provenance: ConversationProvenance::Asserted,
            transcript_path: None,
        };
        inst.resume_intent = ResumeIntent::Use("sid-abc".into());
        inst.resume_binding = Some(binding.clone());

        inst.switch_to_terminal_keep_context(None).unwrap();

        assert_eq!(inst.view, View::Terminal);
        assert_eq!(inst.agent_session_binding.as_ref(), Some(&binding));
        assert_eq!(inst.resume_intent, ResumeIntent::Use("sid-abc".to_string()));
        assert_eq!(
            (
                inst.acp_session_id,
                inst.import_pending,
                inst.acp_load_session_capable
            ),
            (None, None, None)
        );
    }

    #[test]
    fn new_instance_has_a_unique_hex_id_and_defaults() {
        let inst = Instance::new("test", "/tmp/test");
        assert_eq!(
            (inst.title.as_str(), inst.project_path.as_str()),
            ("test", "/tmp/test")
        );
        assert_eq!(inst.status, Status::Idle);
        assert_eq!(inst.id.len(), 16);
        assert!(inst.id.chars().all(|c| c.is_ascii_hexdigit()));
        let ids: std::collections::HashSet<_> =
            (0..100).map(|_| Instance::new("t", "/t").id).collect();
        assert_eq!(ids.len(), 100);
    }

    #[test]
    fn sub_session_and_sandbox_predicates() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_sub_session());
        assert!(!inst.is_sandboxed());
        inst.parent_session_id = Some("parent123".to_string());
        assert!(inst.is_sub_session());
        let mut sandbox = test_sandbox("test", None);
        sandbox.enabled = false;
        inst.sandbox_info = Some(sandbox);
        assert!(!inst.is_sandboxed());
        inst.sandbox_info.as_mut().unwrap().enabled = true;
        assert!(inst.is_sandboxed());
    }

    #[test]
    fn serialization_keeps_persisted_fields_and_drops_runtime_ones() {
        let mut inst = Instance::new("Test Project", "/home/user/project");
        inst.group_path = "work/clients".to_string();
        inst.command = "claude --resume xyz".to_string();
        inst.view = View::Structured;
        inst.agent_name = Some("codex".to_string());
        inst.agent_model = Some("gpt-5".to_string());
        inst.acp_session_id = Some("acp-uuid-1234".to_string());
        inst.worktree_info = Some(worktree("/tmp/main"));
        let floor = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(42);
        inst.capture_started_at = Some(floor);
        inst.retroactive_capture_excludes
            .insert(ConversationBinding::unknown("stale-sid"));
        inst.last_error_check = Some(std::time::Instant::now());
        inst.last_start_time = Some(std::time::Instant::now());
        inst.last_error = Some("test error".to_string());
        inst.acp_load_session_capable = Some(true);

        let json = serde_json::to_string(&inst).unwrap();
        assert!(json.contains("\"view\":\"structured\""));
        for runtime in [
            "last_error_check",
            "last_start_time",
            "last_error",
            "acp_load_session_capable",
        ] {
            assert!(!json.contains(runtime), "{runtime}");
        }
        let back: Instance = serde_json::from_str(&json).unwrap();
        assert_eq!(
            (
                &back.id,
                &back.title,
                &back.project_path,
                &back.group_path,
                &back.tool,
                &back.command
            ),
            (
                &inst.id,
                &inst.title,
                &inst.project_path,
                &inst.group_path,
                &inst.tool,
                &inst.command
            )
        );
        assert_eq!(back.view, View::Structured);
        assert_eq!(back.agent_name.as_deref(), Some("codex"));
        assert_eq!(back.agent_model.as_deref(), Some("gpt-5"));
        assert_eq!(back.acp_session_id.as_deref(), Some("acp-uuid-1234"));
        assert_eq!(back.worktree_info, inst.worktree_info);
        assert_eq!(back.capture_started_at, Some(floor));
        assert!(back
            .retroactive_capture_excludes
            .iter()
            .any(|binding| binding.session_id == "stale-sid"));
        assert_eq!(back.acp_load_session_capable, None);

        let mut structured = Instance::new("Test", "/tmp/test");
        structured.view = View::Structured;
        assert!(!serde_json::to_string(&structured)
            .unwrap()
            .contains("acp_session_id"));

        let old_json = r#"{"id":"old-session-123","title":"Old Session","project_path":"/home/user/old","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z"}"#;
        let old: Instance = serde_json::from_str(old_json).unwrap();
        assert_eq!(
            (old.id.as_str(), old.tool.as_str()),
            ("old-session-123", "claude")
        );
        assert!(old.agent_session_id.is_none());
    }

    #[test]
    fn worktree_and_workspace_ownership_and_repo_path() {
        let mut wt = Instance::new("WT", "/tmp/worktrees/feature");
        assert_eq!(wt.repo_path(), "/tmp/worktrees/feature");
        assert!(!wt.has_managed_worktree_or_workspace());
        wt.worktree_info = Some(worktree("/tmp/main-repo"));
        assert!(wt.has_managed_worktree_or_workspace());
        assert_eq!(wt.repo_path(), "/tmp/main-repo");

        let mut ws = Instance::new("WS", "/tmp/ws/repo-a");
        ws.workspace_info = Some(WorkspaceInfo {
            branch: "feature/abc".to_string(),
            workspace_dir: "/tmp/ws".to_string(),
            repos: vec![WorkspaceRepo {
                name: "repo-a".to_string(),
                source_path: "/tmp/src/repo-a".to_string(),
                branch: "feature/abc".to_string(),
                worktree_path: "/tmp/ws/repo-a".to_string(),
                main_repo_path: "/tmp/src/repo-a".to_string(),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            }],
            created_at: Utc::now(),
            cleanup_on_delete: true,
        });
        assert!(ws.has_managed_worktree_or_workspace());
        ws.workspace_info.as_mut().unwrap().cleanup_on_delete = false;
        assert!(!ws.has_managed_worktree_or_workspace());
    }

    #[test]
    fn native_resume_requires_a_direct_local_builtin_launch() {
        const PROFILE: &str = "resume-custom-launch-test";
        let _registry = install_aliases(PROFILE, &[("work-claude", "claude")]);
        // (tool, command, extra_args, supported)
        for (tool, command, extra, supported) in [
            ("work-claude", "claude --model opus", "", true),
            ("claude", "claude --model opus", "", true),
            ("claude", "", "--model sonnet[1m]", true),
            ("claude", "ssh -t host claude", "", false),
            ("claude", "claude > /tmp/transcript", "", false),
            ("claude", "claude $BARRIER", "", false),
            ("claude", "claude ${BARRIER}", "", false),
            ("claude", "claude *", "", false),
            ("claude", "claude session-?", "", false),
            ("claude", "claude [abc]", "", false),
            ("claude", "claude {one,two}", "", false),
            ("claude", "claude ~/thread", "", false),
            ("claude", "/opt/wrappers/claude", "", false),
            ("claude", "./claude", "", false),
            (
                "claude",
                "claude",
                "--model opus | tee /tmp/transcript",
                false,
            ),
            ("claude", "claude", "--append-system-prompt $PROMPT", false),
            ("claude", "claude # local note", "", false),
            ("claude", "claude", "--model opus # local note", false),
            ("claude", "claude\n", "", false),
            ("claude", "claude\r", "", false),
            ("claude", "claude\r\n", "", false),
            ("claude", "claude --", "", false),
            ("claude", "claude", "--", false),
            ("claude", "claude", "--model opus\n", false),
            ("claude", "claude", "--model opus\r", false),
            ("claude", "claude", "--model opus\r\n", false),
        ] {
            let mut inst = tool_instance(tool, "/tmp/custom");
            inst.source_profile = PROFILE.to_string();
            inst.command = command.to_string();
            inst.extra_args = extra.to_string();
            assert_eq!(
                inst.supports_native_resume(),
                supported,
                "{command:?} {extra:?}"
            );
        }
    }

    #[test]
    fn claude_hook_publisher_proof_respects_hook_disabling_argv() {
        let cases = [
            ("", true),
            ("--model opus", true),
            ("--setting-sources user", true),
            ("--setting-sources=project,user", true),
            ("--safe-mode", false),
            ("--bare", false),
            ("--setting-sources project", false),
            ("--setting-sources=user --setting-sources project", false),
            (
                "--setting-sources=project --setting-sources local,user",
                true,
            ),
            ("--setting-sources", false),
        ];
        for (args, expected) in cases {
            let mut inst = Instance::new("claude", "/tmp/x");
            inst.tool = "claude".to_string();
            inst.extra_args = args.to_string();
            assert_eq!(
                inst.hook_session_publisher_allowed_by_argv(),
                expected,
                "args={args:?}"
            );
        }
    }
    #[test]
    #[serial_test::serial]
    fn handoff_accepts_a_session_home_store() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _config_dir = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let _claude = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "claude",
            "#!/bin/sh\nexit 0\n",
        );
        let session_home = temp.path().join("agent-home");
        let mut inst = Instance::new("claude-session-home", "/tmp");
        inst.pending_host_env = vec![("HOME".into(), session_home.display().to_string())];
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".to_string());

        let worker = inst.resolve_native_execution(None).unwrap().binding;
        inst.switch_to_terminal_keep_context(Some(&worker)).unwrap();

        assert_eq!(
            inst.agent_session_binding
                .as_ref()
                .and_then(|binding| binding.execution.as_ref())
                .and_then(|execution| execution.stores.first())
                .map(std::path::PathBuf::as_path),
            Some(path_identity(&session_home.join(".claude")).as_path())
        );
    }

    #[test]
    #[serial_test::serial]
    fn handoff_refuses_a_store_the_worker_never_wrote() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile = "handoff-declared-store";
        let path =
            crate::session::config::profile_config::get_profile_config_path(profile).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!(
                "[session.agent_config_dir]\nclaude = {:?}\n",
                temp.path().join("declared-claude").to_str().unwrap()
            ),
        )
        .unwrap();
        let mut inst = Instance::new("claude-declared", "/tmp");
        inst.source_profile = profile.into();
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".to_string());

        let error = inst.switch_to_terminal_keep_context(None).unwrap_err();
        assert!(error.to_string().contains("set-session-id"));
        assert_eq!(inst.view, View::Structured);
        assert_eq!(inst.acp_session_id.as_deref(), Some("sid-abc"));
    }
    #[test]
    #[serial_test::serial]
    fn handoff_reports_native_resolution_failure_without_losing_acp_session() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _claude = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "claude",
            "#!/bin/sh\nexit 0\n",
        );
        let mut inst = Instance::new("claude-handoff-error", "/tmp");
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".into());
        let worker = inst.resolve_native_execution(None).unwrap().binding;
        inst.extra_args = "--mcp-config /tmp/unattested.json".into();

        let error = inst
            .switch_to_terminal_keep_context(Some(&worker))
            .unwrap_err();
        assert!(error.to_string().contains("--mcp-config"), "{error:#}");
        assert_eq!(inst.view, View::Structured);
        assert_eq!(inst.acp_session_id.as_deref(), Some("sid-abc"));
    }
}
