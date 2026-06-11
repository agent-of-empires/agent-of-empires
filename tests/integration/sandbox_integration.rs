//! Integration tests for Docker sandbox functionality
//!
//! These tests validate the sandbox container lifecycle:
//! - Container creation when starting a sandboxed session
//! - Container cleanup when deleting a sandboxed session
//! - Docker availability validation

use agent_of_empires::containers::{self, ContainerRuntimeInterface, DockerContainer};
use agent_of_empires::session::{GroupTree, Instance, SandboxInfo, Storage};
use serial_test::serial;

fn docker_available() -> bool {
    let rt = containers::get_container_runtime();
    rt.is_available() && rt.is_daemon_running()
}

#[test]
fn test_sandbox_info_serialization() {
    let sandbox_info = SandboxInfo {
        enabled: true,
        container_id: Some("abc123".to_string()),
        image: "ubuntu:latest".to_string(),
        container_name: "aoe-sandbox-test1234".to_string(),
        extra_env: Some(vec!["MY_VAR".to_string()]),
        custom_instruction: None,
        before_start_env: Vec::new(),
    };

    let json = serde_json::to_string(&sandbox_info).unwrap();
    let deserialized: SandboxInfo = serde_json::from_str(&json).unwrap();

    assert!(deserialized.enabled);
    assert_eq!(deserialized.container_id, Some("abc123".to_string()));
    assert_eq!(deserialized.container_name, "aoe-sandbox-test1234");
    assert_eq!(deserialized.image, "ubuntu:latest");
    assert_eq!(deserialized.extra_env, Some(vec!["MY_VAR".to_string()]));
}

#[test]
fn test_instance_is_sandboxed() {
    let mut inst = Instance::new("test", "/tmp/test");
    assert!(!inst.is_sandboxed());

    inst.sandbox_info = Some(SandboxInfo {
        enabled: true,
        container_id: None,
        image: "test-image".to_string(),
        container_name: "aoe-sandbox-test".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
    });
    assert!(inst.is_sandboxed());

    inst.sandbox_info = Some(SandboxInfo {
        enabled: false,
        container_id: None,
        image: "test-image".to_string(),
        container_name: "aoe-sandbox-test".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
    });
    assert!(!inst.is_sandboxed());
}

// `#[serial]` because this test mutates the process-global `HOME` env var.
// Without serialization, parallel tests in the same binary that also set HOME
// will race with us and may drop their TempDir mid-test (see `setup_temp_home`
// in `crate::common`).
#[test]
#[serial]
fn test_sandbox_info_persists_across_save_load() {
    let temp = tempfile::TempDir::new().unwrap();
    std::env::set_var("HOME", temp.path());

    let storage = Storage::new_unwatched("sandbox_test").unwrap();

    let mut inst = Instance::new("sandbox-session", "/tmp/project");
    inst.sandbox_info = Some(SandboxInfo {
        enabled: true,
        container_id: Some("container123".to_string()),
        image: "custom:image".to_string(),
        container_name: "aoe-sandbox-abcd1234".to_string(),
        extra_env: Some(vec!["API_KEY".to_string(), "SECRET=my_secret".to_string()]),
        custom_instruction: None,
        before_start_env: Vec::new(),
    });

    let seeded = vec![inst.clone()];
    storage
        .update(|i, g| {
            *i = seeded.to_vec();
            *g = GroupTree::new_with_groups(&seeded, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let loaded = storage.load().unwrap();
    assert_eq!(loaded.len(), 1);

    let loaded_inst = &loaded[0];
    assert!(loaded_inst.sandbox_info.is_some());

    let sandbox = loaded_inst.sandbox_info.as_ref().unwrap();
    assert!(sandbox.enabled);
    assert_eq!(sandbox.container_id, Some("container123".to_string()));
    assert_eq!(sandbox.image, "custom:image");
    assert_eq!(sandbox.container_name, "aoe-sandbox-abcd1234");
}

#[test]
fn test_container_name_generation() {
    let name1 = DockerContainer::generate_name("abcd1234");
    assert_eq!(name1, "aoe-sandbox-abcd1234");

    let name2 = DockerContainer::generate_name("abcdefghijklmnop");
    assert_eq!(name2, "aoe-sandbox-abcdefgh");

    let name3 = DockerContainer::generate_name("abc");
    assert_eq!(name3, "aoe-sandbox-abc");
}

#[test]
#[ignore = "requires Docker daemon"]
fn test_container_lifecycle() {
    if !docker_available() {
        eprintln!("Skipping: Docker not available");
        return;
    }

    let session_id = format!(
        "test{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );

    let container = DockerContainer::new(&session_id, "alpine:latest");

    assert!(!container.exists().unwrap());

    let config = containers::ContainerConfig {
        working_dir: "/workspace".to_string(),
        volumes: vec![],
        anonymous_volumes: vec![],
        named_ignore_volumes: vec![],
        environment: vec![],
        cpu_limit: None,
        memory_limit: None,
        port_mappings: vec![],
        ..Default::default()
    };

    let container_id = container.create(&config).unwrap();
    assert!(!container_id.is_empty());
    assert!(container.exists().unwrap());
    assert!(container.is_running().unwrap());

    container.stop().unwrap();
    assert!(container.exists().unwrap());
    assert!(!container.is_running().unwrap());

    container.remove(false).unwrap();
    assert!(!container.exists().unwrap());
}

#[test]
#[ignore = "requires Docker daemon"]
fn test_container_force_remove() {
    if !docker_available() {
        eprintln!("Skipping: Docker not available");
        return;
    }

    let session_id = format!(
        "testforce{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );

    let container = containers::DockerContainer::new(&session_id, "alpine:latest");

    let config = containers::ContainerConfig {
        working_dir: "/workspace".to_string(),
        volumes: vec![],
        anonymous_volumes: vec![],
        named_ignore_volumes: vec![],
        environment: vec![],
        cpu_limit: None,
        memory_limit: None,
        port_mappings: vec![],
        ..Default::default()
    };

    container.create(&config).unwrap();
    assert!(container.is_running().unwrap());

    // Force remove while running
    container.remove(true).unwrap();
    assert!(!container.exists().unwrap());
}
