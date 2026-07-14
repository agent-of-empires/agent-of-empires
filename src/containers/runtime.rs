//! The unified `ContainerRuntime`. Shared behavior lives on `RuntimeBase`;
//! this impl dispatches the four genuinely runtime-specific operations
//! (existence probe, running-state probe, exec-command formatting, and
//! batch status query) on a `RuntimeKind` discriminant.

use std::collections::HashMap;

use serde_json::Value;

use super::container_interface::ContainerConfig;
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
                // `RepoDigests` holds `repo@sha256:...` entries for the pulled
                // manifest. One subprocess, newline-joined so a multi-registry
                // image still lets us pick the entry matching this reference.
                let output = self
                    .base
                    .command()
                    .args([
                        "image",
                        "inspect",
                        "--format",
                        "{{range .RepoDigests}}{{println .}}{{end}}",
                        image,
                    ])
                    .output()
                    .ok()?;
                if !output.status.success() {
                    return None;
                }
                let stdout = String::from_utf8_lossy(&output.stdout);
                super::image_update::pick_repo_digest(image, &stdout)
            }
            // Apple Container's `image inspect` doesn't expose a Docker-style
            // repo digest; skip the staleness check there rather than guess.
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
                // `container inspect` (not `docker inspect`): pins the stderr
                // wording DOCKER_MISSING captures in the runtime_base tests,
                // so is_not_found classifies "absent" cleanly. Changing this
                // argv or the per-runtime not_found / daemon_down /
                // permission_denied markers without new fixtures silently
                // breaks the classifier. See is_container_running above and
                // the pinning comment at #2596 / #2652.
                let mut cmd = self.base.command();
                cmd.args(["container", "inspect", name]);
                let output = self.base.probe_output(&mut cmd)?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_exists_failure(&stderr);
                }
                Ok(true)
            }
            RuntimeKind::AppleContainer => {
                // Apple Container's `inspect` returns success(0) for
                // non-existent containers, so we use `logs` which properly
                // fails for missing containers. APPLE_MISSING captures
                // Apple's absent-container stderr (from `rm/delete`);
                // not_found_markers / daemon_down_markers /
                // permission_denied_markers on RuntimeBase::APPLE_CONTAINER
                // key off Apple's not-found style and are expected to match
                // `logs` stderr by substring, though `logs` stderr has not
                // been captured as a fixture. Switching argv here (or
                // tightening the markers) needs new fixtures. Same
                // silent-break risk as the Docker/Podman pinning comment
                // above. See #2596.
                // TODO: verify Apple `container logs` semantics on
                //       stopped-but-existing containers (cf. #2730 for
                //       fixture capture).
                let mut cmd = self.base.command();
                cmd.args(["logs", name]);
                let output = self.base.probe_output(&mut cmd)?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_exists_failure(&stderr);
                }
                Ok(true)
            }
        }
    }

    pub fn is_container_running(&self, name: &str) -> Result<bool> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                // `container inspect` (not the shorter `docker inspect`): the two
                // subcommands emit different stderr for a missing container
                // ("No such container" vs "No such object"), and DOCKER_MISSING
                // in the runtime_base tests pins the former. Changing this argv
                // silently breaks is_not_found classification. See #2596.
                let mut cmd = self.base.command();
                cmd.args(["container", "inspect", "-f", "{{.State.Running}}", name]);
                let output = self.base.probe_output(&mut cmd)?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_inspect_failure(&stderr);
                }

                let stdout = String::from_utf8_lossy(&output.stdout);
                Ok(stdout.trim() == "true")
            }
            RuntimeKind::AppleContainer => {
                // Apple's `container inspect` is the only inspect subcommand
                // (no `container container inspect`), but the stderr wording
                // is pinned in RuntimeBase::APPLE_CONTAINER.not_found_markers
                // (`container with id`) and daemon_down_markers. Do not
                // tighten this argv or those markers without capturing new
                // fixtures; same silent-break risk as the Docker/Podman
                // comment above. See #2596.
                let mut cmd = self.base.command();
                cmd.args(["inspect", name]);
                let output = self.base.probe_output(&mut cmd)?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_inspect_failure(&stderr);
                }

                let out_json: Value = serde_json::from_slice(&output.stdout)
                    // serde_json::Error::Display is single-line by construction
                    // (format is "<code> at line N column M"); no sanitize_stderr
                    // wrapping needed to preserve the single-line convention.
                    .map_err(|e| DockerError::InspectFailed(e.to_string()))?;

                if let Some(status) = out_json.pointer("/0/status") {
                    // as_str() guard: if Apple ever changes /0/status from a
                    // string to a nested object, `status == "running"` would
                    // silently return false and route to Probe::NotRunning:
                    // exact fail-open swallowing-existence-probe (#2596) one
                    // JSON schema shift away. Surface schema drift as Err.
                    match status.as_str() {
                        Some(s) => Ok(s == "running"),
                        None => Err(DockerError::InspectFailed(
                            "apple container inspect: /0/status present but not a string".into(),
                        )),
                    }
                } else {
                    // Exit 0 with no /0/status: schema surprise. Same
                    // reasoning as the as_str() None branch above: Err,
                    // not Ok(false), so gates fail closed instead of fail
                    // open on a genuinely running container.
                    Err(DockerError::InspectFailed(
                        "apple container inspect: exit 0 but no /0/status in output".into(),
                    ))
                }
            }
        }
    }

    /// The container's configured working directory (`Config.WorkingDir`), or
    /// `None` if it can't be determined (container gone, inspect failed, or the
    /// field is empty). Used to backfill the create-time-pinned workdir for
    /// sandbox sessions that predate it (#2414). Works on stopped containers
    /// too, since `inspect` reads static config.
    ///
    /// Apple's `container` CLI does not expose this via a stable `inspect`
    /// field we rely on, so it returns `None` there and the caller keeps the
    /// create-time value (or the live fallback for legacy sessions).
    pub fn container_working_dir(&self, name: &str) -> Option<String> {
        if !matches!(self.kind, RuntimeKind::Docker | RuntimeKind::Podman) {
            return None;
        }
        let output = self
            .base
            .command()
            .args(["container", "inspect", "-f", "{{.Config.WorkingDir}}", name])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let wd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!wd.is_empty()).then_some(wd)
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
        self.base.run_create(name, image, config)
    }

    pub fn start_container(&self, name: &str) -> Result<()> {
        self.base.start_container(name)
    }

    pub fn stop_container(&self, name: &str) -> Result<()> {
        self.base.stop_container(name)
    }

    pub fn remove(&self, name: &str, force: bool) -> Result<()> {
        self.base.remove(name, force)
    }

    pub fn exec_command(&self, name: &str, options: Option<&str>, cmd: &str) -> String {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                // Docker/Podman containers inherit a full PATH, so the command
                // can be appended directly without wrapping in `sh -c`.
                self.base.exec_command(name, options, cmd)
            }
            RuntimeKind::AppleContainer => {
                // Apple Container has a very limited initial PATH, so we wrap
                // the command in `sh -c` to get a proper shell environment.
                // Single-quote with escaped embedded quotes to avoid issues
                // with double-quote metacharacters ($, `, \, !) in the command.
                let escaped = cmd.replace('\'', "'\\''");
                let cmd_str = format!("'{}'", escaped);

                if let Some(opt_str) = options {
                    [
                        "container",
                        "exec",
                        "-it",
                        opt_str,
                        name,
                        "sh",
                        "-c",
                        &cmd_str,
                    ]
                    .join(" ")
                } else {
                    ["container", "exec", "-it", name, "sh", "-c", &cmd_str].join(" ")
                }
            }
        }
    }

    pub fn exec(&self, name: &str, cmd: &[&str]) -> Result<std::process::Output> {
        self.base.exec(name, cmd)
    }

    pub fn batch_running_states(&self, prefix: &str) -> HashMap<String, bool> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                let output = self
                    .base
                    .command()
                    .args([
                        "ps",
                        "-a",
                        "--filter",
                        &format!("name={}", prefix),
                        "--format",
                        "{{.Names}}\t{{.State}}",
                    ])
                    .output();

                let output = match output {
                    Ok(o) if o.status.success() => o,
                    _ => return HashMap::new(),
                };

                let stdout = String::from_utf8_lossy(&output.stdout);
                stdout
                    .lines()
                    .filter_map(|line| {
                        let mut parts = line.splitn(2, '\t');
                        let name = parts.next()?.trim();
                        let state = parts.next()?.trim();
                        // Docker/Podman's --filter name= does substring matching, so
                        // post-filter to ensure we only include exact prefix matches.
                        if name.is_empty() || !name.starts_with(prefix) {
                            return None;
                        }
                        Some((name.to_string(), state == "running"))
                    })
                    .collect()
            }
            RuntimeKind::AppleContainer => {
                let _ = prefix;
                HashMap::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn docker_if_available() -> Option<ContainerRuntime> {
        let rt = ContainerRuntime::docker();
        if !rt.is_available() || !rt.is_daemon_running() {
            None
        } else {
            Some(rt)
        }
    }

    fn apple_container_if_available() -> Option<ContainerRuntime> {
        let rt = ContainerRuntime::apple_container();
        if !rt.is_available() || !rt.is_daemon_running() {
            None
        } else {
            Some(rt)
        }
    }

    fn podman_if_available() -> Option<ContainerRuntime> {
        let rt = ContainerRuntime::podman();
        if !rt.is_available() || !rt.is_daemon_running() {
            None
        } else {
            Some(rt)
        }
    }

    // Pulls `hello-world` from a live registry, so it flakes on a transient
    // pull failure or network hang in CI. Per the Docker-test convention,
    // gate it behind `#[ignore]` so it only runs when explicitly requested.
    #[test]
    #[ignore = "pulls hello-world from a live registry; run with --ignored"]
    fn test_image_exists_locally_with_common_image() {
        for rt in [
            docker_if_available(),
            apple_container_if_available(),
            podman_if_available(),
        ]
        .into_iter()
        .flatten()
        {
            rt.pull_image("hello-world").unwrap();
            assert!(rt.image_exists_locally("hello-world"));
        }
    }

    #[test]
    fn test_image_exists_locally_nonexistent() {
        for rt in [
            docker_if_available(),
            apple_container_if_available(),
            podman_if_available(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(!rt.image_exists_locally("nonexistent-image-that-does-not-exist:v999"));
        }
    }

    // Pulls `hello-world` from a live registry; same flake risk as
    // `test_image_exists_locally_with_common_image`, so gate it the same way.
    #[test]
    #[ignore = "pulls hello-world from a live registry; run with --ignored"]
    fn test_ensure_image_uses_local_image() {
        for rt in [
            docker_if_available(),
            apple_container_if_available(),
            podman_if_available(),
        ]
        .into_iter()
        .flatten()
        {
            rt.pull_image("hello-world").unwrap();
            assert!(rt.ensure_image("hello-world").is_ok());
        }
    }

    #[test]
    fn test_ensure_image_fails_for_nonexistent_remote() {
        for rt in [
            docker_if_available(),
            apple_container_if_available(),
            podman_if_available(),
        ]
        .into_iter()
        .flatten()
        {
            assert!(rt
                .ensure_image("nonexistent-image-that-does-not-exist:v999")
                .is_err());
        }
    }

    #[test]
    fn test_podman_runtime_uses_podman_binary() {
        let rt = ContainerRuntime::podman();
        assert_eq!(rt.kind, RuntimeKind::Podman);
        assert_eq!(rt.base.binary, "podman");
        assert_eq!(rt.base.name, "Podman");
    }

    #[test]
    fn test_podman_supports_docker_compatible_features() {
        // Podman is a drop-in for Docker, so it must support the same feature
        // set the shared base relies on. If this regresses, the create-args
        // builder will silently produce broken output for podman users.
        let rt = ContainerRuntime::podman();
        assert!(rt.base.supports_read_only_volumes);
        assert!(rt.base.supports_remove_volumes);
        assert!(rt.base.supports_named_volumes);
        assert_eq!(rt.base.remove_subcommand, "rm");
        assert_eq!(rt.base.pull_prefix, &["pull"]);
    }

    #[test]
    fn test_podman_exec_command_format_matches_docker() {
        // The CLI surfaces this string to the user via tmux; it must not
        // wrap the command in `sh -c` the way Apple Container does.
        let rt = ContainerRuntime::podman();
        let cmd = rt.exec_command("aoe-sandbox-test1234", None, "claude");
        assert_eq!(cmd, "podman exec -it aoe-sandbox-test1234 claude");
    }
}
