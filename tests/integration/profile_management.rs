//! Integration tests for profile management: create, delete, list, default, rename, and isolation.

use agent_of_empires::session::{
    create_profile, delete_profile, list_profiles, rename_profile, set_default_profile, Config,
    GroupTree, Instance, Storage,
};
use anyhow::Result;
use serial_test::serial;

use crate::common::setup_temp_home;

#[test]
#[serial]
fn test_create_profile() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("work")?;

    let profiles = list_profiles()?;
    assert!(profiles.contains(&"work".to_string()));

    Ok(())
}

#[test]
#[serial]
fn test_list_profiles_includes_default() -> Result<()> {
    let _temp = setup_temp_home();

    // Trigger creation of default profile dir by accessing it
    let _ = Storage::new_unwatched("default")?;

    let profiles = list_profiles()?;
    assert!(profiles.contains(&"default".to_string()));

    Ok(())
}

#[test]
#[serial]
fn test_delete_profile() -> Result<()> {
    let _temp = setup_temp_home();

    // Deletion is now count-gated (at least one profile must remain), so we
    // need a sibling profile around for the deletion under test to succeed.
    create_profile("keeper")?;
    create_profile("temporary")?;
    assert!(list_profiles()?.contains(&"temporary".to_string()));

    delete_profile("temporary")?;
    assert!(!list_profiles()?.contains(&"temporary".to_string()));
    assert!(list_profiles()?.contains(&"keeper".to_string()));

    Ok(())
}

#[test]
#[serial]
fn test_cannot_delete_last_remaining_profile() -> Result<()> {
    let _temp = setup_temp_home();

    // The protection used to be "cannot delete the profile literally named
    // 'default'". It is now "cannot delete the only profile that exists",
    // applied uniformly regardless of name. Create exactly one profile and
    // verify it cannot be removed.
    create_profile("solo")?;

    let result = delete_profile("solo");
    assert!(result.is_err());
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("at least one profile must exist"));
    assert!(list_profiles()?.contains(&"solo".to_string()));

    Ok(())
}

#[test]
#[serial]
fn test_delete_profile_named_default_allowed_when_others_exist() -> Result<()> {
    // A profile literally named "default" is now an ordinary profile and is
    // deletable as long as deleting it would not leave zero. Regression
    // guard against the old name-based protection sneaking back in.
    let _temp = setup_temp_home();

    create_profile("default")?;
    create_profile("work")?;

    delete_profile("default")?;
    let remaining = list_profiles()?;
    assert!(!remaining.contains(&"default".to_string()));
    assert!(remaining.contains(&"work".to_string()));

    Ok(())
}

#[test]
#[serial]
fn test_set_default_profile() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("work")?;
    set_default_profile("work")?;

    let config = Config::load()?;
    assert_eq!(config.default_profile, "work");

    Ok(())
}

#[test]
#[serial]
fn test_profile_session_isolation() -> Result<()> {
    let _temp = setup_temp_home();

    // Create two profiles
    create_profile("alpha")?;
    create_profile("beta")?;

    // Save a session in profile alpha
    let storage_a = Storage::new_unwatched("alpha")?;
    let instance = Instance::new("Alpha Session", "/path/alpha");
    let seeded = vec![instance];
    storage_a.update(|i, g| {
        *i = seeded.to_vec();
        *g = GroupTree::new_with_groups(&seeded, &[]).get_all_groups();
        Ok(())
    })?;

    // Load from profile beta - should be empty
    let storage_b = Storage::new_unwatched("beta")?;
    let loaded = storage_b.load()?;
    assert!(loaded.is_empty(), "Profile beta should have no sessions");

    // Verify alpha still has its session
    let loaded_a = storage_a.load()?;
    assert_eq!(loaded_a.len(), 1);
    assert_eq!(loaded_a[0].title, "Alpha Session");

    Ok(())
}

#[test]
#[serial]
fn test_rename_profile() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("old_name")?;
    // Add a session so we can verify data moves with the rename
    let storage = Storage::new_unwatched("old_name")?;
    let instance = Instance::new("Test Session", "/path/test");
    let seeded = vec![instance];
    storage.update(|i, g| {
        *i = seeded.to_vec();
        *g = GroupTree::new_with_groups(&seeded, &[]).get_all_groups();
        Ok(())
    })?;

    rename_profile("old_name", "new_name")?;

    let profiles = list_profiles()?;
    assert!(!profiles.contains(&"old_name".to_string()));
    assert!(profiles.contains(&"new_name".to_string()));

    // Verify sessions moved with the profile
    let new_storage = Storage::new_unwatched("new_name")?;
    let sessions = new_storage.load()?;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].title, "Test Session");

    Ok(())
}

#[test]
#[serial]
fn test_rename_profile_updates_default() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("primary")?;
    set_default_profile("primary")?;

    rename_profile("primary", "renamed")?;

    let config = Config::load()?;
    assert_eq!(config.default_profile, "renamed");

    Ok(())
}

#[test]
#[serial]
fn test_rename_profile_nondefault_keeps_default() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("main_profile")?;
    create_profile("other")?;
    set_default_profile("main_profile")?;

    rename_profile("other", "renamed_other")?;

    let config = Config::load()?;
    assert_eq!(config.default_profile, "main_profile");

    Ok(())
}

#[test]
#[serial]
fn test_rename_profile_to_existing_fails() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("first")?;
    create_profile("second")?;

    let result = rename_profile("first", "second");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("already exists"));

    Ok(())
}

#[test]
#[serial]
fn test_rename_nonexistent_profile_fails() -> Result<()> {
    let _temp = setup_temp_home();

    let result = rename_profile("nonexistent", "new_name");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("does not exist"));

    Ok(())
}

#[test]
#[serial]
fn test_rename_profile_empty_name_fails() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("valid")?;

    let result = rename_profile("valid", "");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("cannot be empty"));

    Ok(())
}

#[test]
#[serial]
fn test_rename_profile_with_path_separator_fails() -> Result<()> {
    let _temp = setup_temp_home();

    create_profile("valid")?;

    let result = rename_profile("valid", "bad/name");
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("path separators"));

    Ok(())
}

#[test]
#[serial]
fn test_profile_config_isolation() -> Result<()> {
    use agent_of_empires::session::{load_profile_config, save_profile_config, ProfileConfig};
    use serde_json::json;

    let _temp = setup_temp_home();

    create_profile("custom")?;

    // Save a profile-specific config override for "custom"
    let custom_config: ProfileConfig =
        serde_json::from_value(json!({"updates": {"update_check_mode": "off"}}))?;
    save_profile_config("custom", &custom_config)?;

    // Load config for "default" profile - should have no overrides
    let default_config = load_profile_config("default")?;
    assert!(
        !default_config.overrides.contains_key("updates"),
        "Default profile should have no update overrides"
    );

    // Verify custom profile has its override
    let loaded_custom = load_profile_config("custom")?;
    let ov = serde_json::to_value(&loaded_custom)?;
    assert_eq!(
        ov["updates"]["update_check_mode"],
        json!("off"),
        "Custom profile should have update_check_mode = Off"
    );

    Ok(())
}
