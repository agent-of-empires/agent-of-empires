pub mod container_interface;
pub mod error;
pub mod image_update;
mod runtime;
pub(crate) mod runtime_base;

use std::collections::HashMap;

use crate::cli::truncate_id;
use crate::session::{Config, ContainerRuntimeName};
pub use container_interface::{ContainerConfig, EnvEntry, NamedVolumeMount, VolumeMount};
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

/// Outcome of an idempotent container teardown.
#[derive(Debug)]
pub enum Teardown {
    /// The container was force-removed.
    Removed,
    /// No container existed to remove; the teardown was a no-op.
    AlreadyGone,
    /// Removal failed for a reason other than the container being absent.
    Failed(error::DockerError),
}

/// Classify a force-remove result into an idempotent teardown outcome.
///
/// A `ContainerNotFound` error means there was nothing to remove and maps to
/// `AlreadyGone`; every other error is a genuine `Failed`. Keeping this
/// classification separate from I/O lets it be reasoned about and tested
/// without a live runtime.
fn classify_removal(result: Result<()>) -> Teardown {
    match result {
        Ok(()) => Teardown::Removed,
        Err(error::DockerError::ContainerNotFound(_)) => Teardown::AlreadyGone,
        Err(e) => Teardown::Failed(e),
    }
}

/// Outcome of a running-state probe that preserves the difference between
/// a definitive "not running" and a transient inspection failure.
///
/// Callers gating a mutation on the container being stopped must match on
/// all three variants and treat [`Probe::Unknown`] conservatively (typically
/// as "possibly running"). Collapsing the underlying `is_running() -> Result<bool>`
/// to a plain `bool` via `unwrap_or(false)` re-introduces the swallowing-
/// existence-probe class of bug fixed in #2596.
/// [`DockerContainer::probe_running`] is the constructor for this type.
#[derive(Debug)]
pub enum Probe {
    /// The container is running.
    Running,
    /// The container is definitively not running (stopped or absent).
    NotRunning,
    /// The inspection itself failed; the running state is unknown.
    Unknown(error::DockerError),
}

/// Classify a running-state result into an idempotent probe outcome.
///
/// A transient inspection error must not be swallowed into a `NotRunning`
/// false negative. Keeping this classification separate from I/O lets it
/// be reasoned about and tested without a live runtime.
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

    /// The container's configured working directory, read from the live
    /// container. `None` if it can't be determined; see
    /// [`ContainerRuntime::container_working_dir`].
    pub fn working_dir(&self) -> Option<String> {
        self.runtime.container_working_dir(&self.name)
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

    /// Force-remove this container, then sweep its named ignore volumes.
    ///
    /// Idempotent: a container that is already gone yields
    /// [`Teardown::AlreadyGone`], not a failure. Named ignore volumes outlive
    /// the container, so they are swept regardless of the removal outcome.
    ///
    /// This method must be invoked unconditionally by callers, which then
    /// act on the returned outcome; it must never be gated behind a separate
    /// existence probe, whose transient failure would skip removal and orphan
    /// a live container.
    pub fn teardown(&self, session_id: &str) -> Teardown {
        let outcome = classify_removal(self.remove(true));
        self.remove_named_ignore_volumes(session_id);
        outcome
    }

    /// Force-remove this container, preserving its named ignore volumes.
    ///
    /// Idempotent counterpart to [`Self::teardown`]: same removal and
    /// classification, but the session-scoped named ignore volumes
    /// (`aoe-vi-{session_id}-*`, e.g. `target/`, `node_modules/`) are left
    /// intact so the recreated container re-attaches them on next start.
    /// Used on the worktree-move discard path where the container is dropped
    /// to pick up a new bind mount and will be recreated immediately.
    ///
    /// The same invariant as [`Self::teardown`] applies: callers must invoke
    /// this unconditionally and act on the returned outcome; it must never
    /// be gated behind a separate existence probe, whose transient failure
    /// would skip removal and orphan a live container (#2596).
    pub fn discard(&self) -> Teardown {
        classify_removal(self.remove(true))
    }

    /// Probe this container's running state, preserving the difference
    /// between a definitive "not running" and a transient inspection failure.
    ///
    /// Prefer this over `is_running().unwrap_or(false)` at any call site
    /// where the returned boolean gates a mutation on the container being
    /// stopped: `unwrap_or(false)` swallows a transient `docker inspect`
    /// failure into a false negative and lets the gate open against a
    /// possibly-live container: the swallowing-existence-probe class of
    /// bug fixed on the removal path in #2596.
    pub fn probe_running(&self) -> Probe {
        classify_running_probe(self.is_running())
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
    fn classify_ok_is_removed() {
        assert!(matches!(classify_removal(Ok(())), Teardown::Removed));
    }

    #[test]
    fn classify_not_found_is_already_gone() {
        let r = Err(error::DockerError::ContainerNotFound(
            "aoe-sandbox-x".into(),
        ));
        assert!(matches!(classify_removal(r), Teardown::AlreadyGone));
    }

    #[test]
    fn classify_other_error_is_failed() {
        let r = Err(error::DockerError::RemoveFailed("daemon busy".into()));
        assert!(matches!(classify_removal(r), Teardown::Failed(_)));
    }

    #[test]
    fn probe_ok_true_is_running() {
        assert!(matches!(classify_running_probe(Ok(true)), Probe::Running));
    }

    #[test]
    fn probe_ok_false_is_not_running() {
        assert!(matches!(
            classify_running_probe(Ok(false)),
            Probe::NotRunning
        ));
    }

    #[test]
    fn probe_err_is_unknown() {
        let r = Err(error::DockerError::InspectFailed("inspect exit 1".into()));
        assert!(matches!(classify_running_probe(r), Probe::Unknown(_)));
    }

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
