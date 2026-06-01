//! Integration tests for the config merge pipeline: global + profile overrides with real TOML files.

use agent_of_empires::session::{
    load_profile_config, merge_configs, save_config, save_profile_config, Config, ProfileConfig,
};
use anyhow::Result;
use serde_json::json;
use serial_test::serial;

use crate::common::setup_temp_home;

/// Build a `ProfileConfig` from a sparse override object (the on-disk shape).
fn profile_from(overrides: serde_json::Value) -> ProfileConfig {
    serde_json::from_value(overrides).expect("profile override deserializes")
}

#[test]
#[serial]
fn test_merge_overrides_global() -> Result<()> {
    let _temp = setup_temp_home();

    // Save global config with sandbox.auto_cleanup = true (default)
    let mut global = Config::default();
    global.sandbox.auto_cleanup = true;
    save_config(&global)?;

    // Save profile override with sandbox.auto_cleanup = false
    let profile = profile_from(json!({"sandbox": {"auto_cleanup": false}}));
    save_profile_config("default", &profile)?;

    // Load and merge
    let loaded_global = Config::load()?;
    let loaded_profile = load_profile_config("default")?;
    let merged = merge_configs(loaded_global, &loaded_profile);

    assert!(
        !merged.sandbox.auto_cleanup,
        "Profile override should take precedence"
    );

    Ok(())
}

#[test]
#[serial]
fn test_merge_inherits_unset_fields() -> Result<()> {
    let _temp = setup_temp_home();

    // Save global config with specific values
    let mut global = Config::default();
    global.updates.check_interval_hours = 12;
    global.worktree.enabled = true;
    save_config(&global)?;

    // Profile only overrides theme
    let profile = profile_from(json!({"theme": {"name": "dark"}}));
    save_profile_config("default", &profile)?;

    let loaded_global = Config::load()?;
    let loaded_profile = load_profile_config("default")?;
    let merged = merge_configs(loaded_global, &loaded_profile);

    assert_eq!(merged.theme.name, "dark", "Theme should be overridden");
    assert_eq!(
        merged.updates.check_interval_hours, 12,
        "check_interval_hours should inherit from global"
    );
    assert!(
        merged.worktree.enabled,
        "worktree.enabled should inherit from global"
    );

    Ok(())
}

#[test]
#[serial]
fn test_config_toml_round_trip() -> Result<()> {
    let _temp = setup_temp_home();

    let mut config = Config::default();
    config.theme.name = "monokai".to_string();
    config.updates.update_check_mode = agent_of_empires::session::config::UpdateCheckMode::Off;
    config.updates.check_interval_hours = 72;
    config.worktree.enabled = true;
    config.worktree.auto_cleanup = false;
    config.sandbox.enabled_by_default = true;
    config.sandbox.auto_cleanup = false;

    save_config(&config)?;
    let loaded = Config::load()?;

    assert_eq!(loaded.theme.name, "monokai");
    assert_eq!(
        loaded.updates.update_check_mode,
        agent_of_empires::session::config::UpdateCheckMode::Off
    );
    assert_eq!(loaded.updates.check_interval_hours, 72);
    assert!(loaded.worktree.enabled);
    assert!(!loaded.worktree.auto_cleanup);
    assert!(loaded.sandbox.enabled_by_default);
    assert!(!loaded.sandbox.auto_cleanup);

    Ok(())
}

#[test]
#[serial]
fn test_profile_config_toml_round_trip() -> Result<()> {
    let _temp = setup_temp_home();

    let profile = profile_from(json!({
        "updates": {"update_check_mode": "off", "check_interval_hours": 48},
        "worktree": {"enabled": true, "auto_cleanup": false},
        "sandbox": {"auto_cleanup": false},
    }));

    save_profile_config("default", &profile)?;
    let loaded = load_profile_config("default")?;

    // Overrides survive the TOML round trip as a sparse tree.
    let ov = serde_json::to_value(&loaded)?;
    assert_eq!(ov["updates"]["update_check_mode"], json!("off"));
    assert_eq!(ov["updates"]["check_interval_hours"], json!(48));
    assert_eq!(ov["worktree"]["enabled"], json!(true));
    assert_eq!(ov["worktree"]["auto_cleanup"], json!(false));
    assert_eq!(ov["sandbox"]["auto_cleanup"], json!(false));

    Ok(())
}

#[test]
#[serial]
fn test_empty_profile_config_returns_global() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.updates.check_interval_hours = 99;
    save_config(&global)?;

    // Load profile config for a profile with no override file
    let profile = load_profile_config("default")?;
    let loaded_global = Config::load()?;
    let merged = merge_configs(loaded_global, &profile);

    assert_eq!(
        merged.updates.check_interval_hours, 99,
        "With no profile overrides, merged config should equal global"
    );

    Ok(())
}
