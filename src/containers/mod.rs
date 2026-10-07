pub mod container_interface;
pub mod error;
mod execution;
pub mod image_update;
mod runtime;
pub(crate) mod runtime_base;
pub mod stats;

use std::collections::{HashMap, HashSet};

use crate::cli::truncate_id;
use crate::session::{Config, ContainerRuntimeName};
pub(crate) use container_interface::InspectedContainer;
pub use container_interface::{
    ContainerConfig, EnvEntry, NamedVolumeMount, RunPolicy, VolumeMount,
};
use error::Result;
pub(crate) use execution::{ContainerExecutionSnapshot, RuntimeExecutionSnapshot};
pub use runtime::{ContainerRuntime, ContainerState};

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

pub const SANDBOX_NAME_PREFIX: &str = "aoe-sandbox-";

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

pub fn batch_container_health() -> HashMap<String, bool> {
    let start = std::time::Instant::now();
    let map = get_container_runtime().batch_running_states(SANDBOX_NAME_PREFIX);
    tracing::debug!(
        target: "containers.runtime",
        count = map.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "batch container health fetched",
    );
    map
}

pub fn batch_container_states() -> HashMap<String, ContainerState> {
    let start = std::time::Instant::now();
    let map = get_container_runtime().batch_container_states(SANDBOX_NAME_PREFIX);
    tracing::debug!(
        target: "containers.runtime",
        count = map.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "batch container states fetched",
    );
    map
}

pub fn batch_container_stats() -> stats::StatsMap {
    let start = std::time::Instant::now();
    let map = get_container_runtime().batch_stats(SANDBOX_NAME_PREFIX);
    tracing::debug!(
        target: "containers.runtime",
        count = map.len(),
        duration_ms = start.elapsed().as_millis() as u64,
        "batch container stats fetched",
    );
    map
}

#[derive(Debug)]
pub enum Teardown {
    Removed,
    AlreadyGone,
    Failed(error::DockerError),
}

fn classify_removal(result: Result<()>) -> Teardown {
    match result {
        Ok(()) => Teardown::Removed,
        Err(error::DockerError::ContainerNotFound(_)) => Teardown::AlreadyGone,
        Err(e) => Teardown::Failed(e),
    }
}

/// Callers gating a mutation must treat `Unknown` as possibly running, never as `NotRunning`.
#[derive(Debug)]
pub enum Probe {
    Running,
    NotRunning,
    Unknown(error::DockerError),
}

fn classify_running_probe(result: Result<bool>) -> Probe {
    match result {
        Ok(true) => Probe::Running,
        Ok(false) => Probe::NotRunning,
        Err(e) => Probe::Unknown(e),
    }
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
        format!("{SANDBOX_NAME_PREFIX}{}", truncate_id(session_id, 8))
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

    pub fn working_dir(&self) -> Option<String> {
        self.runtime.container_working_dir(&self.name)
    }

    pub fn sandbox_store_generation_matches(&self) -> Result<Option<bool>> {
        self.runtime.sandbox_store_generation_matches(&self.name)
    }

    pub(crate) fn inspect(&self) -> Result<Option<InspectedContainer>> {
        self.runtime.inspect_container(&self.name)
    }

    pub fn shared_credential_mounts_match(&self, config: &ContainerConfig) -> Result<Option<bool>> {
        self.runtime
            .shared_credential_mounts_match(&self.name, config)
    }

    pub fn carries_shared_credential_label(&self) -> Result<Option<bool>> {
        self.runtime.carries_shared_credential_label(&self.name)
    }

    pub fn agent_tool_matches(&self, identity: &str) -> Result<Option<bool>> {
        self.runtime.agent_tool_matches(&self.name, identity)
    }

    pub fn mount_fingerprint_matches(&self, config: &ContainerConfig) -> Result<Option<bool>> {
        self.runtime
            .mount_fingerprint_matches(&self.name, &config.mount_fingerprint())
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

    /// `names` is an allowlist. Call before the create, while no container holds the volumes.
    pub fn remove_stranded_named_ignore_volumes(&self, session_id: &str, names: &[String]) {
        if names.is_empty() || !self.runtime.base.supports_named_volumes {
            return;
        }
        tracing::info!(
            target: "containers.runtime",
            %session_id,
            ?names,
            "reclaiming named ignore volumes stranded by a worktree move"
        );
        let names: HashSet<&str> = names.iter().map(String::as_str).collect();
        let prefix = format!("aoe-vi-{}-", session_id);
        if let Err(e) = self
            .runtime
            .base
            .remove_named_ignore_volumes_in(&prefix, &names)
        {
            tracing::warn!(
                target: "containers.runtime",
                name = %self.name,
                %session_id,
                error = %e,
                "failed to remove stranded named ignore volumes"
            );
        }
    }

    /// Callers must invoke this unconditionally, never behind an existence probe whose
    /// transient failure would orphan a live container.
    pub fn teardown(&self, session_id: &str) -> Teardown {
        let outcome = classify_removal(self.remove(true));
        self.remove_named_ignore_volumes(session_id);
        outcome
    }

    /// Keeps named ignore volumes for the immediate recreate. Same invoke-unconditionally rule
    /// as [`Self::teardown`].
    pub fn discard(&self) -> Teardown {
        classify_removal(self.remove(true))
    }

    /// Non-force removal for the one `probe_running()`-gated caller: a container that started
    /// after the probe is refused rather than killed under a live agent.
    pub fn discard_if_stopped(&self) -> Teardown {
        classify_removal(self.remove(false))
    }

    pub fn probe_running(&self) -> Probe {
        classify_running_probe(self.is_running())
    }

    pub fn exec_command(&self, options: Option<&str>, cmd: &str) -> String {
        self.runtime.exec_command(&self.name, options, cmd)
    }

    pub fn build_exec_argv(&self, workdir: &str, cmd: &[String]) -> Vec<String> {
        self.runtime.build_exec_argv(&self.name, workdir, cmd)
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
    fn classify_removal_separates_gone_from_failed() {
        assert!(matches!(classify_removal(Ok(())), Teardown::Removed));
        assert!(matches!(
            classify_removal(Err(error::DockerError::ContainerNotFound(
                "aoe-sandbox-x".into()
            ))),
            Teardown::AlreadyGone
        ));
        assert!(matches!(
            classify_removal(Err(error::DockerError::RemoveFailed("daemon busy".into()))),
            Teardown::Failed(_)
        ));
    }

    #[test]
    fn classify_running_probe_keeps_errors_out_of_the_running_answer() {
        assert!(matches!(classify_running_probe(Ok(true)), Probe::Running));
        assert!(matches!(
            classify_running_probe(Ok(false)),
            Probe::NotRunning
        ));
        assert!(matches!(
            classify_running_probe(Err(error::DockerError::InspectFailed(
                "inspect exit 1".into()
            ))),
            Probe::Unknown(_)
        ));
    }

    #[test]
    fn generate_name_prefixes_and_truncates_the_session_id() {
        assert_eq!(DockerContainer::generate_name("abc"), "aoe-sandbox-abc");
        assert_eq!(
            DockerContainer::generate_name("abcdefghijklmnop"),
            "aoe-sandbox-abcdefgh"
        );
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

        assert!(!args.contains(&"-v".to_string()));
    }
}
