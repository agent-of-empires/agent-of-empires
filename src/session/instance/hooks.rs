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
        // `$$` is the launch shell, which `exec`s into the agent.
        format!(
            "AOE_PROFILE={} AOE_INSTANCE_ID={} AOE_HOOK_BIN={} AOE_AGENT_PID=$$ AOE_AGENT_BIN={} ",
            shell_escape(profile),
            shell_escape(instance_id),
            shell_escape(&hook_bin.to_string_lossy()),
            shell_escape(agent.map_or("", |agent| agent.binary))
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
        let agent = self.status_agent();
        self.ensure_disclosed_host_hook_path(agent)?;
        if self.is_sandboxed() {
            self.install_agent_status_hooks(agent, None);
        }
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

    /// Make AoE-managed skills available to the agent this session launches, by reconciling the
    /// managed store into that agent's own skills directory.
    fn propagate_managed_skills(&self) {
        // Read the global config, not the profile chain.
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
        let Some(agent) = agent else {
            return Ok(());
        };
        if self.is_sandboxed() {
            return Ok(());
        }
        let config = crate::session::config::profile_config::resolve_config_or_warn(
            &self.effective_profile(),
        );
        if !crate::agents::hook_install_required(agent, config.session.agent_status_hooks) {
            return Ok(());
        }
        if !host_hooks_acknowledged() {
            bail!(
                "agent hook paths have not been acknowledged; approve them in the AoE TUI before launching this host session"
            );
        }
        let profile_environment = self.profile_host_environment();
        let resolved_environment = self.resolved_host_environment();
        let profile_home = host_home(&profile_environment)
            .context("home directory unavailable for disclosed hook path")?;
        let resolved_home = host_home(&resolved_environment)
            .context("home directory unavailable for resolved hook path")?;
        let disclosed = host_hook_config_path(
            &self.tool,
            agent,
            &profile_home,
            &config.session,
            &profile_environment,
        );
        let resolved = host_hook_config_path(
            &self.tool,
            agent,
            &resolved_home,
            &config.session,
            &resolved_environment,
        );
        if let (Some(disclosed), Some(resolved)) = (disclosed, resolved) {
            if !same_hook_target(&disclosed, &resolved) {
                bail!(
                    "before_session changed the agent hook path from {} to {}; declare the override in the profile environment before consenting",
                    disclosed.display(),
                    resolved.display()
                );
            }
        }
        Ok(())
    }

    /// Install optional status hooks and mandatory authoritative identity hooks.
    pub(super) fn install_agent_status_hooks(
        &mut self,
        agent: Option<&'static crate::agents::AgentDef>,
        execution: Option<&super::execution::NativeExecution>,
    ) {
        self.identity_publisher_launched = false;
        let config = crate::session::config::profile_config::resolve_config_or_warn(
            &self.effective_profile(),
        );
        let status_hooks_enabled = config.session.agent_status_hooks;
        let Some(agent) = agent else {
            return;
        };
        if !self.is_sandboxed()
            && crate::agents::hook_install_required(agent, status_hooks_enabled)
            && !host_hooks_acknowledged()
        {
            tracing::warn!(
                target: "hooks.install",
                instance = %self.id,
                "skipping host hook installation until the user acknowledges the hook paths"
            );
            return;
        }
        let Some(events) = resolved_host_hook_events(agent, &config, status_hooks_enabled) else {
            return;
        };
        if self.is_sandboxed() {
            return;
        }
        // An empty event set installs nothing, so the profile's own environment resolves the path;
        // a real install has to use what `before_session` left behind.
        let environment = if events.is_empty() {
            self.profile_host_environment()
        } else {
            self.resolved_host_environment()
        };
        let Some(home) = host_home(&environment) else {
            return;
        };
        let installed = self.install_host_hooks(
            agent,
            &home,
            &config.session,
            &environment,
            &events,
            execution,
        );
        self.identity_publisher_launched =
            events.iter().any(|event| event.identity_field.is_some())
                && installed
                && self.hook_session_publisher_allowed_by_argv();
    }

    /// Install this agent's hooks into its host config file; `true` when they landed.
    fn install_host_hooks(
        &self,
        agent: &'static crate::agents::AgentDef,
        home: &Path,
        session_cfg: &crate::session::config::SessionConfig,
        host_environment: &[String],
        events: &[crate::agents::ResolvedHookEvent],
        execution: Option<&super::execution::NativeExecution>,
    ) -> bool {
        if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
            return self.install_sidecar_host_hooks(
                sidecar,
                home,
                session_cfg,
                host_environment,
                events,
            );
        }
        let Some(hook_cfg) = agent.hook_config.as_ref() else {
            return false;
        };
        let selected_root = execution
            .filter(|execution| execution.agent.name == "claude" && !events.is_empty())
            .and_then(|execution| {
                execution
                    .routing
                    .iter()
                    .find(|(key, _)| key == "CLAUDE_CONFIG_DIR")
            })
            .and_then(|(_, value)| value.as_deref());
        let path = selected_root
            .map(|root| Path::new(root).join("settings.json"))
            .unwrap_or_else(|| {
                generic_host_config_path_for(
                    &self.tool,
                    hook_cfg,
                    home,
                    session_cfg,
                    host_environment,
                )
            });
        match hook_cfg.format {
            crate::agents::HookFormat::CodexJson => {
                match crate::hooks::install_codex_json_hooks(
                    &path,
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
            crate::agents::HookFormat::JsonSettings => install_json_host_hooks(&path, events),
        }
    }

    /// Pre-trust this session's worktree in the agent's host config so it does not open on a
    /// folder-trust prompt.
    fn ensure_host_folder_trust(&self, agent: Option<&'static crate::agents::AgentDef>) {
        if self.is_sandboxed() {
            return;
        }
        let profile = self.effective_profile();
        let config = crate::session::config::profile_config::resolve_config_or_warn(&profile);
        if !config.session.pre_trust_agent_folders {
            return;
        }
        let (Some(agent), Some(home)) = (agent, host_home(&self.resolved_host_environment()))
        else {
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

    /// Install a sidecar agent's host hooks.
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
                    let Some(agent) = self.status_agent() else {
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

        let Some(agent) = self.status_agent() else {
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
}

/// The host config file this agent installs its hooks into, if it has hooks at all.
fn host_hook_config_path(
    tool: &str,
    agent: &crate::agents::AgentDef,
    home: &Path,
    session_cfg: &crate::session::config::SessionConfig,
    host_environment: &[String],
) -> Option<std::path::PathBuf> {
    if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
        return Some(sidecar_host_config_path_for(
            tool,
            agent,
            sidecar,
            home,
            session_cfg,
            host_environment,
        ));
    }
    agent.hook_config.as_ref().map(|hook_cfg| {
        generic_host_config_path_for(tool, hook_cfg, home, session_cfg, host_environment)
    })
}

pub(super) fn host_home(host_environment: &[String]) -> Option<std::path::PathBuf> {
    crate::session::environment::resolve_host_environment_value(host_environment, "HOME")
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
}

fn host_hooks_acknowledged() -> bool {
    crate::session::config::load_config()
        .ok()
        .flatten()
        .is_some_and(|config| config.app_state.has_acknowledged_agent_hooks)
}

/// The hook events to install: the profile's status hooks when it enables them, plus the identity
/// hooks, which are not optional.
fn resolved_host_hook_events(
    agent: &'static crate::agents::AgentDef,
    config: &crate::session::config::Config,
    status_hooks_enabled: bool,
) -> Option<Vec<crate::agents::ResolvedHookEvent>> {
    let resolved = if agent.sidecar_hooks.is_some() {
        crate::agents::resolved_sidecar_hook_events(agent, config)
    } else if agent.hook_config.is_some() {
        crate::agents::resolved_hook_events(agent, config)
    } else {
        return None;
    };
    let mut events = match resolved {
        Ok(events) => events,
        Err(error) => {
            tracing::warn!(target: "session.store",
                "Failed to resolve {} status hooks: {}", agent.name, error);
            return None;
        }
    };
    if !status_hooks_enabled {
        events.retain(|event| event.identity_field.is_some());
        for event in &mut events {
            event.status = None;
        }
    }
    Some(events)
}

/// Install JSON-settings hooks, reporting an unwritable target once per process.
fn install_json_host_hooks(
    settings_path: &Path,
    events: &[crate::agents::ResolvedHookEvent],
) -> bool {
    match crate::hooks::install_hooks(settings_path, events, crate::hooks::HookInstallTarget::Host)
    {
        Ok(()) => true,
        Err(error) => {
            if is_read_only_filesystem(&error) {
                match first_read_only_report(settings_path) {
                    Some(target) if target != settings_path => {
                        tracing::warn!(target: "session.store",
                            "Agent settings at {} resolve to {}, which is on a read-only filesystem, so AoE status hooks cannot be installed there; not reported again for that target while this process runs.",
                            settings_path.display(), target.display());
                    }
                    Some(_) => {
                        tracing::warn!(target: "session.store",
                            "Agent settings at {} are on a read-only filesystem, so AoE status hooks cannot be installed there; not reported again for that file while this process runs.",
                            settings_path.display());
                    }
                    None => {
                        tracing::debug!(target: "session.store",
                            "Agent settings at {} are still read-only; hook install skipped again.",
                            settings_path.display());
                    }
                }
            } else {
                tracing::warn!(target: "session.store", "Failed to install agent hooks: {}", error);
            }
            false
        }
    }
}

/// Resolved settings targets already warned about as read-only. Entries live for the process, so a
/// target that turns writable and later read-only again is only logged at debug.
static READ_ONLY_SETTINGS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));

fn is_read_only_filesystem(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| io.kind() == std::io::ErrorKind::ReadOnlyFilesystem)
}

/// Resolve `path` to the file it actually names, following symlinks. A file
/// that does not exist yet resolves through its parent directory, so a hook
/// file AoE has not written yet still compares by the directory it will land
/// in. Falls back to the literal path when neither resolves.
fn hook_target_identity(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| std::fs::canonicalize(parent).ok())
            .zip(path.file_name())
            .map(|(parent, file_name)| parent.join(file_name))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

/// Whether the consented hook path and the resolved one name the same file.
///
/// Compared by resolved target rather than by literal path: an account
/// switcher driven from `before_session` points `CLAUDE_CONFIG_DIR` (or
/// `CODEX_HOME`) at a per-account directory whose `settings.json` /
/// `config.toml` is a symlink back to the one shared file, so the two paths
/// differ as strings while naming the same bytes on disk. Consent is about
/// which file gets written, so that case must pass.
///
/// A `before_session` that redirects the install into a genuinely different
/// file still resolves differently and is still refused, which is the property
/// the consent gate exists for.
fn same_hook_target(disclosed: &std::path::Path, resolved: &std::path::Path) -> bool {
    disclosed == resolved || hook_target_identity(disclosed) == hook_target_identity(resolved)
}

/// The resolved target to report, or `None` when it has already been reported.
fn first_read_only_report(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let key = hook_target_identity(path);
    READ_ONLY_SETTINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key.clone())
        .then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::instance::test_helpers::*;

    use crate::session::test_support::EnvGuard;

    fn expected_status_prefix(profile: &str, instance_id: &str, agent: &str) -> String {
        format!(
            "AOE_PROFILE={} AOE_INSTANCE_ID={} AOE_HOOK_BIN={} AOE_AGENT_PID=$$ AOE_AGENT_BIN={} ",
            shell_escape(profile),
            shell_escape(instance_id),
            shell_escape(&std::env::current_exe().unwrap().to_string_lossy()),
            shell_escape(crate::agents::get_agent(agent).unwrap().binary)
        )
    }

    fn hook_inst(tool: &str) -> Instance {
        let mut inst = Instance::new(tool, "/tmp/test");
        inst.tool = tool.to_string();
        inst.detect_as = tool.to_string();
        inst
    }

    fn write_profile(profile: &str, config: &str) {
        let dir = crate::session::get_profile_dir(profile).unwrap();
        std::fs::write(dir.join("config.toml"), config).unwrap();
    }

    fn assert_aoe_codex_hooks(path: &std::path::Path) {
        let hooks = std::fs::read_to_string(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&hooks).unwrap();
        assert!(parsed["hooks"]["PreToolUse"].is_array());
        assert!(hooks.contains("aoe-hooks"));
    }

    fn acknowledge_hooks() {
        crate::session::config::update_app_state(|state| {
            state.has_acknowledged_agent_hooks = true;
        })
        .unwrap();
    }

    /// The consent gate compares which FILE gets written, not which string
    /// names it. An account switcher driven from `before_session` points the
    /// agent's config dir at a per-account directory whose settings file is a
    /// symlink back to the shared one; those two paths differ as strings and
    /// must still count as consented. A redirect onto a genuinely different
    /// file must still be refused, which is the whole point of the gate.
    #[test]
    fn same_hook_target_follows_symlinks_but_not_distinct_files() {
        let dir = tempfile::tempdir().unwrap();

        // Identical paths never need to touch the filesystem.
        let plain = dir.path().join("settings.json");
        assert!(same_hook_target(&plain, &plain));

        // Two distinct real files are distinct, existing or not.
        let other = dir.path().join("other.json");
        std::fs::write(&plain, "{}").unwrap();
        std::fs::write(&other, "{}").unwrap();
        assert!(!same_hook_target(&plain, &other));
        assert!(!same_hook_target(
            &dir.path().join("absent-a.json"),
            &dir.path().join("absent-b.json")
        ));

        #[cfg(unix)]
        {
            // The switcher shape: <profile>/settings.json -> ~/.claude/settings.json.
            let shared = dir.path().join("claude");
            std::fs::create_dir(&shared).unwrap();
            let real = shared.join("settings.json");
            std::fs::write(&real, "{}").unwrap();

            let profile = dir.path().join("profiles").join("acct");
            std::fs::create_dir_all(&profile).unwrap();
            let linked = profile.join("settings.json");
            std::os::unix::fs::symlink(&real, &linked).unwrap();

            assert!(
                same_hook_target(&real, &linked),
                "a per-account symlink onto the consented file is the same target"
            );

            // A symlinked config DIRECTORY resolves too, which is the other
            // shape a switcher can take.
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&shared, &alias).unwrap();
            assert!(same_hook_target(&real, &alias.join("settings.json")));

            // A file that does not exist yet resolves through its parent, so a
            // first launch (nothing written) still compares by directory.
            assert!(same_hook_target(
                &shared.join("absent.json"),
                &alias.join("absent.json")
            ));

            // A redirect into an unrelated directory stays refused.
            let elsewhere = dir.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            assert!(
                !same_hook_target(&real, &elsewhere.join("settings.json")),
                "the gate must still refuse a genuinely different file"
            );
        }
    }

    #[test]
    fn read_only_settings_are_reported_once_per_target() {
        let cases = [
            (
                anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::ReadOnlyFilesystem)),
                true,
            ),
            (
                anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                false,
            ),
            (anyhow::anyhow!("hooks key is not a JSON object"), false),
        ];
        for (error, want) in &cases {
            assert_eq!(is_read_only_filesystem(error), *want, "{error}");
        }

        // Per target, so one unwritable settings file does not silence the
        // report for another, and two paths onto one target report once.
        let dir = tempfile::tempdir().unwrap();
        let one = dir.path().join("one.json");
        let two = dir.path().join("two.json");
        std::fs::write(&one, "{}").unwrap();
        std::fs::write(&two, "{}").unwrap();
        assert!(first_read_only_report(&one).is_some());
        assert!(first_read_only_report(&one).is_none());
        assert!(first_read_only_report(&two).is_some());

        #[cfg(unix)]
        {
            let target = dir.path().join("shared.json");
            std::fs::write(&target, "{}").unwrap();
            let link = dir.path().join("link.json");
            std::os::unix::fs::symlink(&target, &link).unwrap();
            let reported = first_read_only_report(&target).expect("first report names the target");
            assert_eq!(reported, std::fs::canonicalize(&target).unwrap());
            assert!(
                first_read_only_report(&link).is_none(),
                "a symlink onto an already-reported target must not report again"
            );

            let real = dir.path().join("real");
            std::fs::create_dir(&real).unwrap();
            let alias = dir.path().join("alias");
            std::os::unix::fs::symlink(&real, &alias).unwrap();
            let reported = first_read_only_report(&real.join("absent.json"))
                .expect("first report for a missing file");
            assert_eq!(
                reported,
                std::fs::canonicalize(&real).unwrap().join("absent.json")
            );
            assert!(
                first_read_only_report(&alias.join("absent.json")).is_none(),
                "a missing file reached through a symlinked parent shares its target"
            );
        }
    }

    /// The cases above build their own errors, so they cannot show that a real `install_hooks`
    /// failure carries a downcastable `io::Error` at all, which is what the classification rests
    /// on.
    #[test]
    fn a_real_install_hooks_error_keeps_its_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let settings = dir.path().join("settings.json");
        // A directory where the settings file belongs: `install_hooks` reads it
        // before writing, so the error comes from the real path.
        std::fs::create_dir(&settings).unwrap();

        let error = crate::hooks::install_hooks(
            &settings,
            &[] as &[crate::agents::ResolvedHookEvent],
            crate::hooks::HookInstallTarget::Host,
        )
        .expect_err("reading a directory as settings must fail");

        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<std::io::Error>().is_some()),
            "install_hooks must keep an io::Error in its chain: {error:?}"
        );
        assert!(!is_read_only_filesystem(&error), "{error:?}");
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

        let mut inst = tool_instance("cursor", "/tmp/test");
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
        let mut inst = tool_instance("cursor", "/tmp/test");
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
            host_home(&inst.resolved_host_environment()).as_deref(),
            Some(profile_home.path())
        );
        inst.install_agent_status_hooks(crate::agents::get_agent("cursor"), None);

        assert!(profile_home.path().join(".cursor/hooks.json").is_file());
        assert!(!process_home.path().join(".cursor/hooks.json").exists());
        assert!(inst.identity_publisher_launched);
    }

    #[test]
    #[serial_test::serial]
    fn codex_hook_installer_follows_detect_as_and_profile_hook_setting() {
        // (tool, profile config, global hooks off, expect hooks.json)
        for (tool, profile, global_off, installed) in [
            ("my-codex-wrapper", None, false, true),
            (
                "codex",
                Some("[session]\nagent_status_hooks = false\n"),
                false,
                false,
            ),
            (
                "codex",
                Some("[session]\nagent_status_hooks = true\n"),
                true,
                true,
            ),
        ] {
            let tmp = tempfile::TempDir::new().unwrap();
            let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
            let _home_guard = crate::session::test_support::isolate_home(tmp.path());
            if global_off {
                crate::session::config::update_config(|global| {
                    global.session.agent_status_hooks = false;
                })
                .unwrap();
            }
            acknowledge_hooks();
            let mut inst = hook_inst(tool);
            inst.detect_as = "codex".to_string();
            if let Some(config) = profile {
                write_profile("codex-hooks", config);
                inst.source_profile = "codex-hooks".to_string();
            }
            inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as), None);

            let hooks = tmp.path().join(".codex").join("hooks.json");
            if installed {
                assert_aoe_codex_hooks(&hooks);
            } else {
                assert!(!hooks.exists(), "{tool} {profile:?}");
            }
            assert!(!tmp.path().join(".codex").join("config.toml").exists());
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_hook_installer_uses_resolved_codex_home() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
        let _home_guard = crate::session::test_support::isolate_home(tmp.path());

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
        let mut inst = hook_inst("codex");
        inst.source_profile = "codex-profile".to_string();
        inst.pending_host_env = vec![(
            "CODEX_HOME".to_string(),
            resolved_codex_home.to_string_lossy().into_owned(),
        )];
        inst.install_agent_status_hooks(crate::agents::get_agent(&inst.detect_as), None);

        assert_aoe_codex_hooks(&resolved_codex_home.join("hooks.json"));
        assert!(!profile_codex_home.join("hooks.json").exists());
        assert!(!tmp.path().join(".codex").join("hooks.json").exists());
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_host_hook_install_honors_codex_feature_opt_out() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
        let _home_guard = EnvGuard::set(&[("HOME", tmp.path())]);
        let profile_config = crate::session::config::Config::default();
        let agent = crate::agents::get_agent("codex").unwrap();
        let events = crate::agents::resolved_hook_events(agent, &profile_config).unwrap();

        let writes = [
            ("hooks-off", "[features]\nhooks = false\n"),
            ("legacy-hooks-off", "[features]\ncodex_hooks = false\n"),
            ("hooks-on", "[features]\nhooks = true\n"),
        ]
        .map(|(case, config)| {
            let codex_home = tmp.path().join(case);
            std::fs::create_dir_all(&codex_home).unwrap();
            std::fs::write(codex_home.join("config.toml"), config).unwrap();

            let mut inst = Instance::new(case, "/tmp/test");
            inst.tool = "codex".to_string();
            inst.detect_as = "codex".to_string();
            inst.pending_host_env = vec![(
                "CODEX_HOME".to_string(),
                codex_home.to_string_lossy().into_owned(),
            )];
            let environment = inst.resolved_host_environment();
            let home = host_home(&environment).unwrap();
            inst.install_host_hooks(
                agent,
                &home,
                &profile_config.session,
                &environment,
                &events,
                None,
            );

            codex_home.join("hooks.json").exists()
        });

        assert_eq!(writes, [false, false, true]);
    }

    #[test]
    #[serial_test::serial]
    fn host_hook_disclosure_gate_requires_ack_only_for_host_hooks_it_will_write() {
        // Nothing is acknowledged and HOME is redirected, so only a skipped gate passes.
        // (tool, sandboxed, profile config, hooks file that must stay absent, refused)
        let mut failures = Vec::new();
        for (tool, sandboxed, profile, hooks_file, refused) in [
            ("cursor", false, None, ".cursor/hooks.json", true),
            ("cursor", true, None, ".cursor/hooks.json", false),
            ("opencode", false, None, ".opencode", false),
            (
                "gemini",
                false,
                Some("[session]\nagent_status_hooks = false\n"),
                ".gemini/settings.json",
                false,
            ),
        ] {
            let tmp = tempfile::TempDir::new().unwrap();
            let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
            let _home_guard = crate::session::test_support::isolate_home(tmp.path());
            let mut inst = hook_inst(tool);
            if sandboxed {
                inst.sandbox_info = Some(crate::session::instance::test_helpers::test_sandbox(
                    "sandbox-hooks",
                    None,
                ));
            }
            if let Some(config) = profile {
                write_profile("hook-gate", config);
                inst.source_profile = "hook-gate".to_string();
            }
            inst.pending_host_env = vec![("HOME".to_string(), "/undisclosed/home".to_string())];
            let agent = crate::agents::get_agent(tool);

            let gate = inst.ensure_disclosed_host_hook_path(agent);
            inst.install_agent_status_hooks(agent, None);

            let gate_ok = match &gate {
                Err(error) => refused && error.to_string().contains("have not been acknowledged"),
                Ok(()) => !refused,
            };
            if !gate_ok || tmp.path().join(hooks_file).exists() || inst.identity_publisher_launched
            {
                failures.push(format!(
                    "{tool} sandboxed={sandboxed}: gate {gate:?}, wrote {hooks_file}: {}",
                    tmp.path().join(hooks_file).exists()
                ));
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }

    #[test]
    #[serial_test::serial]
    fn identity_hooks_remain_when_status_hooks_are_disabled() {
        let tmp = tempfile::TempDir::new().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
        let _home_guard = crate::session::test_support::isolate_home(tmp.path());
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
        let mut inst = hook_inst("cursor");
        inst.source_profile = "identity-only-hooks".to_string();
        inst.install_agent_status_hooks(crate::agents::get_agent("cursor"), None);

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
            let _home_guard = crate::session::test_support::isolate_home(tmp.path());

            let profile_dir = crate::session::get_profile_dir(profile).unwrap();
            std::fs::write(
                profile_dir.join("config.toml"),
                format!("[session]\npre_trust_agent_folders = {enabled}\n"),
            )
            .unwrap();

            let project = tmp.path().join("repo");
            std::fs::create_dir_all(&project).unwrap();
            let mut inst = tool_instance("claude", project.to_str().unwrap());
            inst.detect_as = "claude".to_string();
            inst.source_profile = profile.to_string();
            if sandboxed {
                inst.sandbox_info = Some(crate::session::instance::test_helpers::test_sandbox(
                    "test-container",
                    None,
                ));
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
    fn status_hook_env_prefix_is_set_for_hook_agents_only() {
        for agent in ["codex", "hermes", "settl", "claude", "kiro", "kimi"] {
            assert_eq!(
                status_hook_env_prefix("work", "abc123", crate::agents::get_agent(agent)),
                expected_status_prefix("work", "abc123", agent)
            );
        }
        assert_eq!(
            status_hook_env_prefix("work", "abc123", crate::agents::get_agent("opencode")),
            ""
        );
    }

    #[test]
    #[serial_test::serial]
    fn disabling_status_hooks_removes_stale_aoe_entries_but_keeps_foreign_hooks() {
        // (agent, hooks file, native config written after install); codex also has its own
        // hooks feature turned off (#3781).
        for (tool, hooks_file, native_config) in [
            ("gemini", ".gemini/settings.json", None),
            (
                "codex",
                ".codex/hooks.json",
                Some((".codex/config.toml", "[features]\nhooks = false\n")),
            ),
        ] {
            let tmp = tempfile::TempDir::new().unwrap();
            let _app = crate::session::test_support::isolate_app_dir_at(&tmp.path().join("app"));
            let _codex_home_guard = EnvGuard::unset(&["CODEX_HOME"]);
            let _home_guard = EnvGuard::set(&[("HOME", tmp.path())]);
            acknowledge_hooks();

            let agent = crate::agents::get_agent(tool);
            let mut inst = hook_inst(tool);
            inst.install_agent_status_hooks(agent, None);
            let path = tmp.path().join(hooks_file);
            let mut settings: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            settings["hooks"]["ForeignEvent"] = serde_json::json!([{
                "hooks": [{"type": "command", "command": "printf foreign"}]
            }]);
            std::fs::write(&path, serde_json::to_vec_pretty(&settings).unwrap()).unwrap();
            if let Some((file, config)) = native_config {
                std::fs::write(tmp.path().join(file), config).unwrap();
            }

            write_profile(
                "cleanup-disabled-hooks",
                "[session]\nagent_status_hooks = false\n",
            );
            inst.source_profile = "cleanup-disabled-hooks".to_string();
            inst.ensure_disclosed_host_hook_path(agent).unwrap();
            inst.install_agent_status_hooks(agent, None);

            let content = std::fs::read_to_string(path).unwrap();
            assert!(content.contains("printf foreign"), "{tool}");
            assert!(!content.contains("aoe-hooks"), "{tool}");
            assert!(!inst.identity_publisher_launched, "{tool}");
        }
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
            inst.install_agent_status_hooks(crate::agents::get_agent(tool), None);

            let path = root.join(filename);
            assert!(
                path.is_file(),
                "missing declared hook path {}",
                path.display()
            );
            assert!(std::fs::read_to_string(path).unwrap().contains("aoe-hooks"));
        }
    }
}
