pub mod container_interface;
pub mod error;
mod runtime;
pub(crate) mod runtime_base;

use std::collections::HashMap;

use crate::cli::truncate_id;
use crate::session::{Config, ContainerRuntimeName};
pub use container_interface::{
    ContainerConfig, ContainerRuntimeInterface, EnvEntry, NamedVolumeMount, VolumeMount,
};
use error::Result;
pub use runtime::ContainerRuntime;

/// Returns the CLI binary name for the configured container runtime.
pub fn runtime_binary() -> &'static str {
    if let Ok(cfg) = Config::load() {
        match cfg.sandbox.container_runtime {
            ContainerRuntimeName::AppleContainer => "container",
            ContainerRuntimeName::Docker => "docker",
            ContainerRuntimeName::Podman => "podman",
        }
    } else {
        "docker"
    }
}

pub fn get_container_runtime() -> ContainerRuntime {
    if let Ok(cfg) = Config::load() {
        match cfg.sandbox.container_runtime {
            ContainerRuntimeName::AppleContainer => ContainerRuntime::apple_container(),
            ContainerRuntimeName::Docker => ContainerRuntime::docker(),
            ContainerRuntimeName::Podman => ContainerRuntime::podman(),
        }
    } else {
        ContainerRuntime::default()
    }
}

/// Check running state of all aoe sandbox containers in a single subprocess call.
/// Returns a map of container name -> is_running.
pub fn batch_container_health() -> HashMap<String, bool> {
    let start = std::time::Instant::now();
    let map = get_container_runtime().batch_running_states("aoe-sandbox-");
    tracing::debug!(
        target: "containers.runtime",
        count = map.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "batch container health fetched",
    );
    map
}

pub struct DockerContainer {
    pub name: String,
    pub image: String,
    runtime: ContainerRuntime,
}

impl DockerContainer {
    pub fn new(session_id: &str, image: &str) -> Self {
        Self {
            name: Self::generate_name(session_id),
            image: image.to_string(),
            runtime: get_container_runtime(),
        }
    }

    pub fn generate_name(session_id: &str) -> String {
        format!("aoe-sandbox-{}", truncate_id(session_id, 8))
    }

    pub fn from_session_id(session_id: &str) -> Self {
        Self {
            name: Self::generate_name(session_id),
            image: String::new(),
            runtime: get_container_runtime(),
        }
    }

    pub fn exists(&self) -> Result<bool> {
        self.runtime.does_container_exist(&self.name)
    }

    pub fn is_running(&self) -> Result<bool> {
        self.runtime.is_container_running(&self.name)
    }

    pub fn build_create_args(&self, config: &ContainerConfig) -> Vec<String> {
        self.runtime
            .build_create_args(&self.name, &self.image, config)
    }

    #[tracing::instrument(target = "containers.runtime", skip_all, fields(name = %self.name, image = %self.image))]
    pub fn create(&self, config: &ContainerConfig) -> Result<String> {
        tracing::info!(target: "containers.runtime", "creating container");
        let result = self
            .runtime
            .create_container(&self.name, &self.image, config);
        match &result {
            Ok(id) => tracing::info!(target: "containers.runtime", id = %id, "created"),
            Err(e) => tracing::error!(target: "containers.runtime", error = %e, "create failed"),
        }
        result
    }

    #[tracing::instrument(target = "containers.runtime", skip_all, fields(name = %self.name))]
    pub fn start(&self) -> Result<()> {
        tracing::info!(target: "containers.runtime", "starting container");
        let result = self.runtime.start_container(&self.name);
        if let Err(e) = &result {
            tracing::error!(target: "containers.runtime", error = %e, "start failed");
        }
        result
    }

    #[tracing::instrument(target = "containers.runtime", skip_all, fields(name = %self.name))]
    pub fn stop(&self) -> Result<()> {
        tracing::info!(target: "containers.runtime", "stopping container");
        let result = self.runtime.stop_container(&self.name);
        if let Err(e) = &result {
            tracing::warn!(target: "containers.runtime", error = %e, "stop failed");
        }
        result
    }

    #[tracing::instrument(target = "containers.runtime", skip_all, fields(name = %self.name, force))]
    pub fn remove(&self, force: bool) -> Result<()> {
        tracing::info!(target: "containers.runtime", "removing container");
        let result = self.runtime.remove(&self.name, force);
        if let Err(e) = &result {
            tracing::warn!(target: "containers.runtime", error = %e, "remove failed");
        }
        result
    }

    /// Remove all named ignore volumes for this session (prefix = `aoe-vi-{session_id}-`).
    ///
    /// Must be called after container removal during session deletion. Named volumes are not
    /// removed by `docker rm -v`; they require explicit cleanup. Safe to call even when the
    /// container is already gone — volumes can outlive their container.
    pub fn remove_named_ignore_volumes(&self, session_id: &str) {
        let prefix = format!("aoe-vi-{}-", session_id);
        if let Err(e) = self.runtime.base.remove_named_ignore_volumes(&prefix) {
            tracing::warn!(
                target: "containers.runtime",
                name = %self.name,
                %session_id,
                error = %e,
                "failed to remove named ignore volumes"
            );
        }
    }

    pub fn exec_command(&self, options: Option<&str>, cmd: &str) -> String {
        self.runtime.exec_command(&self.name, options, cmd)
    }

    #[tracing::instrument(target = "containers.exec", skip_all, fields(name = %self.name, cmd = ?cmd))]
    pub fn exec(&self, cmd: &[&str]) -> Result<std::process::Output> {
        let result = self.runtime.exec(&self.name, cmd);
        match &result {
            Ok(out) => tracing::debug!(
                target: "containers.exec",
                status = ?out.status,
                stdout_bytes = out.stdout.len(),
                stderr_bytes = out.stderr.len(),
                "exec completed",
            ),
            Err(e) => tracing::warn!(target: "containers.exec", error = %e, "exec failed"),
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_container_generate_name_short_id() {
        let name = DockerContainer::generate_name("abc");
        assert_eq!(name, "aoe-sandbox-abc");
    }

    #[test]
    fn test_container_generate_name_long_id() {
        let name = DockerContainer::generate_name("abcdefghijklmnop");
        assert_eq!(name, "aoe-sandbox-abcdefgh");
    }

    #[test]
    fn test_container_exec_command() {
        let mut container = DockerContainer::new("test1234567890ab", "ubuntu:latest");
        container.runtime = ContainerRuntime::docker();

        let cmd = container.exec_command(None, "my-agent");
        assert_eq!(cmd, "docker exec -it aoe-sandbox-test1234 my-agent");
    }
    #[test]
    fn test_anonymous_volumes_in_create_args() {
        let container = DockerContainer::new("test1234567890ab", "alpine:latest");
        let config = ContainerConfig {
            working_dir: "/workspace/myproject".to_string(),
            volumes: vec![],
            anonymous_volumes: vec![
                "/workspace/myproject/target".to_string(),
                "/workspace/myproject/node_modules".to_string(),
            ],
            named_ignore_volumes: vec![],
            environment: vec![],
            cpu_limit: None,
            memory_limit: None,
            port_mappings: vec![],
            ..Default::default()
        };

        let args = container.build_create_args(&config);

        // Find the anonymous volume flags
        let v_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-v")
            .map(|(i, _)| i)
            .collect();

        let volume_values: Vec<&str> = v_positions.iter().map(|&i| args[i + 1].as_str()).collect();

        assert!(volume_values.contains(&"/workspace/myproject/target"));
        assert!(volume_values.contains(&"/workspace/myproject/node_modules"));
    }

    #[test]
    fn test_no_anonymous_volumes_when_empty() {
        let container = DockerContainer::new("test1234567890ab", "alpine:latest");
        let config = ContainerConfig {
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

        let args = container.build_create_args(&config);

        // No -v flags at all
        assert!(!args.contains(&"-v".to_string()));
    }
}
