use super::container_interface::{docker_env_args, ContainerConfig, RunFlag};
use super::error::{sanitize_stderr, DockerError, Result};
use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `docker pull` has no timeout of its own; this only fires on a wedged pull.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// Sized above the default 10s stop grace so a slow but legitimate stop completes.
const RUNTIME_CMD_TIMEOUT: Duration = Duration::from_secs(60);

const RUNTIME_EXEC_TIMEOUT: Duration = Duration::from_secs(600);

pub(crate) struct RuntimeBase {
    pub binary: &'static str,
    pub name: &'static str,
    pub daemon_check_args: &'static [&'static str],
    pub pull_prefix: &'static [&'static str],
    pub remove_subcommand: &'static str,
    pub supports_read_only_volumes: bool,
    pub supports_remove_volumes: bool,
    pub supports_named_volumes: bool,
    pub supports_selinux_relabel: bool,
    /// Apple Container's `run` has no Docker-style `--network` modes.
    pub supports_network_mode: bool,
    pub supports_labels: bool,
    pub supported_run_flags: &'static [RunFlag],
    /// Case-insensitive stderr substrings for "container does not exist".
    pub not_found_markers: &'static [&'static str],
    /// Case-sensitive stderr substrings for "daemon is not reachable".
    pub daemon_down_markers: &'static [&'static str],
    /// Case-insensitive stderr substrings for socket permission errors.
    pub permission_denied_markers: &'static [&'static str],
}

const ALL_RUN_FLAGS: &[RunFlag] = &[
    RunFlag::Privileged,
    RunFlag::CapAdd,
    RunFlag::CapDrop,
    RunFlag::SecurityOpt,
];

impl RuntimeBase {
    pub const DOCKER: Self = Self {
        binary: "docker",
        name: "Docker",
        daemon_check_args: &["info"],
        pull_prefix: &["pull"],
        remove_subcommand: "rm",
        supports_read_only_volumes: true,
        supports_remove_volumes: true,
        supports_named_volumes: true,
        supports_selinux_relabel: true,
        supports_network_mode: true,
        supports_labels: true,
        supported_run_flags: ALL_RUN_FLAGS,
        not_found_markers: &["no such container"],
        daemon_down_markers: &["Cannot connect to the Docker daemon"],
        // Scoped to "docker daemon socket" so unrelated permission errors are not misclassified.
        permission_denied_markers: &[
            "permission denied while trying to connect to the docker daemon socket",
        ],
    };

    pub const APPLE_CONTAINER: Self = Self {
        binary: "container",
        name: "Apple Container",
        daemon_check_args: &["system", "status"],
        pull_prefix: &["image", "pull"],
        remove_subcommand: "delete",
        supports_read_only_volumes: false,
        supports_remove_volumes: false,
        supports_named_volumes: false,
        supports_selinux_relabel: false,
        supports_network_mode: false,
        supports_labels: true,
        supported_run_flags: &[RunFlag::CapAdd, RunFlag::CapDrop],
        // Avoid bare "not found": daemon errors like "socket not found" would read as absent.
        not_found_markers: &["container with id", "container not found"],
        // Placeholder: wording not yet captured from a real Apple daemon-down.
        daemon_down_markers: &["connect to container daemon"],
        permission_denied_markers: &["permission denied"],
    };

    pub const PODMAN: Self = Self {
        binary: "podman",
        name: "Podman",
        daemon_check_args: &["info"],
        pull_prefix: &["pull"],
        remove_subcommand: "rm",
        supports_read_only_volumes: true,
        supports_remove_volumes: true,
        supports_named_volumes: true,
        supports_selinux_relabel: true,
        supports_network_mode: true,
        supports_labels: true,
        supported_run_flags: ALL_RUN_FLAGS,
        not_found_markers: &["no such container"],
        daemon_down_markers: &["connect to Podman socket", "Cannot connect to Podman."],
        // Placeholder: Podman surfaces the OS socket error, which contains this.
        permission_denied_markers: &["permission denied"],
    };

    pub fn is_daemon_down(&self, stderr: &str) -> bool {
        self.daemon_down_markers.iter().any(|m| stderr.contains(m))
    }

    pub fn is_permission_denied(&self, stderr: &str) -> bool {
        let lower = stderr.to_lowercase();
        self.permission_denied_markers
            .iter()
            .any(|m| lower.contains(m))
    }

    pub fn is_not_found(&self, stderr: &str) -> bool {
        let lower = stderr.to_lowercase();
        self.not_found_markers.iter().any(|m| lower.contains(m))
    }

    /// Splits a failed state probe into definitive absence (`Ok(false)`) or a runtime failure
    /// that must surface, so fail-closed gates do not swallow a daemon-down signal.
    pub fn classify_probe_failure(&self, stderr: &str) -> Result<bool> {
        if self.is_not_found(stderr) {
            return Ok(false);
        }
        // Permission-denied first: Podman's socket-permission stderr also matches its daemon-down marker.
        if self.is_permission_denied(stderr) {
            return Err(DockerError::PermissionDenied);
        }
        if self.is_daemon_down(stderr) {
            return Err(DockerError::DaemonNotRunning);
        }
        Err(DockerError::InspectFailed(sanitize_stderr(stderr)))
    }

    pub fn command(&self) -> Command {
        Command::new(self.binary)
    }

    /// Maps a missing binary to `NotInstalled` and a timeout to an `IoError`.
    pub fn probe_output(&self, cmd: &mut Command) -> Result<std::process::Output> {
        self.probe_output_with_timeout(cmd, RUNTIME_CMD_TIMEOUT)
    }

    fn probe_output_with_timeout(
        &self,
        cmd: &mut Command,
        timeout: Duration,
    ) -> Result<std::process::Output> {
        cmd.stdin(Stdio::null());
        match crate::process::run_with_timeout(cmd, timeout) {
            Ok(Some(output)) => Ok(output),
            Ok(None) => Err(DockerError::IoError(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "{} command timed out after {}s and was killed",
                    self.binary,
                    timeout.as_secs()
                ),
            ))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(DockerError::NotInstalled),
            Err(e) => Err(DockerError::IoError(e)),
        }
    }

    fn probe_succeeds(&self, args: &[&str]) -> bool {
        let mut cmd = self.command();
        cmd.args(args);
        self.probe_output(&mut cmd)
            .is_ok_and(|output| output.status.success())
    }

    pub fn is_available(&self) -> bool {
        self.probe_succeeds(&["--version"])
    }

    pub fn is_daemon_running(&self) -> bool {
        self.probe_succeeds(self.daemon_check_args)
    }

    pub fn image_exists_locally(&self, image: &str) -> bool {
        self.probe_succeeds(&["image", "inspect", image])
    }

    pub fn pull_image(&self, image: &str) -> Result<()> {
        let mut cmd = self.command();
        cmd.args(self.pull_prefix);
        cmd.arg(image);
        cmd.stdin(Stdio::null());
        let start = Instant::now();
        tracing::info!(target: "containers.image", runtime = %self.name, %image, "pulling image");
        let output = match crate::process::run_with_timeout(&mut cmd, PULL_TIMEOUT)? {
            Some(output) => output,
            None => {
                let dur_ms = start.elapsed().as_millis() as u64;
                tracing::warn!(
                    target: "containers.image",
                    runtime = %self.name,
                    %image,
                    duration_ms = dur_ms,
                    timeout_s = PULL_TIMEOUT.as_secs(),
                    "image pull timed out; killed",
                );
                return Err(DockerError::ImageNotFound(format!(
                    "{}: pull timed out after {}s",
                    image,
                    PULL_TIMEOUT.as_secs()
                )));
            }
        };
        let dur_ms = start.elapsed().as_millis() as u64;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(
                target: "containers.image",
                runtime = %self.name,
                %image,
                duration_ms = dur_ms,
                stderr_summary = %stderr.trim().chars().take(200).collect::<String>(),
                "image pull failed"
            );
            return Err(DockerError::ImageNotFound(format!(
                "{}: {}",
                image,
                stderr.trim()
            )));
        }

        tracing::info!(
            target: "containers.image",
            runtime = %self.name,
            %image,
            duration_ms = dur_ms,
            "image pull completed"
        );
        Ok(())
    }

    pub fn ensure_image(&self, image: &str) -> Result<()> {
        if self.image_exists_locally(image) {
            tracing::info!(target: "containers.runtime", "Using local {} image '{}'", self.name, image);
            return Ok(());
        }

        tracing::info!(target: "containers.runtime", "Pulling {} image '{}'", self.name, image);
        self.pull_image(image)
    }

    pub fn default_sandbox_image(&self) -> &'static str {
        "ghcr.io/agent-of-empires/aoe-sandbox:latest"
    }

    pub fn effective_default_image(&self) -> String {
        crate::session::Config::load()
            .ok()
            .map(|c| c.sandbox.default_image)
            .unwrap_or_else(|| self.default_sandbox_image().to_string())
    }

    pub fn build_create_args(
        &self,
        name: &str,
        image: &str,
        config: &ContainerConfig,
    ) -> Vec<String> {
        let mut args = vec![
            "run".to_string(),
            "-d".to_string(),
            "--name".to_string(),
            name.to_string(),
            "-w".to_string(),
            config.working_dir.clone(),
        ];

        let mut push = |flag: &str, value: String| {
            args.push(flag.to_string());
            args.push(value);
        };
        if self.supports_labels {
            use crate::containers::container_interface::{
                AGENT_TOOL_LABEL, SHARED_CREDENTIAL_MOUNTS_LABEL,
            };
            push(
                "--label",
                "com.agent-of-empires.sandbox-store-generation=2".to_string(),
            );
            push(
                "--label",
                format!(
                    "com.agent-of-empires.mount-fingerprint={}",
                    config.mount_fingerprint()
                ),
            );
            if !config.shared_credential_mounts.is_empty() {
                push(
                    "--label",
                    format!(
                        "{SHARED_CREDENTIAL_MOUNTS_LABEL}={}",
                        config.shared_credential_label()
                    ),
                );
            }
            if !config.agent_tool.is_empty() {
                push(
                    "--label",
                    format!("{AGENT_TOOL_LABEL}={}", config.agent_tool),
                );
            }
        }

        for vol in &config.volumes {
            if !self.supports_read_only_volumes && vol.read_only {
                tracing::warn!(target: "containers.runtime",
                    "{} does not support read-only volumes, mounting {} read-write",
                    self.name,
                    vol.container_path
                );
            }
            let mut opts: Vec<&str> = Vec::new();
            if vol.read_only && self.supports_read_only_volumes {
                opts.push("ro");
            }
            if config.selinux_relabel && self.supports_selinux_relabel {
                // `:z` (shared), not `:Z`: the credential dir is mounted into several sandbox containers.
                opts.push("z");
            }
            let mut mount = format!("{}:{}", vol.host_path, vol.container_path);
            if !opts.is_empty() {
                mount = format!("{mount}:{}", opts.join(","));
            }
            push("-v", mount);
        }

        for path in &config.anonymous_volumes {
            push("-v", path.clone());
        }

        if !self.supports_named_volumes && !config.named_ignore_volumes.is_empty() {
            tracing::warn!(
                target: "containers.runtime",
                runtime = %self.name,
                "named volume_ignores_strategy is not supported; falling back to anonymous volumes"
            );
        }
        for nv in &config.named_ignore_volumes {
            if self.supports_named_volumes {
                push("-v", format!("{}:{}", nv.volume_name, nv.container_path));
            } else {
                push("-v", nv.container_path.clone());
            }
        }

        let (env_argv, _inherit) = docker_env_args(&config.environment);
        args.extend(env_argv);

        let network = match config.network.as_deref() {
            Some(n) if !self.supports_network_mode => {
                tracing::warn!(
                    target: "containers.runtime",
                    "{} does not support --network modes; ignoring sandbox.network = {:?}",
                    self.name,
                    n
                );
                None
            }
            other => other,
        };
        let network_none = network.is_some_and(|n| n.eq_ignore_ascii_case("none"));
        if let Some(network) = network {
            args.push("--network".to_string());
            args.push(network.to_string());
        }

        // `-p` fails with `--network none`, so the mappings are dropped with a warning.
        if network_none && !config.port_mappings.is_empty() {
            tracing::warn!(
                target: "containers.runtime",
                "Ignoring {} port mapping(s) because sandbox.network = \"none\"",
                config.port_mappings.len()
            );
        } else {
            for port in &config.port_mappings {
                args.push("-p".to_string());
                args.push(port.clone());
            }
        }

        if let Some(cpu) = &config.cpu_limit {
            args.push("--cpus".to_string());
            args.push(cpu.clone());
        }

        if let Some(mem) = &config.memory_limit {
            args.push("-m".to_string());
            args.push(mem.clone());
        }

        let policy = &config.run_policy;
        let pairs = |flag: &str, values: &[String]| -> Vec<String> {
            values
                .iter()
                .flat_map(|v| [flag.to_string(), v.clone()])
                .collect()
        };
        for (flag, config_key, argv) in [
            (
                RunFlag::Privileged,
                "privileged",
                Vec::from_iter(policy.privileged.then(|| "--privileged".to_string())),
            ),
            (
                RunFlag::CapAdd,
                "cap_add",
                pairs("--cap-add", &policy.cap_add),
            ),
            (
                RunFlag::CapDrop,
                "cap_drop",
                pairs("--cap-drop", &policy.cap_drop),
            ),
            (
                RunFlag::SecurityOpt,
                "security_opt",
                pairs("--security-opt", &policy.security_opt),
            ),
        ] {
            if argv.is_empty() {
                continue;
            }
            if self.supported_run_flags.contains(&flag) {
                args.extend(argv);
            } else {
                tracing::warn!(
                    target: "containers.runtime",
                    "ignoring sandbox.{config_key}: {} does not support it",
                    self.name
                );
            }
        }

        args.extend(policy.extra_run_args.iter().cloned());

        args.push(image.to_string());
        args.push("sleep".to_string());
        args.push("infinity".to_string());

        args
    }

    pub fn run_create(&self, name: &str, image: &str, config: &ContainerConfig) -> Result<String> {
        let args = self.build_create_args(name, image, config);
        tracing::debug!(target: "containers.runtime", "{} create args: {}", self.name, args.join(" "));

        let mut cmd = self.command();
        cmd.args(&args);
        // Inherited values reach docker via the child env, keeping them out of argv.
        let (_, inherit) = docker_env_args(&config.environment);
        for (key, value) in inherit {
            cmd.env(key, value);
        }
        let output = self.probe_output(&mut cmd)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::debug!(target: "containers.runtime", "stderr: {}", stderr);
            if self.is_permission_denied(&stderr) {
                return Err(DockerError::PermissionDenied);
            }
            if self.is_daemon_down(&stderr) {
                return Err(DockerError::DaemonNotRunning);
            }
            if stderr.contains("No such image") || stderr.contains("Unable to find image") {
                return Err(DockerError::ImageNotFound(image.to_string()));
            }
            return Err(DockerError::CreateFailed(sanitize_stderr(&stderr)));
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(container_id)
    }

    pub fn start_container(&self, name: &str) -> Result<()> {
        tracing::info!(target: "containers.runtime", runtime = %self.name, %name, "starting container");
        let mut cmd = self.command();
        cmd.args(["start", name]);
        let output = self.probe_output(&mut cmd)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DockerError::StartFailed(sanitize_stderr(&stderr)));
        }

        Ok(())
    }

    pub fn stop_container(&self, name: &str) -> Result<()> {
        tracing::info!(target: "containers.runtime", runtime = %self.name, %name, "stopping container");
        let mut cmd = self.command();
        cmd.args(["stop", name]);
        let output = self.probe_output(&mut cmd)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if self.is_not_found(&stderr) {
                return Err(DockerError::ContainerNotFound(name.to_string()));
            }
            return Err(DockerError::StopFailed(sanitize_stderr(&stderr)));
        }

        Ok(())
    }

    pub fn remove(&self, name: &str, force: bool) -> Result<()> {
        let mut args = vec![self.remove_subcommand.to_string()];
        if force {
            args.push("-f".to_string());
        }
        if self.supports_remove_volumes {
            // Removes anonymous volumes only; named volumes survive.
            args.push("-v".to_string());
        }
        args.push(name.to_string());

        tracing::debug!(target: "containers.runtime", runtime = %self.name, %name, %force, "removing container");
        let mut cmd = self.command();
        cmd.args(&args);
        let output = self.probe_output(&mut cmd)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if self.is_not_found(&stderr) {
                return Err(DockerError::ContainerNotFound(name.to_string()));
            }
            return Err(DockerError::RemoveFailed(sanitize_stderr(&stderr)));
        }

        Ok(())
    }

    /// Volumes outlive the container, so call this even when it is already gone.
    pub fn remove_named_ignore_volumes(&self, prefix: &str) -> Result<()> {
        self.remove_named_ignore_volumes_where(prefix, |_| true)
    }

    pub fn remove_named_ignore_volumes_in(
        &self,
        prefix: &str,
        names: &HashSet<&str>,
    ) -> Result<()> {
        self.remove_named_ignore_volumes_where(prefix, |name| names.contains(name))
    }

    fn remove_named_ignore_volumes_where(
        &self,
        prefix: &str,
        select: impl Fn(&str) -> bool,
    ) -> Result<()> {
        if !self.supports_named_volumes {
            return Ok(());
        }

        let mut list_cmd = self.command();
        list_cmd.args([
            "volume",
            "ls",
            "--filter",
            &format!("name={}", prefix),
            "-q",
        ]);
        let list_output = self.probe_output(&mut list_cmd)?;

        if !list_output.status.success() {
            let stderr = String::from_utf8_lossy(&list_output.stderr);
            tracing::warn!(target: "containers.runtime", runtime = %self.name, %prefix, "failed to list named ignore volumes: {}", stderr);
            return Ok(());
        }

        let stdout = String::from_utf8_lossy(&list_output.stdout);
        let names = selected_named_ignore_volumes(&stdout, prefix, select);

        if names.is_empty() {
            return Ok(());
        }

        tracing::debug!(target: "containers.runtime", runtime = %self.name, ?names, "removing named ignore volumes");
        let mut rm_args = vec!["volume", "rm"];
        rm_args.extend(names.iter().copied());
        let mut rm_cmd = self.command();
        rm_cmd.args(&rm_args);
        let rm_output = self.probe_output(&mut rm_cmd)?;

        if !rm_output.status.success() {
            let stderr = String::from_utf8_lossy(&rm_output.stderr);
            tracing::warn!(target: "containers.runtime", runtime = %self.name, "failed to remove named ignore volumes: {}", stderr);
        }

        Ok(())
    }

    pub fn exec_command(&self, name: &str, options: Option<&str>, cmd: &str) -> String {
        [self.binary, "exec", "-it"]
            .into_iter()
            .chain(options)
            .chain([name, cmd])
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// No `-it` (stdout is piped, stdin closed) and argv, not a shell string, so the prompt is never shell-parsed.
    pub fn build_exec_argv(&self, name: &str, workdir: &str, cmd: &[String]) -> Vec<String> {
        let mut argv = vec![self.binary.to_string(), "exec".to_string()];
        if !workdir.is_empty() {
            argv.push("-w".to_string());
            argv.push(workdir.to_string());
        }
        argv.push(name.to_string());
        argv.extend(cmd.iter().cloned());
        argv
    }

    pub fn exec(&self, name: &str, cmd: &[&str]) -> Result<std::process::Output> {
        let mut args = vec!["exec", name];
        args.extend(cmd);

        let mut command = self.command();
        command.args(&args);
        self.probe_output_with_timeout(&mut command, RUNTIME_EXEC_TIMEOUT)
    }
}

/// Re-filter on the prefix: docker's `--filter name=` is a substring match.
fn selected_named_ignore_volumes<'a>(
    listing: &'a str,
    prefix: &str,
    select: impl Fn(&str) -> bool,
) -> Vec<&'a str> {
    listing
        .lines()
        .map(str::trim)
        .filter(|n| !n.is_empty() && n.starts_with(prefix) && select(n))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::container_interface::{
        EnvEntry, NamedVolumeMount, RunPolicy, VolumeMount,
    };

    const DOCKER: RuntimeBase = RuntimeBase::DOCKER;
    const PODMAN: RuntimeBase = RuntimeBase::PODMAN;
    const APPLE: RuntimeBase = RuntimeBase::APPLE_CONTAINER;

    // Real stderr captured from `<runtime> rm/delete <missing>`.
    const DOCKER_MISSING: &str =
        "Error response from daemon: No such container: aoe-sandbox-abc123";
    const APPLE_MISSING: &str = "Error: internalError: \"failed to delete container\" (cause: \"notFound: \"container with ID aoe-sandbox-abc123 not found\"\")";
    const APPLE_INSPECT_MISSING: &str = "Error: container not found: aoe-sandbox-abc123";
    const PODMAN_MISSING: &str =
        "Error: no container with name or ID \"aoe-sandbox-abc123\" found: no such container";

    fn probe_kind(result: Result<bool>) -> String {
        match result {
            Ok(present) => format!("ok:{present}"),
            Err(DockerError::DaemonNotRunning) => "daemon_down".into(),
            Err(DockerError::PermissionDenied) => "permission_denied".into(),
            Err(DockerError::InspectFailed(stderr)) => format!("inspect_failed:{stderr}"),
            Err(other) => format!("other:{other}"),
        }
    }

    #[test]
    fn classify_probe_failure_per_runtime() {
        let cases = [
            (&DOCKER, DOCKER_MISSING, "ok:false"),
            (&APPLE, APPLE_MISSING, "ok:false"),
            // Regression: this inspect shape once failed every new Apple sandbox's first start.
            (&APPLE, APPLE_INSPECT_MISSING, "ok:false"),
            (&PODMAN, PODMAN_MISSING, "ok:false"),
            (
                &DOCKER,
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. \
                 Is the docker daemon running?",
                "daemon_down",
            ),
            (
                &PODMAN,
                "Error: unable to connect to Podman socket: Connection refused",
                "daemon_down",
            ),
            (
                &PODMAN,
                "Cannot connect to Podman. Please verify your connection to the Linux system",
                "daemon_down",
            ),
            (
                &APPLE,
                "Error: internalError: \"failed to connect to container daemon\" (cause: \"transient\")",
                "daemon_down",
            ),
            (
                &DOCKER,
                "Got permission denied while trying to connect to the Docker daemon socket \
                 at unix:///var/run/docker.sock",
                "permission_denied",
            ),
            (
                &PODMAN,
                "Error: unable to connect to Podman socket: dial unix \
                 /run/user/1000/podman/podman.sock: connect: permission denied",
                "permission_denied",
            ),
            (
                &APPLE,
                "Error: permission denied accessing container socket",
                "permission_denied",
            ),
            (
                &DOCKER,
                "Error response from daemon: internal server error 500",
                "inspect_failed:Error response from daemon: internal server error 500",
            ),
            (&DOCKER, "", "inspect_failed:<no stderr>"),
        ];
        for (base, stderr, expected) in cases {
            assert_eq!(
                probe_kind(base.classify_probe_failure(stderr)),
                expected,
                "{}: {stderr}",
                base.name
            );
        }
        assert!(
            !DOCKER.is_not_found("Error response from daemon: container is running: stop it first")
        );
        assert!(!APPLE.is_not_found(
            "Error: internalError: \"failed to delete container\" (cause: \"resource busy\")"
        ));
    }

    #[test]
    fn stderr_markers_do_not_bleed_across_runtimes() {
        let runtimes = [&DOCKER, &PODMAN, &APPLE];
        let daemon_down = [
            "Cannot connect to the Docker daemon at ...",
            "Error: unable to connect to Podman socket: ...",
            "Error: internalError: \"failed to connect to container daemon\"",
        ];
        for (owner, stderr) in daemon_down.iter().enumerate() {
            for (i, base) in runtimes.iter().enumerate() {
                assert_eq!(
                    base.is_daemon_down(stderr),
                    i == owner,
                    "{}: {stderr}",
                    base.name
                );
            }
        }

        // Podman and Apple keep broad placeholders; Docker's marker is tightly scoped.
        let docker_pd = "Got permission denied while trying to connect to the Docker daemon socket";
        assert!(runtimes
            .iter()
            .all(|base| base.is_permission_denied(docker_pd)));
        let podman_only = "Error: unable to connect to Podman socket: connect: permission denied";
        assert!(!DOCKER.is_permission_denied(podman_only));
        assert!(PODMAN.is_permission_denied(podman_only));
        assert!(!DOCKER.is_permission_denied(
            "docker: Error response from daemon: pull access denied: permission denied by policy"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn probe_output_maps_spawn_failures_and_timeouts() {
        let mut missing = Command::new("aoe-nonexistent-runtime-binary-zzz");
        assert!(matches!(
            DOCKER.probe_output(&mut missing),
            Err(DockerError::NotInstalled)
        ));

        let output = DOCKER
            .probe_output(&mut Command::new("false"))
            .expect("a spawnable binary must not map to a DockerError");
        assert!(!output.status.success());

        assert!(RUNTIME_EXEC_TIMEOUT > RUNTIME_CMD_TIMEOUT);
        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 5"]);
        assert!(matches!(
            &DOCKER.probe_output_with_timeout(&mut slow, Duration::from_millis(10)),
            Err(DockerError::IoError(error)) if error.kind() == std::io::ErrorKind::TimedOut
        ));
    }

    fn create_args(base: &RuntimeBase, config: ContainerConfig) -> Vec<String> {
        base.build_create_args(
            "c",
            "alpine:latest",
            &ContainerConfig {
                working_dir: "/workspace".to_string(),
                ..config
            },
        )
    }

    fn mount(host: &str, container: &str, read_only: bool) -> VolumeMount {
        VolumeMount {
            host_path: host.to_string(),
            container_path: container.to_string(),
            read_only,
        }
    }

    fn arg_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
            .map(String::as_str)
    }

    fn values_of<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|pair| pair[0] == flag)
            .map(|pair| pair[1].as_str())
            .collect()
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn create_args_render_mount_options_per_runtime() {
        let volumes = vec![
            mount("/host/rw", "/container/rw", false),
            mount("/host/ro", "/container/ro", true),
        ];
        let named = || {
            vec![NamedVolumeMount {
                volume_name: "aoe-vi-sess1-workspace-node_modules-abc123".to_string(),
                container_path: "/workspace/node_modules".to_string(),
            }]
        };
        let config = || ContainerConfig {
            volumes: volumes.clone(),
            anonymous_volumes: vec!["/tmp/cache".to_string()],
            named_ignore_volumes: named(),
            ..Default::default()
        };
        assert_eq!(
            values_of(&create_args(&DOCKER, config()), "-v"),
            [
                "/host/rw:/container/rw",
                "/host/ro:/container/ro:ro",
                "/tmp/cache",
                "aoe-vi-sess1-workspace-node_modules-abc123:/workspace/node_modules",
            ]
        );
        let relabeled = ContainerConfig {
            selinux_relabel: true,
            ..config()
        };
        assert_eq!(
            values_of(&create_args(&PODMAN, relabeled), "-v")[..2],
            ["/host/rw:/container/rw:z", "/host/ro:/container/ro:ro,z"]
        );
        let relabeled = ContainerConfig {
            selinux_relabel: true,
            ..config()
        };
        assert_eq!(
            values_of(&create_args(&APPLE, relabeled), "-v"),
            [
                "/host/rw:/container/rw",
                "/host/ro:/container/ro",
                "/tmp/cache",
                "/workspace/node_modules",
            ]
        );
    }

    #[test]
    fn create_args_network_and_ports() {
        let with = |network: Option<&str>| ContainerConfig {
            network: network.map(str::to_string),
            port_mappings: strings(&["3000:3000", "5432:5432"]),
            ..Default::default()
        };
        let args = create_args(&DOCKER, with(None));
        assert_eq!(arg_after(&args, "--network"), None);
        assert_eq!(values_of(&args, "-p"), ["3000:3000", "5432:5432"]);

        let args = create_args(&DOCKER, with(Some("egress-proxy")));
        assert_eq!(arg_after(&args, "--network"), Some("egress-proxy"));
        assert_eq!(values_of(&args, "-p"), ["3000:3000", "5432:5432"]);

        let args = create_args(&DOCKER, with(Some("none")));
        assert_eq!(arg_after(&args, "--network"), Some("none"));
        assert!(values_of(&args, "-p").is_empty());

        let args = create_args(&APPLE, with(Some("none")));
        assert_eq!(arg_after(&args, "--network"), None);
        assert_eq!(values_of(&args, "-p"), ["3000:3000", "5432:5432"]);
    }

    #[test]
    fn create_args_run_policy_respects_runtime_support() {
        let policy = || RunPolicy {
            privileged: true,
            cap_add: strings(&["SYS_ADMIN"]),
            cap_drop: strings(&["NET_RAW"]),
            security_opt: strings(&["seccomp=unconfined"]),
            extra_run_args: strings(&["--isolation", "chroot"]),
        };
        let args = create_args(
            &DOCKER,
            ContainerConfig {
                run_policy: policy(),
                ..Default::default()
            },
        );
        assert!(args.contains(&"--privileged".to_string()));
        assert_eq!(arg_after(&args, "--cap-add"), Some("SYS_ADMIN"));
        assert_eq!(arg_after(&args, "--cap-drop"), Some("NET_RAW"));
        assert_eq!(
            arg_after(&args, "--security-opt"),
            Some("seccomp=unconfined")
        );
        assert!(args.ends_with(&strings(&[
            "--isolation",
            "chroot",
            "alpine:latest",
            "sleep",
            "infinity"
        ])));

        let args = create_args(
            &APPLE,
            ContainerConfig {
                run_policy: policy(),
                ..Default::default()
            },
        );
        assert!(!args.contains(&"--privileged".to_string()));
        assert!(!args.contains(&"--security-opt".to_string()));
        assert_eq!(arg_after(&args, "--cap-add"), Some("SYS_ADMIN"));
        assert!(args.contains(&"--isolation".to_string()));

        let args = create_args(&DOCKER, ContainerConfig::default());
        for flag in ["--privileged", "--cap-add", "--cap-drop", "--security-opt"] {
            assert!(!args.contains(&flag.to_string()), "unexpected {flag}");
        }
    }

    #[test]
    fn create_args_base_limits_and_env() {
        let args = create_args(
            &DOCKER,
            ContainerConfig {
                environment: vec![
                    EnvEntry::Inherit {
                        key: "GH_TOKEN".to_string(),
                        value: "ghp_secret123".to_string(),
                    },
                    EnvEntry::Literal {
                        key: "TERM".to_string(),
                        value: "xterm".to_string(),
                    },
                ],
                cpu_limit: Some("2".to_string()),
                memory_limit: Some("4g".to_string()),
                ..Default::default()
            },
        );
        assert_eq!(
            args[..6],
            strings(&["run", "-d", "--name", "c", "-w", "/workspace"])
        );
        assert_eq!(values_of(&args, "-e"), ["GH_TOKEN", "TERM=xterm"]);
        assert!(!args.iter().any(|a| a.contains("ghp_secret123")));
        assert_eq!(arg_after(&args, "--cpus"), Some("2"));
        assert_eq!(arg_after(&args, "-m"), Some("4g"));
    }

    #[test]
    fn exec_command_formats() {
        assert_eq!(
            DOCKER.exec_command("my-container", Some("-w /workspace"), "my-agent"),
            "docker exec -it -w /workspace my-container my-agent"
        );
        assert_eq!(
            APPLE.exec_command("my-container", None, "my-agent"),
            "container exec -it my-container my-agent"
        );
    }

    #[test]
    fn selected_named_ignore_volumes_never_reaches_outside_the_prefix() {
        // Real `named_volume_for` output: `DefaultHasher` drift would orphan existing named volumes.
        const MAIN: &str = "aoe-vi-sess1-workspace-otari-target-8ec07926d6b0";
        const PRE_MOVE: &str = "aoe-vi-sess1-workspace-otari-worktrees-905-target-31ddd0322290";
        const POST_MOVE: &str =
            "aoe-vi-sess1-workspace-otari-worktrees-rev-912-target-873cf2685e47";

        let moved = format!("{MAIN}\n{PRE_MOVE}\n{POST_MOVE}\n");
        let other_session =
            format!("{PRE_MOVE}\naoe-vi-sess10-workspace-a-target-c8c9b4754ab1\n\n");

        let cases = [
            (
                "an allowlist of one stranded name",
                moved.as_str(),
                Some(HashSet::from([PRE_MOVE])),
                vec![PRE_MOVE],
            ),
            (
                "the deletion sweep",
                moved.as_str(),
                None::<HashSet<&str>>,
                vec![MAIN, PRE_MOVE, POST_MOVE],
            ),
            (
                "a longer session id in the listing",
                other_session.as_str(),
                None,
                vec![PRE_MOVE],
            ),
        ];

        for (case, listing, allowed, expected) in cases {
            let selected = selected_named_ignore_volumes(listing, "aoe-vi-sess1-", |name| {
                allowed.as_ref().is_none_or(|names| names.contains(name))
            });
            assert_eq!(selected, expected, "{case}");
        }
    }

    #[test]
    fn store_generation_label_is_emitted_by_supported_runtimes() {
        let shared = || ContainerConfig {
            shared_credential_mounts: vec!["/root/.claude/.credentials.json".to_string()],
            agent_tool: "claude".to_string(),
            ..Default::default()
        };
        for base in [&DOCKER, &PODMAN, &APPLE] {
            let args = create_args(base, ContainerConfig::default());
            let labels = values_of(&args, "--label");
            assert_eq!(labels[0], "com.agent-of-empires.sandbox-store-generation=2");
            assert!(labels[1].starts_with("com.agent-of-empires.mount-fingerprint="));
            assert_eq!(labels.len(), 2);
            let shared_args = create_args(base, shared());
            let labels = values_of(&shared_args, "--label");
            assert_eq!(
                labels[2..],
                [
                    "com.agent-of-empires.shared-credential-mounts=/root/.claude/.credentials.json",
                    "com.agent-of-empires.agent-tool=claude",
                ]
            );
        }
    }
}
