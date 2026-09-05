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
            pending_initial_turn_attachments: Vec::new(),
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
            session_id_poller_retry_after: None,
            retroactive_capture_excludes: HashSet::new(),
            pane_dead_observed: false,
            file_watch: None,
        }
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
    ///
    /// `detect_as` is resolved once at session build and persisted, so it is
    /// empty on a row created before its tool gained an
    /// `[session.agent_detect_as]` entry. Treat the stored field as a cache
    /// and let [`tmux::status_rules::effective_detect_as`] consult the live
    /// registry when it is empty, the same way the pane detector, hook
    /// reconciliation, and the status-change log line already do (#3398).
    pub(super) fn effective_detect_as(&self) -> std::borrow::Cow<'_, str> {
        tmux::status_rules::effective_detect_as(&self.source_profile, &self.tool, &self.detect_as)
    }

    /// The built-in agent backing this session: its own tool when that names
    /// one, else the agent its `agent_detect_as` alias points at.
    ///
    /// Every launch-time consumer resolves through here rather than reading
    /// `detect_as` raw, because a miss is silent and permanent. `None` drops
    /// the `AOE_PROFILE`/`AOE_INSTANCE_ID` prefix from the launch line
    /// ([`status_hook_env_prefix`]) and skips hook install, so every hook the
    /// agent does have bails on `[ -n "$AOE_INSTANCE_ID" ]` and the session
    /// reports Idle forever with nothing logged.
    pub(crate) fn resolved_agent(&self) -> Option<&'static crate::agents::AgentDef> {
        resolved_agent_for(&self.source_profile, &self.tool, &self.detect_as)
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
    fn contains_active_shell_syntax(value: &str) -> bool {
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

    /// The basename of the program this launch actually runs, which is the
    /// token a live process carries in argv.
    ///
    /// A command override names it; otherwise it is the resolved agent's own
    /// binary. Matching on this rather than on `agent.binary` is what lets the
    /// orphan scan see a renamed wrapper, which carries the pane's
    /// `AOE_INSTANCE_ID` because [`status_hook_env_prefix`] injects the marker
    /// on hook presence alone.
    pub(crate) fn launch_executable_token(&self) -> Option<String> {
        let words = parse_launch_command(self.get_tool_command())?.words;
        Path::new(words.first()?)
            .file_name()?
            .to_str()
            .map(str::to_owned)
    }

    /// Whether a resume selector appended to this launch reaches the agent.
    ///
    /// [`Self::launch_invokes_resolved_agent_directly`] is the stricter test
    /// and stays the one that authorizes capture: inferring ownership from a
    /// store, mirroring a binary under `opencode serve`, or matching a process
    /// by argv all need the agent's own name. Emitting a selector needs less.
    /// A single bare token is the program the pane runs whatever it is called,
    /// which is the wrapper shape `custom_agents` and `agent_command_override`
    /// document, and `agent_detect_as` is the user declaring what it wraps.
    /// Refusing it takes resume away from every renamed wrapper and each
    /// restart silently starts a fresh conversation (#3638).
    ///
    /// A path-qualified token still fails: a bare one resolves through the
    /// launch shell's `PATH`, which AoE controls, and a path escapes it.
    pub(crate) fn launch_can_carry_resume_selector(&self, agent: &crate::agents::AgentDef) -> bool {
        if self.launch_invokes_resolved_agent_directly(agent) {
            return true;
        }
        let Some(parsed_command) = parse_launch_command(self.get_tool_command()) else {
            return false;
        };
        let [token] = parsed_command.words.as_slice() else {
            return false;
        };
        if token.contains('/') || token.starts_with('-') {
            return false;
        }
        if Self::contains_active_shell_syntax(self.get_tool_command())
            || Self::contains_active_shell_syntax(&self.extra_args)
        {
            return false;
        }
        shell_words::split(&self.extra_args)
            .is_ok_and(|extra| !extra.iter().any(|word| word == "--"))
    }

    /// Whether this launch shape leaves Claude user hooks enabled.
    ///
    /// This is evidence for the identity publisher only. It does not change
    /// native resume support.
    pub(crate) fn hook_session_publisher_allowed_by_argv(&self) -> bool {
        if !self
            .resolved_agent()
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
        let agent = self.resolved_agent()?;
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
        // These backends publish under this pane's own `AOE_INSTANCE_ID`, so
        // the write proves its own attribution and a renamed wrapper cannot
        // claim another pane's conversation. The rest infer ownership from the
        // launch itself: OMP reads a store it routed through the launch
        // environment, OpenCode mirrors the binary under `opencode serve`, and
        // the managed stores match on cwd and a launch floor. Those need the
        // agent's own binary on the command line to mean anything.
        let self_attributing = matches!(
            capture.backend,
            crate::agents::SessionCaptureBackend::Claude
                | crate::agents::SessionCaptureBackend::HookSidecar
                | crate::agents::SessionCaptureBackend::Pi
        );
        let authorized = if self_attributing {
            self.launch_can_carry_resume_selector(agent)
        } else {
            self.launch_invokes_resolved_agent_directly(agent)
        };
        authorized.then_some((capture, context))
    }

    pub(super) fn resolved_capture_backend(&self) -> Option<crate::agents::SessionCaptureBackend> {
        self.resolved_session_support()
            .map(|(capture, _)| capture.backend)
    }

    pub fn supports_native_resume(&self) -> bool {
        let Some(agent) = self.resolved_agent() else {
            return false;
        };
        if !self.launch_can_carry_resume_selector(agent) {
            return false;
        }
        if agent.session_support.is_none() {
            return false;
        }
        // Automatic capture in this environment, or an id the user named
        // themselves, which stays authoritative where capture is unsupported.
        self.resolved_session_support().is_some()
            || matches!(
                self.resume_intent,
                ResumeIntent::Use(_) | ResumeIntent::Fork { .. }
            )
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
    }

    /// True when this session renders in the structured (ACP) view. Rows
    /// damaged by pre-fix writers are healed on reload by the server's
    /// structured row repair path.
    pub fn is_structured(&self) -> bool {
        self.view == View::Structured
    }

    /// Switch this structured-view session to terminal mode while keeping the
    /// conversation resumable (#2252). Carries the ACP-side `acp_session_id`
    /// into the terminal-side `agent_session_id` and pins it as the resume
    /// target (`ResumeIntent::Use`), so the next `start()` launches
    /// `<tool> --resume <sid>` instead of a fresh pane, then drops the
    /// structured-view-only ids.
    ///
    /// The caller must have confirmed the agent pairing shares a
    /// CLI-resumable transcript (see `agents::acp_transcript_cli_resumable`).
    /// When `acp_session_id` is unset this only flips the view, leaving no
    /// resume target, which is why the caller also gates on it being present.
    pub(crate) fn switch_to_terminal_keep_context(&mut self) {
        if let Some(sid) = self.acp_session_id.take() {
            self.agent_session_id = Some(sid.clone());
            self.resume_intent = ResumeIntent::Use(sid);
        }
        self.import_pending = None;
        self.acp_load_session_capable = None;
        self.view = View::Terminal;
    }
}

/// Resolve a built-in from the instance's stored alias or its profile registry.
/// The stored value wins; legacy rows with no value consult the live registry.
fn resolved_agent_for(
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

    #[test]
    fn switch_to_terminal_keep_context_carries_acp_id_into_resume_target() {
        let mut inst = Instance::new("claude", "/tmp");
        inst.view = View::Structured;
        inst.acp_session_id = Some("sid-abc".to_string());
        inst.import_pending = Some(true);
        inst.acp_load_session_capable = Some(true);

        inst.switch_to_terminal_keep_context();

        assert_eq!(inst.view, View::Terminal);
        assert_eq!(inst.agent_session_id.as_deref(), Some("sid-abc"));
        assert_eq!(inst.resume_intent, ResumeIntent::Use("sid-abc".to_string()));
        // Structured-view-only state is dropped before terminal launch.
        assert_eq!(inst.acp_session_id, None);
        assert_eq!(inst.import_pending, None);
        assert_eq!(inst.acp_load_session_capable, None);
    }

    #[test]
    fn acp_load_session_capability_is_runtime_only() {
        let mut inst = Instance::new("structured", "/tmp");
        inst.acp_load_session_capable = Some(true);

        let json = serde_json::to_value(&inst).unwrap();
        assert!(json.get("acp_load_session_capable").is_none());

        let decoded: Instance = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.acp_load_session_capable, None);
    }

    #[test]
    fn test_new_instance() {
        let inst = Instance::new("test", "/tmp/test");
        assert_eq!(inst.title, "test");
        assert_eq!(inst.project_path, "/tmp/test");
        assert_eq!(inst.status, Status::Idle);
        assert_eq!(inst.id.len(), 16);
    }

    #[test]
    fn test_is_sub_session() {
        let mut inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_sub_session());

        inst.parent_session_id = Some("parent123".to_string());
        assert!(inst.is_sub_session());
    }

    // Additional tests for is_sandboxed
    #[test]
    fn test_is_sandboxed_without_sandbox_info() {
        let inst = Instance::new("test", "/tmp/test");
        assert!(!inst.is_sandboxed());
    }

    #[test]
    fn test_is_sandboxed_with_disabled_sandbox() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.sandbox_info = Some(SandboxInfo {
            enabled: false,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        assert!(!inst.is_sandboxed());
    }

    #[test]
    fn test_is_sandboxed_with_enabled_sandbox() {
        let mut inst = Instance::new("test", "/tmp/test");
        inst.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test-image".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        assert!(inst.is_sandboxed());
    }

    // Tests for Instance serialization
    #[test]
    fn test_instance_serialization_roundtrip() {
        let mut inst = Instance::new("Test Project", "/home/user/project");
        inst.tool = "claude".to_string();
        inst.group_path = "work/clients".to_string();
        inst.command = "claude --resume xyz".to_string();

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert_eq!(inst.id, deserialized.id);
        assert_eq!(inst.title, deserialized.title);
        assert_eq!(inst.project_path, deserialized.project_path);
        assert_eq!(inst.group_path, deserialized.group_path);
        assert_eq!(inst.tool, deserialized.tool);
        assert_eq!(inst.command, deserialized.command);
    }

    #[test]
    fn test_instance_serialization_skips_runtime_fields() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.last_error_check = Some(std::time::Instant::now());
        inst.last_start_time = Some(std::time::Instant::now());
        inst.last_error = Some("test error".to_string());

        let json = serde_json::to_string(&inst).unwrap();

        // Runtime fields should not appear in JSON
        assert!(!json.contains("last_error_check"));
        assert!(!json.contains("last_start_time"));
        assert!(!json.contains("last_error"));
    }

    #[test]
    fn test_instance_acp_acp_session_id_roundtrip() {
        let mut inst = Instance::new("Test", "/tmp/test");
        inst.view = View::Structured;
        inst.agent_name = Some("codex".to_string());
        inst.agent_model = Some("gpt-5".to_string());
        inst.acp_session_id = Some("acp-uuid-1234".to_string());

        let json = serde_json::to_string(&inst).unwrap();
        assert!(json.contains("\"view\":\"structured\""));
        assert!(json.contains("agent_name"));
        assert!(json.contains("agent_model"));
        assert!(json.contains("acp_session_id"));
        let deserialized: Instance = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.view, View::Structured);
        assert_eq!(deserialized.agent_name, Some("codex".to_string()));
        assert_eq!(deserialized.agent_model, Some("gpt-5".to_string()));
        assert_eq!(
            deserialized.acp_session_id,
            Some("acp-uuid-1234".to_string())
        );

        // None should not be serialized.
        let mut inst2 = Instance::new("Test", "/tmp/test");
        inst2.view = View::Structured;
        let json2 = serde_json::to_string(&inst2).unwrap();
        assert!(!json2.contains("acp_session_id"));
    }

    #[test]
    fn test_instance_with_worktree_info() {
        let mut inst = Instance::new("Test", "/tmp/worktree");
        inst.worktree_info = Some(WorktreeInfo {
            branch: "feature/abc".to_string(),
            main_repo_path: "/tmp/main".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });

        let json = serde_json::to_string(&inst).unwrap();
        let deserialized: Instance = serde_json::from_str(&json).unwrap();

        assert!(deserialized.worktree_info.is_some());
        let wt = deserialized.worktree_info.unwrap();
        assert_eq!(wt.branch, "feature/abc");
        assert!(wt.managed_by_aoe);
    }

    #[test]
    fn has_managed_worktree_or_workspace_covers_both_shapes() {
        // Single-repo aoe-managed worktree.
        let mut wt = Instance::new("WT", "/tmp/wt");
        wt.worktree_info = Some(WorktreeInfo {
            branch: "feature/abc".to_string(),
            main_repo_path: "/tmp/main".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        assert!(wt.has_managed_worktree_or_workspace());

        // Multi-repo workspace opting into cleanup (worktree_info is None).
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

        // Workspace that opted out of cleanup: nothing to clean.
        if let Some(info) = ws.workspace_info.as_mut() {
            info.cleanup_on_delete = false;
        }
        assert!(!ws.has_managed_worktree_or_workspace());

        // Plain session: neither worktree nor workspace.
        let plain = Instance::new("Plain", "/tmp/plain");
        assert!(!plain.has_managed_worktree_or_workspace());
    }

    #[test]
    fn test_repo_path_prefers_worktree_main_repo() {
        let mut inst = Instance::new("Test", "/tmp/worktrees/feature");
        assert_eq!(inst.repo_path(), "/tmp/worktrees/feature");
        inst.worktree_info = Some(WorktreeInfo {
            branch: "feature".to_string(),
            main_repo_path: "/tmp/main-repo".to_string(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: None,
        });
        assert_eq!(
            inst.repo_path(),
            "/tmp/main-repo",
            "worktree sessions group under the main repo, not the worktree dir"
        );
    }

    // Test generate_id function properties
    #[test]
    fn test_generate_id_uniqueness() {
        let ids: Vec<String> = (0..100).map(|_| Instance::new("t", "/t").id).collect();
        let unique_ids: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique_ids.len());
    }

    #[test]
    fn test_generate_id_format() {
        let inst = Instance::new("test", "/tmp/test");
        // ID should be 16 hex characters
        assert_eq!(inst.id.len(), 16);
        assert!(inst.id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // Test: backwards compatibility - load old JSON without agent_session_id
    #[test]
    fn test_backwards_compatibility() {
        // Old JSON without agent_session_id field
        let old_json = r#"{"id":"old-session-123","title":"Old Session","project_path":"/home/user/old","group_path":"","command":"","tool":"claude","yolo_mode":false,"status":"idle","created_at":"2024-01-01T00:00:00Z"}"#;

        let inst: Instance = serde_json::from_str(old_json).unwrap();

        // Should parse successfully with agent_session_id defaulting to None
        assert_eq!(inst.id, "old-session-123");
        assert_eq!(inst.title, "Old Session");
        assert_eq!(inst.project_path, "/home/user/old");
        assert_eq!(inst.tool, "claude");
        assert!(inst.agent_session_id.is_none());

        // After loading, can set a new session ID
        let mut inst = inst;
        inst.agent_session_id = Some("new-session-456".to_string());
        assert_eq!(inst.agent_session_id, Some("new-session-456".to_string()));
    }

    /// A custom-agent row whose stored `detect_as` is empty must still resolve
    /// its built-in agent at launch. Without it `status_hook_env_prefix` drops
    /// `AOE_INSTANCE_ID`, every hook in the agent's settings file bails on
    /// `[ -n "$AOE_INSTANCE_ID" ]`, and the session reports Idle forever with
    /// nothing logged. #3398 taught the read sites to consult the live
    /// registry; this is the launch site.
    #[test]
    fn empty_detect_as_still_resolves_the_launch_agent() {
        const PROFILE: &str = "detect-as-launch-path-test";
        let _registry = install_aliases(PROFILE, &[("claude-personal", "claude")]);

        let mut inst = Instance::new("orch", "/tmp/x");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "claude-personal".to_string();
        inst.command = "claude-personal".to_string();
        inst.detect_as = String::new();

        assert_eq!(
            inst.resolved_agent().map(|a| a.name),
            Some("claude"),
            "empty detect_as must fall back to the live agent_detect_as registry"
        );
        assert_eq!(
            status_hook_env_prefix(&inst.effective_profile(), "abc123", inst.resolved_agent()),
            format!(
                "AOE_PROFILE='{PROFILE}' AOE_INSTANCE_ID='abc123' AOE_HOOK_BIN={} ",
                shell_escape(&std::env::current_exe().unwrap().to_string_lossy())
            ),
        );
    }
    #[test]
    fn native_resume_requires_a_direct_local_builtin_launch() {
        const PROFILE: &str = "resume-custom-launch-test";
        let _registry = install_aliases(PROFILE, &[("work-claude", "claude")]);

        let mut inst = Instance::new("custom", "/tmp/custom");
        inst.source_profile = PROFILE.to_string();
        inst.tool = "work-claude".to_string();

        inst.command = "claude --model opus".to_string();
        assert!(inst.supports_native_resume());

        inst.tool = "claude".to_string();
        assert!(inst.supports_native_resume());
        inst.command = "ssh -t host claude".to_string();
        assert!(!inst.supports_native_resume());
        inst.command = "claude > /tmp/transcript".to_string();
        assert!(!inst.supports_native_resume());

        for command in [
            "claude $BARRIER",
            "claude ${BARRIER}",
            "claude *",
            "claude session-?",
            "claude [abc]",
            "claude {one,two}",
            "claude ~/thread",
        ] {
            inst.command = command.to_string();
            assert!(!inst.supports_native_resume(), "accepted {command:?}");
        }

        inst.command = "/opt/wrappers/claude".to_string();
        assert!(!inst.supports_native_resume());

        inst.command = "./claude".to_string();
        assert!(!inst.supports_native_resume());

        inst.command = "claude".to_string();
        inst.extra_args = "--model opus | tee /tmp/transcript".to_string();
        assert!(!inst.supports_native_resume());
        inst.extra_args = "--append-system-prompt $PROMPT".to_string();
        assert!(!inst.supports_native_resume());
        inst.command.clear();
        inst.extra_args = "--model sonnet[1m]".to_string();
        assert!(inst.supports_native_resume());

        inst.extra_args.clear();
        inst.command = "claude # local note".to_string();
        assert!(!inst.supports_native_resume());

        inst.command = "claude".to_string();
        inst.extra_args = "--model opus # local note".to_string();
        assert!(!inst.supports_native_resume());

        for value in ["claude\n", "claude\r", "claude\r\n"] {
            inst.command = value.to_string();
            inst.extra_args.clear();
            assert!(!inst.supports_native_resume(), "accepted {value:?}");
        }
        inst.command = "claude --".to_string();
        inst.extra_args.clear();
        assert!(!inst.supports_native_resume());
        inst.command = "claude".to_string();
        inst.extra_args = "--".to_string();
        assert!(!inst.supports_native_resume());

        inst.command = "claude".to_string();
        for value in ["--model opus\n", "--model opus\r", "--model opus\r\n"] {
            inst.extra_args = value.to_string();
            assert!(!inst.supports_native_resume(), "accepted {value:?}");
        }
    }

    #[test]
    fn capture_generation_guards_survive_serialization() {
        let mut inst = Instance::new("claude", "/tmp/custom");
        let floor = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(42);
        inst.capture_started_at = Some(floor);
        inst.retroactive_capture_excludes
            .insert("stale-sid".to_string());

        let encoded = serde_json::to_string(&inst).unwrap();
        let decoded: Instance = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded.capture_started_at, Some(floor));
        assert!(decoded.retroactive_capture_excludes.contains("stale-sid"));
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
            assert!(
                inst.supports_native_resume(),
                "hook-disabling argv must not disable native resume: {args:?}"
            );
        }
    }
}
