use super::container_interface::{docker_env_args, ContainerConfig};
use super::error::{sanitize_stderr, DockerError, Result};
use std::io::Read;
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Upper bound on a single `docker pull`. `docker pull` has no overall timeout
/// of its own, so a stalled registry connection blocks the caller forever
/// (observed as the TUI freezing mid-pull on a sandbox restart). Sized
/// generously so a genuinely large
/// image on a slow link still completes; it only fires on a wedged pull.
const PULL_TIMEOUT: Duration = Duration::from_secs(600);

/// Wait for `child` to exit, killing it if it outlives `timeout`.
///
/// stdout/stderr are drained on dedicated threads so a full pipe buffer cannot
/// wedge the child (and thus this wait) before the deadline. Returns `Ok(None)`
/// when the timeout fired and the child was killed.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> std::io::Result<Option<Output>> {
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let (otx, orx) = mpsc::channel();
    let (etx, erx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stdout_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = otx.send(buf);
    });
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(ref mut p) = stderr_pipe {
            let _ = p.read_to_end(&mut buf);
        }
        let _ = etx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            // The child exited, but if it spawned a grandchild that inherited
            // the pipe, `read_to_end` (and thus an unbounded `recv`) would block
            // forever. Cap the drain at the remaining deadline so the timeout
            // guarantee holds even then; the exit status is already in hand.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let stdout = orx.recv_timeout(remaining).unwrap_or_default();
            let remaining = deadline.saturating_duration_since(Instant::now());
            let stderr = erx.recv_timeout(remaining).unwrap_or_default();
            return Ok(Some(Output {
                status,
                stdout,
                stderr,
            }));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Shared implementation for container runtimes.
///
/// Captures the behavioral differences between runtimes (Docker, Apple Container, etc.)
/// as configuration, then provides a single implementation of all the shared logic.
/// Runtime-specific methods (like container existence checks or running state detection)
/// remain in the individual runtime impls.
pub(crate) struct RuntimeBase {
    /// CLI binary name (e.g., "docker", "container")
    pub binary: &'static str,
    /// Human-readable name for log messages (e.g., "Docker", "Apple Container")
    pub name: &'static str,
    /// Args to check if daemon is running (e.g., ["info"] or ["system", "status"])
    pub daemon_check_args: &'static [&'static str],
    /// Args preceding the image name when pulling (e.g., ["pull"] or ["image", "pull"])
    pub pull_prefix: &'static [&'static str],
    /// Subcommand for removing containers (e.g., "rm" or "delete")
    pub remove_subcommand: &'static str,
    /// Whether this runtime supports the `:ro` read-only volume flag
    pub supports_read_only_volumes: bool,
    /// Whether this runtime supports `-v` on remove to clean up anonymous volumes
    pub supports_remove_volumes: bool,
    /// Whether this runtime supports `volume ls` / `volume rm` for named volumes
    pub supports_named_volumes: bool,
    /// Whether this runtime supports the `:z`/`:Z` SELinux relabel volume flag
    /// (Docker and Podman do; Apple Container does not).
    pub supports_selinux_relabel: bool,
    /// Case-insensitive stderr substrings that identify a "container does not
    /// exist" error for this runtime. Each runtime words it differently (Docker
    /// "No such container", Apple Container "notFound … not found"), so the
    /// markers are per-runtime rather than a single shared string.
    pub not_found_markers: &'static [&'static str],
    /// Case-sensitive stderr substrings that identify a "daemon is not
    /// reachable" error for this runtime. Structural parallel to
    /// `not_found_markers`: keeping this per-runtime prevents cross-runtime
    /// substring bleed and lets `classify_inspect_failure` surface an
    /// actionable [`DockerError::DaemonNotRunning`] at the fail-closed gate
    /// sites (#2596 follow-up). Case sensitivity is intentional; every
    /// runtime's daemon-down message has stable capitalization at the source.
    pub daemon_down_markers: &'static [&'static str],
    /// Case-insensitive stderr substrings that identify a "permission
    /// denied" error (typically Linux docker/podman socket without
    /// docker-group membership). Structural parallel to `not_found_markers`
    /// and `daemon_down_markers` (#2656 follow-up to #2596). Isolation is
    /// intentionally asymmetric: Docker's marker is tightly scoped to the
    /// canonical "docker daemon socket" wording, so cross-runtime bleed is
    /// prevented by construction there; Podman and Apple use the broad
    /// "permission denied" placeholder pending real-fixture capture, which
    /// does bleed across runtimes but preserves pre-#2656 behavior. Case
    /// handling mirrors [`Self::is_not_found`]: OS-emitted "permission
    /// denied" strings vary in capitalization across kernel versions and
    /// locales, so case-fold matching is safest.
    pub permission_denied_markers: &'static [&'static str],
}

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
        not_found_markers: &["no such container"],
        // moby/moby client/errors.go connectionFailed() is the single source
        // of this message across every Docker OS variant (macOS Desktop, Linux
        // CE, Windows Desktop).
        daemon_down_markers: &["Cannot connect to the Docker daemon"],
        // Docker's canonical Linux socket-permission wording, per the
        // post-install docs: "Got permission denied while trying to connect
        // to the Docker daemon socket at unix:///var/run/docker.sock ...".
        // The "docker daemon socket" clause is specific enough to exclude
        // unrelated permission errors (image policy, volume mount, registry
        // auth) that a broad "permission denied" match would misclassify.
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
        // Apple Container reports a missing container as
        // `notFound: "container with ID <id> not found"`. The bare "not found"
        // substring would be dangerously broad; a plausible daemon-connectivity
        // error containing "socket not found" or "endpoint not found" would
        // misclassify as absent and silently reintroduce #2596 on this runtime.
        // Match the container-specific prefix instead (lowercased to align with
        // is_not_found's case-fold).
        not_found_markers: &["container with id"],
        // Placeholder: Apple's `container` CLI daemon-down wording is not
        // captured in this repo. The fallback to InspectFailed still fails
        // closed at gate sites, so an unmatched real message only degrades
        // log actionability, not correctness. Replace with a captured
        // fixture when available.
        daemon_down_markers: &["connect to container daemon"],
        // Placeholder: Apple's `container` CLI permission-denied wording is
        // not captured in this repo. The bare "permission denied" match
        // preserves the pre-#2656 broad-inline-substring behavior for this
        // runtime; tighten to an Apple-specific pattern once captured to
        // prevent future cross-runtime substring bleed.
        permission_denied_markers: &["permission denied"],
    };

    pub const PODMAN: Self = Self {
        binary: "podman",
        name: "Podman",
        // Podman is daemonless, but `podman info` succeeds when the local
        // engine (and its rootless/rootful storage) is healthy, mirroring
        // the Docker daemon-running probe.
        daemon_check_args: &["info"],
        pull_prefix: &["pull"],
        remove_subcommand: "rm",
        supports_read_only_volumes: true,
        supports_remove_volumes: true,
        supports_named_volumes: true,
        supports_selinux_relabel: true,
        not_found_markers: &["no such container"],
        // Two distinct daemon-down wordings observed in real Podman output:
        // - "connect to Podman socket" fires on Linux socket mode
        //   (libpod service unavailable, "unable to connect to Podman
        //   socket: Connection refused").
        // - "Cannot connect to Podman." fires on Podman Desktop / machine
        //   mode (macOS / Windows), when the VM is stopped.
        daemon_down_markers: &["connect to Podman socket", "Cannot connect to Podman."],
        // Placeholder: Podman's socket-permission wording is not captured
        // in this repo. In practice Podman surfaces the underlying Linux
        // socket permission error, which contains "permission denied" (e.g.
        // "unable to connect to Podman socket: dial unix ...: connect:
        // permission denied"). Matches the pre-#2656 broad-inline-substring
        // behavior; tighten to a Podman-specific pattern once captured.
        permission_denied_markers: &["permission denied"],
    };

    /// Whether `stderr` from a container inspect indicates the runtime's
    /// daemon (or equivalent local engine) is unreachable. Case-sensitive,
    /// per-runtime; see [`Self::daemon_down_markers`] rationale.
    pub fn is_daemon_down(&self, stderr: &str) -> bool {
        self.daemon_down_markers.iter().any(|m| stderr.contains(m))
    }

    /// Whether `stderr` indicates a permission-denied error for this
    /// runtime's socket / daemon. Case-insensitive to tolerate wording
    /// drift across kernel versions and locales, per
    /// [`Self::permission_denied_markers`] rationale.
    pub fn is_permission_denied(&self, stderr: &str) -> bool {
        let lower = stderr.to_lowercase();
        self.permission_denied_markers
            .iter()
            .any(|m| lower.contains(m))
    }

    /// Whether `stderr` from a remove/stop indicates the container did not
    /// exist. Case-insensitive to tolerate wording drift across CLI versions
    /// (e.g. capitalization changes between Docker releases). Contrast
    /// [`Self::is_daemon_down`], which is case-sensitive because daemon-down
    /// stderr wording is stable at each runtime's source.
    pub fn is_not_found(&self, stderr: &str) -> bool {
        let lower = stderr.to_lowercase();
        self.not_found_markers.iter().any(|m| lower.contains(m))
    }

    /// Classify a non-success `container inspect` stderr into either a
    /// definitive "not running" (container absent, matches `is_not_found`)
    /// or a genuine runtime failure (daemon down / 500 / any other
    /// transient) that must surface to the caller.
    ///
    /// Without this split, `ContainerRuntime::is_container_running`
    /// collapses BOTH failure modes into `Ok(false)`, and every fail-closed
    /// probe site silently swallows the daemon-down signal as
    /// `Probe::NotRunning`: the same swallowing-existence-probe class
    /// fixed on the removal path by #2576 and on the discard path by
    /// #2596. Mirrors the stderr-sniff pattern `remove()` already uses.
    pub fn classify_inspect_failure(&self, stderr: &str) -> Result<bool> {
        if self.is_not_found(stderr) {
            return Ok(false);
        }
        // Mirror run_create's daemon-down / permission-denied sniff so the
        // Probe::Unknown(e) warn logs at gate sites show the actionable
        // DaemonNotRunning / PermissionDenied Display messages rather than
        // a raw stderr wrapped in InspectFailed. Markers live per-runtime on
        // Self (parallel to `not_found_markers`): daemon-down is fully
        // isolated across runtimes by construction, permission-denied is
        // asymmetric because Podman and Apple placeholders intentionally
        // bleed pending real-fixture capture (see field docs for details).
        //
        // Order matters: permission-denied is checked BEFORE daemon-down
        // because Podman's socket-permission stderr ("unable to connect to
        // Podman socket: ... connect: permission denied") also matches
        // Podman's daemon_down_markers ("connect to Podman socket"). The
        // permission-denied path is the more specific classification, so it
        // must win when both markers match the same stderr.
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

    pub fn is_available(&self) -> bool {
        self.command()
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn is_daemon_running(&self) -> bool {
        self.command()
            .args(self.daemon_check_args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn image_exists_locally(&self, image: &str) -> bool {
        self.command()
            .args(["image", "inspect", image])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    pub fn pull_image(&self, image: &str) -> Result<()> {
        let mut cmd = self.command();
        cmd.args(self.pull_prefix);
        cmd.arg(image);
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let start = Instant::now();
        tracing::info!(target: "containers.image", runtime = %self.name, %image, "pulling image");
        let child = cmd.spawn()?;
        let output = match wait_with_timeout(child, PULL_TIMEOUT)? {
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
                // `:z` (shared) relabels the host path to a container-accessible
                // SELinux type. Shared rather than `:Z` because aoe mounts the
                // credential dir into multiple sandbox containers.
                opts.push("z");
            }
            let mount = if opts.is_empty() {
                format!("{}:{}", vol.host_path, vol.container_path)
            } else {
                format!(
                    "{}:{}:{}",
                    vol.host_path,
                    vol.container_path,
                    opts.join(",")
                )
            };
            args.push("-v".to_string());
            args.push(mount);
        }

        for path in &config.anonymous_volumes {
            args.push("-v".to_string());
            args.push(path.clone());
        }

        if self.supports_named_volumes {
            for nv in &config.named_ignore_volumes {
                args.push("-v".to_string());
                args.push(format!("{}:{}", nv.volume_name, nv.container_path));
            }
        } else if !config.named_ignore_volumes.is_empty() {
            // Apple Container doesn't support named volumes; fall back to anonymous behavior.
            tracing::warn!(
                target: "containers.runtime",
                runtime = %self.name,
                "named volume_ignores_strategy is not supported; falling back to anonymous volumes"
            );
            for nv in &config.named_ignore_volumes {
                args.push("-v".to_string());
                args.push(nv.container_path.clone());
            }
        }

        let (env_argv, _inherit) = docker_env_args(&config.environment);
        args.extend(env_argv);

        for port in &config.port_mappings {
            args.push("-p".to_string());
            args.push(port.clone());
        }

        if let Some(cpu) = &config.cpu_limit {
            args.push("--cpus".to_string());
            args.push(cpu.clone());
        }

        if let Some(mem) = &config.memory_limit {
            args.push("-m".to_string());
            args.push(mem.clone());
        }

        args.push(image.to_string());
        args.push("sleep".to_string());
        args.push("infinity".to_string());

        args
    }

    /// Run the container creation command (after existence has already been checked by the caller).
    pub fn run_create(&self, name: &str, image: &str, config: &ContainerConfig) -> Result<String> {
        let args = self.build_create_args(name, image, config);
        tracing::debug!(target: "containers.runtime", "{} create args: {}", self.name, args.join(" "));

        let mut cmd = self.command();
        cmd.args(&args);
        // Set inherited env vars on the child process so docker can read them
        // via `-e KEY` without the values appearing in argv
        let (_, inherit) = docker_env_args(&config.environment);
        for (key, value) in inherit {
            cmd.env(key, value);
        }
        let output = cmd.output()?;

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
        let output = self.command().args(["start", name]).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DockerError::StartFailed(sanitize_stderr(&stderr)));
        }

        Ok(())
    }

    pub fn stop_container(&self, name: &str) -> Result<()> {
        tracing::info!(target: "containers.runtime", runtime = %self.name, %name, "stopping container");
        let output = self.command().args(["stop", name]).output()?;

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
            // Remove anonymous volumes with the container to prevent orphaned volume buildup.
            // This does NOT affect named volumes (like auth volumes).
            args.push("-v".to_string());
        }
        args.push(name.to_string());

        tracing::debug!(target: "containers.runtime", runtime = %self.name, %name, %force, "removing container");
        let output = self.command().args(&args).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if self.is_not_found(&stderr) {
                return Err(DockerError::ContainerNotFound(name.to_string()));
            }
            return Err(DockerError::RemoveFailed(sanitize_stderr(&stderr)));
        }

        Ok(())
    }

    /// Remove all named ignore volumes whose names start with the given prefix.
    ///
    /// Used to clean up volumes created with `volume_ignores_strategy = "named"` after
    /// a session container is deleted. Volumes can outlive the container, so this must
    /// be called even when the container is already gone.
    ///
    /// This is a no-op on runtimes that don't support named volumes (e.g. Apple Container).
    pub fn remove_named_ignore_volumes(&self, prefix: &str) -> Result<()> {
        if !self.supports_named_volumes {
            return Ok(());
        }

        // List volumes whose names start with the prefix.
        let list_output = self
            .command()
            .args([
                "volume",
                "ls",
                "--filter",
                &format!("name={}", prefix),
                "-q",
            ])
            .output()?;

        if !list_output.status.success() {
            let stderr = String::from_utf8_lossy(&list_output.stderr);
            tracing::warn!(target: "containers.runtime", runtime = %self.name, %prefix, "failed to list named ignore volumes: {}", stderr);
            return Ok(());
        }

        let stdout = String::from_utf8_lossy(&list_output.stdout);
        // Filter in Rust to exact prefix match (docker's --filter is substring-based).
        let names: Vec<&str> = stdout
            .lines()
            .map(str::trim)
            .filter(|n| !n.is_empty() && n.starts_with(prefix))
            .collect();

        if names.is_empty() {
            return Ok(());
        }

        tracing::debug!(target: "containers.runtime", runtime = %self.name, ?names, "removing named ignore volumes");
        let mut rm_args = vec!["volume", "rm"];
        rm_args.extend(names.iter().copied());
        let rm_output = self.command().args(&rm_args).output()?;

        if !rm_output.status.success() {
            let stderr = String::from_utf8_lossy(&rm_output.stderr);
            tracing::warn!(target: "containers.runtime", runtime = %self.name, "failed to remove named ignore volumes: {}", stderr);
        }

        Ok(())
    }

    pub fn exec_command(&self, name: &str, options: Option<&str>, cmd: &str) -> String {
        if let Some(opt_str) = options {
            [self.binary, "exec", "-it", opt_str, name, cmd].join(" ")
        } else {
            [self.binary, "exec", "-it", name, cmd].join(" ")
        }
    }

    pub fn exec(&self, name: &str, cmd: &[&str]) -> Result<std::process::Output> {
        let mut args = vec!["exec", name];
        args.extend(cmd);

        let output = self.command().args(&args).output()?;

        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::container_interface::{EnvEntry, VolumeMount};

    // Real stderr captured from `<runtime> rm/delete <missing>` on 2026-07-01.
    // These pin the per-runtime not-found classification that `remove()` and
    // `stop_container()` rely on to stay idempotent.
    const DOCKER_MISSING: &str =
        "Error response from daemon: No such container: aoe-sandbox-abc123";
    const APPLE_MISSING: &str = "Error: internalError: \"failed to delete container\" (cause: \"notFound: \"container with ID aoe-sandbox-abc123 not found\"\")";
    // Podman not installed locally; representative of its documented output.
    const PODMAN_MISSING: &str =
        "Error: no container with name or ID \"aoe-sandbox-abc123\" found: no such container";

    #[test]
    fn docker_not_found_stderr_classifies() {
        assert!(RuntimeBase::DOCKER.is_not_found(DOCKER_MISSING));
    }

    #[test]
    fn apple_not_found_stderr_classifies() {
        assert!(RuntimeBase::APPLE_CONTAINER.is_not_found(APPLE_MISSING));
    }

    #[test]
    fn podman_not_found_stderr_classifies() {
        assert!(RuntimeBase::PODMAN.is_not_found(PODMAN_MISSING));
    }

    #[test]
    fn genuine_failure_is_not_classified_as_not_found() {
        // A real removal failure must NOT be mistaken for "already gone",
        // which would re-introduce the silent-orphan bug.
        let busy = "Error response from daemon: container is running: stop it first";
        assert!(!RuntimeBase::DOCKER.is_not_found(busy));
        assert!(!RuntimeBase::APPLE_CONTAINER.is_not_found(
            "Error: internalError: \"failed to delete container\" (cause: \"resource busy\")"
        ));
    }

    #[test]
    fn inspect_not_found_stderr_collapses_to_ok_false() {
        // Absent-container stderr from `<runtime> inspect <missing>` must map
        // to Ok(false), so Probe::NotRunning fires and callers can proceed.
        // The three fixture strings are the same as remove/stop uses; sharing
        // the classifier keeps the not-running vs absent collapse consistent
        // across all state-probe paths.
        assert!(matches!(
            RuntimeBase::DOCKER.classify_inspect_failure(DOCKER_MISSING),
            Ok(false)
        ));
        assert!(matches!(
            RuntimeBase::APPLE_CONTAINER.classify_inspect_failure(APPLE_MISSING),
            Ok(false)
        ));
        assert!(matches!(
            RuntimeBase::PODMAN.classify_inspect_failure(PODMAN_MISSING),
            Ok(false)
        ));
    }

    #[test]
    fn inspect_daemon_down_stderr_maps_to_daemon_not_running() {
        // Daemon-unreachable stderr must NOT collapse to Ok(false): that is
        // the exact swallowing-existence-probe failure mode #2596 fixed on
        // the discard path. Surfacing as Err lets classify_running_probe
        // map it to Probe::Unknown so gates fail closed.
        //
        // We route these to DaemonNotRunning (not the generic InspectFailed)
        // because the enum's Display for DaemonNotRunning carries the
        // actionable "Start Docker Desktop or run: sudo systemctl start docker"
        // hint. Mirrors run_create's stderr sniff at the create path.
        //
        // Markers now live in `daemon_down_markers` per-runtime (parallel to
        // `not_found_markers`), so each runtime only matches its own
        // wording; no cross-runtime bleed by construction.
        let docker_daemon_down =
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. \
             Is the docker daemon running?";
        assert!(matches!(
            RuntimeBase::DOCKER.classify_inspect_failure(docker_daemon_down),
            Err(DockerError::DaemonNotRunning)
        ));

        // Podman socket mode (Linux): libpod service unavailable.
        let podman_socket_down = "Error: unable to connect to Podman socket: Connection refused";
        assert!(matches!(
            RuntimeBase::PODMAN.classify_inspect_failure(podman_socket_down),
            Err(DockerError::DaemonNotRunning)
        ));

        // Podman Desktop / machine mode (macOS / Windows): VM stopped.
        let podman_desktop_down = "Cannot connect to Podman. \
             Please verify your connection to the Linux system using \
             `podman system connection list`, or try `podman machine init` \
             and `podman machine start` to manage a new Linux VM";
        assert!(matches!(
            RuntimeBase::PODMAN.classify_inspect_failure(podman_desktop_down),
            Err(DockerError::DaemonNotRunning)
        ));

        // Apple placeholder: real `container` CLI daemon-down wording is
        // not captured in this repo; the marker match is by construction.
        let apple_daemon_down =
            "Error: internalError: \"failed to connect to container daemon\" (cause: \"transient\")";
        assert!(matches!(
            RuntimeBase::APPLE_CONTAINER.classify_inspect_failure(apple_daemon_down),
            Err(DockerError::DaemonNotRunning)
        ));
    }

    #[test]
    fn is_daemon_down_is_cross_runtime_isolated() {
        // Cross-runtime bleed regression guard: Docker's daemon-down stderr
        // must NOT match Podman's or Apple's markers, and vice versa. This
        // is what the daemon_down_markers-per-runtime refactor buys us over
        // the earlier inline-substring implementation.
        let docker_down = "Cannot connect to the Docker daemon at ...";
        assert!(RuntimeBase::DOCKER.is_daemon_down(docker_down));
        assert!(!RuntimeBase::PODMAN.is_daemon_down(docker_down));
        assert!(!RuntimeBase::APPLE_CONTAINER.is_daemon_down(docker_down));

        let podman_down = "Error: unable to connect to Podman socket: ...";
        assert!(RuntimeBase::PODMAN.is_daemon_down(podman_down));
        assert!(!RuntimeBase::DOCKER.is_daemon_down(podman_down));
        assert!(!RuntimeBase::APPLE_CONTAINER.is_daemon_down(podman_down));

        let apple_down =
            "Error: internalError: \"failed to connect to container daemon\" (cause: \"transient\")";
        assert!(RuntimeBase::APPLE_CONTAINER.is_daemon_down(apple_down));
        assert!(!RuntimeBase::DOCKER.is_daemon_down(apple_down));
        assert!(!RuntimeBase::PODMAN.is_daemon_down(apple_down));
    }

    #[test]
    fn inspect_permission_denied_stderr_maps_to_permission_denied() {
        // Docker's canonical Linux socket-permission wording ("Got permission
        // denied while trying to connect to the Docker daemon socket") surfaces
        // the actionable PermissionDenied variant on all three runtimes via
        // per-runtime `permission_denied_markers`. Podman and Apple use the
        // broader "permission denied" placeholder pending real-fixture capture,
        // so the OS-level socket error text matches on those runtimes too.
        let docker_stderr =
            "Got permission denied while trying to connect to the Docker daemon socket \
                             at unix:///var/run/docker.sock";
        assert!(matches!(
            RuntimeBase::DOCKER.classify_inspect_failure(docker_stderr),
            Err(DockerError::PermissionDenied)
        ));

        let podman_stderr = "Error: unable to connect to Podman socket: dial unix \
                             /run/user/1000/podman/podman.sock: connect: permission denied";
        assert!(matches!(
            RuntimeBase::PODMAN.classify_inspect_failure(podman_stderr),
            Err(DockerError::PermissionDenied)
        ));

        // Apple placeholder: no captured wording, but the broad marker still
        // routes generic Linux socket permission errors to PermissionDenied.
        // TODO: replace with captured Apple `container` CLI permission
        // stderr once a real macOS 26 fixture is available (cf. #2655 for
        // the parallel Apple daemon-down fixture follow-up).
        let apple_stderr = "Error: permission denied accessing container socket";
        assert!(matches!(
            RuntimeBase::APPLE_CONTAINER.classify_inspect_failure(apple_stderr),
            Err(DockerError::PermissionDenied)
        ));
    }

    #[test]
    fn is_permission_denied_cross_runtime_isolation_is_asymmetric() {
        // Companion to is_daemon_down_is_cross_runtime_isolated, but the
        // invariant tested here is asymmetric by design: Docker's marker is
        // tightened to the canonical "docker daemon socket" clause, so it
        // isolates cleanly; Podman and Apple use the broad "permission
        // denied" placeholder pending real-fixture capture and thus stay
        // permissive. The asymmetric name flags that this test does NOT
        // assert full three-way isolation like its daemon-down sibling.
        let docker_pd = "Got permission denied while trying to connect to the Docker daemon socket";
        assert!(RuntimeBase::DOCKER.is_permission_denied(docker_pd));
        // Podman and Apple ALSO match "permission denied" (their broader
        // placeholders): this is the pre-#2656 behavior preserved intentionally.
        assert!(RuntimeBase::PODMAN.is_permission_denied(docker_pd));
        assert!(RuntimeBase::APPLE_CONTAINER.is_permission_denied(docker_pd));

        // Conversely, a Podman-only wording that lacks Docker's specific
        // clause MUST NOT match Docker's tight marker.
        let podman_only = "Error: unable to connect to Podman socket: connect: permission denied";
        assert!(!RuntimeBase::DOCKER.is_permission_denied(podman_only));
        assert!(RuntimeBase::PODMAN.is_permission_denied(podman_only));

        // Docker's tightening promise: unrelated "permission denied" errors
        // (image policy, registry auth, volume mount) that a broad substring
        // match would misclassify MUST be rejected. Locks the tightening
        // against future regressions ("just one more case, it will be fine").
        let policy_denied =
            "docker: Error response from daemon: pull access denied: permission denied by policy";
        assert!(!RuntimeBase::DOCKER.is_permission_denied(policy_denied));
    }

    #[test]
    fn inspect_generic_transient_maps_to_inspect_failed() {
        // Any non-not-found, non-daemon-down, non-permission-denied stderr
        // falls through to InspectFailed carrying the raw stderr. This is
        // the generic Probe::Unknown route for "something else went wrong
        // during inspect": the operator sees the underlying runtime message
        // via the warn's error field.
        let stderr = "Error response from daemon: internal server error 500";
        assert!(matches!(
            RuntimeBase::DOCKER.classify_inspect_failure(stderr),
            Err(DockerError::InspectFailed(_))
        ));
    }

    #[test]
    fn test_build_create_args_read_only_supported() {
        let base = RuntimeBase::DOCKER;
        let config = ContainerConfig {
            working_dir: "/workspace/project".to_string(),
            volumes: vec![VolumeMount {
                host_path: "/host/path".to_string(),
                container_path: "/container/path".to_string(),
                read_only: true,
            }],
            anonymous_volumes: vec![],
            named_ignore_volumes: vec![],
            environment: vec![],
            cpu_limit: None,
            memory_limit: None,
            port_mappings: vec![],
            ..Default::default()
        };

        let args = base.build_create_args("test-container", "alpine:latest", &config);

        // Should include :ro suffix
        assert!(args.contains(&"/host/path:/container/path:ro".to_string()));
    }

    #[test]
    fn test_build_create_args_read_only_not_supported() {
        let base = RuntimeBase::APPLE_CONTAINER;
        let config = ContainerConfig {
            working_dir: "/workspace/project".to_string(),
            volumes: vec![VolumeMount {
                host_path: "/host/path".to_string(),
                container_path: "/container/path".to_string(),
                read_only: true,
            }],
            anonymous_volumes: vec![],
            named_ignore_volumes: vec![],
            environment: vec![],
            cpu_limit: None,
            memory_limit: None,
            port_mappings: vec![],
            ..Default::default()
        };

        let args = base.build_create_args("test-container", "alpine:latest", &config);

        // Should NOT include :ro suffix (Apple Container doesn't support it)
        assert!(args.contains(&"/host/path:/container/path".to_string()));
        assert!(!args.iter().any(|a| a.ends_with(":ro")));
    }

    #[test]
    fn test_build_create_args_selinux_relabel() {
        let base = RuntimeBase::PODMAN;
        let config = ContainerConfig {
            working_dir: "/workspace/project".to_string(),
            volumes: vec![
                VolumeMount {
                    host_path: "/host/rw".to_string(),
                    container_path: "/container/rw".to_string(),
                    read_only: false,
                },
                VolumeMount {
                    host_path: "/host/ro".to_string(),
                    container_path: "/container/ro".to_string(),
                    read_only: true,
                },
            ],
            selinux_relabel: true,
            ..Default::default()
        };

        let args = base.build_create_args("test-container", "alpine:latest", &config);

        // Read-write mount gets :z; read-only gets :ro,z.
        assert!(args.contains(&"/host/rw:/container/rw:z".to_string()));
        assert!(args.contains(&"/host/ro:/container/ro:ro,z".to_string()));
    }

    #[test]
    fn test_build_create_args_selinux_relabel_unsupported_runtime() {
        let base = RuntimeBase::APPLE_CONTAINER;
        let config = ContainerConfig {
            working_dir: "/workspace/project".to_string(),
            volumes: vec![VolumeMount {
                host_path: "/host/path".to_string(),
                container_path: "/container/path".to_string(),
                read_only: false,
            }],
            selinux_relabel: true,
            ..Default::default()
        };

        let args = base.build_create_args("test-container", "alpine:latest", &config);

        // Apple Container doesn't support :z; the mount stays plain.
        assert!(args.contains(&"/host/path:/container/path".to_string()));
        assert!(!args.iter().any(|a| a.contains(":z")));
    }

    #[test]
    fn test_exec_command_with_options() {
        let base = RuntimeBase::DOCKER;
        let cmd = base.exec_command("my-container", Some("-w /workspace"), "my-agent");
        assert_eq!(cmd, "docker exec -it -w /workspace my-container my-agent");
    }

    #[test]
    fn test_exec_command_without_options() {
        let base = RuntimeBase::DOCKER;
        let cmd = base.exec_command("my-container", None, "my-agent");
        assert_eq!(cmd, "docker exec -it my-container my-agent");
    }

    #[test]
    fn test_exec_command_apple_container() {
        let base = RuntimeBase::APPLE_CONTAINER;
        let cmd = base.exec_command("my-container", None, "my-agent");
        assert_eq!(cmd, "container exec -it my-container my-agent");
    }

    #[test]
    fn test_build_create_args_full_config() {
        let base = RuntimeBase::DOCKER;
        let config = ContainerConfig {
            working_dir: "/workspace/project".to_string(),
            volumes: vec![VolumeMount {
                host_path: "/src".to_string(),
                container_path: "/dst".to_string(),
                read_only: false,
            }],
            anonymous_volumes: vec!["/tmp/cache".to_string()],
            named_ignore_volumes: vec![],
            environment: vec![EnvEntry::Literal {
                key: "KEY".to_string(),
                value: "VALUE".to_string(),
            }],
            cpu_limit: Some("2".to_string()),
            memory_limit: Some("4g".to_string()),
            port_mappings: vec!["3000:3000".to_string()],
            ..Default::default()
        };

        let args = base.build_create_args("test", "ubuntu:latest", &config);

        assert!(args.contains(&"run".to_string()));
        assert!(args.contains(&"-d".to_string()));
        assert!(args.contains(&"--name".to_string()));
        assert!(args.contains(&"test".to_string()));
        assert!(args.contains(&"-w".to_string()));
        assert!(args.contains(&"/workspace/project".to_string()));
        assert!(args.contains(&"/src:/dst".to_string()));
        assert!(args.contains(&"/tmp/cache".to_string()));
        assert!(args.contains(&"KEY=VALUE".to_string()));
        assert!(args.contains(&"--cpus".to_string()));
        assert!(args.contains(&"2".to_string()));
        assert!(args.contains(&"-m".to_string()));
        assert!(args.contains(&"4g".to_string()));
        assert!(args.contains(&"-p".to_string()));
        assert!(args.contains(&"3000:3000".to_string()));
        assert!(args.contains(&"ubuntu:latest".to_string()));
        assert!(args.contains(&"sleep".to_string()));
        assert!(args.contains(&"infinity".to_string()));
    }

    #[test]
    fn test_build_create_args_inherit_env_no_value_in_argv() {
        let base = RuntimeBase::DOCKER;
        let config = ContainerConfig {
            working_dir: "/workspace".to_string(),
            volumes: vec![],
            anonymous_volumes: vec![],
            named_ignore_volumes: vec![],
            environment: vec![EnvEntry::Inherit {
                key: "GH_TOKEN".to_string(),
                value: "ghp_secret123".to_string(),
            }],
            cpu_limit: None,
            memory_limit: None,
            port_mappings: vec![],
            ..Default::default()
        };

        let args = base.build_create_args("test", "alpine:latest", &config);

        // Should contain just the key, not the value
        assert!(args.contains(&"GH_TOKEN".to_string()));
        assert!(!args.iter().any(|a| a.contains("ghp_secret123")));
    }

    #[test]
    fn test_build_create_args_mixed_env_entries() {
        let base = RuntimeBase::DOCKER;
        let config = ContainerConfig {
            working_dir: "/workspace".to_string(),
            volumes: vec![],
            anonymous_volumes: vec![],
            named_ignore_volumes: vec![],
            environment: vec![
                EnvEntry::Inherit {
                    key: "SECRET".to_string(),
                    value: "s3cr3t".to_string(),
                },
                EnvEntry::Literal {
                    key: "TERM".to_string(),
                    value: "xterm".to_string(),
                },
            ],
            cpu_limit: None,
            memory_limit: None,
            port_mappings: vec![],
            ..Default::default()
        };

        let args = base.build_create_args("test", "alpine:latest", &config);

        // Inherit: just the key
        assert!(args.contains(&"SECRET".to_string()));
        assert!(!args.iter().any(|a| a.contains("s3cr3t")));
        // Literal: key=value
        assert!(args.contains(&"TERM=xterm".to_string()));
    }

    #[test]
    fn test_build_create_args_port_mappings() {
        let base = RuntimeBase::DOCKER;
        let config = ContainerConfig {
            working_dir: "/workspace".to_string(),
            volumes: vec![],
            anonymous_volumes: vec![],
            named_ignore_volumes: vec![],
            environment: vec![],
            cpu_limit: None,
            memory_limit: None,
            port_mappings: vec!["3000:3000".to_string(), "5432:5432".to_string()],
            ..Default::default()
        };

        let args = base.build_create_args("test", "alpine:latest", &config);

        // Both port mappings should appear with -p flags
        let p_indices: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-p")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(p_indices.len(), 2);
        assert_eq!(args[p_indices[0] + 1], "3000:3000");
        assert_eq!(args[p_indices[1] + 1], "5432:5432");
    }

    #[test]
    fn test_named_ignore_volumes_rendered_as_name_colon_path_on_docker() {
        use crate::containers::container_interface::NamedVolumeMount;
        let base = RuntimeBase::DOCKER;
        let config = ContainerConfig {
            working_dir: "/workspace".to_string(),
            volumes: vec![],
            anonymous_volumes: vec![],
            named_ignore_volumes: vec![NamedVolumeMount {
                volume_name: "aoe-vi-sess1-workspace-node_modules-abc123def456".to_string(),
                container_path: "/workspace/node_modules".to_string(),
            }],
            environment: vec![],
            cpu_limit: None,
            memory_limit: None,
            port_mappings: vec![],
            ..Default::default()
        };

        let args = base.build_create_args("test", "alpine:latest", &config);

        let v_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-v")
            .map(|(i, _)| i)
            .collect();
        let volume_args: Vec<&str> = v_positions.iter().map(|&i| args[i + 1].as_str()).collect();

        assert!(
            volume_args.contains(
                &"aoe-vi-sess1-workspace-node_modules-abc123def456:/workspace/node_modules"
            ),
            "Named volume must render as name:/path, got: {:?}",
            volume_args
        );
    }

    #[test]
    fn test_named_ignore_volumes_fall_back_to_anonymous_on_apple_container() {
        use crate::containers::container_interface::NamedVolumeMount;
        let base = RuntimeBase::APPLE_CONTAINER;
        let config = ContainerConfig {
            working_dir: "/workspace".to_string(),
            volumes: vec![],
            anonymous_volumes: vec![],
            named_ignore_volumes: vec![NamedVolumeMount {
                volume_name: "aoe-vi-sess1-workspace-node_modules-abc123".to_string(),
                container_path: "/workspace/node_modules".to_string(),
            }],
            environment: vec![],
            cpu_limit: None,
            memory_limit: None,
            port_mappings: vec![],
            ..Default::default()
        };

        let args = base.build_create_args("test", "alpine:latest", &config);

        let v_positions: Vec<usize> = args
            .iter()
            .enumerate()
            .filter(|(_, a)| *a == "-v")
            .map(|(i, _)| i)
            .collect();
        let volume_args: Vec<&str> = v_positions.iter().map(|&i| args[i + 1].as_str()).collect();

        // Apple Container must use bare path, not name:path
        assert!(
            volume_args.contains(&"/workspace/node_modules"),
            "Apple Container fallback must use bare container path, got: {:?}",
            volume_args
        );
        assert!(
            !volume_args.iter().any(|a| a.contains("aoe-vi-")),
            "Apple Container must not use the volume name in -v args"
        );
    }

    #[test]
    fn test_supports_named_volumes_flags() {
        const { assert!(RuntimeBase::DOCKER.supports_named_volumes) };
        const { assert!(RuntimeBase::PODMAN.supports_named_volumes) };
        const { assert!(!RuntimeBase::APPLE_CONTAINER.supports_named_volumes) };
    }

    #[test]
    #[cfg(unix)]
    fn wait_with_timeout_kills_child_that_outlives_deadline() {
        // A `docker pull` that wedges on the network has no timeout of its own
        // and blocked the caller forever. `sleep 30` stands in for
        // the wedged child; the timeout must fire and kill it.
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        let child = cmd.spawn().unwrap();

        let start = Instant::now();
        let result = wait_with_timeout(child, Duration::from_millis(300)).unwrap();
        assert!(
            result.is_none(),
            "expected the timeout to fire and kill the child"
        );
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "wait should return promptly after the deadline, not block on the child"
        );
    }

    #[test]
    #[cfg(unix)]
    fn wait_with_timeout_bounds_drain_when_grandchild_holds_pipe() {
        // The immediate child (sh) exits fast but backgrounds a `sleep` that
        // inherits stdout, so the pipe never closes. The drain must still
        // return by the deadline rather than blocking on read_to_end. `sleep 30`
        // (>> the 5s assertion) ensures an unbounded recv would visibly fail.
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 30 >&1 & printf done");
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn().unwrap();

        let start = Instant::now();
        let output = wait_with_timeout(child, Duration::from_millis(500))
            .unwrap()
            .expect("the sh child exits quickly, so an Output is produced");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "drain must be bounded by the deadline even while the pipe stays open"
        );
        assert!(output.status.success());
    }

    #[test]
    #[cfg(unix)]
    fn wait_with_timeout_returns_output_for_fast_child() {
        let mut cmd = Command::new("printf");
        cmd.arg("hello");
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn().unwrap();

        let output = wait_with_timeout(child, Duration::from_secs(10))
            .unwrap()
            .expect("fast child should complete before the timeout");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"hello");
    }
}
