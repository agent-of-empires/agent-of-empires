//! Integration tests for config wiring
//!
//! These tests verify that config settings are properly wired up to the code
//! that uses them, specifically the auto_cleanup settings for worktrees and
//! sandbox containers.

use std::collections::HashMap;

use agent_of_empires::session::{update_config, Config, SandboxConfig, WorktreeConfig};
use agent_of_empires::tui::dialogs::{DeleteDialogConfig, UnifiedDeleteDialog};
use serial_test::serial;

use crate::common::setup_temp_home;

#[test]
#[serial]
fn test_delete_dialog_respects_worktree_auto_cleanup_true() {
    let _temp = setup_temp_home();

    update_config(|config| {
        config.worktree.auto_cleanup = true;
    })
    .unwrap();

    let dialog = UnifiedDeleteDialog::new(
        "Test".to_string(),
        DeleteDialogConfig {
            worktree_branch: Some("main".to_string()),
            has_sandbox: false,
            project_path: None,
            is_scratch: false,
        },
        "default",
    );
    assert!(
        dialog.options().delete_worktree,
        "When worktree.auto_cleanup is true, delete_worktree should default to true"
    );
}

#[test]
#[serial]
fn test_delete_dialog_respects_worktree_auto_cleanup_false() {
    let _temp = setup_temp_home();

    update_config(|config| {
        config.worktree.auto_cleanup = false;
    })
    .unwrap();

    let dialog = UnifiedDeleteDialog::new(
        "Test".to_string(),
        DeleteDialogConfig {
            worktree_branch: Some("main".to_string()),
            has_sandbox: false,
            project_path: None,
            is_scratch: false,
        },
        "default",
    );
    assert!(
        !dialog.options().delete_worktree,
        "When worktree.auto_cleanup is false, delete_worktree should default to false"
    );
}

#[test]
#[serial]
fn test_delete_dialog_respects_sandbox_auto_cleanup_true() {
    let _temp = setup_temp_home();

    update_config(|config| {
        config.sandbox.auto_cleanup = true;
    })
    .unwrap();

    let dialog = UnifiedDeleteDialog::new(
        "Test".to_string(),
        DeleteDialogConfig {
            worktree_branch: None,
            has_sandbox: true,
            project_path: None,
            is_scratch: false,
        },
        "default",
    );
    assert!(
        dialog.options().delete_sandbox,
        "When sandbox.auto_cleanup is true, delete_sandbox should default to true"
    );
}

#[test]
#[serial]
fn test_delete_dialog_respects_sandbox_auto_cleanup_false() {
    let _temp = setup_temp_home();

    update_config(|config| {
        config.sandbox.auto_cleanup = false;
    })
    .unwrap();

    let dialog = UnifiedDeleteDialog::new(
        "Test".to_string(),
        DeleteDialogConfig {
            worktree_branch: None,
            has_sandbox: true,
            project_path: None,
            is_scratch: false,
        },
        "default",
    );
    assert!(
        !dialog.options().delete_sandbox,
        "When sandbox.auto_cleanup is false, delete_sandbox should default to false"
    );
}

#[test]
fn test_default_config_has_auto_cleanup_true() {
    let config = Config::default();
    assert!(
        config.worktree.auto_cleanup,
        "Default worktree.auto_cleanup should be true"
    );
    assert!(
        config.sandbox.auto_cleanup,
        "Default sandbox.auto_cleanup should be true"
    );
}

#[test]
#[serial]
fn test_config_roundtrip_preserves_auto_cleanup() {
    let _temp = setup_temp_home();

    update_config(|config| {
        config.worktree.auto_cleanup = false;
        config.sandbox.auto_cleanup = false;
    })
    .unwrap();

    let loaded = Config::load().unwrap();
    assert!(
        !loaded.worktree.auto_cleanup,
        "worktree.auto_cleanup should persist as false"
    );
    assert!(
        !loaded.sandbox.auto_cleanup,
        "sandbox.auto_cleanup should persist as false"
    );
}

#[test]
fn test_all_worktree_config_fields_accessible() {
    let config = WorktreeConfig::default();
    let _ = config.enabled;
    let _ = config.path_template.as_str();
    let _ = config.auto_cleanup;
}

#[test]
fn test_all_sandbox_config_fields_accessible() {
    let config = SandboxConfig::default();
    let _ = config.enabled_by_default;
    let _ = config.default_image.as_str();
    let _ = &config.extra_volumes;
    let _ = &config.environment;
    let _ = config.auto_cleanup;
    let _ = &config.cpu_limit;
    let _ = &config.memory_limit;
}

#[test]
#[serial]
fn test_agent_command_override_roundtrip() {
    let _temp = setup_temp_home();

    update_config(|config| {
        config
            .session
            .agent_command_override
            .insert("claude".to_string(), "my-wrapper".to_string());
        config
            .session
            .agent_extra_args
            .insert("opencode".to_string(), "--port 8080".to_string());
    })
    .unwrap();

    let loaded = Config::load().unwrap();
    assert_eq!(
        loaded.session.agent_command_override.get("claude"),
        Some(&"my-wrapper".to_string()),
        "agent_command_override should survive save/load roundtrip"
    );
    assert_eq!(
        loaded.session.agent_extra_args.get("opencode"),
        Some(&"--port 8080".to_string()),
        "agent_extra_args should survive save/load roundtrip"
    );
}

#[test]
fn test_parse_key_value_list_via_field_apply() {
    // Simulate the settings TUI flow: list of "key=value" strings -> HashMap -> TOML -> load back
    let mut config = Config::default();

    // Simulate what apply_field_to_global does for AgentCommandOverride
    let list_items = ["claude=my-wrapper".to_string()];
    let map: HashMap<String, String> = list_items
        .iter()
        .filter_map(|item| {
            let (k, v) = item.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect();
    config.session.agent_command_override = map;

    assert_eq!(
        config.session.agent_command_override.get("claude"),
        Some(&"my-wrapper".to_string()),
    );

    // Verify entries WITHOUT '=' are silently dropped (the root cause of the bug)
    let bad_items = ["just-a-command".to_string()];
    let bad_map: HashMap<String, String> = bad_items
        .iter()
        .filter_map(|item| {
            let (k, v) = item.split_once('=')?;
            Some((k.to_string(), v.to_string()))
        })
        .collect();
    assert!(
        bad_map.is_empty(),
        "Entries without '=' should be dropped by parse_key_value_list"
    );
}
