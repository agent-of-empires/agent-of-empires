//! Installing and running agent status hooks around a launch.

use super::*;
use anyhow::bail;

pub(super) fn status_hook_env_prefix(
    profile: &str,
    instance_id: &str,
    agent: Option<&crate::agents::AgentDef>,
) -> String {
    let has_hooks = agent.is_some_and(|a| a.hook_config.is_some() || a.sidecar_hooks.is_some());

    if has_hooks {
        let hook_bin = std::env::current_exe()
            .expect("current executable is required for host identity hooks");
        format!(
            "AOE_PROFILE={} AOE_INSTANCE_ID={} AOE_HOOK_BIN={} ",
            shell_escape(profile),
            shell_escape(instance_id),
            shell_escape(&hook_bin.to_string_lossy())
        )
    } else {
        String::new()
    }
}

pub(crate) fn generic_host_config_path_for(
    tool_name: &str,
    hook_cfg: &crate::agents::AgentHookConfig,
    home: &Path,
    session_cfg: &crate::session::config::SessionConfig,
    host_environment: &[String],
) -> std::path::PathBuf {
    if let Some(root) = session_cfg.agent_config_dir_for(tool_name, home) {
        if let Some(file) = Path::new(hook_cfg.settings_rel_path).file_name() {
            return root.join(file);
        }
    }
    match hook_cfg.format {
        crate::agents::HookFormat::CodexJson => {
            crate::hooks::codex_hooks_json_path_in(home, host_environment)
        }
        crate::agents::HookFormat::JsonSettings => {
            crate::hooks::agent_settings_path_in(home, hook_cfg, host_environment)
        }
    }
}

pub(crate) fn sidecar_host_config_path_for(
    tool_name: &str,
    agent: &crate::agents::AgentDef,
    sidecar: &crate::agents::SidecarHooks,
    home: &Path,
    session_cfg: &crate::session::config::SessionConfig,
    host_environment: &[String],
) -> std::path::PathBuf {
    let relative: std::path::PathBuf = Path::new(sidecar.host_config_subpath)
        .components()
        .skip(1)
        .collect();
    if let Some(root) = session_cfg.agent_config_dir_for(tool_name, home) {
        return root.join(relative);
    }
    if agent.name == "cursor" {
        if let Some(root) = crate::session::environment::resolve_host_environment_value(
            host_environment,
            "CURSOR_CONFIG_DIR",
        )
        .filter(|root| !root.is_empty())
        {
            return std::path::PathBuf::from(root).join(relative);
        }
    }
    home.join(sidecar.host_config_subpath)
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
        self.ensure_disclosed_host_hook_path(agent)?;
        self.install_agent_status_hooks(agent);
        self.ensure_host_folder_trust(agent);
        self.propagate_managed_skills();

        let on_launch_hooks = self.resolve_on_launch_hooks(skip_on_launch, profile);
        if self.is_sandboxed() {
            self.get_container_for_instance()?;
            if let (Some(hook_cmds), Some(sandbox)) =
                (on_launch_hooks.as_ref(), self.sandbox_info.as_ref())
            {
                let hook_env = crate::session::config::repo_config::lifecycle_env_vars(self);
                let workdir = self.container_workdir();
                if let Err(error) = crate::session::config::repo_config::execute_hooks_in_container(
                    hook_cmds,
                    &sandbox.container_name,
                    &workdir,
                    &hook_env,
                ) {
                    if error.chain().any(|cause| {
                        cause
                            .downcast_ref::<crate::session::config::repo_config::HookTimeout>()
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
            let hook_env = crate::session::config::repo_config::lifecycle_env_vars(self);
            if let Err(error) = crate::session::config::repo_config::execute_hooks(
                hook_cmds,
                Path::new(&self.project_path),
                &hook_env,
            ) {
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<crate::session::config::repo_config::HookTimeout>()
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
            crate::session::config::profile_config::resolve_config_or_warn(profile)
                .hooks
                .on_launch;

        // Check if repo has trusted hooks that override. Only the hooks surface
        // matters here; untrusted project MCP must not suppress trusted hooks.
        if let Ok(trust) =
            crate::session::config::repo_config::check_repo_trust(Path::new(&self.project_path))
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

    fn ensure_disclosed_host_hook_path(
        &self,
        agent: Option<&'static crate::agents::AgentDef>,
    ) -> Result<()> {
        let sandboxed = self.is_sandboxed();
        let Some(agent) = agent else {
            return Ok(());
        };
        if sandboxed {
            return Ok(());
        }
        let profile = self.effective_profile();
        let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
        let hook_install_required =
            crate::agents::hook_install_required(agent, config.session.agent_status_hooks);
        if !hook_install_required {
            return Ok(());
        }
        if !sandboxed
            && hook_install_required
            && !crate::session::config::load_config()
                .ok()
                .flatten()
                .is_some_and(|config| config.app_state.has_acknowledged_agent_hooks)
        {
            bail!(
                "agent hook paths have not been acknowledged; approve them in the AoE TUI before launching this host session"
            );
        }
        let profile_environment = self.profile_host_environment();
        let resolved_environment = self.resolved_host_environment();
        let home_from = |environment: &[String]| {
            crate::session::environment::resolve_host_environment_value(environment, "HOME")
                .map(std::path::PathBuf::from)
                .or_else(dirs::home_dir)
        };
        let profile_home = home_from(&profile_environment)
            .context("home directory unavailable for disclosed hook path")?;
        let resolved_home = home_from(&resolved_environment)
            .context("home directory unavailable for resolved hook path")?;
        let paths = if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
            Some((
                sidecar_host_config_path_for(
                    &self.tool,
                    agent,
                    sidecar,
                    &profile_home,
                    &config.session,
                    &profile_environment,
                ),
                sidecar_host_config_path_for(
                    &self.tool,
                    agent,
                    sidecar,
                    &resolved_home,
                    &config.session,
                    &resolved_environment,
                ),
            ))
        } else if let Some(hook_cfg) = agent.hook_config.as_ref() {
            Some((
                generic_host_config_path_for(
                    &self.tool,
                    hook_cfg,
                    &profile_home,
                    &config.session,
                    &profile_environment,
                ),
                generic_host_config_path_for(
                    &self.tool,
                    hook_cfg,
                    &resolved_home,
                    &config.session,
                    &resolved_environment,
                ),
            ))
        } else {
            None
        };
        if let Some((disclosed, resolved)) = paths {
            if disclosed != resolved {
                bail!(
                    "before_session changed the agent hook path from {} to {}; declare the override in the profile environment before consenting",
                    disclosed.display(),
                    resolved.display()
                );
            }
        }
        Ok(())
    }

    fn resolved_host_home(&self) -> Option<std::path::PathBuf> {
        crate::session::environment::resolve_host_environment_value(
            &self.resolved_host_environment(),
            "HOME",
        )
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
    }

    /// Install optional status hooks and mandatory authoritative identity hooks.
    ///
    /// Sandboxed sessions install through build_container_config. Disabling
    /// agent_status_hooks removes status writers but cannot disable identity
    /// publication for a resume-capable pane.
    fn install_agent_status_hooks(&mut self, agent: Option<&'static crate::agents::AgentDef>) {
        self.identity_publisher_launched = false;
        let profile = self.effective_profile();
        let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
        let status_hooks_enabled = config.session.agent_status_hooks;
        if !self.is_sandboxed()
            && agent.is_some_and(|agent| {
                crate::agents::hook_install_required(agent, status_hooks_enabled)
            })
            && !crate::session::config::load_config()
                .ok()
                .flatten()
                .is_some_and(|config| config.app_state.has_acknowledged_agent_hooks)
        {
            tracing::warn!(
                target: "hooks.install",
                instance = %self.id,
                "skipping host hook installation until the user acknowledges the hook paths"
            );
            return;
        }
        if let Some(agent) = agent {
            if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
                let mut events = match crate::agents::resolved_sidecar_hook_events(agent, &config) {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!(target: "session.store", "Failed to resolve {} status hooks: {}", agent.name, e);
                        return;
                    }
                };
                if !status_hooks_enabled {
                    events.retain(|event| event.identity_field.is_some());
                    for event in &mut events {
                        event.status = None;
                    }
                }
                let publishes_identity = events.iter().any(|event| event.identity_field.is_some());
                self.identity_publisher_launched = if self.is_sandboxed() {
                    false
                } else {
                    let environment = if events.is_empty() {
                        self.profile_host_environment()
                    } else {
                        self.resolved_host_environment()
                    };
                    let home = crate::session::environment::resolve_host_environment_value(
                        &environment,
                        "HOME",
                    )
                    .map(std::path::PathBuf::from)
                    .or_else(dirs::home_dir);
                    let installed = home.is_some_and(|home| {
                        self.install_sidecar_host_hooks(
                            sidecar,
                            &home,
                            &config.session,
                            &environment,
                            &events,
                        )
                    });
                    publishes_identity && installed && self.hook_session_publisher_allowed_by_argv()
                };
            } else if let Some(hook_cfg) = agent.hook_config.as_ref() {
                let mut events = match crate::agents::resolved_hook_events(agent, &config) {
                    Ok(events) => events,
                    Err(e) => {
                        tracing::warn!(target: "session.store", "Failed to resolve {} status hooks: {}", agent.name, e);
                        return;
                    }
                };
                if !status_hooks_enabled {
                    events.retain(|event| event.identity_field.is_some());
                    for event in &mut events {
                        event.status = None;
                    }
                }
                let publishes_identity = events.iter().any(|event| event.identity_field.is_some());
                self.identity_publisher_launched = if self.is_sandboxed() {
                    false
                } else {
                    let installed = match hook_cfg.format {
                        crate::agents::HookFormat::CodexJson => {
                            self.install_codex_host_hooks(&config.session, &events)
                        }
                        crate::agents::HookFormat::JsonSettings => {
                            self.install_json_host_hooks(hook_cfg, &config.session, &events)
                        }
                    };
                    publishes_identity && installed && self.hook_session_publisher_allowed_by_argv()
                };
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
        let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
        if !config.session.pre_trust_agent_folders {
            return;
        }
        let (Some(agent), Some(home)) = (agent, self.resolved_host_home()) else {
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
        host_environment: &[String],
        events: &[crate::agents::ResolvedHookEvent],
    ) -> bool {
        if session_cfg.merge_hooks_into_selected_agent {
            if let Some(selected) = sidecar.selected_agent_hooks.as_ref() {
                if let Some(name) =
                    crate::agents::parse_selected_agent(&self.selected_agent_args(), selected.flag)
                {
                    let Some(agent) = self.resolved_agent() else {
                        return false;
                    };
                    let config_path = sidecar_host_config_path_for(
                        &self.tool,
                        agent,
                        sidecar,
                        home,
                        session_cfg,
                        host_environment,
                    );
                    let agents_dir = config_path.parent().unwrap_or(Path::new("."));
                    let path = (selected.resolve_config_file)(agents_dir, &name);
                    return match (sidecar.install)(
                        &path,
                        crate::hooks::HookInstallTarget::Host,
                        events,
                    ) {
                        Ok(()) => {
                            tracing::info!(target: "session.store",
                                "Installed AoE status hooks into {} agent '{}' at {}", self.tool, name, path.display());
                            true
                        }
                        Err(error) => {
                            tracing::warn!(target: "session.store",
                                "Failed to install AoE hooks into {} agent '{}' at {}: {}", self.tool, name, path.display(), error);
                            false
                        }
                    };
                }
            }
        }

        let Some(agent) = self.resolved_agent() else {
            return false;
        };
        let config_path = sidecar_host_config_path_for(
            &self.tool,
            agent,
            sidecar,
            home,
            session_cfg,
            host_environment,
        );
        match (sidecar.install)(&config_path, crate::hooks::HookInstallTarget::Host, events) {
            Ok(()) => {
                tracing::info!(target: "session.store",
                    "Installed AoE status hooks for {} via standalone hooks agent", self.tool);
                if !events.is_empty() {
                    if let Some(post_install) = sidecar.post_install_host {
                        post_install();
                    }
                }
                true
            }
            Err(error) => {
                tracing::warn!(target: "session.store",
                    "Failed to install {} hooks: {}", self.tool, error);
                false
            }
        }
    }

    fn install_codex_host_hooks(
        &self,
        session_cfg: &crate::session::config::SessionConfig,
        events: &[crate::agents::ResolvedHookEvent],
    ) -> bool {
        let environment = if events.is_empty() {
            self.profile_host_environment()
        } else {
            self.resolved_host_environment()
        };
        let home =
            crate::session::environment::resolve_host_environment_value(&environment, "HOME")
                .map(std::path::PathBuf::from)
                .or_else(dirs::home_dir);
        let Some(home) = home else {
            return false;
        };
        let Some(agent) = self.resolved_agent() else {
            return false;
        };
        let Some(hook_cfg) = agent.hook_config.as_ref() else {
            return false;
        };
        let hooks_path =
            generic_host_config_path_for(&self.tool, hook_cfg, &home, session_cfg, &environment);
        match crate::hooks::install_hooks(
            &hooks_path,
            events,
            crate::hooks::HookInstallTarget::Host,
        ) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(target: "session.store", "Failed to install Codex hooks: {}", error);
                false
            }
        }
    }
    fn install_json_host_hooks(
        &self,
        hook_cfg: &crate::agents::AgentHookConfig,
        session_cfg: &crate::session::config::SessionConfig,
        events: &[crate::agents::ResolvedHookEvent],
    ) -> bool {
        let environment = if events.is_empty() {
            self.profile_host_environment()
        } else {
            self.resolved_host_environment()
        };
        let home =
            crate::session::environment::resolve_host_environment_value(&environment, "HOME")
                .map(std::path::PathBuf::from)
                .or_else(dirs::home_dir);
        let Some(home) = home else {
            return false;
        };
        let settings_path =
            generic_host_config_path_for(&self.tool, hook_cfg, &home, session_cfg, &environment);
        match crate::hooks::install_hooks(
            &settings_path,
            events,
            crate::hooks::HookInstallTarget::Host,
        ) {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(target: "session.store", "Failed to install agent hooks: {}", error);
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::session::test_support::EnvGuard;

    fn expected_status_prefix(profile: &str, instance_id: &str) -> String {
        format!(
            "AOE_PROFILE={} AOE_INSTANCE_ID={} AOE_HOOK_BIN={} ",
            shell_escape(profile),
            shell_escape(instance_id),
            shell_escape(&std::env::current_exe().unwrap().to_string_lossy())
        )
    }

    fn acknowledge_hooks() {
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn cursor_sidecar_resolves_bare_config_dir_environment_entry() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let custom = temp.path().join("cursor-custom");
        let _cursor = EnvGuard::set(&[("CURSOR_CONFIG_DIR", custom.as_os_str())]);
        crate::session::config::update_config(|config| {
            config.environment = vec!["CURSOR_CONFIG_DIR".to_string()];
        })
        .unwrap();

        let mut inst = Instance::new("cursor", "/tmp/test");
        inst.tool = "cursor".to_string();
        inst.pending_host_env = vec![(
            "CURSOR_CONFIG_DIR".to_string(),
            temp.path()
                .join("undisclosed-dynamic-path")
                .to_string_lossy()
                .into_owned(),
        )];
        let config = crate::session::config::profile_config::resolve_config_or_warn("");
        let sidecar = crate::agents::get_agent("cursor")
            .unwrap()
            .sidecar_hooks
            .as_ref()
            .unwrap();

        assert_eq!(
            sidecar_host_config_path_for(
                &inst.tool,
                inst.resolved_agent().unwrap(),
                sidecar,
                temp.path(),
                &config.session,
                &inst.resolved_host_environment(),
            ),
            temp.path().join("undisclosed-dynamic-path/hooks.json")
        );
    }

    #[test]
    #[serial_test::serial]
    fn cursor_before_session_config_dir_change_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let profile_path = temp.path().join("profile-cursor");
        let launch_path = temp.path().join("launch-cursor");
        crate::session::config::update_config(|config| {
            config.environment = vec![format!(
                "CURSOR_CONFIG_DIR={}",
                profile_path.to_string_lossy()
            )];
        })
        .unwrap();
        acknowledge_hooks();
        let mut inst = Instance::new("cursor", "/tmp/test");
        inst.tool = "cursor".to_string();
        inst.pending_host_env = vec![(
            "CURSOR_CONFIG_DIR".to_string(),
            launch_path.to_string_lossy().into_owned(),
        )];

        let error = inst
            .ensure_disclosed_host_hook_path(crate::agents::get_agent("cursor"))
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("before_session changed the agent hook path"));
        assert!(!launch_path.join("hooks.json").exists());
        assert!(!profile_path.join("hooks.json").exists());
    }

    #[test]
    #[serial_test::serial]
    fn profile_home_routes_host_hook_installation() {
        let process_home = tempfile::tempdir().unwrap();
        let profile_home = tempfile::tempdir().unwrap();
        let _env = EnvGuard::set(&[
            ("HOME", process_home.path().as_os_str()),
            ("AOE_TEST_ALT_HOME", profile_home.path().as_os_str()),
        ]);
        let _app = crate::session::test_support::isolate_app_dir_at(process_home.path());
        acknowledge_hooks();
        crate::session::config::update_config(|config| {
            config.environment = vec!["HOME=$AOE_TEST_ALT_HOME".to_string()];
        })
        .unwrap();
        let mut inst = Instance::new("profile home", "/tmp/test");
        inst.tool = "cursor".to_string();

        assert_eq!(
            inst.resolved_host_home().as_deref(),
            Some(profile_home.path())
        );
        inst.install_agent_status_hooks(crate::agents::get_agent("cursor"));

        assert!(profile_home.path().join(".cursor/hooks.json").is_file());
        assert!(!process_home.path().join(".cursor/hooks.json").exists());
        assert!(inst.identity_publisher_launched);
    }

    #[test]
    #[serial_test::serial]
    fn sandbox_skips_host_hook_path_disclosure_guard() {
        let temp = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(temp.path());
        let mut inst = Instance::new("sandbox cursor", "/tmp/test");
        inst.tool = "cursor".to_string();
        inst.sandbox_info = Some(crate::session::SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test:latest".to_string(),
            container_name: "sandbox-cursor".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        inst.pending_host_env = vec![("HOME".to_string(), "/tmp/runtime-home".to_string())];

        inst.ensure_disclosed_host_hook_path(crate::agents::get_agent("cursor"))
            .unwrap();
    }

    #[test]
    fn test_codex_gets_status_hook_env_prefix() {
        let agent = crate::agents::get_agent("codex");
        assert_eq!(
            status_hook_env_prefix("work", "abc123", agent),
            expected_status_prefix("work", "abc123")
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

        acknowledge_hooks();
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

        acknowledge_hooks();
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

    #[test]
    #[serial_test::serial]
    fn host_hook_mutation_requires_durable_acknowledgement() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        std::env::set_var("HOME", tmp.path());
        let mut inst = Instance::new("cursor-unacknowledged", "/tmp/test");
        inst.tool = "cursor".to_string();
        inst.detect_as = "cursor".to_string();

        let error = inst
            .ensure_disclosed_host_hook_path(crate::agents::get_agent("cursor"))
            .unwrap_err();
        inst.install_agent_status_hooks(crate::agents::get_agent("cursor"));

        assert!(error.to_string().contains("have not been acknowledged"));
        assert!(!tmp.path().join(".cursor/hooks.json").exists());
    }

    #[test]
    #[serial_test::serial]
    fn status_only_agent_needs_no_ack_when_status_hooks_are_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        let profile_dir = crate::session::get_profile_dir("status-hooks-disabled").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "[session]
agent_status_hooks = false
",
        )
        .unwrap();
        let mut inst = Instance::new("gemini", "/tmp/test");
        inst.tool = "gemini".to_string();
        inst.detect_as = "gemini".to_string();
        inst.source_profile = "status-hooks-disabled".to_string();
        let agent = crate::agents::get_agent("gemini");

        inst.ensure_disclosed_host_hook_path(agent).unwrap();
        inst.install_agent_status_hooks(agent);

        assert!(!tmp.path().join(".gemini/settings.json").exists());
        assert!(!inst.identity_publisher_launched);
    }

    #[test]
    #[serial_test::serial]
    fn identity_hooks_remain_when_status_hooks_are_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        let profile_dir = crate::session::get_profile_dir("identity-only-hooks").unwrap();
        let custom_config = tmp.path().join("cursor-custom");
        std::fs::write(
            profile_dir.join("config.toml"),
            format!(
                "[session]\nagent_status_hooks = false\nagent_config_dir = {{ cursor = \"{}\" }}\n",
                custom_config.display()
            ),
        )
        .unwrap();

        acknowledge_hooks();
        let mut inst = Instance::new("cursor", "/tmp/test");
        inst.tool = "cursor".to_string();
        inst.detect_as = "cursor".to_string();
        inst.source_profile = "identity-only-hooks".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent("cursor"));

        let hooks_path = custom_config.join("hooks.json");
        let hooks: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&hooks_path).unwrap()).unwrap();
        let entries = hooks["hooks"]["beforeSubmitPrompt"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0]["command"]
            .as_str()
            .unwrap()
            .contains("conversation-id-or-session-id"));
        assert!(!tmp.path().join(".cursor/hooks.json").exists());
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

        acknowledge_hooks();
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
        acknowledge_hooks();

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
            expected_status_prefix("work", "abc123")
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("settl")),
            expected_status_prefix("work", "abc123")
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("claude")),
            expected_status_prefix("work", "abc123")
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("opencode")),
            ""
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("kiro")),
            expected_status_prefix("work", "abc123")
        );
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("kimi")),
            expected_status_prefix("work", "abc123")
        );
    }
    #[test]
    #[serial_test::serial]
    fn disabling_status_hooks_removes_stale_aoe_entries_but_keeps_foreign_hooks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let _env = EnvGuard::set(&[("HOME", tmp.path().as_os_str())]);
        acknowledge_hooks();

        let mut inst = Instance::new("gemini", "/tmp/test");
        inst.tool = "gemini".to_string();
        inst.detect_as = "gemini".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent("gemini"));
        let path = tmp.path().join(".gemini/settings.json");
        let mut settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        settings["hooks"]["ForeignEvent"] = serde_json::json!([{
            "hooks": [{"type": "command", "command": "printf foreign"}]
        }]);
        std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();

        let profile = "cleanup-disabled-hooks";
        let profile_dir = crate::session::get_profile_dir(profile).unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            "[session]
agent_status_hooks = false
",
        )
        .unwrap();
        inst.source_profile = profile.to_string();
        inst.ensure_disclosed_host_hook_path(crate::agents::get_agent("gemini"))
            .unwrap();
        inst.install_agent_status_hooks(crate::agents::get_agent("gemini"));

        let content = std::fs::read_to_string(path).unwrap();
        assert!(content.contains("printf foreign"));
        assert!(!content.contains("aoe-hooks"));
        assert!(!inst.identity_publisher_launched);
    }

    #[test]
    #[serial_test::serial]
    fn declared_generic_agent_config_roots_win_for_guard_and_install() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let _env = EnvGuard::set(&[("HOME", tmp.path().as_os_str())]);
        acknowledge_hooks();

        for (tool, env_key, filename) in [
            ("codex", "CODEX_HOME", "hooks.json"),
            ("claude", "CLAUDE_CONFIG_DIR", "settings.json"),
        ] {
            let profile = format!("declared-root-{tool}");
            let root = tmp.path().join(format!("custom-{tool}"));
            let profile_dir = crate::session::get_profile_dir(&profile).unwrap();
            std::fs::write(
                profile_dir.join("config.toml"),
                format!(
                    r#"environment = ["{env_key}=/profile/ignored"]
[session.agent_config_dir]
{tool} = "{}"
"#,
                    root.display()
                ),
            )
            .unwrap();

            let mut inst = Instance::new(tool, "/tmp/test");
            inst.tool = tool.to_string();
            inst.detect_as = tool.to_string();
            inst.source_profile = profile;
            inst.pending_host_env = vec![
                ("HOME".to_string(), "/runtime/ignored".to_string()),
                (env_key.to_string(), "/runtime/ignored-config".to_string()),
            ];
            inst.ensure_disclosed_host_hook_path(crate::agents::get_agent(tool))
                .unwrap();
            inst.install_agent_status_hooks(crate::agents::get_agent(tool));

            let path = root.join(filename);
            assert!(
                path.is_file(),
                "missing declared hook path {}",
                path.display()
            );
            assert!(std::fs::read_to_string(path).unwrap().contains("aoe-hooks"));
        }
    }
    #[test]
    #[serial_test::serial]
    fn agent_without_hooks_skips_host_path_disclosure_checks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let mut inst = Instance::new("opencode", "/tmp/test");
        inst.tool = "opencode".to_string();
        inst.pending_host_env = vec![("HOME".to_string(), "/undisclosed/home".to_string())];
        inst.ensure_disclosed_host_hook_path(crate::agents::get_agent("opencode"))
            .unwrap();
    }
}
