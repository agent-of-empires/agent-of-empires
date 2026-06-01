//! Integration tests for hooks config resolution across global, profile, and repo levels.

use agent_of_empires::session::{
    merge_configs, merge_repo_config, resolve_config, save_config, save_profile_config, Config,
    ProfileConfig, RepoConfig,
};
use anyhow::Result;
use serde_json::json;
use serial_test::serial;

use crate::common::setup_temp_home;

/// Build a `ProfileConfig` from a sparse override object.
fn profile_from(overrides: serde_json::Value) -> ProfileConfig {
    serde_json::from_value(overrides).expect("profile override deserializes")
}

/// Build a `RepoConfig` from a sparse override object.
fn repo_from(overrides: serde_json::Value) -> RepoConfig {
    serde_json::from_value(overrides).expect("repo override deserializes")
}

// T014: Global hooks resolve when no repo config exists
#[test]
#[serial]
fn test_global_hooks_resolve_without_repo() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_create = vec!["npm install".to_string()];
    global.hooks.on_launch = vec!["echo hello".to_string()];
    save_config(&global)?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_create, vec!["npm install"]);
    assert_eq!(resolved.hooks.on_launch, vec!["echo hello"]);

    Ok(())
}

// T015: Repo hooks override global hooks per-field
#[test]
#[serial]
fn test_repo_hooks_override_global_per_field() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_create = vec!["global_create".to_string()];
    global.hooks.on_launch = vec!["global_launch".to_string()];
    save_config(&global)?;

    let resolved = resolve_config("default")?;

    // Repo only defines on_create
    let repo = repo_from(json!({"hooks": {"on_create": ["repo_create"]}}));

    let merged = merge_repo_config(resolved, &repo);

    // Repo on_create should override global
    assert_eq!(merged.hooks.on_create, vec!["repo_create"]);
    // Global on_launch should be preserved (repo on_launch is empty)
    assert_eq!(merged.hooks.on_launch, vec!["global_launch"]);

    Ok(())
}

// T015 additional: repo hooks override both fields
#[test]
#[serial]
fn test_repo_hooks_override_both_fields() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_create = vec!["global_create".to_string()];
    global.hooks.on_launch = vec!["global_launch".to_string()];
    save_config(&global)?;

    let resolved = resolve_config("default")?;

    let repo = repo_from(json!({
        "hooks": {"on_create": ["repo_create"], "on_launch": ["repo_launch"]}
    }));

    let merged = merge_repo_config(resolved, &repo);
    assert_eq!(merged.hooks.on_create, vec!["repo_create"]);
    assert_eq!(merged.hooks.on_launch, vec!["repo_launch"]);

    Ok(())
}

// T016: Global/profile hooks are NOT subject to trust checking.
// This is a design invariant: check_hook_trust() only reads .agent-of-empires/config.toml,
// so global/profile hooks never enter the trust pipeline. We verify that
// resolve_config returns hooks without any trust gate.
#[test]
#[serial]
fn test_global_profile_hooks_bypass_trust() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_create = vec!["global_cmd".to_string()];
    save_config(&global)?;

    // resolve_config returns hooks directly - no trust check involved
    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_create, vec!["global_cmd"]);

    // With profile override - also no trust check
    let profile = profile_from(json!({"hooks": {"on_launch": ["profile_launch"]}}));
    save_profile_config("default", &profile)?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_create, vec!["global_cmd"]);
    assert_eq!(resolved.hooks.on_launch, vec!["profile_launch"]);

    Ok(())
}

// T018: Profile on_create override replaces global on_create, on_launch falls back
#[test]
#[serial]
fn test_profile_overrides_on_create_only() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_create = vec!["global_create".to_string()];
    global.hooks.on_launch = vec!["global_launch".to_string()];
    save_config(&global)?;

    let profile = profile_from(json!({"hooks": {"on_create": ["profile_create"]}}));
    save_profile_config("default", &profile)?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_create, vec!["profile_create"]);
    assert_eq!(resolved.hooks.on_launch, vec!["global_launch"]);

    Ok(())
}

// T019: Clearing profile hooks override restores global hooks
#[test]
#[serial]
fn test_clearing_profile_hooks_restores_global() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_create = vec!["global_create".to_string()];
    global.hooks.on_launch = vec!["global_launch".to_string()];
    save_config(&global)?;

    // First set profile override
    let profile = profile_from(json!({
        "hooks": {"on_create": ["profile_create"], "on_launch": ["profile_launch"]}
    }));
    save_profile_config("default", &profile)?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_create, vec!["profile_create"]);
    assert_eq!(resolved.hooks.on_launch, vec!["profile_launch"]);

    // Clear profile override (empty profile)
    save_profile_config("default", &ProfileConfig::default())?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_create, vec!["global_create"]);
    assert_eq!(resolved.hooks.on_launch, vec!["global_launch"]);

    Ok(())
}

// T020: Full three-level resolution (global + profile + repo) with per-field semantics
#[test]
#[serial]
fn test_three_level_resolution() -> Result<()> {
    let _temp = setup_temp_home();

    // Global: both hooks
    let mut global = Config::default();
    global.hooks.on_create = vec!["global_create".to_string()];
    global.hooks.on_launch = vec!["global_launch".to_string()];
    save_config(&global)?;

    // Profile: only overrides on_create
    let profile = profile_from(json!({"hooks": {"on_create": ["profile_create"]}}));
    save_profile_config("default", &profile)?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_create, vec!["profile_create"]);
    assert_eq!(resolved.hooks.on_launch, vec!["global_launch"]);

    // Repo: only overrides on_launch
    let repo = repo_from(json!({"hooks": {"on_launch": ["repo_launch"]}}));

    let final_config = merge_repo_config(resolved, &repo);
    // on_create: profile > global (repo is empty, so profile value stays)
    assert_eq!(final_config.hooks.on_create, vec!["profile_create"]);
    // on_launch: repo > profile > global
    assert_eq!(final_config.hooks.on_launch, vec!["repo_launch"]);

    Ok(())
}

// T020 additional: Verify merge_configs directly
#[test]
#[serial]
fn test_merge_configs_hooks_override() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_create = vec!["g1".to_string(), "g2".to_string()];
    global.hooks.on_launch = vec!["gl".to_string()];

    let profile = profile_from(json!({"hooks": {"on_create": ["p1"]}}));

    let merged = merge_configs(global, &profile);
    assert_eq!(merged.hooks.on_create, vec!["p1"]);
    assert_eq!(merged.hooks.on_launch, vec!["gl"]);

    Ok(())
}

// on_destroy: global hooks resolve
#[test]
#[serial]
fn test_global_on_destroy_hooks_resolve() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_destroy = vec!["docker-compose down".to_string()];
    save_config(&global)?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_destroy, vec!["docker-compose down"]);

    Ok(())
}

// on_destroy: profile override replaces global
#[test]
#[serial]
fn test_profile_on_destroy_override() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_destroy = vec!["global_cleanup".to_string()];
    save_config(&global)?;

    let profile = profile_from(json!({"hooks": {"on_destroy": ["profile_cleanup"]}}));
    save_profile_config("default", &profile)?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_destroy, vec!["profile_cleanup"]);

    Ok(())
}

// on_destroy: repo override replaces global/profile
#[test]
#[serial]
fn test_repo_on_destroy_override() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_destroy = vec!["global_cleanup".to_string()];
    save_config(&global)?;

    let resolved = resolve_config("default")?;

    let repo = repo_from(json!({"hooks": {"on_destroy": ["repo_cleanup"]}}));

    let merged = merge_repo_config(resolved, &repo);
    assert_eq!(merged.hooks.on_destroy, vec!["repo_cleanup"]);
    // Global on_destroy should be overridden by repo
    Ok(())
}

// on_destroy: clearing profile override restores global
#[test]
#[serial]
fn test_clearing_profile_on_destroy_restores_global() -> Result<()> {
    let _temp = setup_temp_home();

    let mut global = Config::default();
    global.hooks.on_destroy = vec!["global_cleanup".to_string()];
    save_config(&global)?;

    let profile = profile_from(json!({"hooks": {"on_destroy": ["profile_cleanup"]}}));
    save_profile_config("default", &profile)?;

    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_destroy, vec!["profile_cleanup"]);

    // Clear override
    save_profile_config("default", &ProfileConfig::default())?;
    let resolved = resolve_config("default")?;
    assert_eq!(resolved.hooks.on_destroy, vec!["global_cleanup"]);

    Ok(())
}
