//! Profile-specific configuration with override support
//!
//! Profile configs allow per-profile overrides of global settings.
//! Fields set to None inherit from the global config.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

use super::config::{
    ColorMode, Config, ContainerRuntimeName, DefaultTerminalMode, TmuxClipboardMode, TmuxMouseMode,
    TmuxStatusBarMode, VolumeIgnoresStrategy,
};
use super::get_profile_dir;

/// Profile-specific settings. All fields are Option<T> - None means "inherit from global"
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// Short, human-readable description of what this profile does.
    /// Surfaced as helper text in the new-session profile picker (TUI + web).
    /// Profile-only: there is no global counterpart to inherit from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<ThemeConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updates: Option<UpdatesConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux: Option<TmuxConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HooksConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<crate::sound::SoundConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_hooks: Option<crate::status_hooks::StatusHookConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cockpit: Option<CockpitConfigOverride>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffConfigOverride>,

    /// Per-profile override for the host-side `environment` list. When
    /// `Some`, replaces the global list entirely (matching the existing
    /// `sandbox.environment` override semantics). `None` inherits the
    /// global value. Same entry grammar as `Config.environment`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::serde_helpers::option_string_or_vec"
    )]
    pub environment: Option<Vec<String>>,
}

/// Per-profile overrides for the [cockpit] config section. Every field
/// is `Option<T>`; when `None`, the global value wins. The TUI's
/// "Clear override" action sets the field to None.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CockpitConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_for_claude: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_workers: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_events: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_tool_durations: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_drain_mode: Option<crate::session::config::QueueDrainMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent_resumes: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_end_turn_threshold_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silent_orphan_grace_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub silent_orphan_fast_grace_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_stop_idle_secs: Option<u32>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThemeConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color_mode: Option<ColorMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_decay_minutes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdatesConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_check_mode: Option<crate::session::config::UpdateCheckMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_interval_hours: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_in_cli: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_poll_interval_minutes: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorktreeConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_template: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bare_repo_path_template: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_cleanup: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_branch_in_tui: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_branch_on_cleanup: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path_template: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init_submodules: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SandboxConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_by_default: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_image: Option<String>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::serde_helpers::option_string_or_vec"
    )]
    pub extra_volumes: Option<Vec<String>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::serde_helpers::option_string_or_vec"
    )]
    pub port_mappings: Option<Vec<String>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::serde_helpers::option_string_or_vec"
    )]
    pub environment: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_cleanup: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_limit: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_limit: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_terminal_mode: Option<DefaultTerminalMode>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::serde_helpers::option_string_or_vec"
    )]
    pub volume_ignores: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mount_ssh: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selinux_relabel: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_instruction: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_runtime: Option<ContainerRuntimeName>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ignores_strategy: Option<VolumeIgnoresStrategy>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TmuxConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_bar: Option<TmuxStatusBarMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse: Option<TmuxMouseMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<TmuxClipboardMode>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_tool: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yolo_mode_default: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_extra_args: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_command_override: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_status_hooks: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mouse_capture: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_agents: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_detect_as: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_cockpit_cmd: Option<HashMap<String, String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict_hotkeys: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snooze_duration_minutes: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_stop_idle_secs: Option<u32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restart_wake_message: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_tag: Option<super::config::RowTagMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_send_exit_chord: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_send_leader: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_session_attach_mode: Option<super::config::NewSessionAttachMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_attach_mode: Option<super::config::NewSessionAttachMode>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub click_action: Option<super::config::ClickAction>,
}

/// Per-profile override for diff view preferences.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiffConfigOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split_view: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HooksConfigOverride {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::serde_helpers::option_string_or_vec"
    )]
    pub on_create: Option<Vec<String>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::serde_helpers::option_string_or_vec"
    )]
    pub on_launch: Option<Vec<String>>,

    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::serde_helpers::option_string_or_vec"
    )]
    pub on_destroy: Option<Vec<String>>,
}

/// Load profile-specific config. Returns empty config if file doesn't exist.
///
/// Pure read: never creates the profile directory. Goes through the
/// non-creating path resolver so a GET /api/settings?profile=<unknown>
/// (which the dashboard fires on mount before profiles resolve) does
/// not pollute `profiles/` with a stub directory.
pub fn load_profile_config(profile: &str) -> Result<ProfileConfig> {
    let path = super::get_profile_dir_path(profile)?.join("config.toml");
    if !path.exists() {
        return Ok(ProfileConfig::default());
    }
    let content = fs::read_to_string(&path)?;
    if content.trim().is_empty() {
        return Ok(ProfileConfig::default());
    }
    let config: ProfileConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Save profile-specific config
pub fn save_profile_config(profile: &str, config: &ProfileConfig) -> Result<()> {
    let path = get_profile_config_path(profile)?;
    let content = toml::to_string_pretty(config)?;
    super::atomic_write(&path, content.as_bytes())?;
    Ok(())
}

/// Get the path to a profile's config file. This goes through the
/// creating [`get_profile_dir`] because the only remaining caller is
/// [`save_profile_config`], which needs the directory to exist before
/// the atomic write.
pub fn get_profile_config_path(profile: &str) -> Result<std::path::PathBuf> {
    Ok(get_profile_dir(profile)?.join("config.toml"))
}

/// Check if a profile has any overrides set
pub fn profile_has_overrides(config: &ProfileConfig) -> bool {
    config.description.is_some()
        || config.theme.is_some()
        || config.updates.is_some()
        || config.worktree.is_some()
        || config.sandbox.is_some()
        || config.tmux.is_some()
        || config.session.is_some()
        || config.hooks.is_some()
        || config.sound.is_some()
        || config.status_hooks.is_some()
        || config.cockpit.is_some()
        || config.diff.is_some()
        || config.environment.is_some()
}

/// Load effective config for a profile (global + profile overrides merged)
pub fn resolve_config(profile: &str) -> Result<Config> {
    let global = Config::load()?;
    let profile_config = load_profile_config(profile)?;
    Ok(merge_configs(global, &profile_config))
}

/// Like [`resolve_config`], but logs a warning on failure and returns defaults
/// instead of propagating the error.
pub fn resolve_config_or_warn(profile: &str) -> Config {
    match resolve_config(profile) {
        Ok(config) => config,
        Err(e) => {
            tracing::warn!(target: "session.profile",
                "Failed to load config for profile '{}', using defaults: {e}",
                profile
            );
            Config::default()
        }
    }
}

/// Apply sandbox config overrides to a target config.
pub fn apply_sandbox_overrides(
    target: &mut super::config::SandboxConfig,
    source: &SandboxConfigOverride,
) {
    if let Some(enabled_by_default) = source.enabled_by_default {
        target.enabled_by_default = enabled_by_default;
    }
    if let Some(ref default_image) = source.default_image {
        target.default_image = default_image.clone();
    }
    if let Some(ref extra_volumes) = source.extra_volumes {
        target.extra_volumes = extra_volumes.clone();
    }
    if let Some(ref port_mappings) = source.port_mappings {
        target.port_mappings = port_mappings.clone();
    }
    if let Some(ref environment) = source.environment {
        target.environment = environment.clone();
    }
    if let Some(auto_cleanup) = source.auto_cleanup {
        target.auto_cleanup = auto_cleanup;
    }
    if let Some(ref cpu_limit) = source.cpu_limit {
        target.cpu_limit = Some(cpu_limit.clone());
    }
    if let Some(ref memory_limit) = source.memory_limit {
        target.memory_limit = Some(memory_limit.clone());
    }
    if let Some(default_terminal_mode) = source.default_terminal_mode {
        target.default_terminal_mode = default_terminal_mode;
    }
    if let Some(ref volume_ignores) = source.volume_ignores {
        target.volume_ignores = volume_ignores.clone();
    }
    if let Some(mount_ssh) = source.mount_ssh {
        target.mount_ssh = mount_ssh;
    }
    if let Some(selinux_relabel) = source.selinux_relabel {
        target.selinux_relabel = selinux_relabel;
    }
    if let Some(ref custom_instruction) = source.custom_instruction {
        target.custom_instruction = Some(custom_instruction.clone());
    }
    if let Some(container_runtime) = source.container_runtime {
        target.container_runtime = container_runtime;
    }
    if let Some(volume_ignores_strategy) = source.volume_ignores_strategy {
        target.volume_ignores_strategy = volume_ignores_strategy;
    }
}

/// Apply worktree config overrides to a target config.
pub fn apply_worktree_overrides(
    target: &mut super::config::WorktreeConfig,
    source: &WorktreeConfigOverride,
) {
    if let Some(enabled) = source.enabled {
        target.enabled = enabled;
    }
    if let Some(ref path_template) = source.path_template {
        target.path_template = path_template.clone();
    }
    if let Some(ref bare_repo_path_template) = source.bare_repo_path_template {
        target.bare_repo_path_template = bare_repo_path_template.clone();
    }
    if let Some(auto_cleanup) = source.auto_cleanup {
        target.auto_cleanup = auto_cleanup;
    }
    if let Some(show_branch_in_tui) = source.show_branch_in_tui {
        target.show_branch_in_tui = show_branch_in_tui;
    }
    if let Some(delete_branch_on_cleanup) = source.delete_branch_on_cleanup {
        target.delete_branch_on_cleanup = delete_branch_on_cleanup;
    }
    if let Some(ref workspace_path_template) = source.workspace_path_template {
        target.workspace_path_template = workspace_path_template.clone();
    }
    if let Some(init_submodules) = source.init_submodules {
        target.init_submodules = init_submodules;
    }
}

/// Apply hooks config overrides to a target config.
pub fn apply_hooks_overrides(
    target: &mut crate::session::repo_config::HooksConfig,
    source: &HooksConfigOverride,
) {
    if let Some(ref on_create) = source.on_create {
        target.on_create = on_create.clone();
    }
    if let Some(ref on_launch) = source.on_launch {
        target.on_launch = on_launch.clone();
    }
    if let Some(ref on_destroy) = source.on_destroy {
        target.on_destroy = on_destroy.clone();
    }
}

/// Apply session config overrides to a target config.
pub fn apply_session_overrides(
    target: &mut super::config::SessionConfig,
    source: &SessionConfigOverride,
) {
    if source.default_tool.is_some() {
        target.default_tool = source.default_tool.clone();
    }
    if let Some(yolo_mode_default) = source.yolo_mode_default {
        target.yolo_mode_default = yolo_mode_default;
    }
    if let Some(ref args) = source.agent_extra_args {
        target.agent_extra_args = args.clone();
    }
    if let Some(ref overrides) = source.agent_command_override {
        target.agent_command_override = overrides.clone();
    }
    if let Some(agent_status_hooks) = source.agent_status_hooks {
        target.agent_status_hooks = agent_status_hooks;
    }
    if let Some(mouse_capture) = source.mouse_capture {
        target.mouse_capture = mouse_capture;
    }
    if let Some(ref custom_agents) = source.custom_agents {
        target.custom_agents = custom_agents.clone();
    }
    if let Some(ref detect_as) = source.agent_detect_as {
        target.agent_detect_as = detect_as.clone();
    }
    if let Some(ref cockpit_cmd) = source.agent_cockpit_cmd {
        target.agent_cockpit_cmd = cockpit_cmd.clone();
    }
    if let Some(strict_hotkeys) = source.strict_hotkeys {
        target.strict_hotkeys = strict_hotkeys;
    }
    if let Some(snooze_duration_minutes) = source.snooze_duration_minutes {
        target.snooze_duration_minutes = snooze_duration_minutes;
    }
    if let Some(auto_stop_idle_secs) = source.auto_stop_idle_secs {
        target.auto_stop_idle_secs = auto_stop_idle_secs;
    }
    if let Some(ref restart_wake_message) = source.restart_wake_message {
        target.restart_wake_message = restart_wake_message.clone();
    }
    if let Some(row_tag) = source.row_tag {
        target.row_tag = row_tag;
    }
    if let Some(ref live_send_exit_chord) = source.live_send_exit_chord {
        target.live_send_exit_chord = live_send_exit_chord.clone();
    }
    if let Some(ref live_send_leader) = source.live_send_leader {
        target.live_send_leader = live_send_leader.clone();
    }
    if let Some(new_session_attach_mode) = source.new_session_attach_mode {
        target.new_session_attach_mode = new_session_attach_mode;
    }
    if let Some(default_attach_mode) = source.default_attach_mode {
        target.default_attach_mode = default_attach_mode;
    }
    if let Some(click_action) = source.click_action {
        target.click_action = click_action;
    }
}

/// Apply tmux config overrides to a target config.
pub fn apply_tmux_overrides(target: &mut super::config::TmuxConfig, source: &TmuxConfigOverride) {
    if let Some(status_bar) = source.status_bar {
        target.status_bar = status_bar;
    }
    if let Some(mouse) = source.mouse {
        target.mouse = mouse;
    }
    if let Some(clipboard) = source.clipboard {
        target.clipboard = clipboard;
    }
}

/// Merge profile overrides into global config
pub fn merge_configs(mut global: Config, profile: &ProfileConfig) -> Config {
    if let Some(ref theme_override) = profile.theme {
        if let Some(ref name) = theme_override.name {
            global.theme.name = name.clone();
        }
        if let Some(ref color_mode) = theme_override.color_mode {
            global.theme.color_mode = color_mode.clone();
        }
        if let Some(idle_decay_minutes) = theme_override.idle_decay_minutes {
            global.theme.idle_decay_minutes = idle_decay_minutes;
        }
    }

    if let Some(ref updates_override) = profile.updates {
        if let Some(update_check_mode) = updates_override.update_check_mode {
            global.updates.update_check_mode = update_check_mode;
        }
        if let Some(check_interval_hours) = updates_override.check_interval_hours {
            global.updates.check_interval_hours = check_interval_hours;
        }
        if let Some(notify_in_cli) = updates_override.notify_in_cli {
            global.updates.notify_in_cli = notify_in_cli;
        }
        if let Some(web_poll_interval_minutes) = updates_override.web_poll_interval_minutes {
            global.updates.web_poll_interval_minutes = web_poll_interval_minutes;
        }
    }

    if let Some(ref worktree_override) = profile.worktree {
        apply_worktree_overrides(&mut global.worktree, worktree_override);
    }

    if let Some(ref sandbox_override) = profile.sandbox {
        apply_sandbox_overrides(&mut global.sandbox, sandbox_override);
    }

    if let Some(ref tmux_override) = profile.tmux {
        apply_tmux_overrides(&mut global.tmux, tmux_override);
    }

    if let Some(ref session_override) = profile.session {
        apply_session_overrides(&mut global.session, session_override);
    }

    if let Some(ref hooks_override) = profile.hooks {
        apply_hooks_overrides(&mut global.hooks, hooks_override);
    }

    if let Some(ref sound_override) = profile.sound {
        crate::sound::apply_sound_overrides(&mut global.sound, sound_override);
    }

    if let Some(ref status_hook_override) = profile.status_hooks {
        crate::status_hooks::apply_status_hook_overrides(
            &mut global.status_hooks,
            status_hook_override,
        );
    }

    if let Some(ref environment) = profile.environment {
        // Replace semantics (matches sandbox.environment override behaviour).
        global.environment = environment.clone();
    }

    if let Some(ref cockpit_override) = profile.cockpit {
        if let Some(v) = cockpit_override.enabled {
            global.cockpit.enabled = v;
        }
        if let Some(v) = cockpit_override.default_for_claude {
            global.cockpit.default_for_claude = v;
        }
        if let Some(ref v) = cockpit_override.default_agent {
            global.cockpit.default_agent = v.clone();
        }
        if let Some(v) = cockpit_override.max_concurrent_workers {
            global.cockpit.max_concurrent_workers = v;
        }
        if let Some(v) = cockpit_override.replay_events {
            global.cockpit.replay_events = v;
        }
        if let Some(v) = cockpit_override.replay_bytes {
            global.cockpit.replay_bytes = v;
        }
        if let Some(ref v) = cockpit_override.node_path {
            global.cockpit.node_path = v.clone();
        }
        if let Some(v) = cockpit_override.show_tool_durations {
            global.cockpit.show_tool_durations = v;
        }
        if let Some(v) = cockpit_override.queue_drain_mode {
            global.cockpit.queue_drain_mode = v;
        }
        if let Some(v) = cockpit_override.max_concurrent_resumes {
            global.cockpit.max_concurrent_resumes = v;
        }
        if let Some(v) = cockpit_override.force_end_turn_threshold_secs {
            global.cockpit.force_end_turn_threshold_secs = v;
        }
        if let Some(v) = cockpit_override.silent_orphan_grace_secs {
            global.cockpit.silent_orphan_grace_secs = v;
        }
        if let Some(v) = cockpit_override.silent_orphan_fast_grace_secs {
            global.cockpit.silent_orphan_fast_grace_secs = v;
        }
        if let Some(v) = cockpit_override.auto_stop_idle_secs {
            global.cockpit.auto_stop_idle_secs = v;
        }
    }

    if let Some(ref diff_override) = profile.diff {
        if let Some(split_view) = diff_override.split_view {
            global.diff.split_view = split_view;
        }
    }

    global
}

/// Validate Docker volume format (host:container[:options])
pub fn validate_volume_format(volume: &str) -> Result<(), String> {
    if volume.is_empty() {
        return Err("Volume cannot be empty".to_string());
    }

    let parts: Vec<&str> = volume.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return Err("Volume must be in format host:container[:options]".to_string());
    }

    if parts[0].is_empty() || parts[1].is_empty() {
        return Err("Host and container paths cannot be empty".to_string());
    }

    Ok(())
}

/// Validate Docker memory limit format (e.g., "512m", "2g")
pub fn validate_memory_limit(limit: &str) -> Result<(), String> {
    if limit.is_empty() {
        return Ok(());
    }

    let re = regex::Regex::new(r"^\d+[bkmgBKMG]?$").unwrap();
    if re.is_match(limit) {
        Ok(())
    } else {
        Err("Memory limit must be a number optionally followed by b, k, m, or g".to_string())
    }
}

/// Validate check interval is positive
pub fn validate_check_interval(hours: u64) -> Result<(), String> {
    if hours == 0 {
        Err("Check interval must be greater than 0".to_string())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_profile_config_default() {
        let config = ProfileConfig::default();
        assert!(config.theme.is_none());
        assert!(config.updates.is_none());
        assert!(config.worktree.is_none());
        assert!(config.sandbox.is_none());
        assert!(config.tmux.is_none());
    }

    #[test]
    fn test_profile_config_serialization_empty() {
        let config = ProfileConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        // Empty config should serialize to empty (skip_serializing_if)
        assert!(serialized.trim().is_empty());
    }

    #[test]
    fn test_profile_config_serialization_partial() {
        use crate::session::config::UpdateCheckMode;
        let config = ProfileConfig {
            updates: Some(UpdatesConfigOverride {
                update_check_mode: Some(UpdateCheckMode::Off),
                ..Default::default()
            }),
            ..Default::default()
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("[updates]"));
        assert!(serialized.contains("update_check_mode = \"off\""));
    }

    #[test]
    fn test_profile_config_deserialization() {
        use crate::session::config::UpdateCheckMode;
        let toml = r#"
            [updates]
            update_check_mode = "off"
            check_interval_hours = 48

            [sandbox]
            enabled_by_default = true
        "#;

        let config: ProfileConfig = toml::from_str(toml).unwrap();
        assert!(config.updates.is_some());
        let updates = config.updates.unwrap();
        assert_eq!(updates.update_check_mode, Some(UpdateCheckMode::Off));
        assert_eq!(updates.check_interval_hours, Some(48));

        assert!(config.sandbox.is_some());
        let sandbox = config.sandbox.unwrap();
        assert_eq!(sandbox.enabled_by_default, Some(true));
    }

    #[test]
    fn test_merge_configs_no_overrides() {
        let global = Config::default();
        let profile = ProfileConfig::default();
        let merged = merge_configs(global.clone(), &profile);

        assert_eq!(
            merged.updates.update_check_mode,
            global.updates.update_check_mode
        );
        assert_eq!(merged.worktree.enabled, global.worktree.enabled);
    }

    #[test]
    fn test_merge_configs_with_overrides() {
        use crate::session::config::UpdateCheckMode;
        let global = Config::default();
        let profile = ProfileConfig {
            updates: Some(UpdatesConfigOverride {
                update_check_mode: Some(UpdateCheckMode::Off),
                check_interval_hours: Some(48),
                ..Default::default()
            }),
            worktree: Some(WorktreeConfigOverride {
                enabled: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);

        assert_eq!(merged.updates.update_check_mode, UpdateCheckMode::Off);
        assert_eq!(merged.updates.check_interval_hours, 48);
        // notify_in_cli should retain global default since not overridden
        assert!(merged.updates.notify_in_cli);
        assert!(merged.worktree.enabled);
    }

    #[test]
    fn test_merge_configs_with_status_hook_overrides() {
        let mut global = Config::default();
        global.status_hooks.enabled = false;
        global.status_hooks.on_waiting = Some("global-waiting".to_string());
        global.status_hooks.debounce_ms = 100;

        let profile = ProfileConfig {
            status_hooks: Some(crate::status_hooks::StatusHookConfigOverride {
                enabled: Some(true),
                debounce_ms: Some(500),
                on_waiting: Some("profile-waiting".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        assert!(merged.status_hooks.enabled);
        assert_eq!(
            merged.status_hooks.on_waiting.as_deref(),
            Some("profile-waiting")
        );
        assert_eq!(merged.status_hooks.debounce_ms, 500);
    }

    #[test]
    fn test_profile_has_overrides() {
        let empty = ProfileConfig::default();
        assert!(!profile_has_overrides(&empty));

        let with_override = ProfileConfig {
            theme: Some(ThemeConfigOverride {
                name: Some("dark".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(profile_has_overrides(&with_override));
    }

    #[test]
    fn test_validate_volume_format() {
        assert!(validate_volume_format("/host:/container").is_ok());
        assert!(validate_volume_format("/host:/container:ro").is_ok());
        assert!(validate_volume_format("").is_err());
        assert!(validate_volume_format("/only-one").is_err());
        assert!(validate_volume_format(":/container").is_err());
        assert!(validate_volume_format("/host:").is_err());
    }

    #[test]
    fn test_validate_memory_limit() {
        assert!(validate_memory_limit("").is_ok());
        assert!(validate_memory_limit("512m").is_ok());
        assert!(validate_memory_limit("2g").is_ok());
        assert!(validate_memory_limit("1024").is_ok());
        assert!(validate_memory_limit("invalid").is_err());
        assert!(validate_memory_limit("512mb").is_err());
    }

    #[test]
    fn test_validate_check_interval() {
        assert!(validate_check_interval(1).is_ok());
        assert!(validate_check_interval(24).is_ok());
        assert!(validate_check_interval(0).is_err());
    }

    #[test]
    fn test_merge_configs_with_tmux_mouse_override() {
        let global = Config::default();
        assert_eq!(global.tmux.mouse, TmuxMouseMode::Auto);

        let profile = ProfileConfig {
            tmux: Some(TmuxConfigOverride {
                mouse: Some(TmuxMouseMode::Enabled),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.mouse, TmuxMouseMode::Enabled);
    }

    #[test]
    fn test_merge_configs_tmux_mouse_inherits_when_not_overridden() {
        let mut global = Config::default();
        global.tmux.mouse = TmuxMouseMode::Enabled;

        let profile = ProfileConfig {
            tmux: Some(TmuxConfigOverride {
                status_bar: Some(TmuxStatusBarMode::Enabled),
                mouse: None,
                clipboard: None,
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.mouse, TmuxMouseMode::Enabled); // Should inherit from global
        assert_eq!(merged.tmux.status_bar, TmuxStatusBarMode::Enabled);
    }

    #[test]
    fn test_merge_configs_tmux_mouse_disabled_override() {
        let mut global = Config::default();
        global.tmux.mouse = TmuxMouseMode::Enabled;

        let profile = ProfileConfig {
            tmux: Some(TmuxConfigOverride {
                mouse: Some(TmuxMouseMode::Disabled),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.mouse, TmuxMouseMode::Disabled);
    }

    #[test]
    fn test_merge_configs_with_tmux_clipboard_override() {
        let global = Config::default();
        assert_eq!(global.tmux.clipboard, TmuxClipboardMode::Auto);

        let profile = ProfileConfig {
            tmux: Some(TmuxConfigOverride {
                clipboard: Some(TmuxClipboardMode::Disabled),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.clipboard, TmuxClipboardMode::Disabled);
    }

    #[test]
    fn test_merge_configs_tmux_clipboard_inherits_when_not_overridden() {
        let mut global = Config::default();
        global.tmux.clipboard = TmuxClipboardMode::Enabled;

        let profile = ProfileConfig {
            tmux: Some(TmuxConfigOverride {
                mouse: Some(TmuxMouseMode::Enabled),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.clipboard, TmuxClipboardMode::Enabled);
    }

    #[test]
    fn test_merge_configs_with_volume_ignores_override() {
        let global = Config::default();
        assert!(global.sandbox.volume_ignores.is_empty());

        let profile = ProfileConfig {
            sandbox: Some(SandboxConfigOverride {
                volume_ignores: Some(vec!["target".to_string(), "node_modules".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        assert_eq!(
            merged.sandbox.volume_ignores,
            vec!["target", "node_modules"]
        );
    }

    #[test]
    fn test_merge_configs_volume_ignores_inherits_when_not_overridden() {
        let mut global = Config::default();
        global.sandbox.volume_ignores = vec!["target".to_string()];

        let profile = ProfileConfig {
            sandbox: Some(SandboxConfigOverride {
                enabled_by_default: Some(true),
                volume_ignores: None,
                ..Default::default()
            }),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        assert_eq!(merged.sandbox.volume_ignores, vec!["target"]);
        assert!(merged.sandbox.enabled_by_default);
    }

    #[test]
    fn test_volume_ignores_override_serialization() {
        let config = ProfileConfig {
            sandbox: Some(SandboxConfigOverride {
                volume_ignores: Some(vec!["target".to_string(), ".venv".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("volume_ignores"));

        let deserialized: ProfileConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.sandbox.unwrap().volume_ignores,
            Some(vec!["target".to_string(), ".venv".to_string()])
        );
    }

    #[test]
    fn test_tmux_config_override_serialization() {
        let config = ProfileConfig {
            tmux: Some(TmuxConfigOverride {
                status_bar: Some(TmuxStatusBarMode::Enabled),
                mouse: Some(TmuxMouseMode::Enabled),
                clipboard: Some(TmuxClipboardMode::Enabled),
            }),
            ..Default::default()
        };

        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("[tmux]"));
        assert!(serialized.contains(r#"mouse = "enabled""#));

        let deserialized: ProfileConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(
            deserialized.tmux.as_ref().unwrap().mouse,
            Some(TmuxMouseMode::Enabled)
        );
    }

    #[test]
    fn test_merge_configs_with_theme_override() {
        let global = Config::default();
        let profile = ProfileConfig {
            theme: Some(ThemeConfigOverride {
                name: Some("tokyo-night".to_string()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.theme.name, "tokyo-night");
    }

    #[test]
    fn test_merge_configs_theme_inherits_when_not_overridden() {
        let mut global = Config::default();
        global.theme.name = "catppuccin-latte".to_string();

        let profile = ProfileConfig::default();
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.theme.name, "catppuccin-latte");
    }

    #[test]
    fn test_sandbox_override_string_shorthand() {
        // Regression test: all Option<Vec<String>> sandbox fields accept a plain string
        let toml = r#"
            [sandbox]
            environment = "ANTHROPIC_API_KEY"
            extra_volumes = "/data:/data:ro"
            volume_ignores = "node_modules"
            port_mappings = "3000:3000"
        "#;
        let config: ProfileConfig = toml::from_str(toml).unwrap();
        let sb = config.sandbox.unwrap();
        assert_eq!(sb.environment, Some(vec!["ANTHROPIC_API_KEY".to_string()]));
        assert_eq!(sb.extra_volumes, Some(vec!["/data:/data:ro".to_string()]));
        assert_eq!(sb.volume_ignores, Some(vec!["node_modules".to_string()]));
        assert_eq!(sb.port_mappings, Some(vec!["3000:3000".to_string()]));
    }

    #[test]
    fn test_hooks_override_string_shorthand() {
        // Regression test: HooksConfigOverride accepts a plain string
        let toml = r#"
            [hooks]
            on_create = "npm install"
            on_launch = "npm start"
        "#;
        let config: ProfileConfig = toml::from_str(toml).unwrap();
        let hooks = config.hooks.unwrap();
        assert_eq!(hooks.on_create, Some(vec!["npm install".to_string()]));
        assert_eq!(hooks.on_launch, Some(vec!["npm start".to_string()]));
    }

    #[test]
    fn test_environment_override_round_trips() {
        let toml_in = r#"
            environment = ["CLAUDE_CONFIG_DIR=/home/me/.claude-accounts/work", "GH_TOKEN"]
        "#;
        let config: ProfileConfig = toml::from_str(toml_in).unwrap();
        let env = config.environment.clone().unwrap();
        assert_eq!(
            env,
            vec![
                "CLAUDE_CONFIG_DIR=/home/me/.claude-accounts/work".to_string(),
                "GH_TOKEN".to_string(),
            ]
        );

        let out = toml::to_string_pretty(&config).unwrap();
        assert!(out.contains("CLAUDE_CONFIG_DIR=/home/me/.claude-accounts/work"));
        assert!(out.contains("GH_TOKEN"));
    }

    #[test]
    fn test_environment_string_shorthand_deserializes() {
        // `string_or_vec` lets a single string stand in for a one-element list.
        let toml_in = r#"environment = "FOO=bar""#;
        let config: ProfileConfig = toml::from_str(toml_in).unwrap();
        assert_eq!(config.environment, Some(vec!["FOO=bar".to_string()]));
    }

    #[test]
    fn test_environment_override_promotes_profile_has_overrides() {
        let mut profile = ProfileConfig::default();
        assert!(!profile_has_overrides(&profile));
        profile.environment = Some(vec!["FOO=bar".to_string()]);
        assert!(profile_has_overrides(&profile));
    }

    #[test]
    fn test_merge_configs_replaces_global_environment() {
        let global = Config {
            environment: vec!["FROM_GLOBAL=1".to_string()],
            ..Default::default()
        };

        let profile = ProfileConfig {
            environment: Some(vec!["FROM_PROFILE=2".to_string()]),
            ..Default::default()
        };

        let merged = merge_configs(global, &profile);
        // Profile env replaces (matches sandbox.environment semantics).
        assert_eq!(merged.environment, vec!["FROM_PROFILE=2".to_string()]);
    }

    #[test]
    fn test_description_round_trips() {
        let toml_in = r#"description = "Read-only review profile""#;
        let config: ProfileConfig = toml::from_str(toml_in).unwrap();
        assert_eq!(
            config.description.as_deref(),
            Some("Read-only review profile"),
        );

        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("Read-only review profile"));
    }

    #[test]
    fn test_description_default_is_none() {
        let config = ProfileConfig::default();
        assert!(config.description.is_none());
        // Default empty config must still serialize empty so untouched profiles
        // don't grow a description line on first save.
        let serialized = toml::to_string(&config).unwrap();
        assert!(serialized.trim().is_empty());
    }

    #[test]
    fn test_description_promotes_profile_has_overrides() {
        let mut profile = ProfileConfig::default();
        assert!(!profile_has_overrides(&profile));
        profile.description = Some("My profile".to_string());
        assert!(profile_has_overrides(&profile));
    }

    #[test]
    fn test_merge_configs_inherits_global_environment_when_profile_none() {
        let global = Config {
            environment: vec!["FROM_GLOBAL=1".to_string()],
            ..Default::default()
        };

        let profile = ProfileConfig::default();

        let merged = merge_configs(global, &profile);
        assert_eq!(merged.environment, vec!["FROM_GLOBAL=1".to_string()]);
    }

    #[test]
    fn profile_diff_split_view_override_applies() {
        let mut global = Config::default();
        global.diff.split_view = false;
        let profile = ProfileConfig {
            diff: Some(DiffConfigOverride {
                split_view: Some(true),
            }),
            ..Default::default()
        };
        let merged = merge_configs(global, &profile);
        assert!(merged.diff.split_view);
    }
}
