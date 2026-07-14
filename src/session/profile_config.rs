//! Profile-specific configuration with override support
//!
//! Profile configs allow per-profile overrides of global settings.
//! Fields set to None inherit from the global config.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;

use super::config::Config;
use super::get_profile_dir;

/// Profile-specific settings, stored as a sparse override tree (#1692).
///
/// Every override is a section table keyed by config-section name (e.g.
/// `sandbox`, `acp`) mirroring the `Config` JSON shape; an absent key
/// inherits the global value. There are no typed per-section structs: a field
/// is overridable purely by virtue of existing in the `Config` schema, so
/// adding one never touches this file. Merging is the generic recursive
/// [`merge_configs_generic`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// Short, human-readable description of what this profile does.
    /// Surfaced as helper text in the new-session profile picker (TUI + web).
    /// Profile-only: there is no global counterpart to inherit from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Sparse overrides, keyed by config section. Flattened so the on-disk TOML
    /// keeps the historical `[section]` table layout (no migration needed).
    #[serde(flatten)]
    pub overrides: serde_json::Map<String, serde_json::Value>,
}

impl ProfileConfig {
    /// The overrides as a JSON object, ready to merge onto a serialized
    /// `Config`. Excludes the profile-only `description`.
    fn overrides_value(&self) -> serde_json::Value {
        serde_json::Value::Object(self.overrides.clone())
    }
}

/// Load profile-specific config. Returns empty config if file doesn't exist.
///
/// Pure read: never creates the profile directory. Goes through the
/// non-creating path resolver so a GET `/api/settings?profile=<unknown>`
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
    // Type-check the overrides by merging onto a default Config. The sparse map
    // accepts any JSON, so a wrong-typed value (e.g. `worktree.enabled = "yes"`)
    // would otherwise only surface as a panic at merge time; reject it here so
    // the caller warns and falls back to defaults.
    validate_overrides_typecheck(&config.overrides_value())?;
    Ok(config)
}

/// Confirm a sparse override object deserializes back into a [`Config`] when
/// merged onto the defaults. Used at load time so a malformed override file is
/// a graceful error rather than a merge-time panic.
pub(super) fn validate_overrides_typecheck(overrides: &serde_json::Value) -> Result<()> {
    let mut base = serde_json::to_value(Config::default())?;
    crate::session::settings_schema::merge_json(&mut base, overrides);
    serde_json::from_value::<Config>(base)
        .map_err(|e| anyhow::anyhow!("invalid override value: {e}"))?;
    Ok(())
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
    config.description.is_some() || !config.overrides.is_empty()
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

/// Merge profile overrides into global config.
///
/// Delegates to [`merge_configs_generic`]: the profile's sparse override tree is
/// JSON-merged onto the global config, so adding a config field never touches
/// this function.
pub fn merge_configs(global: Config, profile: &ProfileConfig) -> Config {
    merge_configs_generic(&global, &profile.overrides_value())
}

/// Generic single-source merge (#1692): serialize the global config to JSON,
/// apply the overrides as a sparse JSON merge (object keys recurse, scalars and
/// arrays replace), and deserialize back into a typed [`Config`].
///
/// This works for every section without per-field arms, so adding a config
/// field never touches a merge function. The deserialize is infallible in
/// practice because every override-writing path (file load, server PATCH, TUI)
/// type-checks against the schema first; see `validate_overrides_typecheck`.
pub fn merge_configs_generic(global: &Config, overrides: &serde_json::Value) -> Config {
    let mut base = serde_json::to_value(global).expect("Config serializes to JSON");
    crate::session::settings_schema::merge_json(&mut base, overrides);
    serde_json::from_value(base).expect("merged config deserializes")
}

/// Validate Docker volume format (`host:container[:options]`)
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

/// Validate a sandbox env entry: bare `KEY` or `KEY=VALUE`. The key is
/// letters, digits, and underscores and must not start with a digit; the
/// value (after `=`) is unconstrained. Mirrors the dashboard's client-side
/// check so the schema drives both surfaces.
pub fn validate_env_format(entry: &str) -> Result<(), String> {
    let re = regex::Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(=.*)?$").unwrap();
    if re.is_match(entry) {
        Ok(())
    } else {
        Err("Must be KEY or KEY=VALUE (letters, digits, underscores)".to_string())
    }
}

/// Validate a `host:container` port mapping (digits only on both sides).
pub fn validate_port_mapping_format(mapping: &str) -> Result<(), String> {
    let re = regex::Regex::new(r"^\d+:\d+$").unwrap();
    if re.is_match(mapping) {
        Ok(())
    } else {
        Err("Must be port:port (e.g. 3000:3000)".to_string())
    }
}

/// Validate a container network mode (`sandbox.network`). Empty (unset),
/// `none`, `bridge`, or a named network matching Docker's network-name grammar
/// are accepted. `host` is rejected outright because sharing the host network
/// namespace defeats sandbox isolation, and the `container:`/`ns:` namespace
/// forms are rejected by the name grammar (they contain a colon).
pub fn validate_network_format(network: &str) -> Result<(), String> {
    if network.is_empty() {
        return Ok(());
    }
    if network.eq_ignore_ascii_case("host") {
        return Err("host network mode defeats sandbox isolation and is not allowed".to_string());
    }
    let re = regex::Regex::new(r"^[a-zA-Z0-9][a-zA-Z0-9_.-]*$").unwrap();
    if re.is_match(network) {
        Ok(())
    } else {
        Err("Must be 'none', 'bridge', or a network name".to_string())
    }
}

/// Validate Docker memory limit format (e.g., "512m", "2g")
pub fn validate_memory_limit(limit: &str) -> Result<(), String> {
    if limit.is_empty() {
        return Ok(());
    }

    // Require a unit suffix. A bare number is bytes to Docker, which is almost
    // never intended and falls below Docker's ~6MB floor anyway, so reject it
    // up front with a message that matches the field's "512m"/"8g" examples
    // (issue #2083 smoke test).
    let re = regex::Regex::new(r"^\d+[bkmgBKMG]$").unwrap();
    if re.is_match(limit) {
        Ok(())
    } else {
        Err("Memory limit must be a number followed by b, k, m, or g (e.g. 512m, 8g)".to_string())
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
    use serde_json::json;

    /// Build a `ProfileConfig` from a sparse override object (the on-disk shape).
    fn profile_from(overrides: serde_json::Value) -> ProfileConfig {
        serde_json::from_value(overrides).expect("profile override deserializes")
    }

    #[test]
    fn validate_network_accepts_empty_none_bridge_and_named() {
        assert!(validate_network_format("").is_ok());
        assert!(validate_network_format("none").is_ok());
        assert!(validate_network_format("bridge").is_ok());
        assert!(validate_network_format("egress-proxy").is_ok());
        assert!(validate_network_format("my_net.1").is_ok());
    }

    #[test]
    fn validate_network_rejects_host_and_namespace_forms() {
        assert!(validate_network_format("host").is_err());
        assert!(validate_network_format("HOST").is_err());
        assert!(validate_network_format("container:abc").is_err());
        assert!(validate_network_format("ns:/var/run/netns/x").is_err());
        assert!(validate_network_format("has space").is_err());
    }

    #[test]
    fn test_profile_config_default() {
        let config = ProfileConfig::default();
        assert!(config.description.is_none());
        assert!(config.overrides.is_empty());
    }

    #[test]
    fn test_profile_config_serialization_empty() {
        let config = ProfileConfig::default();
        let serialized = toml::to_string(&config).unwrap();
        // Empty config should serialize to empty (skip_serializing_if + empty map).
        assert!(serialized.trim().is_empty());
    }

    #[test]
    fn test_profile_config_serialization_partial() {
        let config = profile_from(json!({"updates": {"update_check_mode": "off"}}));
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("[updates]"));
        assert!(serialized.contains("update_check_mode = \"off\""));
    }

    #[test]
    fn test_profile_config_deserialization() {
        let toml = r#"
            [updates]
            update_check_mode = "off"
            check_interval_hours = 48

            [sandbox]
            enabled_by_default = true
        "#;

        let config: ProfileConfig = toml::from_str(toml).unwrap();
        let ov = serde_json::to_value(&config).unwrap();
        assert_eq!(ov["updates"]["update_check_mode"], json!("off"));
        assert_eq!(ov["updates"]["check_interval_hours"], json!(48));
        assert_eq!(ov["sandbox"]["enabled_by_default"], json!(true));
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
        let profile = profile_from(json!({
            "updates": {"update_check_mode": "off", "check_interval_hours": 48},
            "worktree": {"enabled": true},
        }));

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

        let profile = profile_from(json!({
            "status_hooks": {"enabled": true, "debounce_ms": 500, "on_waiting": "profile-waiting"}
        }));

        let merged = merge_configs(global, &profile);
        assert!(merged.status_hooks.enabled);
        assert_eq!(
            merged.status_hooks.on_waiting.as_deref(),
            Some("profile-waiting")
        );
        assert_eq!(merged.status_hooks.debounce_ms, 500);
    }

    #[test]
    fn test_merge_configs_with_agent_status_map_overrides() {
        let mut global = Config::default();
        global
            .agents
            .entry("claude".to_string())
            .or_default()
            .status_map
            .insert("Stop".to_string(), crate::agents::HookStatus::Idle);
        global
            .agents
            .entry("claude".to_string())
            .or_default()
            .status_map
            .insert("PreToolUse".to_string(), crate::agents::HookStatus::Running);

        let profile = profile_from(json!({
            "agents": {"claude": {"status_map": {"Stop": "error"}}}
        }));

        let merged = merge_configs(global, &profile);
        let status_map = &merged.agents["claude"].status_map;
        assert_eq!(
            status_map.get("PreToolUse"),
            Some(&crate::agents::HookStatus::Running)
        );
        assert_eq!(
            status_map.get("Stop"),
            Some(&crate::agents::HookStatus::Error)
        );
    }

    #[test]
    fn test_profile_has_overrides() {
        let empty = ProfileConfig::default();
        assert!(!profile_has_overrides(&empty));

        let with_override = profile_from(json!({"theme": {"name": "dark"}}));
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
        assert!(validate_memory_limit("").is_ok()); // empty == no limit
        assert!(validate_memory_limit("512m").is_ok());
        assert!(validate_memory_limit("2g").is_ok());
        assert!(validate_memory_limit("8G").is_ok());
        // A unit suffix is required: a bare number (bytes to Docker) is rejected.
        assert!(validate_memory_limit("1024").is_err());
        assert!(validate_memory_limit("12").is_err());
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
        use crate::session::config::TmuxMouseMode;
        let global = Config::default();
        assert_eq!(global.tmux.mouse, TmuxMouseMode::Auto);

        let profile = profile_from(json!({"tmux": {"mouse": "enabled"}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.mouse, TmuxMouseMode::Enabled);
    }

    #[test]
    fn test_merge_configs_tmux_mouse_inherits_when_not_overridden() {
        use crate::session::config::{TmuxMouseMode, TmuxStatusBarMode};
        let mut global = Config::default();
        global.tmux.mouse = TmuxMouseMode::Enabled;

        let profile = profile_from(json!({"tmux": {"status_bar": "enabled"}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.mouse, TmuxMouseMode::Enabled); // inherits from global
        assert_eq!(merged.tmux.status_bar, TmuxStatusBarMode::Enabled);
    }

    #[test]
    fn test_merge_configs_tmux_mouse_disabled_override() {
        use crate::session::config::TmuxMouseMode;
        let mut global = Config::default();
        global.tmux.mouse = TmuxMouseMode::Enabled;

        let profile = profile_from(json!({"tmux": {"mouse": "disabled"}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.mouse, TmuxMouseMode::Disabled);
    }

    #[test]
    fn test_merge_configs_with_tmux_clipboard_override() {
        use crate::session::config::TmuxClipboardMode;
        let global = Config::default();
        assert_eq!(global.tmux.clipboard, TmuxClipboardMode::Auto);

        let profile = profile_from(json!({"tmux": {"clipboard": "disabled"}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.clipboard, TmuxClipboardMode::Disabled);
    }

    #[test]
    fn test_merge_configs_tmux_clipboard_inherits_when_not_overridden() {
        use crate::session::config::TmuxClipboardMode;
        let mut global = Config::default();
        global.tmux.clipboard = TmuxClipboardMode::Enabled;

        let profile = profile_from(json!({"tmux": {"mouse": "enabled"}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.tmux.clipboard, TmuxClipboardMode::Enabled);
    }

    #[test]
    fn test_merge_configs_with_volume_ignores_override() {
        let global = Config::default();
        assert!(global.sandbox.volume_ignores.is_empty());

        let profile =
            profile_from(json!({"sandbox": {"volume_ignores": ["target", "node_modules"]}}));
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

        let profile = profile_from(json!({"sandbox": {"enabled_by_default": true}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.sandbox.volume_ignores, vec!["target"]);
        assert!(merged.sandbox.enabled_by_default);
    }

    #[test]
    fn test_volume_ignores_override_serialization() {
        let config = profile_from(json!({"sandbox": {"volume_ignores": ["target", ".venv"]}}));
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("volume_ignores"));

        let deserialized: ProfileConfig = toml::from_str(&serialized).unwrap();
        let ov = serde_json::to_value(&deserialized).unwrap();
        assert_eq!(ov["sandbox"]["volume_ignores"], json!(["target", ".venv"]));
    }

    #[test]
    fn test_tmux_config_override_serialization() {
        let config = profile_from(json!({
            "tmux": {"status_bar": "enabled", "mouse": "enabled", "clipboard": "enabled"}
        }));
        let serialized = toml::to_string_pretty(&config).unwrap();
        assert!(serialized.contains("[tmux]"));
        assert!(serialized.contains(r#"mouse = "enabled""#));

        let deserialized: ProfileConfig = toml::from_str(&serialized).unwrap();
        let ov = serde_json::to_value(&deserialized).unwrap();
        assert_eq!(ov["tmux"]["mouse"], json!("enabled"));
    }

    #[test]
    fn test_merge_configs_with_theme_override() {
        let global = Config::default();
        let profile = profile_from(json!({"theme": {"name": "tokyo-night"}}));
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
        // Regression: a single string stands in for a one-element list, coerced
        // by the target `SandboxConfig`'s `string_or_vec` deserializer on merge.
        let toml = r#"
            [sandbox]
            environment = "ANTHROPIC_API_KEY"
            extra_volumes = "/data:/data:ro"
            volume_ignores = "node_modules"
            port_mappings = "3000:3000"
        "#;
        let config: ProfileConfig = toml::from_str(toml).unwrap();
        let merged = merge_configs(Config::default(), &config);
        assert_eq!(merged.sandbox.environment, vec!["ANTHROPIC_API_KEY"]);
        assert_eq!(merged.sandbox.extra_volumes, vec!["/data:/data:ro"]);
        assert_eq!(merged.sandbox.volume_ignores, vec!["node_modules"]);
        assert_eq!(merged.sandbox.port_mappings, vec!["3000:3000"]);
    }

    #[test]
    fn test_hooks_override_string_shorthand() {
        // Regression: HooksConfig accepts a plain string, coerced on merge.
        let toml = r#"
            [hooks]
            on_create = "npm install"
            on_launch = "npm start"
        "#;
        let config: ProfileConfig = toml::from_str(toml).unwrap();
        let merged = merge_configs(Config::default(), &config);
        assert_eq!(merged.hooks.on_create, vec!["npm install"]);
        assert_eq!(merged.hooks.on_launch, vec!["npm start"]);
    }

    #[test]
    fn test_environment_override_round_trips() {
        let toml_in = r#"
            environment = ["CLAUDE_CONFIG_DIR=/home/me/.claude-accounts/work", "GH_TOKEN"]
        "#;
        let config: ProfileConfig = toml::from_str(toml_in).unwrap();
        let merged = merge_configs(Config::default(), &config);
        assert_eq!(
            merged.environment,
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
        // A single string stands in for a one-element list, coerced on merge.
        let toml_in = r#"environment = "FOO=bar""#;
        let config: ProfileConfig = toml::from_str(toml_in).unwrap();
        let merged = merge_configs(Config::default(), &config);
        assert_eq!(merged.environment, vec!["FOO=bar".to_string()]);
    }

    #[test]
    fn test_environment_override_promotes_profile_has_overrides() {
        let profile = ProfileConfig::default();
        assert!(!profile_has_overrides(&profile));
        let profile = profile_from(json!({"environment": ["FOO=bar"]}));
        assert!(profile_has_overrides(&profile));
    }

    #[test]
    fn test_merge_configs_replaces_global_environment() {
        let global = Config {
            environment: vec!["FROM_GLOBAL=1".to_string()],
            ..Default::default()
        };
        let profile = profile_from(json!({"environment": ["FROM_PROFILE=2"]}));
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

    // Replace (not extend) semantics for the Vec sandbox overrides.
    #[test]
    fn test_merge_configs_replaces_extra_volumes() {
        let mut global = Config::default();
        global.sandbox.extra_volumes = vec!["/from-global:/g".to_string()];

        let profile = profile_from(json!({"sandbox": {"extra_volumes": ["/from-profile:/p"]}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.sandbox.extra_volumes, vec!["/from-profile:/p"]);
    }

    #[test]
    fn test_merge_configs_extra_volumes_inherits_when_none() {
        let mut global = Config::default();
        global.sandbox.extra_volumes = vec!["/from-global:/g".to_string()];

        let profile = profile_from(json!({"sandbox": {"enabled_by_default": true}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.sandbox.extra_volumes, vec!["/from-global:/g"]);
    }

    #[test]
    fn test_merge_configs_replaces_port_mappings() {
        let mut global = Config::default();
        global.sandbox.port_mappings = vec!["3000:3000".to_string()];

        let profile =
            profile_from(json!({"sandbox": {"port_mappings": ["8080:8080", "9090:9090"]}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.sandbox.port_mappings, vec!["8080:8080", "9090:9090"]);
    }

    #[test]
    fn test_merge_configs_port_mappings_inherits_when_none() {
        let mut global = Config::default();
        global.sandbox.port_mappings = vec!["3000:3000".to_string()];

        let profile = profile_from(json!({"sandbox": {"cpu_limit": "2"}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.sandbox.port_mappings, vec!["3000:3000"]);
    }

    #[test]
    fn test_merge_configs_with_acp_overrides() {
        let global = Config::default();

        let profile = profile_from(json!({"acp": {
            "default_agent": "claude-code",
            "max_concurrent_workers": 9,
            "replay_bytes": 1024,
            "node_path": "/opt/node",
        }}));

        let merged = merge_configs(global, &profile);
        assert_eq!(merged.acp.default_agent, "claude-code");
        assert_eq!(merged.acp.max_concurrent_workers, 9);
        assert_eq!(merged.acp.replay_bytes, 1024);
        assert_eq!(merged.acp.node_path, "/opt/node");
        // Not overridden: inherits global default.
        assert!(merged.acp.show_tool_durations);
    }

    #[test]
    fn test_merge_configs_acp_inherits_when_none() {
        let mut global = Config::default();
        global.acp.default_agent = "from-global".to_string();
        global.acp.max_concurrent_workers = 7;

        let profile = profile_from(json!({"acp": {"replay_events": 42}}));
        let merged = merge_configs(global, &profile);
        assert_eq!(merged.acp.replay_events, 42);
        assert_eq!(merged.acp.default_agent, "from-global");
        assert_eq!(merged.acp.max_concurrent_workers, 7);
    }

    #[test]
    fn generic_merge_inherits_with_empty_overrides() {
        let mut global = Config::default();
        global.acp.max_concurrent_workers = 7;
        let generic = merge_configs_generic(&global, &json!({}));
        assert_eq!(
            serde_json::to_value(&global).unwrap(),
            serde_json::to_value(&generic).unwrap(),
        );
    }
}
