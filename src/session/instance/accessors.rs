//! Construction, identity, and the small accessors every other slice of
//! `Instance` builds on.

use super::*;

/// How to bind a Structured view conversation so a switch to Terminal view can carry it.
pub(crate) const UNBOUND_STORE_HINT: &str = "ACP does not prove a native conversation store; bind its current ID with aoe session set-session-id SESSION ID --store /absolute/claude-store before switching to terminal";

/// A sub-session was named as a new session's parent.
#[derive(Debug, thiserror::Error)]
#[error("Cannot create sub-session of a sub-session (single level only)")]
pub struct NestedSubSession;

pub(crate) fn generate_id() -> String {
    Uuid::new_v4().to_string().replace("-", "")[..16].to_string()
}

/// The resolved config the launcher-store preview reads.
pub(crate) struct OwnCmdConfig<C> {
    pub session: C,
    pub profile_environment: Vec<String>,
    pub before_session_hook: bool,
}

impl Instance {
    pub fn new(title: &str, project_path: &str) -> Self {
        Self {
            id: generate_id(),
            title: title.to_string(),
            terminal_scopes: Some(Vec::new()),
            last_auto_title: None,
            first_launch_names_agent: false,
            sort_index: None,
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
            created_at: Timestamp::now(),
            last_accessed_at: None,
            idle_entered_at: None,
            archived_at: None,
            ignored_at: None,
            favorited_at: None,
            snoozed_until: None,
            unread: false,
            idle_dormant_since: None,
            pinned_at: None,
            trashed_at: None,
            failed_purge: None,
            trash_lifecycle_id: None,
            pre_trash_project_path: None,
            lifecycle_reservation: None,
            terminal_handoff: None,
            plugin_meta: std::collections::BTreeMap::new(),
            created_by_plugin: None,
            plugin_create_idempotency: None,
            suggestion: None,
            plugin_revival_pending: false,
            pending_initial_turn: None,
            queued_prompts: Vec::new(),
            queued_prompt_next_seq: 0,
            acp_mode_id: None,
            prior_tool_session_ids: HashMap::new(),
            scratch: false,
            created_without_setup: false,
            worktree_info: None,
            workspace_info: None,
            sandbox_info: None,
            sandbox_pin: None,
            sandbox_store_generation:
                crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION,
            sandbox_store_transition_paths: Vec::new(),
            sandbox_content_policy: 0,
            sandbox_content_resets: Vec::new(),
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
            view: View::Terminal,
            agent_name: None,
            agent_model: None,
            agent_provider: None,
            acp_effort: None,
            acp_model_config: std::collections::BTreeMap::new(),
            acp_session_id: None,
            import_pending: None,
            fork_pending: None,
            terminal_rebind_pending: false,
            acp_load_session_capable: None,
            last_error_check: None,
            last_start_time: None,
            live_status_baseline: None,
            ever_confirmed_present: false,
            unknown_since: None,
            detection: DetectionState::default(),
            pending_host_env: Vec::new(),
            launch_admission: None,
            before_start_minted_for_launch: false,
            capture_started_at: None,
            launch_executable: None,
            pi_extension_launched: false,
            identity_publisher_launched: false,
            pi_session_path: None,
            last_error: LastError::default(),
            capture_poller: Default::default(),
            retroactive_capture_excludes: HashSet::new(),
            pane_dead_observed: false,
            agent_pane: PaneObservation::default(),
            auxiliary: Vec::new(),
            file_watch: None,
        }
    }

    pub fn auxiliary_presence(&self, target: &AuxiliaryTarget) -> PanePresence {
        self.auxiliary
            .iter()
            .find(|observation| &observation.target == target)
            .map_or(PanePresence::Unknown, |observation| observation.pane.state)
    }

    pub fn tool_presence(&self, name: &str) -> PanePresence {
        self.auxiliary.iter().find(|observation| matches!(&observation.target, AuxiliaryTarget::Tool { tool_name } if tool_name == name))
            .map_or(PanePresence::Unknown, |observation| observation.pane.state)
    }

    /// Inject the live FileWatchService Arc into this Instance for
    /// in-process Local fast-path notifications during subsequent storage
    /// mutations. Called by `Storage::load*` automatically; manual call
    /// sites are daemon-side recovery and TUI session-creation paths that
    /// build Instances without going through Storage::load.
    pub(crate) fn set_file_watch(
        &mut self,
        fw: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) {
        self.file_watch = Some(fw);
    }

    /// Resolve the live `Arc<FileWatchService>` for this Instance, falling
    /// back to a noop service when none was injected (ad-hoc construction
    /// or pre-injection state). Use sites pair this with `Storage::new`
    /// directly because `new_unwatched` would shadow a live injection.
    pub(super) fn resolve_file_watch(&self) -> std::sync::Arc<crate::file_watch::FileWatchService> {
        self.file_watch
            .clone()
            .unwrap_or_else(crate::file_watch::FileWatchService::noop)
    }

    /// Whether a title rename should also move the worktree directory leaf,
    /// given the resolved `session.tie_workdir_to_name` setting. True only for
    /// aoe-managed worktree sessions: non-worktree (scratch, plain tmux) and
    /// externally-attached worktrees are always a no-op. See #1927.
    pub fn tie_workdir_applies(&self, tie_setting: bool) -> bool {
        tie_setting
            && self
                .worktree_info
                .as_ref()
                .is_some_and(|w| w.managed_by_aoe)
    }

    /// Whether deleting this session has aoe-managed worktree state to clean
    /// up, covering BOTH single-repo and multi-repo (workspace) sessions.
    /// Single-repo sessions carry an aoe-managed `worktree_info`; workspace
    /// sessions carry `workspace_info` instead (with `worktree_info = None`),
    /// and opt into cleanup via `cleanup_on_delete`. Entry points use this to
    /// decide whether to set `delete_worktree`; gating on `worktree_info`
    /// alone silently leaks the workspace directory (#2363). Mirrors the TUI
    /// group-delete predicate so every surface agrees.
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
    ///
    /// The one accessor consumers read, so nothing has to know that a session
    /// gains repos two ways: created multi-repo, or converted by
    /// `attach_project` (#3103). Both end up in `workspace_info.repos`, which is
    /// the point of converting rather than keeping a second list: a repo added
    /// later is indistinguishable from one present at creation.
    pub fn all_repos(&self) -> &[WorkspaceRepo] {
        self.workspace_info
            .as_ref()
            .map(|ws| ws.repos.as_slice())
            .unwrap_or(&[])
    }

    /// Return the profile that should drive config resolution for this
    /// instance, falling back to the user's globally configured default
    /// when `source_profile` was never populated (e.g. legacy callers).
    pub fn effective_profile(&self) -> String {
        crate::session::config::effective_profile(&self.source_profile)
    }

    /// The `agent_detect_as` alias that actually applies to this session.
    pub(super) fn effective_detect_as(&self) -> std::borrow::Cow<'_, str> {
        tmux::status_rules::effective_detect_as(&self.source_profile, &self.tool, &self.detect_as)
    }

    /// Native execution identity; explicit targets never trust status aliases.
    pub(crate) fn resolved_agent(&self) -> Option<&'static crate::agents::AgentDef<'static>> {
        if self.declared_agent().is_some() {
            return None;
        }
        self.execution_agent().ok()
    }

    /// Whether structured view can carry this terminal transcript to `acp_agent`.
    /// Resolves config only for a non-claude tool paired with a claude agent.
    pub(crate) fn acp_transcript_cli_resumable(&self, acp_agent: &str) -> bool {
        crate::agents::acp_transcript_cli_resumable(
            &self.execution_identity_name(acp_agent),
            acp_agent,
        )
    }

    /// As [`Self::acp_transcript_cli_resumable`], with the caller's resolved `config`.
    pub(crate) fn acp_transcript_cli_resumable_with(
        &self,
        acp_agent: &str,
        config: &crate::session::config::SessionConfig,
    ) -> bool {
        let identity = if self.tool == "claude"
            || !matches!(acp_agent, "claude" | "claude-code")
            || self.declared_agent_in(config).is_some()
        {
            self.tool.clone()
        } else {
            // No declaration applies under `config`, so the empty-command fallback must not
            // look one up again from disk.
            let command = if self.command.is_empty() {
                self.tool_command_with(None)
            } else {
                std::borrow::Cow::Borrowed(self.command.as_str())
            };
            Self::execution_agent_for(&self.tool, &command, config)
                .map_or_else(|_| self.tool.clone(), |agent| agent.name.to_string())
        };
        crate::agents::acp_transcript_cli_resumable(&identity, acp_agent)
    }

    /// A custom tool running claude through its own `agent_acp_cmd` launcher, which reads the
    /// tool's `agent_config_dir` rather than any pinned store. Its terminal transcript loads
    /// only when the bound store is that directory; without a binding this is false.
    /// Resolves config only for such a tool.
    pub(crate) fn resumes_claude_under_own_acp_cmd(&self, acp_agent: &str) -> bool {
        self.resumes_claude_under_own_acp_cmd_with(acp_agent, || {
            let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                &self.effective_profile(),
                self.repo_config_path(),
            );
            OwnCmdConfig {
                before_session_hook: !config.host_hooks.before_session.is_empty(),
                profile_environment: config.environment,
                session: config.session,
            }
        })
    }

    /// As [`Self::resumes_claude_under_own_acp_cmd`], with the caller's resolved config.
    /// The launcher's store is previewed from the profile `environment` HOME, which is exact
    /// only without a `before_session` hook (it can change HOME at launch), so a configured
    /// hook starts fresh.
    pub(crate) fn resumes_claude_under_own_acp_cmd_with<
        C: std::borrow::Borrow<crate::session::config::SessionConfig>,
    >(
        &self,
        acp_agent: &str,
        config: impl FnOnce() -> OwnCmdConfig<C>,
    ) -> bool {
        self.own_acp_cmd_stores_with(acp_agent, config)
            .is_some_and(|probe| probe.launches_bound_store())
    }

    /// The route inputs whose probe admits [`Self::resumes_claude_under_own_acp_cmd_with`].
    /// Does no filesystem work, so a caller can run the probe later and once per distinct value.
    pub(crate) fn own_acp_cmd_stores_with<
        C: std::borrow::Borrow<crate::session::config::SessionConfig>,
    >(
        &self,
        acp_agent: &str,
        config: impl FnOnce() -> OwnCmdConfig<C>,
    ) -> Option<crate::session::capture::OwnCmdStoreProbe> {
        if self.tool == "claude"
            || acp_agent != self.tool
            || crate::session::fork::builtin_acp_registry()
                .get(&self.tool)
                .is_some()
        {
            return None;
        }
        let bound_store = self.selected_claude_bound_store()?;
        let config = config();
        let session = config.session.borrow();
        let own_cmd = session
            .agent_acp_cmd
            .get(&self.tool)
            .is_some_and(|cmd| crate::acp::agent_registry::runs_claude_adapter(cmd));
        // Launch exports the declared dir only for an explicit claude `agent_execution_as`.
        let maps_to_claude = session
            .agent_execution_as
            .get(&self.tool)
            .is_some_and(|base| {
                crate::acp::agent_profiles::resolve(base).native_config_agent == Some("claude")
            });
        if !own_cmd || !maps_to_claude || config.before_session_hook {
            return None;
        }
        // ACP builds its environment from the profile alone, never from the terminal's saved
        // hook output. As launch does, an unresolvable HOME yields no store.
        let home = super::hooks::host_home(&config.profile_environment)
            .filter(|home| home.is_absolute())?;
        let launcher_store = session.agent_config_dir_for(&self.tool, &home)?;
        // Launch's route: an implicit default store stays unexported and keeps the ambient one.
        let ambient = crate::session::capture::ambient_claude_config_dir(
            crate::session::environment::resolve_host_environment_value(
                &config.profile_environment,
                "CLAUDE_CONFIG_DIR",
            ),
            || {
                session
                    .inherit_host_environment
                    .then(|| std::env::var("CLAUDE_CONFIG_DIR").ok())
                    .flatten()
            },
            Path::new(&self.project_path),
        );
        self.acp_transcript_cli_resumable_with("claude", session)
            .then_some(crate::session::capture::OwnCmdStoreProbe {
                declared: launcher_store,
                home,
                ambient,
                bound: bound_store,
            })
    }

    /// The ACP agent a structured switch launches; resolves config at the repo
    /// only for a custom tool.
    pub(crate) fn effective_acp_agent(&self) -> String {
        self.effective_acp_agent_with(|| {
            crate::session::config::repo_config::resolve_config_with_repo_or_warn(
                &self.effective_profile(),
                self.repo_config_path(),
            )
            .session
        })
    }

    /// Mirrors `pick_agent_for_tool` minus the live registry and default-agent
    /// fallback. `session` runs only for a custom tool.
    pub(crate) fn effective_acp_agent_with<
        C: std::borrow::Borrow<crate::session::config::SessionConfig>,
    >(
        &self,
        session: impl FnOnce() -> C,
    ) -> String {
        if let Some(name) = self.agent_name.as_deref().filter(|name| !name.is_empty()) {
            return name.to_string();
        }
        if crate::session::fork::builtin_acp_registry()
            .get(&self.tool)
            .is_some()
        {
            return self.tool.clone();
        }
        let session = session();
        let session = session.borrow();
        if session
            .agent_acp_cmd
            .get(&self.tool)
            .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(&self.tool, cmd).is_ok())
        {
            return self.tool.clone();
        }
        crate::acp::inherited_acp_base(&self.tool, &session.agent_detect_as)
            .unwrap_or_else(|| self.tool.clone())
    }

    /// Name of the native agent this session executes as, so a custom tool with
    /// `agent_execution_as = "claude"` counts as claude. Falls back to the tool.
    pub(crate) fn execution_identity_name(&self, acp_agent: &str) -> String {
        if self.tool == "claude" || !matches!(acp_agent, "claude" | "claude-code") {
            return self.tool.clone();
        }
        self.resolved_agent()
            .map_or_else(|| self.tool.clone(), |agent| agent.name.to_string())
    }

    pub(crate) fn declared_agent(&self) -> Option<crate::plugin::agent_integration::DeclaredAgent> {
        if !self.may_be_declared() {
            return None;
        }
        let config = crate::session::config::repo_config::resolve_config_with_repo(
            &self.effective_profile(),
            Path::new(&self.project_path),
        )
        .ok()?;
        self.declared_agent_in(&config.session)
    }

    /// As [`Self::declared_agent`], with the caller's resolved `config`.
    pub(crate) fn declared_agent_in(
        &self,
        config: &crate::session::config::SessionConfig,
    ) -> Option<crate::plugin::agent_integration::DeclaredAgent> {
        if !self.may_be_declared() {
            return None;
        }
        if config
            .agent_command_override
            .get(&self.tool)
            .is_some_and(|command| !command.trim().is_empty())
        {
            return None;
        }
        crate::plugin::agent_integration::declared_agent(
            crate::plugin::registry(),
            &self.tool,
            config
                .custom_agents
                .iter()
                .filter(|(_, command)| !command.trim().is_empty())
                .map(|(name, _)| name.as_str()),
        )
        .filter(|agent| {
            self.command.is_empty()
                || agent.contribution().command.as_deref() == Some(self.command.as_str())
        })
    }

    /// Cheap config-free check: only a non-builtin tool the plugin registry declares can match.
    fn may_be_declared(&self) -> bool {
        crate::agents::get_agent(&self.tool).is_none()
            && crate::plugin::agent_integration::registry_declares(
                &crate::plugin::registry(),
                &self.tool,
            )
    }

    /// The pause to leave between typing into this session's pane and Enter.
    ///
    /// Only the program a declaration actually owns is paced by it. The
    /// effective identity rules out a custom wrapper, command override, stored
    /// command or ambiguous claim first, so those keep the built-in table's
    /// pace for their tool instead of inheriting another program's delay.
    pub(crate) fn send_keys_enter_delay(&self) -> u64 {
        let declared = self.declared_agent().and_then(|agent| {
            agent
                .contribution()
                .launch
                .as_ref()
                .and_then(|launch| launch.send_keys_enter_delay_ms)
        });
        declared.unwrap_or_else(|| crate::agents::send_keys_enter_delay(&self.tool))
    }

    fn declared_session_support(
        &self,
        sandboxed: bool,
    ) -> Option<(
        crate::agents::SessionCaptureSpec,
        crate::agents::SessionCaptureContext,
    )> {
        self.declared_agent()?
            .with_definition(|agent| {
                let capture = agent.session_support?.capture?;
                let context = if sandboxed {
                    capture.sandbox
                } else {
                    capture.host
                };
                (context != crate::agents::SessionCaptureContext::Unsupported
                    && self.launch_invokes_resolved_agent_directly(agent))
                .then_some((capture, context))
            })
            .flatten()
    }

    pub(crate) fn status_agent(&self) -> Option<&'static crate::agents::AgentDef<'static>> {
        self.resolved_agent()
            .or_else(|| crate::agents::get_agent(&self.tool))
            .or_else(|| crate::agents::get_agent(&self.effective_detect_as()))
    }

    pub(super) fn effective_detect_as_in<'a>(
        &'a self,
        config: &'a crate::session::config::SessionConfig,
    ) -> &'a str {
        if self.detect_as.is_empty() {
            config
                .agent_detect_as
                .get(&self.tool)
                .map_or("", String::as_str)
        } else {
            &self.detect_as
        }
    }

    pub(super) fn resolved_agent_in(
        &self,
        config: &crate::session::config::SessionConfig,
    ) -> Option<&'static crate::agents::AgentDef<'static>> {
        crate::agents::get_agent(&self.tool)
            .or_else(|| crate::agents::get_agent(self.effective_detect_as_in(config)))
    }

    /// The built-in identity used to compare capture stores and aliases.
    ///
    /// This is classification only. Capture and resume authorization still
    /// goes through `resolved_session_support` or `supports_native_resume`,
    /// which also prove the launch command and capture context.
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
            || contains_active_shell_syntax(&raw_command)
            || contains_active_shell_syntax(&launch_extra_args)
        {
            return false;
        }
        let Some(parsed_command) = parse_launch_command(&raw_command) else {
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

    /// The basename of the program this launch actually runs, which is the
    /// token a live process carries in argv.
    ///
    /// A command override names it; otherwise it is the resolved agent's own
    /// binary. Matching on this rather than on `agent.binary` is what lets the
    /// orphan scan see a renamed wrapper, which carries the pane's
    /// `AOE_INSTANCE_ID` because [`status_hook_env_prefix`] injects the marker
    /// on hook presence alone.
    pub(crate) fn launch_executable_token(&self) -> Option<String> {
        let words = parse_launch_command(&self.get_tool_command())?.words;
        Path::new(words.first()?)
            .file_name()?
            .to_str()
            .map(str::to_owned)
    }

    /// Whether a resume selector appended to this launch reaches the agent.
    /// Only a verified direct invocation or an explicit wrapper contract carries selectors.
    pub(crate) fn launch_can_carry_resume_selector(&self, agent: &crate::agents::AgentDef) -> bool {
        self.launch_carries_selector(self.resolved_agent(), agent)
    }

    /// [`Self::launch_can_carry_resume_selector`] for a caller that already holds the execution identity.
    pub(super) fn launch_carries_selector(
        &self,
        native: Option<&crate::agents::AgentDef>,
        agent: &crate::agents::AgentDef,
    ) -> bool {
        native.is_some_and(|actual| actual.name == agent.name)
            && self.managed_user_argv(agent).is_ok()
    }

    /// A direct native launch can try its stored ID without attesting every argument.
    pub(super) fn can_attempt_default_resume(&self, agent: &crate::agents::AgentDef) -> bool {
        matches!(self.resume_intent, ResumeIntent::Default)
            && self.agent_session_id.is_some()
            && self.launch_invokes_resolved_agent_directly(agent)
    }

    /// Whether this launch shape leaves Claude user hooks enabled.
    ///
    /// This is evidence for the identity publisher only. It does not change
    /// native resume support.
    pub(crate) fn hook_session_publisher_allowed_by_argv(&self) -> bool {
        if !self
            .default_selector_agent()
            .is_some_and(|agent| agent.name == "claude")
        {
            return true;
        }
        let Some(parsed_command) = parse_launch_command(&self.get_tool_command()) else {
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
        crate::agents::SessionCaptureSpec,
        crate::agents::SessionCaptureContext,
    )> {
        if self.declared_agent().is_some() {
            return self.declared_session_support(self.is_sandboxed());
        }
        let native = self.resolved_agent();
        let Some(agent) = native.or_else(|| self.legacy_default_selector_agent()) else {
            return self.declared_session_support(self.is_sandboxed());
        };
        let support = agent.session_support.as_ref()?;
        let capture = support.capture?;
        let context = if self.is_sandboxed() {
            capture.sandbox
        } else {
            capture.host
        };
        if context == crate::agents::SessionCaptureContext::Unsupported {
            return None;
        }
        // Pane-scoped publishers confine Default/Cleared wrapper capture to this pane.
        let self_attributing = capture.reads_hook_sidecar(context)
            || capture.backend == crate::agents::SessionCaptureBackend::Pi;
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
        crate::agents::SessionCaptureSpec,
        crate::agents::SessionCaptureContext,
    )> {
        let Some(active) = &self.active_execution else {
            return self.resolved_session_support();
        };
        let Some(native) = crate::agents::native_definition(&active.binding.agent) else {
            if active.binding.agent != self.tool {
                return None;
            }
            return self.declared_session_support(active.container.is_some());
        };
        let capture = native.session_support?.capture?;
        let context = if active.container.is_some() {
            capture.sandbox
        } else {
            capture.host
        };
        (context != crate::agents::SessionCaptureContext::Unsupported).then_some((capture, context))
    }

    /// Whether this pane's conversation id is published into its AoE hook sidecar. See
    /// [`crate::agents::SessionCaptureSpec::reads_hook_sidecar`].
    pub(super) fn capture_reads_hook_sidecar(&self) -> bool {
        self.source_session_support()
            .is_some_and(|(capture, context)| capture.reads_hook_sidecar(context))
    }

    pub(super) fn source_capture_backend(&self) -> Option<crate::agents::SessionCaptureBackend> {
        self.source_session_support()
            .map(|(capture, _)| capture.backend)
    }

    pub(super) fn resolved_capture_backend(&self) -> Option<crate::agents::SessionCaptureBackend> {
        self.resolved_session_support()
            .map(|(capture, _)| capture.backend)
    }

    /// Bare wrappers can receive fresh-session selectors without attesting execution.
    pub(super) fn legacy_default_selector_agent(
        &self,
    ) -> Option<&'static crate::agents::AgentDef<'static>> {
        let config = crate::session::config::profile_config::resolve_config_or_warn(
            &self.effective_profile(),
        );
        self.legacy_default_selector_agent_in(&config.session)
    }

    pub(super) fn legacy_default_selector_agent_in(
        &self,
        config: &crate::session::config::SessionConfig,
    ) -> Option<&'static crate::agents::AgentDef<'static>> {
        if matches!(
            self.resume_intent,
            ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
        ) {
            return None;
        }
        let agent = resolved_agent_for(&self.effective_profile(), &self.tool, &self.detect_as)?;
        if config.agent_execution_as.contains_key(&self.tool) {
            return None;
        }
        let command = self.get_tool_command();
        if Self::contains_active_shell_syntax(&command)
            || Self::contains_active_shell_syntax(&self.extra_args)
        {
            return None;
        }
        let parsed = parse_launch_command(&command)?;
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
            .chain(crate::plugin::bundled_agents::definitions())
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

    pub(super) fn default_selector_agent(
        &self,
    ) -> Option<&'static crate::agents::AgentDef<'static>> {
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
        if let Some(declared) = self.declared_agent() {
            return declared
                .with_definition(|agent| {
                    agent.session_support.is_some()
                        && self.launch_invokes_resolved_agent_directly(agent)
                        && self.managed_user_argv(agent).is_ok()
                })
                .unwrap_or(false);
        }
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

    /// Resolve the physical store without granting authority to read it.
    pub(super) fn sandbox_capture_store_path(&self) -> Option<std::path::PathBuf> {
        if !self.is_sandboxed() {
            return None;
        }
        // The host home, not `resolved_host_home()`: a sandboxed session
        // ignores `session.environment`, whose `HOME` entry is the host
        // command line's, never the container's. The launcher stages the store
        // under the process home (`container_config::build_container_config`,
        // `ensure_folder_trust_config_for_active_agent`), so that is where the
        // identity sidecar, the exclusivity lease and this read all have to
        // agree; resolving the profile entry instead names a directory nothing
        // mounts.
        let home = dirs::home_dir()?;
        let config = crate::session::config::profile_config::resolve_config_or_warn(
            &self.effective_profile(),
        );
        self.sandbox_capture_store_dir_for(self.resolved_agent()?, &config, &home)
    }

    pub(super) fn sandbox_capture_store_dir_for(
        &self,
        agent: &crate::agents::AgentDef,
        config: &crate::session::config::Config,
        home: &Path,
    ) -> Option<std::path::PathBuf> {
        let declared = config.session.agent_config_dir_for(&self.tool, home);
        if self.sandbox_store_generation
            >= crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION
        {
            crate::session::config::container_config::sandbox_store_dir(
                agent.name,
                home,
                declared.as_deref(),
                &self.id,
            )
            .ok()
            .flatten()
        } else {
            crate::session::config::container_config::sandbox_store_migration_paths(
                agent.name,
                home,
                declared.as_deref(),
                &self.id,
            )
            .ok()?
            .into_iter()
            .next()
            .map(|(shared, _)| shared)
        }
    }

    pub(crate) fn is_managed_capture_peer(
        &self,
        current_id: &str,
        is_current_profile: bool,
    ) -> bool {
        !(is_current_profile && self.id == current_id)
            && self.is_sandboxed()
            && self.archived_at.is_none()
            && self.trashed_at.is_none()
            && !matches!(self.status, Status::Stopped | Status::Deleting)
    }

    pub(crate) fn managed_capture_store_conflicts(
        &self,
        backend: crate::agents::SessionCaptureBackend,
        current_store: &Path,
        config: &crate::session::config::Config,
        home: &Path,
    ) -> bool {
        let agent = crate::agents::get_agent(&self.tool).or_else(|| {
            let detect_as = if self.detect_as.is_empty() {
                config.session.agent_detect_as.get(&self.tool)?.as_str()
            } else {
                &self.detect_as
            };
            crate::agents::get_agent(detect_as)
        });
        let Some(agent) = agent else { return false };
        if !self
            .source_session_support()
            .is_some_and(|(capture, _)| capture.backend == backend)
        {
            return false;
        }
        self.sandbox_capture_store_dir_for(agent, config, home)
            .and_then(|path| std::fs::canonicalize(path).ok())
            .is_none_or(|path| path == current_store)
    }

    pub(super) fn sandbox_capture_store_dir(&self) -> Option<std::path::PathBuf> {
        crate::migrations::v033_isolate_sandbox_content::instance_ready(self)
            .ok()?
            .then(|| self.sandbox_capture_store_path())
            .flatten()
    }
    pub fn is_sub_session(&self) -> bool {
        self.parent_session_id.is_some()
    }

    /// Sessions nest one level deep, so a sub-session cannot parent another.
    pub fn ensure_can_parent(&self) -> Result<(), NestedSubSession> {
        if self.is_sub_session() {
            Err(NestedSubSession)
        } else {
            Ok(())
        }
    }

    pub fn is_sandboxed(&self) -> bool {
        self.sandbox_info.as_ref().is_some_and(|s| s.enabled)
    }

    /// The repo this session groups under: the worktree's main repo when
    /// present (so all branches of a repo group together), else the project
    /// path. Shared by sidebar project grouping and new-session prefill so
    /// the "which directory does this session belong to" rule lives in one
    /// place.
    pub fn repo_path(&self) -> &str {
        self.worktree_info
            .as_ref()
            .map(|w| w.main_repo_path.as_str())
            .unwrap_or(&self.project_path)
    }

    pub fn is_yolo_mode(&self) -> bool {
        self.yolo_mode
            || (!self.is_structured()
                && self.declared_agent().is_some_and(|agent| {
                    matches!(
                        agent
                            .contribution()
                            .launch
                            .as_ref()
                            .and_then(|launch| launch.yolo.as_ref()),
                        Some(aoe_plugin_api::agent_integration::AgentYoloDeclaration::Always)
                    )
                }))
    }

    /// True when this session renders in the structured (ACP) view. Rows
    /// damaged by pre-fix writers are healed on reload by the server's
    /// structured row repair path.
    pub fn is_structured(&self) -> bool {
        self.view == View::Structured
    }

    /// Require an asserted or worker-proven native store before planning a terminal handoff.
    pub(crate) fn terminal_keep_context_state(
        &self,
        worker: Option<&ExecutionBinding>,
    ) -> Result<ConversationState> {
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
        let unresolved = UNBOUND_STORE_HINT;
        let persisted = self.persisted_observed_store(&sid);
        let resolved = match asserted {
            Some(asserted) => Ok(Some(asserted)),
            None => worker.or(persisted).map_or(Ok(None), |worker| {
                self.resolved_handoff_binding(&sid, worker)
            }),
        };
        let binding = resolved
            .context(unresolved)?
            .ok_or_else(|| anyhow::anyhow!(unresolved))?;
        Ok(ConversationState {
            tool: self.tool.clone(),
            session_id: Some(sid.clone()),
            binding: Some(binding.clone()),
            intent: ResumeIntent::Use(sid),
            resume_binding: Some(binding),
            active: None,
            pi_session_path: None,
        })
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

    /// The bound store of [`Self::selected_claude_store_pin`], without resolving the legacy
    /// route marker, so callers holding their own resolved config read no config from disk.
    pub(crate) fn selected_claude_bound_store(&self) -> Option<std::path::PathBuf> {
        let (_, execution) = self.selected_claude_conversation()?;
        execution.stores.first().cloned()
    }

    /// Legacy route markers follow current launch configuration so alias changes stay visible.
    pub(crate) fn selected_claude_store_pin(
        &self,
    ) -> Option<crate::session::capture::ClaudeStorePin> {
        let (_, execution) = self.selected_claude_conversation()?;
        let mut pin = crate::session::capture::ClaudeStorePin::of(execution)?;
        if pin.exported_default_store.is_none() {
            let host_env = self.resolved_host_environment();
            let declared = self.declared_agent_config_dir_for(&self.tool);
            let ambient = crate::hooks::resolve_config_dir_override("CLAUDE_CONFIG_DIR", &host_env)
                .map(std::path::PathBuf::from);
            let explicit = super::hooks::host_home(&host_env).is_some_and(|home| {
                crate::session::capture::is_explicit_claude_store_route(
                    &pin.store,
                    &home,
                    declared.as_deref(),
                    ambient.as_deref(),
                )
            });
            pin.exported_default_store = Some(explicit);
        }
        Some(pin)
    }

    /// Attest the store route an observed launch applied onto both bindings
    /// this session persists (#4127). A resume carries whichever of them its
    /// intent adopted, and a sandboxed session routes to an isolated store the
    /// host's Claude namespace says nothing about, so it attests nothing.
    /// Idempotent: the second call finds the marker written and returns false.
    pub(crate) fn attest_launch_default_store(
        &mut self,
        observed: Option<&ExecutionBinding>,
    ) -> bool {
        if self.is_sandboxed() {
            return false;
        }
        let Some(observed) = observed else {
            return false;
        };
        let mut attested = false;
        for binding in [
            self.agent_session_binding.as_mut(),
            self.resume_binding.as_mut(),
        ]
        .into_iter()
        .flatten()
        {
            attested |= super::execution::attest_observed_default_store(binding, Some(observed));
        }
        attested
    }

    /// Persist the store a worker observed for the ACP id it was just assigned,
    /// so a dormant session can still prove it at the next terminal switch.
    pub(crate) fn record_observed_acp_store(
        &mut self,
        sid: &str,
        observed: Option<&ExecutionBinding>,
    ) -> bool {
        let Some(observed) = observed.filter(|o| o.agent == "claude" && o.filesystem == "host")
        else {
            return false;
        };
        if self.is_sandboxed()
            || self.agent_session_binding.as_ref().is_some_and(|binding| {
                binding.session_id == sid
                    && (binding.provenance != ConversationProvenance::Observed
                        || binding.execution.as_ref().is_some_and(|known| {
                            known.stores == observed.stores && known.cwd == observed.cwd
                        }))
            })
        {
            return false;
        }
        self.agent_session_binding = Some(ConversationBinding {
            session_id: sid.to_owned(),
            execution: Some(observed.clone()),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });
        true
    }

    /// The persisted observed store for `sid`, only while its transcript still exists.
    fn persisted_observed_store(&self, sid: &str) -> Option<&ExecutionBinding> {
        let execution = self
            .agent_session_binding
            .as_ref()
            .filter(|binding| {
                binding.session_id == sid && binding.provenance == ConversationProvenance::Observed
            })?
            .execution
            .as_ref()?;
        let [store] = execution.stores.as_slice() else {
            return None;
        };
        store
            .join("projects")
            .join(crate::session::capture::encode_claude_project_path(
                &execution.cwd.to_string_lossy(),
            ))
            .join(format!("{sid}.jsonl"))
            .is_file()
            .then_some(execution)
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
) -> Option<&'static crate::agents::AgentDef<'static>> {
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

    /// A custom tool resumes a terminal transcript only when it declares it
    /// executes as claude (#5938).
    #[test]
    #[serial_test::serial]
    fn custom_tool_executing_as_claude_is_transcript_resumable() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        const PROFILE: &str = "custom-claude-resumable-test";
        let mut inst = Instance::new("custom", home.path().join("project").to_str().unwrap());
        inst.source_profile = PROFILE.to_string();
        inst.tool = "corp-claude".to_string();
        inst.command = "corp-wrapper --serve".to_string();
        assert!(!inst.acp_transcript_cli_resumable("claude"));

        declare_execution_aliases(PROFILE, &[("corp-claude", "claude")], home.path());
        assert!(inst.acp_transcript_cli_resumable("claude"));
        assert!(inst.acp_transcript_cli_resumable("claude-code"));
        assert!(!inst.acp_transcript_cli_resumable("codex"));
    }

    /// A custom tool whose command runs claude directly needs no declaration.
    #[test]
    #[serial_test::serial]
    fn custom_tool_running_claude_directly_is_transcript_resumable() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        const PROFILE: &str = "custom-claude-direct-test";
        let mut inst = Instance::new("custom", home.path().join("project").to_str().unwrap());
        inst.source_profile = PROFILE.to_string();
        inst.tool = "my-claude".to_string();
        inst.command = "corp-wrapper --serve".to_string();
        assert!(!inst.acp_transcript_cli_resumable("claude"));

        inst.command = "claude --model sonnet".to_string();
        assert!(inst.acp_transcript_cli_resumable("claude"));
        assert!(!inst.acp_transcript_cli_resumable("codex"));
    }

    /// The declaration lookup honors the caller's config instead of resolving
    /// repository config again, so list rows sharing one cached config stay consistent.
    #[test]
    #[serial_test::serial]
    fn declared_agent_in_uses_the_passed_config() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _featured = crate::session::test_support::EnvGuard::unset(&["AOE_FEATURED_INDEX_PATH"]);
        let manifest = r#"
id = "acme.wrapped-provider"
name = "Integration test"
version = "1.0.0"
api_version = 19
capabilities = ["session.integration"]

[[agent_integrations]]
tool = "wrapped-tool"
command = "wrapped-tool"
provider_versions = ["^18"]
replies = true

[[agent_integrations]]
tool = "bare-tool"
command = "claude"
provider_versions = ["^18"]
replies = true
"#;
        let dir = crate::plugin::plugins_dir()
            .unwrap()
            .join("acme.wrapped-provider");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("aoe-plugin.toml"), manifest).unwrap();
        crate::session::update_config(|config| {
            config.plugins.insert(
                "acme.wrapped-provider".into(),
                crate::session::PluginConfig {
                    grant: Some(crate::session::CapabilityGrant {
                        manifest_hash: aoe_plugin_api::PluginManifest::hash_bytes(
                            manifest.as_bytes(),
                        ),
                        capabilities: vec!["session.integration".into()],
                        granted_at: crate::time::Timestamp::now(),
                    }),
                    ..Default::default()
                },
            );
        })
        .unwrap();
        crate::plugin::reload_registry();

        let mut inst = Instance::new("declared", home.path().join("project").to_str().unwrap());
        inst.tool = "wrapped-tool".to_string();
        inst.command = "wrapped-tool".to_string();
        assert!(inst.declared_agent().is_some());

        let mut overridden = crate::session::config::SessionConfig::default();
        overridden
            .agent_command_override
            .insert("wrapped-tool".into(), "other --serve".into());
        let default = crate::session::config::SessionConfig::default();
        assert!(inst.declared_agent_in(&default).is_some());
        assert!(inst.declared_agent_in(&overridden).is_none());

        // The resumability preview follows the passed config, not the one on disk.
        overridden
            .agent_execution_as
            .insert("wrapped-tool".into(), "claude".into());
        overridden.agent_config_dir.insert(
            "wrapped-tool".into(),
            home.path().join(".claude").to_str().unwrap().into(),
        );
        assert!(!inst.acp_transcript_cli_resumable_with("claude", &default));
        assert!(inst.acp_transcript_cli_resumable_with("claude", &overridden));

        // An empty command takes its fallback from the passed config: a disk declaration
        // of `claude` must not leak in once the passed config shadows it.
        let mut bare = Instance::new("bare", home.path().join("project").to_str().unwrap());
        bare.tool = "bare-tool".to_string();
        assert!(bare.declared_agent().is_some());
        let mut shadowed = crate::session::config::SessionConfig::default();
        shadowed
            .agent_command_override
            .insert("bare-tool".into(), "other --serve".into());
        assert!(!bare.acp_transcript_cli_resumable_with("claude", &shadowed));
    }

    /// The switch previews resolve the same ACP target as the daemon launch.
    #[test]
    #[serial_test::serial]
    fn wrapper_inheriting_claude_previews_as_resumable() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        const PROFILE: &str = "effective-acp-agent-test";
        let mut inst = Instance::new("custom", home.path().join("project").to_str().unwrap());
        inst.source_profile = PROFILE.to_string();
        inst.tool = "corp-claude".to_string();
        inst.command = "corp-wrapper --serve".to_string();
        declare_execution_aliases(PROFILE, &[("corp-claude", "claude")], home.path());
        let path =
            crate::session::config::profile_config::get_profile_config_path(PROFILE).unwrap();
        let mut config: toml::Table =
            toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        config["session"].as_table_mut().unwrap().insert(
            "agent_detect_as".into(),
            toml::Value::Table(toml::Table::from_iter([(
                "corp-claude".into(),
                toml::Value::String("claude".into()),
            )])),
        );
        std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();

        assert_eq!(inst.effective_acp_agent(), "claude");
        assert!(inst.acp_transcript_cli_resumable(&inst.effective_acp_agent()));

        inst.agent_name = Some("codex".to_string());
        assert_eq!(inst.effective_acp_agent(), "codex");
        assert!(!inst.acp_transcript_cli_resumable(&inst.effective_acp_agent()));
    }

    /// A custom tool with its own `agent_acp_cmd` loads its terminal transcript only
    /// from the store its launcher reads.
    #[test]
    #[serial_test::serial]
    fn own_acp_cmd_claude_tool_resumes_only_from_its_config_dir() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        const PROFILE: &str = "own-acp-cmd-claude-test";
        let own_store = home.path().join(".claude-work");
        let write_config = |execution_as: bool, acp_cmd: &str| {
            let mut session = toml::Table::new();
            let entry = |value: &str| {
                toml::Value::Table(toml::Table::from_iter([(
                    "claude-work".into(),
                    toml::Value::String(value.into()),
                )]))
            };
            if execution_as {
                session.insert("agent_execution_as".into(), entry("claude"));
            }
            session.insert("agent_acp_cmd".into(), entry(acp_cmd));
            session.insert(
                "agent_config_dir".into(),
                entry(own_store.to_str().unwrap()),
            );
            let path =
                crate::session::config::profile_config::get_profile_config_path(PROFILE).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let config = toml::Table::from_iter([("session".into(), toml::Value::Table(session))]);
            std::fs::write(path, toml::to_string(&config).unwrap()).unwrap();
        };
        let instance = |tool: &str, store: Option<&std::path::Path>| {
            let mut inst = Instance::new("own", home.path().join("project").to_str().unwrap());
            inst.source_profile = PROFILE.to_string();
            inst.tool = tool.to_string();
            inst.command = "launcher".to_string();
            if let Some(store) = store {
                inst.agent_session_id = Some("sid".into());
                inst.agent_session_binding = Some(ConversationBinding {
                    session_id: "sid".into(),
                    execution: Some(ExecutionBinding {
                        agent: "claude".into(),
                        stores: vec![store.to_path_buf()],
                        configuration: Vec::new(),
                        cwd: home.path().to_path_buf(),
                        cwd_filesystem: "host".into(),
                        filesystem: "host".into(),
                        exported_default_store: Some(true),
                    }),
                    provenance: ConversationProvenance::Observed,
                    transcript_path: None,
                });
            }
            inst
        };

        write_config(true, "claude-agent-acp");
        let bound = instance("claude-work", Some(&own_store));
        assert!(bound.resumes_claude_under_own_acp_cmd("claude-work"));
        assert!(!bound.resumes_claude_under_own_acp_cmd("claude"));
        assert!(!bound.resumes_claude_under_own_acp_cmd("codex"));
        let other_store = home.path().join(".claude");
        assert!(!instance("claude-work", Some(&other_store))
            .resumes_claude_under_own_acp_cmd("claude-work"));
        assert!(!instance("claude-work", None).resumes_claude_under_own_acp_cmd("claude-work"));
        for builtin in ["claude", "claude-code"] {
            assert!(!instance(builtin, Some(&own_store)).resumes_claude_under_own_acp_cmd(builtin));
        }

        write_config(false, "claude-agent-acp");
        assert!(!instance("claude-work", Some(&own_store))
            .resumes_claude_under_own_acp_cmd("claude-work"));
        // Launch exports the declared dir only under an explicit `agent_execution_as`, so a
        // direct `claude` command alone does not qualify.
        let mut direct = instance("claude-work", Some(&own_store));
        direct.command = "claude --model sonnet".to_string();
        assert!(!direct.resumes_claude_under_own_acp_cmd("claude-work"));
        write_config(true, "claude-agent-acp");
        assert!(direct.resumes_claude_under_own_acp_cmd("claude-work"));

        // An adapter not known to load Claude session ids starts fresh.
        for (cmd, expected) in [
            ("my-adapter --stdio", false),
            ("npx -y claude-code-acp", true),
        ] {
            write_config(true, cmd);
            assert_eq!(
                instance("claude-work", Some(&own_store))
                    .resumes_claude_under_own_acp_cmd("claude-work"),
                expected,
                "{cmd}"
            );
        }

        // A default-store declaration is not exported at launch, so the store comes from the
        // ambient `CLAUDE_CONFIG_DIR`: the profile `environment`, else the daemon's when
        // `inherit_host_environment` is on.
        let default_store = home.path().join(".claude");
        let elsewhere = home.path().join("elsewhere");
        let same = format!("CLAUDE_CONFIG_DIR={}", default_store.display());
        let other = format!("CLAUDE_CONFIG_DIR={}", elsewhere.display());
        // (profile environment, inherit, daemon CLAUDE_CONFIG_DIR, resumes)
        let cases: [(Option<&str>, bool, Option<&str>, bool); 8] = [
            (None, false, None, true),
            (Some(&same), false, None, true),
            (Some(&other), false, None, false),
            (Some(&same), true, Some("/daemon"), true),
            (None, true, Some("/daemon"), false),
            (None, true, Some(default_store.to_str().unwrap()), true),
            (None, false, Some("/daemon"), true),
            (Some(&same), false, Some("/daemon"), true),
        ];
        for (environment, inherit, daemon, expected) in cases {
            let mut session = toml::Table::new();
            let entry = |value: &str| {
                toml::Value::Table(toml::Table::from_iter([(
                    "claude-work".into(),
                    toml::Value::String(value.into()),
                )]))
            };
            session.insert("agent_execution_as".into(), entry("claude"));
            session.insert("agent_acp_cmd".into(), entry("claude-agent-acp"));
            session.insert(
                "agent_config_dir".into(),
                entry(default_store.to_str().unwrap()),
            );
            session.insert(
                "inherit_host_environment".into(),
                toml::Value::Boolean(inherit),
            );
            let mut config =
                toml::Table::from_iter([("session".into(), toml::Value::Table(session))]);
            if let Some(environment) = environment {
                config.insert(
                    "environment".into(),
                    toml::Value::String(environment.into()),
                );
            }
            let path =
                crate::session::config::profile_config::get_profile_config_path(PROFILE).unwrap();
            std::fs::write(path, toml::to_string(&config).unwrap()).unwrap();
            let _daemon = match daemon {
                Some(value) => {
                    crate::session::test_support::EnvGuard::set(&[("CLAUDE_CONFIG_DIR", value)])
                }
                None => crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]),
            };
            assert_eq!(
                instance("claude-work", Some(&default_store))
                    .resumes_claude_under_own_acp_cmd("claude-work"),
                expected,
                "{environment:?} inherit={inherit} daemon={daemon:?}"
            );
        }
    }

    /// The launcher's `~` store resolves against the profile `environment` HOME, as launch
    /// does, and a `before_session` hook (which can move HOME at launch) starts fresh. The
    /// terminal's saved hook HOME plays no part.
    #[test]
    #[serial_test::serial]
    fn own_acp_cmd_store_resolves_against_the_launch_home() {
        let home = tempfile::tempdir().unwrap();
        let _isolation = crate::session::test_support::isolate_app_dir_at(home.path());
        let launch_home = home.path().join("launch-home");
        let configure = |hook: bool| {
            crate::session::config::update_config(|c| {
                let tool = || "claude-work".to_string();
                c.session.agent_execution_as.insert(tool(), "claude".into());
                c.session
                    .agent_acp_cmd
                    .insert(tool(), "claude-agent-acp".into());
                c.session
                    .agent_config_dir
                    .insert(tool(), "~/stores/work".into());
                c.environment = vec![format!("HOME={}", launch_home.display())];
                c.host_hooks.before_session = if hook {
                    vec!["echo HOME=/elsewhere".into()]
                } else {
                    Vec::new()
                };
            })
            .unwrap();
        };
        let bound_to = |store: std::path::PathBuf| {
            let mut inst = Instance::new("own", home.path().join("project").to_str().unwrap());
            inst.source_profile = "default".to_string();
            inst.tool = "claude-work".to_string();
            inst.agent_session_id = Some("sid".into());
            inst.agent_session_binding = Some(ConversationBinding {
                session_id: "sid".into(),
                execution: Some(ExecutionBinding {
                    agent: "claude".into(),
                    stores: vec![store],
                    configuration: Vec::new(),
                    cwd: home.path().to_path_buf(),
                    cwd_filesystem: "host".into(),
                    filesystem: "host".into(),
                    exported_default_store: Some(true),
                }),
                provenance: ConversationProvenance::Observed,
                transcript_path: None,
            });
            inst
        };
        let launch_store = launch_home.join("stores/work");
        let ambient_store = home.path().join("stores/work");

        configure(false);
        assert!(bound_to(launch_store.clone()).resumes_claude_under_own_acp_cmd("claude-work"));
        assert!(!bound_to(ambient_store).resumes_claude_under_own_acp_cmd("claude-work"));

        configure(true);
        assert!(!bound_to(launch_store.clone()).resumes_claude_under_own_acp_cmd("claude-work"));

        configure(false);
        let saved_home = home.path().join("saved-home");
        let with_saved_home = |mut inst: Instance| {
            inst.pending_host_env = vec![("HOME".into(), saved_home.display().to_string())];
            inst
        };
        assert!(!with_saved_home(bound_to(saved_home.join("stores/work")))
            .resumes_claude_under_own_acp_cmd("claude-work"));
        assert!(
            with_saved_home(bound_to(launch_store)).resumes_claude_under_own_acp_cmd("claude-work")
        );
    }

    /// A session stored with the fallback's own command keeps its capture after
    /// an approved declaration takes over the OMP listing.
    #[test]
    #[serial_test::serial]
    fn recorded_native_omp_identity_keeps_its_capture_after_takeover() {
        let dir = tempfile::tempdir().unwrap();
        // Declared first so it reloads after the app directory is restored.
        let _reload = crate::plugin::ReloadRegistryOnDrop;
        let _app = crate::session::test_support::isolate_app_dir_at(dir.path());
        crate::session::test_support::install_omp_takeover("omp-next");
        assert!(crate::agents::get_agent("omp").is_none());

        let mut instance = Instance::new("recorded", dir.path().to_str().unwrap());
        instance.tool = "omp".into();
        instance.command = "omp-next".into();
        instance.active_execution = Some(crate::session::instance::execution::ActiveExecution {
            launch_id: "fixture-launch".into(),
            binding: crate::session::instance::execution::ExecutionBinding {
                agent: "omp".into(),
                stores: vec![dir.path().join(".omp")],
                configuration: Vec::new(),
                cwd: dir.path().to_path_buf(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            },
            capture: None,
            container: None,
        });

        let (capture, context) = instance
            .source_session_support()
            .expect("a recorded native omp execution keeps its capture");
        assert_eq!(capture.backend, crate::agents::SessionCaptureBackend::Omp);
        assert_eq!(context, crate::agents::SessionCaptureContext::PaneScoped);
    }

    /// A wrapper whose bare command is another native agent's binary is not a
    /// bare wrapper of its own tool. The bundled definitions count: `omp` left
    /// the native table, and a wrapper naming it must not become a `claude`
    /// selector just because only `AGENTS` was searched.
    #[test]
    fn a_bare_command_naming_the_bundled_omp_binary_is_not_this_tool() {
        let mut inst = Instance::new("claude", "/tmp");
        inst.command = "omp".into();
        assert!(inst.legacy_default_selector_agent().is_none());

        inst.command = "claude".into();
        assert!(
            inst.legacy_default_selector_agent().is_some(),
            "a bare native wrapper keeps its selector"
        );
    }

    fn worktree(main_repo_path: &str) -> WorktreeInfo {
        WorktreeInfo {
            branch: "feature/abc".to_string(),
            main_repo_path: main_repo_path.to_string(),
            managed_by_aoe: true,
            created_at: Timestamp::now(),
            base_branch: None,
        }
    }

    #[test]
    fn terminal_keep_context_state_carries_asserted_native_binding() {
        let mut inst = Instance::new("claude", "/tmp");
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".to_string());
        let binding = ConversationBinding {
            session_id: "sid-abc".into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec!["/tmp/claude-store".into()],
                configuration: Vec::new(),
                cwd: "/tmp".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: ConversationProvenance::Asserted,
            transcript_path: None,
        };
        inst.resume_intent = ResumeIntent::Use("sid-abc".into());
        inst.resume_binding = Some(binding.clone());

        let planned = inst.terminal_keep_context_state(None).unwrap();

        assert_eq!(planned.binding.as_ref(), Some(&binding));
        assert_eq!(planned.resume_binding.as_ref(), Some(&binding));
        assert_eq!(planned.session_id.as_deref(), Some("sid-abc"));
        assert_eq!(planned.intent, ResumeIntent::Use("sid-abc".to_string()));
    }

    #[test]
    #[serial_test::serial]
    fn terminal_keep_context_state_refuses_unproved_store() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let mut inst = Instance::new("claude-unproved", "/tmp");
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".into());

        assert!(inst.terminal_keep_context_state(None).is_err());
    }

    /// A legacy binding carries no route marker, and the marker it would be
    /// given is a guess about the configuration as it stands right now. It
    /// stays derived at every read instead, so a reconfigured alias is
    /// observed rather than frozen into the row.
    #[test]
    #[serial_test::serial]
    fn legacy_claude_default_binding_derives_implicit_routing_without_freezing_it() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _env = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let sid = "11111111-1111-4111-8111-111111111111";
        let mut inst = Instance::new("legacy-acp-default", temp.path().to_str().unwrap());
        inst.tool = "claude".into();
        inst.resume_intent = ResumeIntent::Use(sid.into());
        inst.resume_binding = Some(ConversationBinding {
            session_id: sid.into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec![temp.path().join(".claude")],
                configuration: Vec::new(),
                cwd: temp.path().to_path_buf(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });

        assert_eq!(
            inst.selected_claude_store_pin()
                .unwrap()
                .exported_default_store,
            Some(false)
        );
        assert_eq!(
            inst.resume_binding
                .as_ref()
                .unwrap()
                .execution
                .as_ref()
                .unwrap()
                .exported_default_store,
            None,
            "the derived route must not be persisted into the binding"
        );
    }

    #[test]
    #[serial_test::serial]
    fn explicit_alias_of_default_store_derives_exported_routing() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _env = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let profile = "alias-routing-provenance";
        let work = temp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::os::unix::fs::symlink(&work, temp.path().join(".claude")).unwrap();
        let config_path =
            crate::session::config::profile_config::get_profile_config_path(profile).unwrap();
        std::fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        let declare = |directory: &std::path::Path| {
            std::fs::write(
                &config_path,
                format!(
                    "[session.agent_config_dir]\nclaude = {:?}\n",
                    directory.to_str().unwrap()
                ),
            )
            .unwrap();
        };
        declare(&work);
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take(profile);
        let sid = "11111111-1111-4111-8111-111111111111";
        let mut inst = Instance::new("alias", temp.path().to_str().unwrap());
        inst.source_profile = profile.into();
        inst.tool = "claude".into();
        inst.resume_intent = ResumeIntent::Use(sid.into());
        inst.resume_binding = Some(ConversationBinding {
            session_id: sid.into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec![work],
                configuration: Vec::new(),
                cwd: temp.path().to_path_buf(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });

        assert_eq!(
            inst.selected_claude_store_pin()
                .unwrap()
                .exported_default_store,
            Some(true)
        );
        // Declaring the built-in store outright is no longer an alias, and the
        // next read has to say so.
        declare(&temp.path().join(".claude"));
        assert_eq!(
            inst.selected_claude_store_pin()
                .unwrap()
                .exported_default_store,
            Some(false)
        );
    }

    /// The ambient `CLAUDE_CONFIG_DIR` names the default store exactly like a
    /// declared alias does, and a legacy binding is re-derived against it: a
    /// wrapper that exports `<home>/.claude` has selected that store, so
    /// dropping the export on the next read would route the conversation into
    /// the bare one.
    #[test]
    #[serial_test::serial]
    fn ambient_default_store_derives_exported_routing() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let default = temp.path().join(".claude");
        let sid = "11111111-1111-4111-8111-111111111111";
        let mut inst = Instance::new("ambient-acp-default", temp.path().to_str().unwrap());
        inst.tool = "claude".into();
        inst.resume_intent = ResumeIntent::Use(sid.into());
        inst.resume_binding = Some(ConversationBinding {
            session_id: sid.into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec![default.clone()],
                configuration: Vec::new(),
                cwd: temp.path().to_path_buf(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        });

        let _env = crate::session::test_support::EnvGuard::set(&[(
            "CLAUDE_CONFIG_DIR",
            default.as_os_str(),
        )]);
        assert_eq!(
            inst.selected_claude_store_pin()
                .unwrap()
                .exported_default_store,
            Some(true)
        );
        assert_eq!(
            inst.resume_binding
                .as_ref()
                .unwrap()
                .execution
                .as_ref()
                .unwrap()
                .exported_default_store,
            None,
            "the derived route must not be persisted into the binding"
        );
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
        inst.last_error.set("test error".to_string());
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
            created_at: Timestamp::now(),
            cleanup_on_delete: true,
        });
        assert!(ws.has_managed_worktree_or_workspace());
        ws.workspace_info.as_mut().unwrap().cleanup_on_delete = false;
        assert!(!ws.has_managed_worktree_or_workspace());
    }

    #[test]
    fn native_resume_requires_a_direct_local_builtin_launch() {
        const PROFILE: &str = "resume-custom-launch-test";
        // The table resolves the agent through the profile config and the
        // detection registry, both of which read the app dir and reinstall
        // themselves; the serialised tests next door do the same. Without an
        // app dir of its own this test reads whichever one a concurrent test
        // left behind, and the resolution flips on cases that are decided by
        // the environment rather than by the argv under test.
        let _app = crate::session::test_support::isolate_app_dir();
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
        let planned = inst.terminal_keep_context_state(Some(&worker)).unwrap();

        assert_eq!(
            planned
                .binding
                .as_ref()
                .and_then(|binding| binding.execution.as_ref())
                .and_then(|execution| execution.stores.first())
                .map(std::path::PathBuf::as_path),
            Some(path_identity(&session_home.join(".claude")).as_path())
        );
    }

    #[test]
    #[serial_test::serial]
    fn handoff_uses_the_persisted_observed_store_only_while_its_transcript_exists() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _config_dir = crate::session::test_support::EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
        let _claude = crate::session::test_support::install_login_shell_path_command(
            temp.path(),
            "claude",
            "#!/bin/sh\nexit 0\n",
        );
        let mut inst = Instance::new("claude-dormant", "/tmp");
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".to_string());
        let worker = inst.resolve_native_execution(None).unwrap().binding;
        assert!(inst.record_observed_acp_store("sid-abc", Some(&worker)));
        assert!(!inst.record_observed_acp_store("sid-abc", Some(&worker)));
        let moved = ExecutionBinding {
            stores: vec![temp.path().join("other-store")],
            ..worker.clone()
        };
        assert!(inst.record_observed_acp_store("sid-abc", Some(&moved)));
        assert!(inst.record_observed_acp_store("sid-abc", Some(&worker)));

        assert!(inst.terminal_keep_context_state(None).is_err());

        let transcript_dir = worker.stores[0].join("projects").join(
            crate::session::capture::encode_claude_project_path(&worker.cwd.to_string_lossy()),
        );
        std::fs::create_dir_all(&transcript_dir).unwrap();
        std::fs::write(transcript_dir.join("sid-abc.jsonl"), "").unwrap();
        let planned = inst.terminal_keep_context_state(None).unwrap();
        assert_eq!(planned.session_id.as_deref(), Some("sid-abc"));

        inst.acp_session_id = Some("sid-new".to_string());
        assert!(inst.terminal_keep_context_state(None).is_err());
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

        assert!(inst.terminal_keep_context_state(None).is_err());
    }

    /// #4127: the launch's route reaches both bindings a resume may persist,
    /// and a sandboxed session, whose store is the container's, attests nothing.
    #[test]
    fn attest_launch_default_store_stamps_both_bindings_and_skips_a_sandbox() {
        let temp = tempfile::tempdir().unwrap();
        let store = temp.path().join("claude");
        std::fs::create_dir_all(&store).unwrap();
        let legacy = || ConversationBinding {
            session_id: "sid-abc".into(),
            execution: Some(ExecutionBinding {
                agent: "claude".into(),
                stores: vec![store.clone()],
                configuration: Vec::new(),
                cwd: temp.path().to_path_buf(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: ConversationProvenance::Observed,
            transcript_path: None,
        };
        let observed = ExecutionBinding {
            exported_default_store: Some(true),
            ..legacy().execution.unwrap()
        };
        let markers = |inst: &Instance| {
            [
                inst.agent_session_binding.as_ref(),
                inst.resume_binding.as_ref(),
            ]
            .into_iter()
            .flatten()
            .map(|binding| {
                binding
                    .execution
                    .as_ref()
                    .and_then(|execution| execution.exported_default_store)
            })
            .collect::<Vec<_>>()
        };

        let mut inst = Instance::new("claude", temp.path().to_str().unwrap());
        inst.agent_session_binding = Some(legacy());
        inst.resume_binding = Some(legacy());
        assert_eq!(markers(&inst), vec![None, None]);
        assert!(inst.attest_launch_default_store(Some(&observed)));
        assert_eq!(markers(&inst), vec![Some(true), Some(true)]);
        assert!(!inst.attest_launch_default_store(Some(&observed)));

        let mut sandboxed = Instance::new("claude", temp.path().to_str().unwrap());
        sandboxed.sandbox_info = Some(crate::session::SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "alpine".to_string(),
            container_name: "attest-sandbox".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            before_start_hook_env: Vec::new(),
            container_workdir: None,
        });
        sandboxed.agent_session_binding = Some(legacy());
        assert!(!sandboxed.attest_launch_default_store(Some(&observed)));
        assert_eq!(markers(&sandboxed), vec![None]);
    }

    /// The store this session's capture reads is the one the container mounts,
    /// and a sandboxed session ignores `session.environment`; so a `HOME`
    /// entry in the profile must not move the store off the host home the
    /// launcher stages it under. Reading the profile entry instead leaves the
    /// identity sidecar with a bind dir nothing mounted (no sidecar is ever
    /// produced) and hands the exclusivity lease a path that does not exist,
    /// which reports "not exclusive" and defers capture for good.
    #[test]
    #[serial_test::serial]
    fn sandbox_capture_store_reads_the_store_the_container_mounts() {
        let temp = tempfile::tempdir().unwrap();
        let host_home = temp.path().join("host-home");
        let profile_home = temp.path().join("profile-home");
        std::fs::create_dir_all(&host_home).unwrap();
        std::fs::create_dir_all(&profile_home).unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _home = crate::session::test_support::isolate_home(&host_home);

        let profile = "capture-store-home";
        std::fs::write(
            crate::session::get_profile_dir(profile)
                .unwrap()
                .join("config.toml"),
            format!("environment = [\"HOME={}\"]\n", profile_home.display()),
        )
        .unwrap();

        let mut inst = Instance::new("gemini", temp.path().to_str().unwrap());
        inst.tool = "gemini".to_string();
        inst.source_profile = profile.to_string();
        inst.sandbox_info = Some(test_sandbox(&inst.id, Some("/workspace/session")));

        let mounted = crate::session::config::container_config::sandbox_store_dir(
            "gemini", &host_home, None, &inst.id,
        )
        .unwrap()
        .expect("gemini mounts a private store");
        assert_eq!(inst.sandbox_capture_store_path(), Some(mounted.clone()));

        // The pair that has to agree with this read: an exclusive-store lease
        // taken over a path the launcher never stages defers capture forever.
        std::fs::create_dir_all(&mounted).unwrap();
        assert_eq!(
            std::fs::canonicalize(&mounted).unwrap(),
            std::fs::canonicalize(inst.sandbox_capture_store_path().unwrap()).unwrap()
        );
    }
}
