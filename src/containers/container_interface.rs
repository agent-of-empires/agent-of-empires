#[derive(Clone, Debug)]
pub struct VolumeMount {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

/// A named Docker/Podman volume mounted at a specific container path.
/// Used by `volume_ignores_strategy = "named"` to bypass VirtioFS shadowing on macOS.
pub struct NamedVolumeMount {
    pub volume_name: String,
    pub container_path: String,
}

/// An environment variable entry for a container.
///
/// `Inherit` entries use Docker's `-e KEY` form (no value in argv), which reads
/// the value from the calling process's environment. This prevents secrets from
/// leaking into `ps` output.
///
/// `Literal` entries use `-e KEY=VALUE` and are appropriate for non-secret,
/// hard-coded values.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvEntry {
    /// Value inherited from host environment. Only the key appears in argv;
    /// the value is passed to Docker via the process environment.
    Inherit { key: String, value: String },
    /// Literal (non-secret) value. Both key and value appear in argv.
    Literal { key: String, value: String },
}

impl EnvEntry {
    pub fn key(&self) -> &str {
        match self {
            EnvEntry::Inherit { key, .. } | EnvEntry::Literal { key, .. } => key,
        }
    }

    pub fn value(&self) -> &str {
        match self {
            EnvEntry::Inherit { value, .. } | EnvEntry::Literal { value, .. } => value,
        }
    }
}

/// Translate env entries into docker `-e` argv flags plus an inherit list.
///
/// For each `Inherit` entry, pushes `-e KEY` to argv and `(KEY, value)` to the
/// returned inherit list; the caller must apply the inherit pairs to the
/// spawning process's environment via `Command::env(k, v)` so docker can
/// resolve the bare `-e KEY` flag without the value ever appearing in argv
/// or `ps` output. For each `Literal` entry, pushes `-e KEY=VALUE` to argv.
///
/// Both the create path (`docker run`) and every exec path (`docker exec` from
/// tmux sessions, ACP agent spawn, and ACP `terminal/create`) share this
/// translation. Keeping it in one place ensures they cannot drift.
///
/// Dedupes by key (first wins). `collect_environment` already dedupes its
/// output, but the helper repeats the check so any caller that builds its
/// own entry list cannot accidentally emit two `-e KEY` flags for the same
/// key (which docker accepts but with last-write-wins semantics that aren't
/// always intended).
pub fn docker_env_args(entries: &[EnvEntry]) -> (Vec<String>, Vec<(String, String)>) {
    let mut argv = Vec::with_capacity(entries.len() * 2);
    let mut inherit = Vec::new();
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for entry in entries {
        let key = entry.key();
        if !seen.insert(key) {
            continue;
        }
        argv.push("-e".to_string());
        match entry {
            EnvEntry::Inherit { key, value } => {
                argv.push(key.clone());
                inherit.push((key.clone(), value.clone()));
            }
            EnvEntry::Literal { key, value } => {
                argv.push(format!("{}={}", key, value));
            }
        }
    }
    (argv, inherit)
}

/// A Docker-style `run` flag supported by a container runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFlag {
    Privileged,
    CapAdd,
    CapDrop,
    SecurityOpt,
}

/// Container run-policy flags mapped from `[sandbox]` settings.
#[derive(Debug, Default, Clone)]
pub struct RunPolicy {
    pub privileged: bool,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    pub security_opt: Vec<String>,
    pub extra_run_args: Vec<String>,
}

#[derive(Default)]
pub struct ContainerConfig {
    pub working_dir: String,
    pub volumes: Vec<VolumeMount>,
    pub anonymous_volumes: Vec<String>,
    /// Named volumes for volume_ignores when strategy = "named". Cleaned up explicitly on session delete.
    pub named_ignore_volumes: Vec<NamedVolumeMount>,
    pub environment: Vec<EnvEntry>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
    pub port_mappings: Vec<String>,
    /// Container network mode passed to `--network`. `None` uses the runtime
    /// default (bridge). Set from `sandbox.network`; `bridge` and the rejected
    /// `host` value are normalized to `None` before reaching here.
    pub network: Option<String>,
    /// Append the SELinux relabel flag (`:z`) to host bind mounts so the container
    /// can access them on SELinux-enforcing hosts (Fedora, RHEL). Set from
    /// `sandbox.selinux_relabel`; only emitted for runtimes that support it.
    pub selinux_relabel: bool,
    /// Runtime-only evidence that this config installed an identity publisher.
    pub identity_publisher_installed: bool,
    pub run_policy: RunPolicy,
}

impl ContainerConfig {
    pub(crate) fn mount_fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};

        let mut digest = Sha256::new();
        for volume in &self.volumes {
            for value in [&volume.host_path, &volume.container_path] {
                digest.update((value.len() as u64).to_le_bytes());
                digest.update(value.as_bytes());
            }
            digest.update([u8::from(volume.read_only)]);
        }
        if let Some(home) = self.environment.iter().find(|entry| entry.key() == "HOME") {
            digest.update(b"HOME");
            digest.update((home.value().len() as u64).to_le_bytes());
            digest.update(home.value().as_bytes());
        } else {
            digest.update(b"NO_HOME");
        }
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub(crate) fn path_is_mounted(
        &self,
        host_path: &std::path::Path,
        container_path: &std::path::Path,
        writable: bool,
    ) -> bool {
        self.volumes
            .iter()
            .filter_map(|volume| {
                container_path
                    .strip_prefix(std::path::Path::new(&volume.container_path))
                    .ok()
                    .map(|relative| (volume, relative))
            })
            .max_by_key(|(volume, _)| volume.container_path.len())
            .is_some_and(|(volume, relative)| {
                (!writable || !volume.read_only)
                    && std::path::Path::new(&volume.host_path).join(relative) == host_path
            })
    }

    pub(crate) fn uses_default_container_home(&self) -> bool {
        self.environment
            .iter()
            .find(|entry| entry.key() == "HOME")
            .is_some_and(|entry| entry.value() == "/root")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publisher_home_must_be_explicit_and_is_fingerprinted() {
        let missing = ContainerConfig::default();
        assert!(!missing.uses_default_container_home());

        let mut root = ContainerConfig::default();
        root.environment.push(EnvEntry::Literal {
            key: "HOME".to_string(),
            value: "/root".to_string(),
        });
        assert!(root.uses_default_container_home());

        let mut alternate = ContainerConfig::default();
        alternate.environment.push(EnvEntry::Literal {
            key: "HOME".to_string(),
            value: "/alternate".to_string(),
        });
        assert!(!alternate.uses_default_container_home());
        assert_ne!(root.mount_fingerprint(), alternate.mount_fingerprint());
        assert_ne!(missing.mount_fingerprint(), root.mount_fingerprint());
    }

    #[test]
    fn docker_env_args_inherit_keeps_value_out_of_argv() {
        let entries = vec![EnvEntry::Inherit {
            key: "GH_TOKEN".to_string(),
            value: "ghp_secret".to_string(),
        }];
        let (argv, inherit) = docker_env_args(&entries);
        assert_eq!(argv, vec!["-e".to_string(), "GH_TOKEN".to_string()]);
        assert_eq!(
            inherit,
            vec![("GH_TOKEN".to_string(), "ghp_secret".to_string())]
        );
        assert!(
            !argv.iter().any(|a| a.contains("ghp_secret")),
            "secret leaked into argv"
        );
    }

    #[test]
    fn docker_env_args_literal_emits_key_eq_value() {
        let entries = vec![EnvEntry::Literal {
            key: "TERM".to_string(),
            value: "xterm-256color".to_string(),
        }];
        let (argv, inherit) = docker_env_args(&entries);
        assert_eq!(
            argv,
            vec!["-e".to_string(), "TERM=xterm-256color".to_string()]
        );
        assert!(inherit.is_empty());
    }

    #[test]
    fn docker_env_args_mixed_preserves_order() {
        let entries = vec![
            EnvEntry::Inherit {
                key: "SECRET".to_string(),
                value: "s3cr3t".to_string(),
            },
            EnvEntry::Literal {
                key: "TERM".to_string(),
                value: "xterm".to_string(),
            },
            EnvEntry::Inherit {
                key: "TOKEN".to_string(),
                value: "tok".to_string(),
            },
        ];
        let (argv, inherit) = docker_env_args(&entries);
        assert_eq!(
            argv,
            vec![
                "-e".to_string(),
                "SECRET".to_string(),
                "-e".to_string(),
                "TERM=xterm".to_string(),
                "-e".to_string(),
                "TOKEN".to_string(),
            ]
        );
        assert_eq!(
            inherit,
            vec![
                ("SECRET".to_string(), "s3cr3t".to_string()),
                ("TOKEN".to_string(), "tok".to_string()),
            ]
        );
    }

    #[test]
    fn docker_env_args_empty() {
        let (argv, inherit) = docker_env_args(&[]);
        assert!(argv.is_empty());
        assert!(inherit.is_empty());
    }

    #[test]
    fn docker_env_args_dedupes_duplicate_keys_first_wins() {
        // Guards against a caller that hand-builds entries and accidentally
        // passes the same key twice. Docker accepts duplicate `-e` flags
        // with last-write-wins, which is rarely what the caller meant.
        let entries = vec![
            EnvEntry::Inherit {
                key: "GH_TOKEN".to_string(),
                value: "ghp_first".to_string(),
            },
            EnvEntry::Literal {
                key: "GH_TOKEN".to_string(),
                value: "literal_should_be_skipped".to_string(),
            },
            EnvEntry::Inherit {
                key: "OTHER".to_string(),
                value: "kept".to_string(),
            },
        ];
        let (argv, inherit) = docker_env_args(&entries);
        // First GH_TOKEN entry wins; the literal duplicate is dropped.
        assert_eq!(
            argv,
            vec![
                "-e".to_string(),
                "GH_TOKEN".to_string(),
                "-e".to_string(),
                "OTHER".to_string(),
            ]
        );
        assert_eq!(
            inherit,
            vec![
                ("GH_TOKEN".to_string(), "ghp_first".to_string()),
                ("OTHER".to_string(), "kept".to_string()),
            ]
        );
        assert!(
            !argv.iter().any(|a| a.contains("literal_should_be_skipped")),
            "duplicate key's value leaked into argv"
        );
    }
}
