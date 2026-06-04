use super::container_interface::{docker_env_args, ContainerConfig};
use super::error::{DockerError, Result};
use std::process::Command;

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
    };

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

    pub fn get_version(&self) -> Result<String> {
        let output = self.command().arg("--version").output()?;

        if !output.status.success() {
            return Err(DockerError::NotInstalled);
        }

        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
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
        let start = std::time::Instant::now();
        tracing::info!(target: "containers.image", runtime = %self.name, %image, "pulling image");
        let output = cmd.output()?;
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
            if stderr.contains("permission denied") {
                return Err(DockerError::PermissionDenied);
            }
            if stderr.contains("Cannot connect to the Docker daemon") {
                return Err(DockerError::DaemonNotRunning);
            }
            if stderr.contains("No such image") || stderr.contains("Unable to find image") {
                return Err(DockerError::ImageNotFound(image.to_string()));
            }
            return Err(DockerError::CreateFailed(stderr.to_string()));
        }

        let container_id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        Ok(container_id)
    }

    pub fn start_container(&self, name: &str) -> Result<()> {
        tracing::info!(target: "containers.runtime", runtime = %self.name, %name, "starting container");
        let output = self.command().args(["start", name]).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(DockerError::StartFailed(stderr.to_string()));
        }

        Ok(())
    }

    pub fn stop_container(&self, name: &str) -> Result<()> {
        tracing::info!(target: "containers.runtime", runtime = %self.name, %name, "stopping container");
        let output = self.command().args(["stop", name]).output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("No such container") {
                return Err(DockerError::ContainerNotFound(name.to_string()));
            }
            return Err(DockerError::StopFailed(stderr.to_string()));
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
            if stderr.contains("No such container") {
                return Err(DockerError::ContainerNotFound(name.to_string()));
            }
            return Err(DockerError::RemoveFailed(stderr.to_string()));
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
}
