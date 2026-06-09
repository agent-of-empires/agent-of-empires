//! Integration tests for the core session lifecycle: create, persist, load, remove.

use agent_of_empires::session::{GroupTree, Instance, Storage};
use anyhow::Result;
use serial_test::serial;
use std::fs;

use crate::common::setup_temp_home;

#[test]
#[serial]
fn test_create_session_persists() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let instance = Instance::new("My Project", "/home/user/project");
    let group_tree = GroupTree::new_with_groups(std::slice::from_ref(&instance), &[]);

    storage.update(|i, g| {
        *i = vec![instance.clone()];
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    let (loaded, _groups) = storage.load_with_groups()?;
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].title, "My Project");
    assert_eq!(loaded[0].project_path, "/home/user/project");
    assert_eq!(loaded[0].id, instance.id);

    Ok(())
}

#[test]
#[serial]
fn test_create_multiple_sessions() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let instances = vec![
        Instance::new("Project A", "/path/a"),
        Instance::new("Project B", "/path/b"),
        Instance::new("Project C", "/path/c"),
    ];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);

    storage.update(|i, g| {
        *i = instances.to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    let (loaded, _) = storage.load_with_groups()?;
    assert_eq!(loaded.len(), 3);
    assert_eq!(loaded[0].title, "Project A");
    assert_eq!(loaded[1].title, "Project B");
    assert_eq!(loaded[2].title, "Project C");

    Ok(())
}

#[test]
#[serial]
fn test_remove_session_by_id() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let inst_a = Instance::new("Keep Me", "/path/keep");
    let inst_b = Instance::new("Remove Me", "/path/remove");
    let remove_id = inst_b.id.clone();

    let instances = vec![inst_a, inst_b];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage.update(|i, g| {
        *i = instances.to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    // Remove by filtering
    let (mut loaded, groups) = storage.load_with_groups()?;
    loaded.retain(|i| i.id != remove_id);
    let group_tree = GroupTree::new_with_groups(&loaded, &groups);
    storage.update(|i, g| {
        *i = loaded.to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    let (reloaded, _) = storage.load_with_groups()?;
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].title, "Keep Me");

    Ok(())
}

#[test]
#[serial]
fn test_create_session_with_group() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let mut instance = Instance::new("Grouped Session", "/path/grouped");
    instance.group_path = "work".to_string();

    let mut group_tree = GroupTree::new_with_groups(std::slice::from_ref(&instance), &[]);
    group_tree.create_group("work");

    storage.update(|i, g| {
        *i = vec![instance.clone()];
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    let (loaded, loaded_groups) = storage.load_with_groups()?;
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].group_path, "work");

    let reloaded_tree = GroupTree::new_with_groups(&loaded, &loaded_groups);
    assert!(reloaded_tree.group_exists("work"));

    Ok(())
}

#[test]
#[serial]
fn test_save_leaves_no_debris() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;

    for i in 0..5 {
        let instances = vec![Instance::new(&format!("iter{i}"), "/tmp/test")];
        storage.update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })?;
    }

    // Atomic write should leave only the persisted JSON files in the profile
    // dir, no .json.bak from the old code path and no leftover tempfiles.
    let profile_dir = agent_of_empires::session::get_profile_dir("default")?;
    let entries: Vec<String> = fs::read_dir(&profile_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    for entry in &entries {
        assert!(
            !entry.contains(".tmp"),
            "atomic_write must not leak temp files; found {}",
            entry
        );
    }
    assert!(entries.contains(&"sessions.json".to_string()));

    Ok(())
}

#[test]
#[serial]
fn test_source_profile_not_serialized() {
    let _temp = setup_temp_home();

    let mut instance = Instance::new("Test", "/tmp/test");
    instance.source_profile = "work".to_string();

    let storage = Storage::new_unwatched("default").unwrap();
    let seeded = vec![instance.clone()];
    storage
        .update(|i, g| {
            *i = seeded.to_vec();
            *g = GroupTree::new_with_groups(&seeded, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    // Read raw JSON -- source_profile should not appear
    let profile_dir = agent_of_empires::session::get_profile_dir("default").unwrap();
    let content = std::fs::read_to_string(profile_dir.join("sessions.json")).unwrap();
    assert!(
        !content.contains("source_profile"),
        "source_profile should not be serialized"
    );

    // Reload -- source_profile should default to empty
    let loaded = storage.load().unwrap();
    assert_eq!(loaded[0].source_profile, "");
}

#[test]
#[serial]
fn test_storage_empty_profile_resolves_to_bootstrap() -> Result<()> {
    // Empty profile names now route through resolve_default_profile, which
    // on a fresh install bootstraps "main" (the PR's intentional rename,
    // see src/session/config.rs::ensure_bootstrap_profile). Confirm the
    // resolution lands on that name and that storage round-trips after.
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("")?;
    assert_eq!(storage.profile(), "main");

    let instances = vec![Instance::new("Test", "/path/test")];
    storage.update(|i, g| {
        *i = instances.to_vec();
        *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
        Ok(())
    })?;
    let loaded = storage.load()?;
    assert_eq!(loaded.len(), 1);

    Ok(())
}
