//! Integration tests for group management with disk persistence.

use agent_of_empires::session::{GroupTree, Instance, Storage};
use anyhow::Result;
use serial_test::serial;

use crate::common::setup_temp_home;

#[test]
#[serial]
fn test_create_group_and_persist() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let instances: Vec<Instance> = vec![];
    let mut group_tree = GroupTree::new_with_groups(&instances, &[]);
    group_tree.create_group("work");

    storage.update(|i, g| {
        *i = instances.to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    let (loaded_instances, loaded_groups) = storage.load_with_groups()?;
    let reloaded_tree = GroupTree::new_with_groups(&loaded_instances, &loaded_groups);
    assert!(reloaded_tree.group_exists("work"));

    Ok(())
}

#[test]
#[serial]
fn test_nested_group_persistence() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let instances: Vec<Instance> = vec![];
    let mut group_tree = GroupTree::new_with_groups(&instances, &[]);
    group_tree.create_group("work/frontend");

    storage.update(|i, g| {
        *i = instances.to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    let (loaded_instances, loaded_groups) = storage.load_with_groups()?;
    let reloaded_tree = GroupTree::new_with_groups(&loaded_instances, &loaded_groups);
    assert!(reloaded_tree.group_exists("work"));
    assert!(reloaded_tree.group_exists("work/frontend"));

    Ok(())
}

#[test]
#[serial]
fn test_delete_group_persists() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let instances: Vec<Instance> = vec![];

    // Create and save a group
    let mut group_tree = GroupTree::new_with_groups(&instances, &[]);
    group_tree.create_group("temporary");
    storage.update(|i, g| {
        *i = instances.to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    // Reload, delete, save again
    let (loaded_instances, loaded_groups) = storage.load_with_groups()?;
    let mut reloaded_tree = GroupTree::new_with_groups(&loaded_instances, &loaded_groups);
    assert!(reloaded_tree.group_exists("temporary"));

    reloaded_tree.delete_group("temporary");
    storage.update(|i, g| {
        *i = loaded_instances.to_vec();
        *g = reloaded_tree.get_all_groups();
        Ok(())
    })?;

    // Reload again and verify deletion
    let (final_instances, final_groups) = storage.load_with_groups()?;
    let final_tree = GroupTree::new_with_groups(&final_instances, &final_groups);
    assert!(!final_tree.group_exists("temporary"));

    Ok(())
}

#[test]
#[serial]
fn test_move_session_between_groups() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let mut instance = Instance::new("Movable", "/path/movable");
    instance.group_path = "group-a".to_string();

    let mut group_tree = GroupTree::new_with_groups(&[instance.clone()], &[]);
    group_tree.create_group("group-a");
    group_tree.create_group("group-b");
    storage.update(|i, g| {
        *i = [instance.clone()].to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    // Move the session to group-b
    let (mut loaded, loaded_groups) = storage.load_with_groups()?;
    loaded[0].group_path = "group-b".to_string();
    let new_tree = GroupTree::new_with_groups(&loaded, &loaded_groups);
    storage.update(|i, g| {
        *i = loaded.to_vec();
        *g = new_tree.get_all_groups();
        Ok(())
    })?;

    // Reload and verify
    let (final_instances, final_groups) = storage.load_with_groups()?;
    assert_eq!(final_instances[0].group_path, "group-b");
    let final_tree = GroupTree::new_with_groups(&final_instances, &final_groups);
    assert!(final_tree.group_exists("group-b"));

    Ok(())
}

#[test]
#[serial]
fn test_group_with_sessions_round_trip() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;

    let mut inst1 = Instance::new("Frontend", "/path/frontend");
    inst1.group_path = "work".to_string();
    let mut inst2 = Instance::new("Backend", "/path/backend");
    inst2.group_path = "work".to_string();
    let mut inst3 = Instance::new("Hobby", "/path/hobby");
    inst3.group_path = "personal".to_string();

    let instances = vec![inst1, inst2, inst3];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage.update(|i, g| {
        *i = instances.to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    let (loaded, loaded_groups) = storage.load_with_groups()?;
    assert_eq!(loaded.len(), 3);

    let work_sessions: Vec<_> = loaded.iter().filter(|i| i.group_path == "work").collect();
    assert_eq!(work_sessions.len(), 2);

    let personal_sessions: Vec<_> = loaded
        .iter()
        .filter(|i| i.group_path == "personal")
        .collect();
    assert_eq!(personal_sessions.len(), 1);

    let reloaded_tree = GroupTree::new_with_groups(&loaded, &loaded_groups);
    assert!(reloaded_tree.group_exists("work"));
    assert!(reloaded_tree.group_exists("personal"));

    Ok(())
}

#[test]
#[serial]
fn test_empty_groups_persist() -> Result<()> {
    let _temp = setup_temp_home();

    let storage = Storage::new_unwatched("default")?;
    let instances: Vec<Instance> = vec![];

    let mut group_tree = GroupTree::new_with_groups(&instances, &[]);
    group_tree.create_group("empty-group");
    group_tree.create_group("another-empty");
    storage.update(|i, g| {
        *i = instances.to_vec();
        *g = group_tree.get_all_groups();
        Ok(())
    })?;

    let (loaded_instances, loaded_groups) = storage.load_with_groups()?;
    assert!(loaded_instances.is_empty());

    let reloaded_tree = GroupTree::new_with_groups(&loaded_instances, &loaded_groups);
    assert!(reloaded_tree.group_exists("empty-group"));
    assert!(reloaded_tree.group_exists("another-empty"));

    Ok(())
}
