//! Profile-specific configuration with override support
//!
//! Profile configs allow per-profile overrides of global settings.
//! Fields set to None inherit from the global config.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;

use super::Config;
use crate::session::get_profile_dir;

/// Profile-specific settings, stored as a sparse override tree (#1692).
///
/// Every override is a section table keyed by config-section name (e.g.
/// `sandbox`, `acp`) mirroring the `Config` JSON shape; an absent key
/// inherits the global value. There are no typed per-section structs: a field
/// is overridable according to its `Config` schema descriptor, so adding one
/// never touches this file. Merging is the generic recursive
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
    let path = crate::session::get_profile_dir_path(profile)?.join("config.toml");
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

/// Merge a sparse override object onto a full serialized `Config::default()`,
/// yielding the resolved JSON that would land in memory if this profile were
/// applied. Shared prelude for [`validate_overrides_typecheck`] (which
/// re-deserializes it into `Config` to type-check) and
/// [`profile_config_ignored_keys`] (which runs `serde_ignored` to enumerate
/// unknown keys).
fn merged_onto_default(overrides: &serde_json::Value) -> Result<serde_json::Value> {
    let mut base = serde_json::to_value(Config::default())?;
    crate::session::config::settings_schema::merge_json(&mut base, overrides);
    Ok(base)
}

/// Confirm a sparse override object deserializes back into a [`Config`] when
/// merged onto the defaults. Used at load time so a malformed override file is
/// a graceful error rather than a merge-time panic.
pub(super) fn validate_overrides_typecheck(overrides: &serde_json::Value) -> Result<()> {
    let base = merged_onto_default(overrides)?;
    serde_json::from_value::<Config>(base)
        .map_err(|e| anyhow::anyhow!("invalid override value: {e}"))?;
    Ok(())
}

/// Dotted paths of keys in this profile that `Config` does not recognize
/// (unknown struct fields at any depth), so a typo like `[sandbox] privildged
/// = true` surfaces instead of being silently dropped. Runs `serde_ignored`
/// against the profile's overrides merged onto a default `Config`; a raw
/// `serde_ignored::deserialize::<ProfileConfig>` would report nothing, since
/// `#[serde(flatten)] overrides` absorbs every unknown key as valid JSON.
/// Map-keyed sections (`agents`, `tools`, `plugins`, `session.custom_agents`,
/// `acp.acp_defaults`, ...) never flag because their keys are entries, not
/// struct fields; nested struct-field typos inside them still do. Takes the
/// already-loaded `ProfileConfig` so the caller does not read+parse the
/// profile file a second time.
pub(crate) fn profile_config_ignored_keys(cfg: &ProfileConfig) -> Vec<String> {
    overrides_ignored_keys(&cfg.overrides_value())
}

/// Dotted paths of keys in a sparse override object that `Config` does not
/// recognize. Shared by the profile probe above and the repo-config loader,
/// whose `#[serde(flatten)]` maps absorb unknown keys the same way.
pub(crate) fn overrides_ignored_keys(overrides: &serde_json::Value) -> Vec<String> {
    let Ok(base) = merged_onto_default(overrides) else {
        return Vec::new();
    };
    let mut ignored = Vec::new();
    let _ = serde_ignored::deserialize::<_, _, Config>(base, |path| {
        ignored.push(path.to_string());
    });
    ignored
}

/// Save profile-specific config
pub fn save_profile_config(profile: &str, config: &ProfileConfig) -> Result<()> {
    let path = get_profile_config_path(profile)?;
    let content = toml::to_string_pretty(config)?;
    crate::session::atomic_write(&path, content.as_bytes())?;
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

/// Force the config values CityHall client mode depends on when
/// `AOE_CITYHALL_MODE` is set. These are hidden from the CityHall settings UI,
/// so pinning them at config-resolution time is the single place they are set:
/// a high worker ceiling and worktree sessions enabled by default. Idempotent;
/// a no-op when the flag is unset. See #7. (Worktree path templates are left at
/// their defaults; sensible CityHall paths are TBD with the container work.)
pub fn apply_cityhall_overrides(config: &mut Config) {
    if std::env::var_os("AOE_CITYHALL_MODE").is_none() {
        return;
    }
    config.acp.max_concurrent_workers = 50;
    config.worktree.enabled = true;
}

/// Load effective config for a profile (global + profile overrides merged)
pub fn resolve_config(profile: &str) -> Result<Config> {
    let global = Config::load()?;
    let profile_config = load_profile_config(profile)?;
    let mut config = merge_configs(global, &profile_config);
    apply_cityhall_overrides(&mut config);
    // (Re)install the declarative status-rule registry from the effective
    // config. The status poll hot path never loads config, so this resolve,
    // which every polling surface passes through at startup, is where
    // `[[agents.<name>.status_rules]]` edits become visible.
    crate::tmux::status_rules::install_from_config(profile, &config);
    Ok(config)
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
            let mut fallback = Config::default();
            apply_cityhall_overrides(&mut fallback);
            // Keep the status-rule registry consistent with the config the
            // caller proceeds with: a failed resolve must not leave this
            // profile's rules from an earlier successful resolve installed.
            // Only this profile's entries are cleared; other profiles' rules
            // are untouched.
            crate::tmux::status_rules::install_from_config(profile, &fallback);
            fallback
        }
    }
}

/// Merge profile overrides, keeping schema-declared global-only fields global.
pub fn merge_configs(global: Config, profile: &ProfileConfig) -> Config {
    let mut overrides = profile.overrides_value();
    for field in super::settings_schema::schema_ref() {
        if !field.profile_overridable {
            super::settings_schema::clear_path(&mut overrides, &field.section, &field.field);
        }
    }
    merge_configs_generic(&global, &overrides)
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
    crate::session::config::settings_schema::merge_json(&mut base, overrides);
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

/// Validate a Linux capability name (`sandbox.cap_add` / `sandbox.cap_drop`).
pub fn validate_capability_format(cap: &str) -> Result<(), String> {
    let re = regex::Regex::new(r"^[A-Z][A-Z_]+$").unwrap();
    if re.is_match(cap) {
        Ok(())
    } else {
        Err("Must be a capability name, e.g. ALL, SYS_ADMIN, CAP_NET_RAW".to_string())
    }
}

/// Validate a `--security-opt` entry (`sandbox.security_opt`).
pub fn validate_security_opt_format(opt: &str) -> Result<(), String> {
    if opt.is_empty() || opt.chars().any(char::is_whitespace) || opt.starts_with('-') {
        return Err(
            "Must be a security option, e.g. seccomp=unconfined or no-new-privileges:true"
                .to_string(),
        );
    }
    Ok(())
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

    /// The value shapes each `validate` helper accepts. `host` and the
    /// namespace-sharing network forms share another stack's network, and a
    /// memory limit without a unit suffix would be read as bytes by Docker.
    #[test]
    fn profile_field_validators_accept_and_reject() {
        for value in ["", "none", "bridge", "egress-proxy", "my_net.1"] {
            assert!(validate_network_format(value).is_ok(), "{value}");
        }
        for value in [
            "host",
            "HOST",
            "container:abc",
            "ns:/var/run/netns/x",
            "has space",
        ] {
            assert!(validate_network_format(value).is_err(), "{value}");
        }

        for value in ["/host:/container", "/host:/container:ro"] {
            assert!(validate_volume_format(value).is_ok(), "{value}");
        }
        for value in ["", "/only-one", ":/container", "/host:"] {
            assert!(validate_volume_format(value).is_err(), "{value}");
        }

        for value in ["", "512m", "2g", "8G"] {
            assert!(validate_memory_limit(value).is_ok(), "{value}");
        }
        for value in ["1024", "12", "invalid", "512mb"] {
            assert!(validate_memory_limit(value).is_err(), "{value}");
        }

        assert!(validate_check_interval(1).is_ok());
        assert!(validate_check_interval(24).is_ok());
        assert!(validate_check_interval(0).is_err());
    }

    /// A plain string stands in for a one-element list wherever the target type
    /// has a `string_or_vec` deserializer; the coercion happens on merge.
    #[test]
    fn profile_string_shorthand_coerces_to_a_one_element_list() {
        let config: ProfileConfig = toml::from_str(
            r#"
            environment = "FOO=bar"

            [sandbox]
            environment = "ANTHROPIC_API_KEY"
            extra_volumes = "/data:/data:ro"
            volume_ignores = "node_modules"
            port_mappings = "3000:3000"

            [hooks]
            on_create = "npm install"
            on_launch = "npm start"
            "#,
        )
        .unwrap();

        let merged = merge_configs(Config::default(), &config);
        assert_eq!(merged.environment, vec!["FOO=bar"]);
        assert_eq!(merged.sandbox.environment, vec!["ANTHROPIC_API_KEY"]);
        assert_eq!(merged.sandbox.extra_volumes, vec!["/data:/data:ro"]);
        assert_eq!(merged.sandbox.volume_ignores, vec!["node_modules"]);
        assert_eq!(merged.sandbox.port_mappings, vec!["3000:3000"]);
        assert_eq!(merged.hooks.on_create, vec!["npm install"]);
        assert_eq!(merged.hooks.on_launch, vec!["npm start"]);
    }

    /// The sparse overlay replaces the keys a profile names and leaves every
    /// other key at its global value. `Vec` fields replace rather than extend,
    /// matching `sandbox.environment`; a per-agent `status_map` merges by key.
    #[test]
    fn merge_configs_overrides_named_keys_and_inherits_the_rest() {
        use crate::session::config::{SidebarPosition, UpdateCheckMode};
        type Case = (fn(&mut Config), serde_json::Value, fn(&Config));
        let cases: Vec<Case> = vec![
            // Nothing overridden.
            (
                |_| {},
                json!({}),
                |merged| {
                    assert_eq!(merged.updates.update_check_mode, UpdateCheckMode::Notify);
                    assert!(!merged.worktree.enabled);
                },
            ),
            // Scalars across sections; a sibling key keeps its global value.
            (
                |_| {},
                json!({"updates": {"update_check_mode": "off"}, "worktree": {"enabled": true}}),
                |merged| {
                    assert_eq!(merged.updates.update_check_mode, UpdateCheckMode::Off);
                    assert!(merged.worktree.enabled);
                    assert!(!merged.updates.auto_update_plugins);
                },
            ),
            (
                |global| {
                    global.status_hooks.enabled = false;
                    global.status_hooks.on_waiting = Some("global-waiting".to_string());
                    global.status_hooks.on_idle = Some("global-idle".to_string());
                },
                json!({"status_hooks": {"enabled": true, "on_waiting": "profile-waiting"}}),
                |merged| {
                    assert!(merged.status_hooks.enabled);
                    assert_eq!(
                        merged.status_hooks.on_waiting.as_deref(),
                        Some("profile-waiting")
                    );
                    assert_eq!(merged.status_hooks.on_idle.as_deref(), Some("global-idle"));
                },
            ),
            (
                |global| {
                    let map = &mut global
                        .agents
                        .entry("claude".to_string())
                        .or_default()
                        .status_map;
                    map.insert("Stop".to_string(), crate::agents::HookStatus::Idle);
                    map.insert("PreToolUse".to_string(), crate::agents::HookStatus::Running);
                },
                json!({"agents": {"claude": {"status_map": {"Stop": "error"}}}}),
                |merged| {
                    let map = &merged.agents["claude"].status_map;
                    assert_eq!(map.get("Stop"), Some(&crate::agents::HookStatus::Error));
                    assert_eq!(
                        map.get("PreToolUse"),
                        Some(&crate::agents::HookStatus::Running)
                    );
                },
            ),
            (
                |global| {
                    global.theme.name = "catppuccin-latte".to_string();
                    global.session.sidebar_position = SidebarPosition::Right;
                },
                json!({
                    "session": {"sidebar_position": "left", "snooze_duration_minutes": 12},
                    "theme": {"name": "tokyo-night", "idle_decay_minutes": 20}
                }),
                |merged| {
                    assert_eq!(merged.theme.name, "catppuccin-latte");
                    assert_eq!(merged.session.sidebar_position, SidebarPosition::Right);
                    assert_eq!(merged.session.snooze_duration_minutes, 12);
                    assert_eq!(merged.theme.idle_decay_minutes, 20);
                },
            ),
            (
                |global| global.theme.name = "catppuccin-latte".to_string(),
                json!({}),
                |merged| assert_eq!(merged.theme.name, "catppuccin-latte"),
            ),
            // The sandbox lists replace wholesale.
            (
                |global| {
                    global.sandbox.volume_ignores = vec!["stale".to_string()];
                    global.sandbox.extra_volumes = vec!["/from-global:/g".to_string()];
                    global.sandbox.port_mappings = vec!["3000:3000".to_string()];
                },
                json!({"sandbox": {
                    "volume_ignores": ["target", "node_modules"],
                    "extra_volumes": ["/from-profile:/p"],
                    "port_mappings": ["8080:8080", "9090:9090"],
                }}),
                |merged| {
                    assert_eq!(
                        merged.sandbox.volume_ignores,
                        vec!["target", "node_modules"]
                    );
                    assert_eq!(merged.sandbox.extra_volumes, vec!["/from-profile:/p"]);
                    assert_eq!(merged.sandbox.port_mappings, vec!["8080:8080", "9090:9090"]);
                },
            ),
            // Naming one sandbox key leaves the others at their global value.
            (
                |global| {
                    global.sandbox.volume_ignores = vec!["target".to_string()];
                    global.sandbox.extra_volumes = vec!["/from-global:/g".to_string()];
                    global.sandbox.port_mappings = vec!["3000:3000".to_string()];
                },
                json!({"sandbox": {"enabled_by_default": true, "cpu_limit": "2"}}),
                |merged| {
                    assert!(merged.sandbox.enabled_by_default);
                    assert_eq!(merged.sandbox.volume_ignores, vec!["target"]);
                    assert_eq!(merged.sandbox.extra_volumes, vec!["/from-global:/g"]);
                    assert_eq!(merged.sandbox.port_mappings, vec!["3000:3000"]);
                },
            ),
            (
                |global| global.environment = vec!["FROM_GLOBAL=1".to_string()],
                json!({"environment": ["FROM_PROFILE=2"]}),
                |merged| assert_eq!(merged.environment, vec!["FROM_PROFILE=2".to_string()]),
            ),
            (
                |global| global.environment = vec!["FROM_GLOBAL=1".to_string()],
                json!({}),
                |merged| assert_eq!(merged.environment, vec!["FROM_GLOBAL=1".to_string()]),
            ),
            (
                |global| {
                    global.acp.default_agent = "from-global".to_string();
                    global.acp.max_concurrent_workers = 7;
                },
                json!({"acp": {"replay_events": 42, "node_path": "/opt/node"}}),
                |merged| {
                    assert_eq!(merged.acp.replay_events, 42);
                    assert_eq!(merged.acp.node_path, "/opt/node");
                    assert_eq!(merged.acp.default_agent, "from-global");
                    assert_eq!(merged.acp.max_concurrent_workers, 7);
                    assert!(merged.acp.show_tool_durations);
                },
            ),
        ];

        for (setup, block, check) in cases {
            let mut global = Config::default();
            setup(&mut global);
            check(&merge_configs(global, &profile_from(block)));
        }
    }

    /// A profile's `[tmux]` block overrides per field and inherits the rest.
    /// One table rather than a test per field: the merge is generic over the
    /// sparse JSON, so the interesting axis is which keys the profile carries.
    #[test]
    fn test_merge_configs_tmux_setting_overrides() {
        use crate::session::config::TmuxSettingMode::{Auto, Disabled, Enabled};
        let default = Config::default();
        assert_eq!(
            (
                default.tmux.status_bar,
                default.tmux.mouse,
                default.tmux.clipboard
            ),
            (Auto, Auto, Auto)
        );

        // (global status_bar/mouse/clipboard, profile block, expected merged)
        let cases = [
            (
                (Auto, Auto, Auto),
                json!({"tmux": {"mouse": "enabled"}}),
                (Auto, Enabled, Auto),
            ),
            (
                (Auto, Enabled, Auto),
                json!({"tmux": {"mouse": "disabled"}}),
                (Auto, Disabled, Auto),
            ),
            // Keys the profile omits inherit the global value rather than
            // reverting to the field default.
            (
                (Auto, Enabled, Enabled),
                json!({"tmux": {"status_bar": "enabled"}}),
                (Enabled, Enabled, Enabled),
            ),
            (
                (Auto, Auto, Enabled),
                json!({"tmux": {"clipboard": "disabled"}}),
                (Auto, Auto, Disabled),
            ),
            // An unrelated section in the profile leaves `[tmux]` alone.
            (
                (Enabled, Enabled, Enabled),
                json!({"session": {"auto_yes": true}}),
                (Enabled, Enabled, Enabled),
            ),
        ];
        for ((status_bar, mouse, clipboard), block, expected) in cases {
            let mut global = Config::default();
            global.tmux.status_bar = status_bar;
            global.tmux.mouse = mouse;
            global.tmux.clipboard = clipboard;
            let merged = merge_configs(global, &profile_from(block.clone()));
            assert_eq!(
                (
                    merged.tmux.status_bar,
                    merged.tmux.mouse,
                    merged.tmux.clipboard
                ),
                expected,
                "{block}"
            );
        }
    }

    /// #3207: [`resolve_tmux_setting`] must read the config it is handed, so a
    /// call site passing the profile-merged config gets the profile's answer.
    /// The resolvers used to call `Config::load_or_warn()` internally, which
    /// made a profile `[tmux]` block inert no matter what a caller resolved.
    /// `Enabled` / `Disabled` are used rather than `Auto` so the assertions do
    /// not depend on whether the host has a `~/.tmux.conf`.
    ///
    /// Iterates [`TmuxSetting::ALL`], so a new managed option is covered by
    /// this invariant without editing the loop. The setup above stays manual
    /// per field: it has to name the `[tmux]` keys it flips. Without it a new
    /// row would resolve as `Auto`, the profile assertion (== `ForceOff`)
    /// would fail deterministically (Auto is never ForceOff), and the global
    /// one (== `Apply`) would additionally depend on the host's tmux config.
    #[test]
    #[serial_test::serial]
    fn test_tmux_decisions_follow_the_config_they_are_given() {
        use crate::session::config::{
            resolve_tmux_setting, TmuxSetting, TmuxSettingAction, TmuxSettingMode,
        };
        // The resolver probes the user's tmux config on every call, even for
        // explicit modes; isolate HOME so the probe stays off the real files.
        let tmp = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(tmp.path());
        let mut global = Config::default();
        global.tmux.mouse = TmuxSettingMode::Enabled;
        global.tmux.status_bar = TmuxSettingMode::Enabled;
        global.tmux.clipboard = TmuxSettingMode::Enabled;

        let profile = profile_from(json!({
            "tmux": {"mouse": "disabled", "status_bar": "disabled", "clipboard": "disabled"}
        }));
        let merged = merge_configs(global.clone(), &profile);

        for setting in TmuxSetting::ALL {
            assert_eq!(
                resolve_tmux_setting(setting, &global),
                TmuxSettingAction::Apply,
                "{setting:?} global"
            );
            assert_eq!(
                resolve_tmux_setting(setting, &merged),
                TmuxSettingAction::ForceOff,
                "{setting:?} profile"
            );
        }
    }

    // #7: CityHall overrides pin the worker ceiling and worktree default, but
    // only when AOE_CITYHALL_MODE is set. Serial because it toggles a process
    // env var.
    #[test]
    #[serial_test::serial]
    fn cityhall_overrides_gate_on_the_env_flag() {
        let guard = crate::session::test_support::EnvGuard::unset(&["AOE_CITYHALL_MODE"]);
        let mut off = Config::default();
        off.worktree.enabled = false;
        apply_cityhall_overrides(&mut off);
        assert!(!off.worktree.enabled, "no override without the flag");

        let _guard = guard.and_set("AOE_CITYHALL_MODE", "1");
        let mut on = Config::default();
        apply_cityhall_overrides(&mut on);
        assert_eq!(on.acp.max_concurrent_workers, 50);
        assert!(on.worktree.enabled);
    }
}
