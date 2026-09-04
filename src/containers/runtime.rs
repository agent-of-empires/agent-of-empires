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
                // `RepoDigests` holds `repo@sha256:...` entries for the pulled
                // manifest. One subprocess, newline-joined so a multi-registry
                // image still lets us pick the entry matching this reference.
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
                    return self.base.classify_probe_failure(&stderr);
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
                    return self.base.classify_probe_failure(&stderr);
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
                    return self.base.classify_probe_failure(&stderr);
                }

                let stdout = String::from_utf8_lossy(&output.stdout);
                Ok(stdout.trim() == "true")
            }
            RuntimeKind::AppleContainer => {
                // Apple's `container inspect` is the only inspect subcommand
                // (no `container container inspect`), and its stderr wording
                // is pinned in RuntimeBase::APPLE_CONTAINER.not_found_markers:
                // it covers both the inspect-specific shape
                // (`container not found: <name>`) and the logs/delete shape
                // (`container with ID <id> not found`). The classifier
                // separately checks daemon_down_markers. Do not tighten this
                // argv or either marker list without capturing new
                // fixtures; same silent-break risk as the Docker/Podman
                // comment above. See #2596.
                let mut cmd = self.base.command();
                cmd.args(["inspect", name]);
                let output = self.base.probe_output(&mut cmd)?;

                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return self.base.classify_probe_failure(&stderr);
                }

                let out_json: Value = serde_json::from_slice(&output.stdout)
                    // serde_json::Error::Display is single-line by construction
                    // (format is "<code> at line N column M"); no sanitize_stderr
                    // wrapping needed to preserve the single-line convention.
                    .map_err(|e| DockerError::InspectFailed(e.to_string()))?;

                Self::apple_container_inspect_state(&out_json).map(|state| state == "running")
            }
        }
    }

    /// The runtime state string from Apple `container inspect` JSON.
    ///
    /// The CLI serialised `/0/status` as a bare string through 0.12.x; the
    /// 1.0.0 ManagedResource cleanup nested it as an object whose `state`
    /// field carries the same value (#3239). Both shapes are supported
    /// because either CLI generation may be installed. Any other shape is
    /// still Err, never Ok(false): a silently unparsed status would route to
    /// Probe::NotRunning, the exact fail-open swallowing-existence-probe
    /// hole (#2596) that the previous string-only guard existed to close.
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

    /// Read the runtime's authoritative container identifier after a create
    /// command whose client-side timeout made its result indeterminate.
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
        let mut cmd = self.base.command();
        cmd.args(["container", "inspect", "-f", "{{.Config.WorkingDir}}", name]);
        let output = self.base.probe_output(&mut cmd).ok()?;
        if !output.status.success() {
            return None;
        }
        let wd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        (!wd.is_empty()).then_some(wd)
    }

    pub fn sandbox_store_generation_matches(&self, name: &str) -> Result<Option<bool>> {
        if !self.base.supports_labels {
            return Ok(None);
        }
        Ok(Some(
            self.inspect_container_label(name, "com.agent-of-empires.sandbox-store-generation")?
                .is_some_and(|value| value == "2"),
        ))
    }

    pub fn mount_fingerprint_matches(&self, name: &str, expected: &str) -> Result<Option<bool>> {
        if !self.base.supports_labels {
            return Ok(None);
        }
        Ok(Some(
            self.inspect_container_label(name, "com.agent-of-empires.mount-fingerprint")?
                .is_some_and(|value| value == expected),
        ))
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
            RuntimeKind::Docker | RuntimeKind::Podman => {
                // Docker/Podman containers inherit a full PATH, so the command
                // can be appended directly without wrapping in `sh -c`.
                self.base.exec_command(name, options, cmd)
            }
            RuntimeKind::AppleContainer => {
                // Apple Container has a very limited initial PATH, so we wrap
                // the command in `/bin/sh -c` to get a proper shell environment.
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

    /// Argv for a non-interactive `exec` of `cmd` in `name`, spawned by the
    /// caller, which keeps its own timeout and output capture.
    ///
    /// Unlike the shell string [`Self::exec_command`] builds for the tmux pane:
    /// no `-it`, because the caller pipes stdout with stdin closed, and argv
    /// rather than a shell string, so an untrusted argument is never
    /// shell-parsed. An empty `workdir` omits `-w`.
    pub fn build_exec_argv(&self, name: &str, workdir: &str, cmd: &[String]) -> Vec<String> {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                self.base.build_exec_argv(name, workdir, cmd)
            }
            RuntimeKind::AppleContainer => {
                // Same limited-PATH problem `exec_command` wraps for, but the
                // command cannot be spliced into the shell string here: `cmd`
                // carries untrusted text. `exec "$@"` re-execs the arguments the
                // shell received positionally, so the shell only supplies PATH
                // and every element survives verbatim. The `sh` after the script
                // is `$0`; the command starts at `$1`.
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

    /// Resource usage of every running container named with `prefix`, in a
    /// single subprocess call.
    ///
    /// `--no-stream` still costs seconds, because the runtime samples every
    /// running container twice to produce a CPU delta; callers go through
    /// [`super::stats::cached_stats`] rather than calling this on a UI cadence.
    pub fn batch_stats(&self, prefix: &str) -> super::stats::StatsMap {
        match self.kind {
            RuntimeKind::Docker | RuntimeKind::Podman => {
                // Stats everything and prefix-filters the rows, unlike
                // `batch_running_states`, which narrows with `--filter name=`.
                // The asymmetry is deliberate: `stats` has no `--filter`, and
                // naming containers positionally fails the whole call with
                // "No such container" if any one of them is gone, which is a
                // normal state for a session whose sandbox has stopped.
                let mut cmd = self.base.command();
                cmd.args([
                    "stats",
                    "--no-stream",
                    "--format",
                    // A literal tab, not the `\t` escape: the argv reaches
                    // the runtime without a shell, and Go template text is
                    // emitted verbatim either way.
                    "{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}\t{{.PIDs}}",
                ]);
                let output = match self.base.probe_output(&mut cmd) {
                    Ok(output) if output.status.success() => output,
                    _ => return super::stats::StatsMap::new(),
                };

                super::stats::parse_stats_output(&String::from_utf8_lossy(&output.stdout), prefix)
            }
            // Apple's `container` CLI has no stats subcommand, so a sandbox on
            // that runtime reports unknown rather than a fabricated number.
            RuntimeKind::AppleContainer => {
                let _ = prefix;
                super::stats::StatsMap::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
    fn test_apple_container_inspect_shapes() {
        use serde_json::json;
        // Both CLI generations (#3239) plus the drift shapes that must stay
        // Err so lifecycle gates fail closed rather than fail open (#2596).
        let cases = [
            (json!([{"status": "running"}]), Some("running")),
            (json!([{"status": "stopped"}]), Some("stopped")),
            // 1.0.0 ManagedResource shape, as emitted by container CLI 1.2.0
            (
                json!([{"status": {"state": "running", "networks": [], "startedDate": "2026-08-04T10:36:08Z"}}]),
                Some("running"),
            ),
            (json!([{"status": {"state": "stopped"}}]), Some("stopped")),
            // object without a string `state`
            (json!([{"status": {"state": 3}}]), None),
            (json!([{"status": {"phase": "running"}}]), None),
            // neither shape
            (json!([{"status": 3}]), None),
            (json!([{"status": null}]), None),
            // missing entirely
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

    #[test]
    fn apple_container_exec_command_uses_absolute_shell() {
        let cmd = ContainerRuntime::apple_container().exec_command(
            "aoe-sandbox-test1234",
            None,
            "printf ok",
        );
        assert_eq!(
            cmd,
            "container exec -it aoe-sandbox-test1234 /bin/sh -c 'printf ok'"
        );
    }

    /// A title one-shot argv: the agent, its flags, and a prompt full of shell
    /// metacharacters as ONE element.
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
    fn build_exec_argv_docker_is_non_interactive_and_sets_workdir() {
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
    }

    #[test]
    fn build_exec_argv_podman_matches_docker_shape() {
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
        // The shell program is a fixed literal and the command follows it as
        // separate elements, so the prompt is never shell-parsed.
        assert_eq!(&argv[9..], &oneshot_argv()[..]);

        let executable = ContainerRuntime::apple_container().build_exec_argv(
            "aoe-sandbox-test1234",
            "",
            &["/bin/printf".to_string(), "PATH_INDEPENDENT".to_string()],
        );
        let output = std::process::Command::new(&executable[3])
            .args(&executable[4..])
            .env_clear()
            .env("PATH", "/definitely-missing")
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"PATH_INDEPENDENT");
    }

    #[test]
    fn build_exec_argv_never_requests_a_tty() {
        // A TTY request against the piped, stdin-closed child smart rename
        // spawns would fail or hang, unlike the tmux `exec_command` path.
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
