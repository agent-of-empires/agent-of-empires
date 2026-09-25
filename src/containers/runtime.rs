//! The unified `ContainerRuntime`: shared behavior lives on `RuntimeBase`, runtime-specific probes dispatch on `RuntimeKind`.

use std::collections::HashMap;

use serde_json::Value;

use super::container_interface::{
    ContainerConfig, InspectedContainer, InspectedMount, VolumeMount,
};
use super::error::{DockerError, Result};
use super::runtime_base::RuntimeBase;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeKind {
    Docker,
    AppleContainer,
    Podman,
}

pub struct ContainerRuntime {
    pub(crate) base: RuntimeBase,
    pub(crate) kind: RuntimeKind,
}

fn is_runtime_timeout(error: &DockerError) -> bool {
    matches!(
        error,
        DockerError::IoError(error) if error.kind() == std::io::ErrorKind::TimedOut
    )
}

impl ContainerRuntime {
    pub fn docker() -> Self {
        Self {
            base: RuntimeBase::DOCKER,
            kind: RuntimeKind::Docker,
        }
    }

    pub fn apple_container() -> Self {
        Self {
            base: RuntimeBase::APPLE_CONTAINER,
            kind: RuntimeKind::AppleContainer,
        }
    }

    pub fn podman() -> Self {
        Self {
            base: RuntimeBase::PODMAN,
            kind: RuntimeKind::Podman,
        }
    }
}

impl Default for ContainerRuntime {
    fn default() -> Self {
        Self::docker()
    }
}

impl ContainerRuntime {
    pub fn is_available(&self) -> bool {
        self.base.is_available()
    }

    pub fn is_daemon_running(&self) -> bool {
        self.base.is_daemon_running()
    }

    pub fn image_exists_locally(&self, image: &str) -> bool {
        self.base.image_exists_locally(image)
    }

    pub fn local_image_digest(&self, image: &str) -> Option<String> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                let mut cmd = self.base.command();
                cmd.args([
                    "image",
                    "inspect",
                    "--format",
                    "{{range .RepoDigests}}{{println .}}{{end}}",
                    image,
                ]);
                let output = self.base.probe_output(&mut cmd).ok()?;
                if !output.status.success() {
                    return None;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                super::image_update::pick_repo_digest(image, &stdout)
            }
            // Apple's `image inspect` exposes no repo digest.
            RuntimeKind::AppleContainer => None,
        }
    }

    pub fn pull_image(&self, image: &str) -> Result<()> {
        self.base.pull_image(image)
    }

    pub fn ensure_image(&self, image: &str) -> Result<()> {
        self.base.ensure_image(image)
    }

    pub fn default_sandbox_image(&self) -> &'static str {
        self.base.default_sandbox_image()
    }

    pub fn effective_default_image(&self) -> String {
        self.base.effective_default_image()
    }

    pub fn does_container_exist(&self, name: &str) -> Result<bool> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                // `container inspect` stderr is what the `DOCKER_MISSING` fixture pins; changing argv needs new fixtures.
                let mut cmd = self.base.command();
                cmd.args(["container", "inspect", name]);
                let output = self.base.probe_output(&mut cmd)?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_probe_failure(&stderr);
                }
                Ok(true)
            }
            RuntimeKind::AppleContainer => {
                // Apple's `inspect` succeeds for missing containers; `logs` fails for them.
                let mut cmd = self.base.command();
                cmd.args(["logs", name]);
                let output = self.base.probe_output(&mut cmd)?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_probe_failure(&stderr);
                }
                Ok(true)
            }
        }
    }

    pub fn is_container_running(&self, name: &str) -> Result<bool> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                // `container inspect`, not `docker inspect`: the two word a missing container differently.
                let mut cmd = self.base.command();
                cmd.args(["container", "inspect", "-f", "{{.State.Running}}", name]);
                let output = self.base.probe_output(&mut cmd)?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_probe_failure(&stderr);
                }

                let stdout = String::from_utf8_lossy(&output.stdout);
                Ok(stdout.trim() == "true")
            }
            RuntimeKind::AppleContainer => {
                let mut cmd = self.base.command();
                cmd.args(["inspect", name]);
                let output = self.base.probe_output(&mut cmd)?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_probe_failure(&stderr);
                }

                let out_json: Value = serde_json::from_slice(&output.stdout)
                    .map_err(|e| DockerError::InspectFailed(e.to_string()))?;

                Self::apple_container_inspect_state(&out_json).map(|state| state == "running")
            }
        }
    }

    /// Accepts both the 0.12.x bare-string and 1.0.0 object `status`; any other shape is Err
    /// so a gate never fails open.
    fn apple_container_inspect_state(out_json: &Value) -> Result<&str> {
        let Some(status) = out_json.pointer("/0/status") else {
            return Err(DockerError::InspectFailed(
                "apple container inspect: exit 0 but no /0/status in output".into(),
            ));
        };
        status
            .as_str()
            .or_else(|| status.pointer("/state").and_then(Value::as_str))
            .ok_or_else(|| {
                DockerError::InspectFailed(
                    "apple container inspect: /0/status is neither a string nor \
                     an object with a string `state`"
                        .into(),
                )
            })
    }

    fn inspect_container_id(&self, name: &str) -> Result<String> {
        let mut cmd = self.base.command();
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                cmd.args(["container", "inspect", "-f", "{{.Id}}", name]);
            }
            RuntimeKind::AppleContainer => {
                cmd.args(["inspect", name]);
            }
        }
        let output = self.base.probe_output(&mut cmd)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            self.base.classify_probe_failure(&stderr)?;
            return Err(DockerError::ContainerNotFound(name.to_string()));
        }

        let id = match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                String::from_utf8_lossy(&output.stdout).trim().to_string()
            }
            RuntimeKind::AppleContainer => {
                let payload: Value = serde_json::from_slice(&output.stdout)
                    .map_err(|error| DockerError::InspectFailed(error.to_string()))?;
                Self::apple_container_inspect_id(&payload)?.to_string()
            }
        };
        if id.is_empty() {
            return Err(DockerError::InspectFailed(
                "container inspect returned an empty id".to_string(),
            ));
        }
        Ok(id)
    }

    fn apple_container_inspect_id(payload: &Value) -> Result<&str> {
        payload
            .pointer("/0/id")
            .or_else(|| payload.pointer("/0/configuration/id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| {
                DockerError::InspectFailed(
                    "apple container inspect: no non-empty /0/id or /0/configuration/id"
                        .to_string(),
                )
            })
    }

    fn apple_container_inspect_label<'a>(payload: &'a Value, key: &str) -> Result<Option<&'a str>> {
        let Some(labels) = payload.pointer("/0/configuration/labels") else {
            return Ok(None);
        };
        let labels = labels.as_object().ok_or_else(|| {
            DockerError::InspectFailed(
                "apple container inspect: /0/configuration/labels is not an object".to_string(),
            )
        })?;
        let Some(value) = labels.get(key) else {
            return Ok(None);
        };
        value.as_str().map(Some).ok_or_else(|| {
            DockerError::InspectFailed(format!(
                "apple container inspect: label {key} is not a string"
            ))
        })
    }

    fn inspect_container_label(&self, name: &str, key: &str) -> Result<Option<String>> {
        let mut cmd = self.base.command();
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                cmd.args([
                    "container",
                    "inspect",
                    "-f",
                    &format!(r#"{{{{index .Config.Labels "{key}"}}}}"#),
                    name,
                ]);
            }
            RuntimeKind::AppleContainer => {
                cmd.args(["inspect", name]);
            }
        }
        let output = self
            .base
            .probe_output(&mut cmd)
            .map_err(|error| DockerError::InspectFailed(error.to_string()))?;
        if !output.status.success() {
            return Err(DockerError::InspectFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
                Ok((!value.is_empty()).then_some(value))
            }
            RuntimeKind::AppleContainer => {
                let payload: Value = serde_json::from_slice(&output.stdout)
                    .map_err(|error| DockerError::InspectFailed(error.to_string()))?;
                Ok(Self::apple_container_inspect_label(&payload, key)?.map(str::to_owned))
            }
        }
    }

    /// Read actual runtime configuration, not desired create arguments.
    pub(crate) fn inspect_container(&self, name_or_id: &str) -> Result<Option<InspectedContainer>> {
        let mut command = self.base.command();
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                command.args(["container", "inspect", name_or_id]);
            }
            RuntimeKind::AppleContainer => {
                command.args(["inspect", name_or_id]);
            }
        }
        let output = self.base.probe_output(&mut command)?;
        if !output.status.success() {
            return Err(DockerError::InspectFailed(
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        let value: Value = serde_json::from_slice(&output.stdout)
            .map_err(|error| DockerError::InspectFailed(error.to_string()))?;
        let mut inspected = match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => Self::parse_inspected_container(&value)?,
            RuntimeKind::AppleContainer => Self::parse_apple_inspected_container(&value)?,
        };
        if matches!(self.kind, RuntimeKind::Docker | RuntimeKind::Podman) {
            Self::resolve_docker_volume_mounts(&mut inspected, |name| {
                let mut command = self.base.command();
                command.args(["volume", "inspect", name]);
                let output = self.base.probe_output(&mut command)?;
                if !output.status.success() {
                    return Err(DockerError::InspectFailed(
                        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
                    ));
                }
                serde_json::from_slice(&output.stdout)
                    .map_err(|error| DockerError::InspectFailed(error.to_string()))
            })?;
        }
        Ok(Some(inspected))
    }

    fn parse_inspected_container(value: &Value) -> Result<InspectedContainer> {
        let malformed = || {
            DockerError::InspectFailed(
                "container inspect returned malformed identity, state or mounts".into(),
            )
        };
        let entries = value
            .as_array()
            .filter(|entries| entries.len() == 1)
            .ok_or_else(malformed)?;
        let container = &entries[0];
        let id = container
            .get("Id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(malformed)?
            .to_owned();
        let running = container
            .pointer("/State/Running")
            .and_then(Value::as_bool)
            .ok_or_else(malformed)?;
        let mounts = container
            .get("Mounts")
            .and_then(Value::as_array)
            .ok_or_else(malformed)?;
        let mut bind_mounts = Vec::new();
        let mut opaque_mounts = Vec::new();
        for mount in mounts {
            let kind = mount
                .get("Type")
                .and_then(Value::as_str)
                .filter(|kind| !kind.is_empty())
                .ok_or_else(malformed)?;
            let target = mount
                .get("Destination")
                .and_then(Value::as_str)
                .filter(|path| std::path::Path::new(path).is_absolute())
                .ok_or_else(malformed)?;
            let writable = mount
                .get("RW")
                .and_then(Value::as_bool)
                .ok_or_else(malformed)?;
            let source = match mount.get("Source") {
                None | Some(Value::Null) => None,
                Some(Value::String(source)) => Some(source.clone()),
                _ => return Err(malformed()),
            };
            let name = match mount.get("Name") {
                None | Some(Value::Null) => None,
                Some(Value::String(name)) if !name.trim().is_empty() => Some(name.clone()),
                _ => return Err(malformed()),
            };
            if kind == "bind" {
                let source = source
                    .filter(|path| std::path::Path::new(path).is_absolute())
                    .ok_or_else(malformed)?;
                bind_mounts.push(VolumeMount {
                    host_path: source,
                    container_path: target.to_owned(),
                    read_only: !writable,
                });
            } else {
                opaque_mounts.push(InspectedMount {
                    kind: kind.to_owned(),
                    name,
                    source,
                    container_path: target.into(),
                    read_only: !writable,
                });
            }
        }
        Ok(InspectedContainer {
            id,
            running,
            bind_mounts,
            ordinary_mounts: Vec::new(),
            opaque_mounts,
            runtime_handler: None,
        })
    }

    fn resolve_docker_volume_mounts(
        inspected: &mut InspectedContainer,
        mut inspect_volume: impl FnMut(&str) -> Result<Value>,
    ) -> Result<()> {
        let malformed = || {
            DockerError::InspectFailed("volume inspect returned malformed driver or options".into())
        };
        let mut unresolved = Vec::new();
        for mount in std::mem::take(&mut inspected.opaque_mounts) {
            if mount.kind != "volume" {
                unresolved.push(mount);
                continue;
            }
            let Some(name) = mount.name.as_deref() else {
                unresolved.push(mount);
                continue;
            };
            let payload = inspect_volume(name)?;
            let entries = payload
                .as_array()
                .filter(|entries| entries.len() == 1)
                .ok_or_else(malformed)?;
            let volume = &entries[0];
            let driver = volume
                .get("Driver")
                .and_then(Value::as_str)
                .filter(|driver| !driver.is_empty())
                .ok_or_else(malformed)?;
            let options = match volume.get("Options") {
                None | Some(Value::Null) => None,
                Some(Value::Object(options)) => Some(options),
                _ => return Err(malformed()),
            };
            if driver == "local" && options.is_none_or(serde_json::Map::is_empty) {
                inspected.ordinary_mounts.push(mount);
                continue;
            }
            let host_device = options.and_then(|options| {
                (driver == "local"
                    && options.len() == 3
                    && options.get("type").and_then(Value::as_str) == Some("none")
                    && options.get("o").and_then(Value::as_str) == Some("bind"))
                .then(|| options.get("device").and_then(Value::as_str))
                .flatten()
                .filter(|device| std::path::Path::new(device).is_absolute())
            });
            if let Some(device) = host_device {
                inspected.bind_mounts.push(VolumeMount {
                    host_path: device.to_owned(),
                    container_path: mount.container_path.to_string_lossy().into_owned(),
                    read_only: mount.read_only,
                });
            } else {
                unresolved.push(mount);
            }
        }
        inspected.opaque_mounts = unresolved;
        Ok(())
    }

    fn parse_apple_inspected_container(value: &Value) -> Result<InspectedContainer> {
        // apple/container 1.0.0 ContainerConfiguration, Filesystem and
        // ProcessConfiguration Codable fields. FSType encodes as one enum key.
        let malformed = || {
            DockerError::InspectFailed("Apple container inspect returned malformed mounts".into())
        };
        let entries = value
            .as_array()
            .filter(|entries| entries.len() == 1)
            .ok_or_else(malformed)?;
        let configuration = entries[0]
            .get("configuration")
            .and_then(Value::as_object)
            .ok_or_else(malformed)?;
        let id = Self::apple_container_inspect_id(value)?.to_owned();
        let running = Self::apple_container_inspect_state(value)? == "running";
        let mounts = configuration
            .get("mounts")
            .and_then(Value::as_array)
            .ok_or_else(malformed)?;
        let mut bind_mounts = Vec::new();
        let mut opaque_mounts = Vec::new();
        for mount in mounts {
            let kind = mount
                .get("type")
                .and_then(Value::as_object)
                .filter(|kind| kind.len() == 1)
                .ok_or_else(malformed)?;
            let target = mount
                .get("destination")
                .and_then(Value::as_str)
                .filter(|path| std::path::Path::new(path).is_absolute())
                .ok_or_else(malformed)?;
            let options = mount
                .get("options")
                .and_then(Value::as_array)
                .ok_or_else(malformed)?;
            if options.iter().any(|option| !option.is_string()) {
                return Err(malformed());
            }
            if let Some(variant) = kind.get("virtiofs") {
                if !variant.as_object().is_some_and(|object| object.is_empty()) {
                    return Err(malformed());
                }
                let source = mount
                    .get("source")
                    .and_then(Value::as_str)
                    .filter(|path| std::path::Path::new(path).is_absolute())
                    .ok_or_else(malformed)?;
                bind_mounts.push(VolumeMount {
                    host_path: source.to_owned(),
                    container_path: target.to_owned(),
                    read_only: options.iter().any(|option| option.as_str() == Some("ro")),
                });
            } else {
                opaque_mounts.push(InspectedMount {
                    kind: kind.keys().next().expect("one mount kind").clone(),
                    name: None,
                    source: mount
                        .get("source")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    container_path: target.into(),
                    read_only: options.iter().any(|option| option.as_str() == Some("ro")),
                });
            }
        }
        let runtime_handler = match configuration.get("runtimeHandler") {
            None | Some(Value::Null) => "container-runtime-linux",
            Some(Value::String(handler)) if !handler.is_empty() => handler,
            _ => return Err(malformed()),
        };
        Ok(InspectedContainer {
            id,
            running,
            bind_mounts,
            ordinary_mounts: Vec::new(),
            opaque_mounts,
            runtime_handler: Some(runtime_handler.to_owned()),
        })
    }

    pub fn container_working_dir(&self, name: &str) -> Option<String> {
        if !matches!(self.kind, RuntimeKind::Docker | RuntimeKind::Podman) {
            return None;
        }
        let mut cmd = self.base.command();
        cmd.args(["container", "inspect", "-f", "{{.Config.WorkingDir}}", name]);
        let output = self.base.probe_output(&mut cmd).ok()?;
        if !output.status.success() {
            return None;
        }
        let wd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!wd.is_empty()).then_some(wd)
    }

    /// `Ok(None)` means the runtime carries no labels, so the caller cannot tell.
    fn label_matches(
        &self,
        name: &str,
        key: &str,
        predicate: impl FnOnce(Option<&str>) -> bool,
    ) -> Result<Option<bool>> {
        if !self.base.supports_labels {
            return Ok(None);
        }
        let value = self.inspect_container_label(name, key)?;
        Ok(Some(predicate(value.as_deref())))
    }

    pub fn sandbox_store_generation_matches(&self, name: &str) -> Result<Option<bool>> {
        self.label_matches(
            name,
            "com.agent-of-empires.sandbox-store-generation",
            |value| value == Some("2"),
        )
    }

    pub fn agent_tool_matches(&self, name: &str, identity: &str) -> Result<Option<bool>> {
        self.label_matches(
            name,
            crate::containers::container_interface::AGENT_TOOL_LABEL,
            |value| value.is_none_or(|value| value == identity),
        )
    }

    pub fn shared_credential_mounts_match(
        &self,
        name: &str,
        config: &ContainerConfig,
    ) -> Result<Option<bool>> {
        if config.shared_credential_mounts.is_empty() {
            return Ok(Some(true));
        }
        let expected = config.shared_credential_label();
        self.label_matches(
            name,
            crate::containers::container_interface::SHARED_CREDENTIAL_MOUNTS_LABEL,
            |value| value == Some(expected.as_str()),
        )
    }

    pub fn carries_shared_credential_label(&self, name: &str) -> Result<Option<bool>> {
        self.label_matches(
            name,
            crate::containers::container_interface::SHARED_CREDENTIAL_MOUNTS_LABEL,
            |value| value.is_some(),
        )
    }

    pub fn mount_fingerprint_matches(&self, name: &str, expected: &str) -> Result<Option<bool>> {
        self.label_matches(name, "com.agent-of-empires.mount-fingerprint", |value| {
            value == Some(expected)
        })
    }

    pub fn build_create_args(
        &self,
        name: &str,
        image: &str,
        config: &ContainerConfig,
    ) -> Vec<String> {
        self.base.build_create_args(name, image, config)
    }

    pub fn create_container(
        &self,
        name: &str,
        image: &str,
        config: &ContainerConfig,
    ) -> Result<String> {
        if self.does_container_exist(name)? {
            return Err(DockerError::ContainerAlreadyExists(name.to_string()));
        }
        match self.base.run_create(name, image, config) {
            Err(error) if is_runtime_timeout(&error) => match self.inspect_container_id(name) {
                Ok(id) => Ok(id),
                Err(_) => Err(error),
            },
            result => result,
        }
    }

    pub fn start_container(&self, name: &str) -> Result<()> {
        match self.base.start_container(name) {
            Err(error) if is_runtime_timeout(&error) => match self.is_container_running(name) {
                Ok(true) => Ok(()),
                _ => Err(error),
            },
            result => result,
        }
    }

    pub fn stop_container(&self, name: &str) -> Result<()> {
        match self.base.stop_container(name) {
            Err(error) if is_runtime_timeout(&error) => match self.is_container_running(name) {
                Ok(false) => Ok(()),
                _ => Err(error),
            },
            result => result,
        }
    }

    pub fn remove(&self, name: &str, force: bool) -> Result<()> {
        match self.base.remove(name, force) {
            Err(error) if is_runtime_timeout(&error) => match self.does_container_exist(name) {
                Ok(false) => Ok(()),
                _ => Err(error),
            },
            result => result,
        }
    }

    pub fn exec_command(&self, name: &str, options: Option<&str>, cmd: &str) -> String {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => self.base.exec_command(name, options, cmd),
            RuntimeKind::AppleContainer => {
                // Apple Container's initial PATH is minimal, so wrap in `/bin/sh -c`.
                let escaped = cmd.replace('\'', "'\\''");
                let cmd_str = format!("'{}'", escaped);

                if let Some(opt_str) = options {
                    [
                        "container",
                        "exec",
                        "-it",
                        opt_str,
                        name,
                        "/bin/sh",
                        "-c",
                        &cmd_str,
                    ]
                    .join(" ")
                } else {
                    ["container", "exec", "-it", name, "/bin/sh", "-c", &cmd_str].join(" ")
                }
            }
        }
    }

    pub fn build_exec_argv(&self, name: &str, workdir: &str, cmd: &[String]) -> Vec<String> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                self.base.build_exec_argv(name, workdir, cmd)
            }
            RuntimeKind::AppleContainer => {
                // `exec "$@"` supplies PATH without shell-parsing the untrusted arguments.
                let mut argv = self.base.build_exec_argv(name, workdir, &[]);
                argv.extend([
                    "/bin/sh".to_string(),
                    "-c".to_string(),
                    "exec \"$@\"".to_string(),
                    "sh".to_string(),
                ]);
                argv.extend(cmd.iter().cloned());
                argv
            }
        }
    }

    pub fn exec(&self, name: &str, cmd: &[&str]) -> Result<std::process::Output> {
        self.base.exec(name, cmd)
    }

    pub fn batch_running_states(&self, prefix: &str) -> HashMap<String, bool> {
        self.batch_container_states(prefix)
            .into_iter()
            .map(|(name, state)| (name, state == ContainerState::Running))
            .collect()
    }

    /// Empty when the runtime cannot list, so an absent name means unknown, not stopped.
    pub fn batch_container_states(&self, prefix: &str) -> HashMap<String, ContainerState> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                let mut cmd = self.base.command();
                cmd.args([
                    "ps",
                    "-a",
                    "--filter",
                    &format!("name={}", prefix),
                    "--format",
                    "{{.Names}}\t{{.State}}",
                ]);
                let output = match self.base.probe_output(&mut cmd) {
                    Ok(output) if output.status.success() => output,
                    _ => return HashMap::new(),
                };
                parse_batch_states(&String::from_utf8_lossy(&output.stdout), prefix)
            }
            RuntimeKind::AppleContainer => {
                let _ = prefix;
                HashMap::new()
            }
        }
    }

    /// Slow: the runtime samples every container twice; use `stats::cached_stats`.
    pub fn batch_stats(&self, prefix: &str) -> super::stats::StatsMap {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                // `stats` has no `--filter`, and naming a gone container fails the whole call.
                let mut cmd = self.base.command();
                cmd.args([
                    "stats",
                    "--no-stream",
                    "--format",
                    "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.PIDs}}",
                ]);
                let output = match self.base.probe_output(&mut cmd) {
                    Ok(output) if output.status.success() => output,
                    _ => return super::stats::StatsMap::new(),
                };

                super::stats::parse_stats_output(&String::from_utf8_lossy(&output.stdout), prefix)
            }
            RuntimeKind::AppleContainer => {
                let _ = prefix;
                super::stats::StatsMap::new()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerState {
    Running,
    Paused,
    Restarting,
    Created,
    Exited,
    Dead,
    Other,
}

impl ContainerState {
    pub fn from_listing(state: &str) -> Self {
        match state {
            "running" => Self::Running,
            "paused" => Self::Paused,
            "restarting" => Self::Restarting,
            "created" => Self::Created,
            "exited" => Self::Exited,
            "dead" => Self::Dead,
            _ => Self::Other,
        }
    }

    /// Moby keeps `State.Running` true while paused or restarting.
    pub fn is_live(self) -> Option<bool> {
        match self {
            Self::Running | Self::Paused | Self::Restarting => Some(true),
            Self::Created | Self::Exited | Self::Dead => Some(false),
            Self::Other => None,
        }
    }
}

/// `--filter name=` matches substrings, so names are post-filtered to the exact prefix.
fn parse_batch_states(stdout: &str, prefix: &str) -> HashMap<String, ContainerState> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(2, '\t');
            let name = parts.next()?.trim();
            let state = parts.next()?.trim();
            if name.is_empty() || !name.starts_with(prefix) {
                return None;
            }
            Some((name.to_string(), ContainerState::from_listing(state)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::EnvGuard;

    #[test]
    fn batch_listing_states_keep_inspect_liveness() {
        let listing = "aoe-sandbox-a\trunning\n\
                       aoe-sandbox-b\tpaused\n\
                       aoe-sandbox-c\trestarting\n\
                       aoe-sandbox-d\texited\n\
                       aoe-sandbox-e\tcreated\n\
                       aoe-sandbox-f\tdead\n\
                       aoe-sandbox-g\tremoving\n\
                       aoe-sandbox-h\tsomething-new\n\
                       other-aoe-sandbox-i\trunning\n\
                       aoe-sandbox-j\n";
        let states = parse_batch_states(listing, "aoe-sandbox-");
        let cases = [
            ("aoe-sandbox-a", ContainerState::Running, Some(true)),
            ("aoe-sandbox-b", ContainerState::Paused, Some(true)),
            ("aoe-sandbox-c", ContainerState::Restarting, Some(true)),
            ("aoe-sandbox-d", ContainerState::Exited, Some(false)),
            ("aoe-sandbox-e", ContainerState::Created, Some(false)),
            ("aoe-sandbox-f", ContainerState::Dead, Some(false)),
            ("aoe-sandbox-g", ContainerState::Other, None),
            ("aoe-sandbox-h", ContainerState::Other, None),
        ];
        assert_eq!(states.len(), cases.len(), "{states:?}");
        for (name, state, live) in cases {
            assert_eq!(states[name], state, "{name}");
            assert_eq!(state.is_live(), live, "{name}");
        }
        let running: HashMap<String, bool> = states
            .iter()
            .map(|(name, state)| (name.clone(), *state == ContainerState::Running))
            .collect();
        assert!(running["aoe-sandbox-a"]);
        assert!(!running["aoe-sandbox-b"]);
    }
    #[test]
    fn runtime_timeout_classification_matches_io_error_kind() {
        let cases = [
            (std::io::ErrorKind::TimedOut, true),
            (std::io::ErrorKind::Other, false),
        ];
        for (kind, expected) in cases {
            let error = DockerError::IoError(std::io::Error::from(kind));
            assert_eq!(is_runtime_timeout(&error), expected, "{kind:?}");
        }
    }

    #[test]
    fn docker_volume_inspection_classifies_host_exposure_fail_closed() {
        let mount = |name: &str, destination: &str| {
            serde_json::json!({
                "Type": "volume",
                "Name": name,
                "Source": format!("/var/lib/docker/volumes/{name}/_data"),
                "Destination": destination,
                "RW": true,
            })
        };
        let payload = serde_json::json!([{
            "Id": "container-id",
            "State": {"Running": true},
            "Mounts": [
                mount("ordinary-empty", "/cache-empty"),
                mount("ordinary-null", "/cache-null"),
                mount("host-bind", "/host-data"),
                mount("plugin", "/plugin-data"),
                mount("unknown-options", "/unknown-data"),
            ],
        }]);
        let mut inspected = ContainerRuntime::parse_inspected_container(&payload).unwrap();
        ContainerRuntime::resolve_docker_volume_mounts(&mut inspected, |name| {
            Ok(match name {
                "ordinary-empty" => serde_json::json!([{"Driver": "local", "Options": {}}]),
                "ordinary-null" => serde_json::json!([{"Driver": "local", "Options": null}]),
                "host-bind" => serde_json::json!([{
                    "Driver": "local",
                    "Options": {"type": "none", "o": "bind", "device": "/srv/data"},
                }]),
                "plugin" => serde_json::json!([{"Driver": "cloud-plugin", "Options": null}]),
                "unknown-options" => serde_json::json!([{
                    "Driver": "local",
                    "Options": {"type": "none", "o": "bind", "device": "relative"},
                }]),
                _ => unreachable!(),
            })
        })
        .unwrap();

        assert_eq!(
            inspected
                .ordinary_mounts
                .iter()
                .filter_map(|mount| mount.name.as_deref())
                .collect::<Vec<_>>(),
            ["ordinary-empty", "ordinary-null"]
        );
        assert!(inspected.bind_mounts.iter().any(|mount| {
            mount.host_path == "/srv/data" && mount.container_path == "/host-data"
        }));
        assert_eq!(
            inspected
                .opaque_mounts
                .iter()
                .filter_map(|mount| mount.name.as_deref())
                .collect::<Vec<_>>(),
            ["plugin", "unknown-options"]
        );
    }

    /// Every runtime installed and running on this host; empty in most CI images.
    /// The guard keeps tests that put a fake `docker` on `PATH` from swapping the binary mid-probe.
    fn available_runtimes() -> (EnvGuard, Vec<ContainerRuntime>) {
        let env = EnvGuard::read_lock();
        let runtimes = [
            ContainerRuntime::docker(),
            ContainerRuntime::apple_container(),
            ContainerRuntime::podman(),
        ]
        .into_iter()
        .filter(|rt| rt.is_available() && rt.is_daemon_running())
        .collect();
        (env, runtimes)
    }

    const MISSING_IMAGE: &str = "nonexistent-image-that-does-not-exist:v999";

    #[test]
    fn image_exists_locally_and_ensure_image_reject_a_missing_image() {
        let (_env, runtimes) = available_runtimes();
        for rt in runtimes {
            assert!(!rt.image_exists_locally(MISSING_IMAGE));
            assert!(rt.ensure_image(MISSING_IMAGE).is_err());
        }
    }

    #[test]
    fn test_apple_container_inspect_shapes() {
        use serde_json::json;
        let cases = [
            (json!([{"status": "running"}]), Some("running")),
            (json!([{"status": "stopped"}]), Some("stopped")),
            (
                json!([{"status": {"state": "running", "networks": [], "startedDate": "2026-08-04T10:36:08Z"}}]),
                Some("running"),
            ),
            (json!([{"status": {"state": "stopped"}}]), Some("stopped")),
            (json!([{"status": {"state": 3}}]), None),
            (json!([{"status": {"phase": "running"}}]), None),
            (json!([{"status": 3}]), None),
            (json!([{"status": null}]), None),
            (json!([{}]), None),
            (json!([]), None),
        ];
        for (payload, expected) in cases {
            let result = ContainerRuntime::apple_container_inspect_state(&payload);
            match expected {
                Some(state) => {
                    let parsed = result.unwrap_or_else(|e| panic!("{payload}: {e}"));
                    assert_eq!(parsed, state);
                }
                None => assert!(result.is_err(), "expected Err for {payload}"),
            }
        }

        let id_cases = [
            (json!([{"id": "new-id"}]), Some("new-id")),
            (
                json!([{"configuration": {"id": "legacy-id"}}]),
                Some("legacy-id"),
            ),
            (json!([{"id": ""}]), None),
            (json!([{"id": "  trimmed-id  "}]), Some("trimmed-id")),
            (json!([{"id": "   "}]), None),
            (json!([{"id": 3}]), None),
            (json!([{}]), None),
            (json!([]), None),
        ];
        for (payload, expected) in id_cases {
            let result = ContainerRuntime::apple_container_inspect_id(&payload);
            match expected {
                Some(id) => assert_eq!(
                    result.unwrap_or_else(|error| panic!("{payload}: {error}")),
                    id
                ),
                None => assert!(result.is_err(), "expected Err for {payload}"),
            }
        }

        let key = "com.agent-of-empires.mount-fingerprint";
        let payload = json!([{"configuration": {"labels": {(key): "expected"}}}]);
        assert_eq!(
            ContainerRuntime::apple_container_inspect_label(&payload, key).unwrap(),
            Some("expected")
        );
        for payload in [json!([{}]), json!([{"configuration": {"labels": {}}}])] {
            assert_eq!(
                ContainerRuntime::apple_container_inspect_label(&payload, key).unwrap(),
                None
            );
        }
        for payload in [
            json!([{"configuration": {"labels": []}}]),
            json!([{"configuration": {"labels": {(key): 3}}}]),
        ] {
            assert!(
                ContainerRuntime::apple_container_inspect_label(&payload, key).is_err(),
                "expected Err for {payload}"
            );
        }
    }

    #[test]
    fn exec_command_per_runtime() {
        let cmd = ContainerRuntime::apple_container().exec_command(
            "aoe-sandbox-test1234",
            None,
            "printf ok",
        );
        assert_eq!(
            cmd,
            "container exec -it aoe-sandbox-test1234 /bin/sh -c 'printf ok'"
        );
        assert_eq!(
            ContainerRuntime::podman().exec_command("aoe-sandbox-test1234", None, "claude"),
            "podman exec -it aoe-sandbox-test1234 claude"
        );
    }

    fn oneshot_argv() -> Vec<String> {
        [
            "claude",
            "-p",
            "--model",
            "haiku",
            "name this: `rm -rf /` $(id) \"quoted\" 'single'\nsecond line",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    #[test]
    fn build_exec_argv_docker_and_podman_are_non_interactive_with_workdir() {
        let rt = ContainerRuntime::docker();
        let argv = rt.build_exec_argv("aoe-sandbox-test1234", "/workspace", &oneshot_argv());
        let mut expected = vec![
            "docker".to_string(),
            "exec".to_string(),
            "-w".to_string(),
            "/workspace".to_string(),
            "aoe-sandbox-test1234".to_string(),
        ];
        expected.extend(oneshot_argv());
        assert_eq!(argv, expected);

        let argv = ContainerRuntime::podman().build_exec_argv(
            "aoe-sandbox-test1234",
            "/workspace",
            &oneshot_argv(),
        );
        assert_eq!(
            &argv[..5],
            &["podman", "exec", "-w", "/workspace", "aoe-sandbox-test1234"]
        );
        assert_eq!(&argv[5..], &oneshot_argv()[..]);
    }

    #[test]
    fn build_exec_argv_apple_container_wraps_for_path_without_splicing() {
        let argv = ContainerRuntime::apple_container().build_exec_argv(
            "aoe-sandbox-test1234",
            "/workspace",
            &oneshot_argv(),
        );
        assert_eq!(
            &argv[..9],
            &[
                "container",
                "exec",
                "-w",
                "/workspace",
                "aoe-sandbox-test1234",
                "/bin/sh",
                "-c",
                "exec \"$@\"",
                "sh",
            ]
        );
        assert_eq!(&argv[9..], &oneshot_argv()[..]);

        let printf = ["/usr/bin/printf", "/bin/printf"]
            .into_iter()
            .find(|candidate| std::path::Path::new(candidate).exists())
            .expect("an absolute printf to prove PATH independence with");
        let executable = ContainerRuntime::apple_container().build_exec_argv(
            "aoe-sandbox-test1234",
            "",
            &[printf.to_string(), "PATH_INDEPENDENT".to_string()],
        );
        let output = std::process::Command::new(&executable[3])
            .args(&executable[4..])
            .env_clear()
            .env("PATH", "/definitely-missing")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{printf} must run with no usable PATH, got {output:?}"
        );
        assert_eq!(output.stdout, b"PATH_INDEPENDENT");
    }

    #[test]
    fn build_exec_argv_never_requests_a_tty() {
        for rt in [
            ContainerRuntime::docker(),
            ContainerRuntime::podman(),
            ContainerRuntime::apple_container(),
        ] {
            let argv = rt.build_exec_argv("aoe-sandbox-test1234", "/workspace", &oneshot_argv());
            assert!(
                !argv.iter().any(|a| a == "-it" || a == "-i" || a == "-t"),
                "{:?} requested a tty: {argv:?}",
                rt.kind
            );
        }
    }

    #[test]
    fn build_exec_argv_omits_workdir_when_empty() {
        let argv = ContainerRuntime::docker().build_exec_argv(
            "aoe-sandbox-test1234",
            "",
            &["claude".to_string()],
        );
        assert_eq!(argv, ["docker", "exec", "aoe-sandbox-test1234", "claude"]);
    }
}
