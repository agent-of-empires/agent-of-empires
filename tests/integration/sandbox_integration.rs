//! Integration tests for Docker sandbox functionality
//!
//! These tests validate the sandbox container lifecycle:
//! - Container creation when starting a sandboxed session
//! - Container cleanup when deleting a sandboxed session
//! - Docker availability validation

use agent_of_empires::containers::{self, DockerContainer};

fn docker_available() -> bool {
    let rt = containers::get_container_runtime();
    rt.is_available() && rt.is_daemon_running()
}

#[test]
#[serial_test::parallel]
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
#[serial_test::parallel]
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
#[serial_test::parallel]
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
