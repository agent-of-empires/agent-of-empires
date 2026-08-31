//! Installing and running agent status hooks around a launch.

use super::*;

pub(super) fn status_hook_env_prefix(
    profile: &str,
    instance_id: &str,
    agent: Option<&crate::agents::AgentDef>,
) -> String {
    let has_hooks = agent.is_some_and(|a| a.hook_config.is_some() || a.sidecar_hooks.is_some());

    if has_hooks {
        format!(
            "AOE_PROFILE={} AOE_INSTANCE_ID={} ",
            shell_escape(profile),
            shell_escape(instance_id)
        )
    } else {
        String::new()
    }
}

impl Instance {
    pub(super) fn run_pre_launch_hooks(
        &mut self,
        skip_on_launch: bool,
        profile: &str,
    ) -> Result<()> {
        self.mint_host_session_env()?;
        self.run_launch_hooks(skip_on_launch, profile)
    }

    fn run_launch_hooks(&mut self, skip_on_launch: bool, profile: &str) -> Result<()> {
        if self.tool == "omp" && !self.has_command_override() {
            reject_omp_secret_args(&crate::session::config::quote_model_value_in_args(
                &self.extra_args,
            ))?;
        }
        let agent = self.resolved_agent();
        self.install_agent_status_hooks(agent);
        self.ensure_host_folder_trust(agent);
        self.propagate_managed_skills();

        let on_launch_hooks = self.resolve_on_launch_hooks(skip_on_launch, profile);
        if self.is_sandboxed() {
            self.get_container_for_instance()?;
            if let (Some(hook_cmds), Some(sandbox)) =
                (on_launch_hooks.as_ref(), self.sandbox_info.as_ref())
            {
                let hook_env = crate::session::repo_config::lifecycle_env_vars(self);
                let workdir = self.container_workdir();
                if let Err(error) = crate::session::repo_config::execute_hooks_in_container(
                    hook_cmds,
                    &sandbox.container_name,
                    &workdir,
                    &hook_env,
                ) {
                    if error.chain().any(|cause| {
                        cause
                            .downcast_ref::<crate::session::repo_config::HookTimeout>()
                            .is_some()
                    }) {
                        return Err(error);
                    }
                    tracing::warn!(
                        target: "session.store",
                        "on_launch hook failed in container: {}",
                        error
                    );
                }
            }
        } else if let Some(hook_cmds) = on_launch_hooks.as_ref() {
            let hook_env = crate::session::repo_config::lifecycle_env_vars(self);
            if let Err(error) = crate::session::repo_config::execute_hooks(
                hook_cmds,
                Path::new(&self.project_path),
                &hook_env,
            ) {
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<crate::session::repo_config::HookTimeout>()
                        .is_some()
                }) {
                    return Err(error);
                }
                tracing::warn!(target: "session.store", "on_launch hook failed: {}", error);
            }
        }
        Ok(())
    }

    /// Resolve on_launch hooks from the full config chain (global > profile > repo).
    ///
    /// Repo hooks go through trust verification; global/profile hooks are
    /// implicitly trusted. Returns `None` when skipped or no hooks are configured.
    pub(crate) fn resolve_on_launch_hooks(
        &self,
        skip_on_launch: bool,
        profile: &str,
    ) -> Option<Vec<String>> {
        if skip_on_launch {
            return None;
        }

        // Start with global+profile hooks as the base
        let mut resolved_on_launch =
            crate::session::profile_config::resolve_config_or_warn(profile)
                .hooks
                .on_launch;

        // Check if repo has trusted hooks that override. Only the hooks surface
        // matters here; untrusted project MCP must not suppress trusted hooks.
        if let Ok(trust) =
            crate::session::repo_config::check_repo_trust(Path::new(&self.project_path))
        {
            if let Some(hooks) = trust.hooks.trusted() {
                if !hooks.on_launch.is_empty() {
                    resolved_on_launch = hooks.on_launch;
                }
            }
        }

        if resolved_on_launch.is_empty() {
            None
        } else {
            Some(resolved_on_launch)
        }
    }

    /// Make AoE-managed skills available to the agent this session launches, by
    /// reconciling the managed store into that agent's own skills directory
    /// (#3053). Skills reach an agent only as files on disk, so there is nothing
    /// to forward over a protocol; the copy is the mechanism.
    ///
    /// Off unless the user opted in, because it writes into their real agent
    /// config dirs. Best-effort: a root that is missing, read-only, or holds a
    /// conflicting skill is logged and never blocks the launch. A sandboxed
    /// session gets its own copy from `build_container_config`, which reconciles
    /// into the sandbox dir rather than relying on this host pass.
    fn propagate_managed_skills(&self) {
        // Read the global config, not the profile chain. `auto_propagate` is
        // declared `global_only`, and the sandbox path reads it globally too, so
        // resolving it per profile here would let a profile enable host
        // propagation while the same profile's sandboxed sessions ignored it,
        // and would widen a privilege the settings UI never offers per profile.
        let config = crate::session::config::Config::load_or_warn();
        if !config.skills.auto_propagate {
            return;
        }
        let (Some(home), Ok(app_dir)) = (dirs::home_dir(), crate::session::get_app_dir()) else {
            tracing::warn!(target: "session.skills", "skipping skill propagation: no home or app dir");
            return;
        };
        let Some(outcomes) =
            crate::session::skills_model::sync_for_agent(&home, &app_dir, &self.tool)
        else {
            tracing::debug!(target: "session.skills", agent = %self.tool, "no skills location known for agent");
            return;
        };
        crate::session::skills_model::log_sync_outcomes(&self.tool, &outcomes);
    }

    /// Install status-detection hooks for agents that support them.
    ///
    /// For sandboxed sessions hooks are installed via `build_container_config`,
    /// so this only acts on host sessions by writing to the user's home directory.
    /// Respects the `agent_status_hooks` config setting.
    fn install_agent_status_hooks(&self, agent: Option<&'static crate::agents::AgentDef>) {
        let profile = self.effective_profile();
        let config = crate::session::profile_config::resolve_config_or_warn(&profile);
        if !config.session.agent_status_hooks {
            return;
        }
        if let Some(agent) = agent {
            if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
                let events = match crate::agents::resolved_sidecar_hook_events(agent, &config) {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!(target: "session.store", "Failed to resolve {} status hooks: {}", agent.name, e);
                        return;
                    }
                };
                // Sidecar agents (settl TOML, hermes YAML, kiro per-agent JSON)
                // install into a host config file; sandbox install is handled by
                // build_container_config. host_only agents (settl) are never
                // sandboxed, so the gate is a no-op for them.
                if !self.is_sandboxed() {
                    if let Some(home) = dirs::home_dir() {
                        self.install_sidecar_host_hooks(sidecar, &home, &config.session, &events);
                    }
                }
            } else if let Some(hook_cfg) = agent.hook_config.as_ref() {
                let events = match crate::agents::resolved_hook_events(agent, &config) {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!(target: "session.store", "Failed to resolve {} status hooks: {}", agent.name, e);
                        return;
                    }
                };
                if !self.is_sandboxed() {
                    match hook_cfg.format {
                        crate::agents::HookFormat::CodexJson => {
                            self.install_codex_host_hooks(&events)
                        }
                        crate::agents::HookFormat::JsonSettings => {
                            self.install_json_host_hooks(hook_cfg, &events)
                        }
                    }
                }
                // Sandboxed sessions install via build_container_config.
            }
        }
    }

    /// Pre-trust this session's worktree in the agent's host config so it does
    /// not open on a folder-trust prompt.
    ///
    /// Sandboxed sessions are handled by `build_container_config` against a
    /// staged config; this writes to the user's real one, so it is opt-in via
    /// `session.pre_trust_agent_folders`. The path is canonicalized because
    /// agents key trust on the resolved directory, not the symlink used to
    /// reach it.
    fn ensure_host_folder_trust(&self, agent: Option<&'static crate::agents::AgentDef>) {
        if self.is_sandboxed() {
            return;
        }
        let profile = self.effective_profile();
        let config = crate::session::profile_config::resolve_config_or_warn(&profile);
        if !config.session.pre_trust_agent_folders {
            return;
        }
        let (Some(agent), Some(home)) = (agent, dirs::home_dir()) else {
            return;
        };
        let project_path = std::fs::canonicalize(&self.project_path)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| self.project_path.clone());
        let environment = self.resolved_host_environment();
        let config_dir = config.session.agent_config_dir_for(&self.tool, &home);
        if let Err(e) = crate::hooks::trust_host_project(
            agent.name,
            &home,
            &environment,
            config_dir.as_deref(),
            &project_path,
        ) {
            tracing::warn!(target: "session.store",
                "Failed to pre-trust {} in the host {} config: {}", project_path, agent.name, e);
        }
    }

    /// Install a sidecar agent's host hooks. For agents whose hooks are scoped
    /// to a user-selected named agent (`selected_agent_hooks`, e.g. Kiro), and
    /// when the user actually selected one and the merge setting is on, install
    /// into that agent's own config file and stop. Otherwise install into the
    /// agent's standalone config and run any `post_install_host` follow-up.
    fn install_sidecar_host_hooks(
        &self,
        sidecar: &'static crate::agents::SidecarHooks,
        home: &Path,
        session_cfg: &crate::session::config::SessionConfig,
        events: &[crate::agents::ResolvedHookEvent],
    ) {
        if session_cfg.merge_hooks_into_selected_agent {
            if let Some(sel) = sidecar.selected_agent_hooks.as_ref() {
                if let Some(name) =
                    crate::agents::parse_selected_agent(&self.selected_agent_args(), sel.flag)
                {
                    // The selected agent is what the CLI loads; install AoE's
                    // hooks into its config (these CLIs have no global hooks) and
                    // skip the standalone-agent install + post_install_host. The
                    // agents directory is the parent of the standalone hooks
                    // agent's config (e.g. `.kiro/agents`); the resolver picks the
                    // right file within it by `name`.
                    let agents_dir = home.join(
                        Path::new(sidecar.host_config_subpath)
                            .parent()
                            .unwrap_or(Path::new(".")),
                    );
                    let path = (sel.resolve_config_file)(&agents_dir, &name);
                    match (sidecar.install)(&path, crate::hooks::HookInstallTarget::Host, events) {
                        Ok(()) => tracing::info!(target: "session.store",
                            "Installed AoE status hooks into {} agent '{}' at {}", self.tool, name, path.display()),
                        Err(e) => tracing::warn!(target: "session.store",
                            "Failed to install AoE hooks into {} agent '{}' at {}: {}", self.tool, name, path.display(), e),
                    }
                    return;
                }
            }
        }

        let config_path = home.join(sidecar.host_config_subpath);
        match (sidecar.install)(&config_path, crate::hooks::HookInstallTarget::Host, events) {
            Ok(()) => {
                tracing::info!(target: "session.store",
                    "Installed AoE status hooks for {} via standalone hooks agent", self.tool);
                if let Some(post_install) = sidecar.post_install_host {
                    post_install();
                }
            }
            Err(e) => tracing::warn!(target: "session.store",
                "Failed to install {} hooks: {}", self.tool, e),
        }
    }

    fn install_codex_host_hooks(&self, events: &[crate::agents::ResolvedHookEvent]) {
        let environment = self.resolved_host_environment();
        match crate::hooks::codex_hooks_json_path_for_host_environment(&environment) {
            Ok(hooks_path) => {
                if let Err(e) = crate::hooks::install_hooks(
                    &hooks_path,
                    events,
                    crate::hooks::HookInstallTarget::Host,
                ) {
                    tracing::warn!(target: "session.store", "Failed to install codex hooks: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!(target: "session.store", "Failed to resolve codex hooks path: {}", e)
            }
        }
    }

    fn install_json_host_hooks(
        &self,
        hook_cfg: &crate::agents::AgentHookConfig,
        events: &[crate::agents::ResolvedHookEvent],
    ) {
        // Install hooks in the agent's host settings file, honoring a
        // config-dir override env var (e.g. CLAUDE_CONFIG_DIR) so hooks
        // land where the agent actually reads them.
        let environment = self.resolved_host_environment();
        match crate::hooks::agent_settings_path_for_host_environment(hook_cfg, &environment) {
            Ok(settings_path) => {
                if let Err(e) = crate::hooks::install_hooks(
                    &settings_path,
                    events,
                    crate::hooks::HookInstallTarget::Host,
                ) {
                    tracing::warn!(target: "session.store", "Failed to install agent hooks: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!(target: "session.store", "Failed to resolve agent hooks path: {}", e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::test_support::EnvGuard;

    #[test]
    fn test_codex_gets_status_hook_env_prefix() {
        let agent = crate::agents::get_agent("codex");
        assert_eq!(
            status_hook_env_prefix("work", "abc123", agent),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_custom_codex_detected_agent_uses_codex_hook_installer() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let mut inst = Instance::new("wrapped", "/tmp/test");
        inst.tool = "my-codex-wrapper".to_string();
        inst.detect_as = "codex".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as));

        let hooks_path = tmp.path().join(".codex").join("hooks.json");
        let hooks = std::fs::read_to_string(hooks_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks).unwrap();
        assert!(parsed["hooks"]["PreToolUse"].is_array());
        assert!(hooks.contains("aoe-hooks"));
        assert!(!tmp.path().join(".codex").join("config.toml").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_hook_installer_uses_resolved_codex_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let profile_codex_home = tmp.path().join("profile-codex-home");
        let resolved_codex_home = tmp.path().join("before-session-codex-home");
        let profile_dir = crate::session::get_profile_dir("codex-profile").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            format!(
                "environment = [\"CODEX_HOME={}\"]\n",
                profile_codex_home.display()
            ),
        )
        .unwrap();

        let mut inst = Instance::new("codex", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.detect_as = "codex".to_string();
        inst.source_profile = "codex-profile".to_string();
        inst.pending_host_env = vec![(
            "CODEX_HOME".to_string(),
            resolved_codex_home.to_string_lossy().into_owned(),
        )];
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as));

        let hooks_path = resolved_codex_home.join("hooks.json");
        let hooks = std::fs::read_to_string(hooks_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks).unwrap();
        assert!(parsed["hooks"]["PreToolUse"].is_array());
        assert!(hooks.contains("aoe-hooks"));
        assert!(!profile_codex_home.join("hooks.json").exists());
        assert!(!tmp.path().join(".codex").join("hooks.json").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_hook_installer_respects_profile_hooks_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let profile_dir = crate::session::get_profile_dir("hooks-disabled").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "[session]\nagent_status_hooks = false\n",
        )
        .unwrap();

        let mut inst = Instance::new("codex", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.detect_as = "codex".to_string();
        inst.source_profile = "hooks-disabled".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as));

        assert!(!tmp.path().join(".codex").join("hooks.json").exists());
    }

    // The host pre-trust is opt-in and host-only. Both gates are what stop it
    // writing into the user's real agent config, so both need a test.
    #[test]
    #[serial_test::serial]
    fn test_host_folder_trust_is_gated_on_the_setting_and_on_host_sessions() {
        // (profile, setting on, sandboxed, expect a trust record)
        let cases = [
            ("trust-off", false, false, false),
            ("trust-on", true, false, true),
            ("trust-on-sandboxed", true, true, false),
        ];
        for (profile, enabled, sandboxed, expected) in cases {
            let tmp = tempfile::TempDir::new().unwrap();
            let _guard = EnvGuard::unset(&["CLAUDE_CONFIG_DIR"]);
            std::env::set_var("HOME", tmp.path());
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

            let profile_dir = crate::session::get_profile_dir(profile).unwrap();
            std::fs::write(
                profile_dir.join("config.toml"),
                format!("[session]\npre_trust_agent_folders = {enabled}\n"),
            )
            .unwrap();

            let project = tmp.path().join("repo");
            std::fs::create_dir_all(&project).unwrap();
            let mut inst = Instance::new("claude", project.to_str().unwrap());
            inst.tool = "claude".to_string();
            inst.detect_as = "claude".to_string();
            inst.source_profile = profile.to_string();
            if sandboxed {
                inst.sandbox_info = Some(crate::session::instance::SandboxInfo {
                    enabled: true,
                    container_id: None,
                    image: "test:latest".to_string(),
                    container_name: "test-container".to_string(),
                    extra_env: None,
                    custom_instruction: None,
                    before_start_env: Vec::new(),
                    container_workdir: None,
                });
            }
            inst.ensure_host_folder_trust(crate::agents::get_agent(&inst.detect_as));

            assert_eq!(
                tmp.path().join(".claude.json").exists(),
                expected,
                "profile={profile}: host trust record presence"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_hook_installer_respects_profile_hooks_enabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        crate::session::config::update_config(|global| {
            global.session.agent_status_hooks = false;
        })
        .unwrap();

        let profile_dir = crate::session::get_profile_dir("hooks-enabled").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "[session]\nagent_status_hooks = true\n",
        )
        .unwrap();

        let mut inst = Instance::new("codex", "/tmp/test");
        inst.tool = "codex".to_string();
        inst.detect_as = "codex".to_string();
        inst.source_profile = "hooks-enabled".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as));

        let hooks_path = tmp.path().join(".codex").join("hooks.json");
        let hooks = std::fs::read_to_string(hooks_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks).unwrap();
        assert!(parsed["hooks"]["PreToolUse"].is_array());
        assert!(hooks.contains("aoe-hooks"));
    }

    #[test]
    #[serial_test::serial]
    fn launch_hooks_run_without_title_or_lifecycle_flocks() {
        if !crate::tmux::tmux_command()
            .arg("-V")
            .output()
            .is_ok_and(|output| output.status.success())
        {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());

        for restart in [false, true] {
            let label = if restart { "restart" } else { "start" };
            let profile = format!("lifecycle-hook-{label}");
            let ready = temp.path().join(format!("{label}-ready"));
            let release = temp.path().join(format!("{label}-release"));
            let hook = format!(
                ": > {}; while [ ! -e {} ]; do sleep 0.01; done",
                super::shell_escape(&ready.to_string_lossy()),
                super::shell_escape(&release.to_string_lossy()),
            );
            crate::session::config::update_config(|global| {
                global.hooks.on_launch = vec![hook];
            })
            .unwrap();

            let storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
            let title = format!("lifecycle hook {label}");
            let mut instance = Instance::new(&title, temp.path().to_str().unwrap());
            instance.source_profile = profile.clone();
            instance.command = "sleep 30".to_string();
            storage
                .update(|instances, _groups| {
                    instances.push(instance.clone());
                    Ok(())
                })
                .unwrap();
            if restart {
                instance
                    .tmux_session()
                    .unwrap()
                    .create(temp.path().to_str().unwrap(), Some("sleep 30"), &profile)
                    .unwrap();
            }

            let (launch_tx, launch_rx) = std::sync::mpsc::channel();
            let launch = std::thread::spawn(move || {
                let result = if restart {
                    instance.restart_with_size_opts(None, false).map(|_| ())
                } else {
                    instance.start_with_size_opts(None, false).map(|_| ())
                };
                launch_tx.send((result, instance)).unwrap();
            });
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !ready.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(ready.exists(), "{label} hook did not start");

            let lock_storage = crate::session::storage::Storage::new_unwatched(&profile).unwrap();
            let id = storage.load().unwrap()[0].id.clone();
            let release_for_lock = release.clone();
            let (title_tx, title_rx) = std::sync::mpsc::channel();
            let (lock_tx, lock_rx) = std::sync::mpsc::channel();
            let lock = std::thread::spawn(move || {
                let title_guard = crate::session::storage::acquire_session_title_lock(&id).unwrap();
                title_tx.send(()).unwrap();
                let lifecycle_guard = lock_storage.acquire_instance_lifecycle_lock(&id).unwrap();
                drop(lifecycle_guard);
                drop(title_guard);
                std::fs::write(release_for_lock, b"release").unwrap();
                lock_tx.send(()).unwrap();
            });
            let title_acquired = title_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            let both_acquired = title_acquired
                && lock_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .is_ok();
            if !both_acquired {
                std::fs::write(&release, b"release").unwrap();
            }

            let (result, instance) = launch_rx
                .recv_timeout(std::time::Duration::from_secs(10))
                .unwrap();
            launch.join().unwrap();
            lock.join().unwrap();
            let _ = instance.tmux_session().unwrap().kill();
            assert!(
                title_acquired,
                "{label} hook ran while the title mutation flock was held"
            );
            assert!(
                both_acquired,
                "{label} hook ran while the lifecycle flock was held"
            );
            result.unwrap();
        }
    }

    #[test]
    fn test_status_hook_env_prefix_includes_hermes() {
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("hermes")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("settl")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("claude")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("opencode")),
            ""
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("kiro")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("kimi")),
            "AOE_PROFILE='work' AOE_INSTANCE_ID='abc123' "
        );
    }
}
