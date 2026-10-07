#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VolumeMount {
    pub host_path: String,
    pub container_path: String,
    pub read_only: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InspectedMount {
    pub(crate) kind: String,
    pub(crate) name: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) container_path: std::path::PathBuf,
    pub(crate) read_only: bool,
}

/// Actual runtime state, never inferred from the desired create configuration.
#[derive(Clone, Debug)]
pub(crate) struct InspectedContainer {
    pub(crate) id: String,
    pub(crate) running: bool,
    pub(crate) bind_mounts: Vec<VolumeMount>,
    pub(crate) ordinary_mounts: Vec<InspectedMount>,
    pub(crate) opaque_mounts: Vec<InspectedMount>,
    /// Apple runtime plugin name; absence on Docker/Podman is not a plugin guess.
    pub(crate) runtime_handler: Option<String>,
}

pub(crate) fn host_path_for_mounts<'a>(
    volumes: &[VolumeMount],
    shadow_mounts: impl Iterator<Item = &'a str>,
    container_path: &std::path::Path,
    writable: bool,
) -> Option<std::path::PathBuf> {
    if container_path
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    let (volume, relative) = volumes
        .iter()
        .filter_map(|volume| {
            container_path
                .strip_prefix(std::path::Path::new(&volume.container_path))
                .ok()
                .map(|relative| (volume, relative))
        })
        .max_by_key(|(volume, _)| {
            std::path::Path::new(&volume.container_path)
                .components()
                .count()
        })?;
    let bind_depth = std::path::Path::new(&volume.container_path)
        .components()
        .count();
    let shadow_depth = shadow_mounts
        .filter_map(|mounted| {
            container_path
                .strip_prefix(std::path::Path::new(mounted))
                .ok()
                .map(|_| std::path::Path::new(mounted).components().count())
        })
        .max();
    if shadow_depth.is_some_and(|depth| depth >= bind_depth) || (writable && volume.read_only) {
        return None;
    }
    Some(std::path::Path::new(&volume.host_path).join(relative))
}

pub struct NamedVolumeMount {
    pub volume_name: String,
    pub container_path: String,
}

/// `Inherit` passes the value through the process environment (`-e KEY`) so secrets stay
/// out of `ps`; `Literal` emits `-e KEY=VALUE`.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvEntry {
    Inherit { key: String, value: String },
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

/// The caller must set the returned inherit pairs on the spawning `Command`. Dedupes by
/// key, first wins.
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFlag {
    Privileged,
    CapAdd,
    CapDrop,
    SecurityOpt,
}

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
    pub named_ignore_volumes: Vec<NamedVolumeMount>,
    /// False unless the paths come from the real project layout; gates volume reclaim.
    pub named_ignore_volumes_authoritative: bool,
    pub environment: Vec<EnvEntry>,
    pub cpu_limit: Option<String>,
    pub memory_limit: Option<String>,
    pub port_mappings: Vec<String>,
    pub network: Option<String>,
    pub selinux_relabel: bool,
    pub identity_publisher_installed: bool,
    /// Labelled at create so a container built before a file was shared can be told apart.
    pub shared_credential_mounts: Vec<String>,
    /// Labelled at create so a container reused after a tool swap can be told apart.
    pub agent_tool: String,
    pub run_policy: RunPolicy,
}

pub(crate) const SHARED_CREDENTIAL_MOUNTS_LABEL: &str =
    "com.agent-of-empires.shared-credential-mounts";

pub(crate) const AGENT_TOOL_LABEL: &str = "com.agent-of-empires.agent-tool";

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

    pub(crate) fn shared_credential_label(&self) -> String {
        self.shared_credential_mounts.join(",")
    }

    pub(crate) fn host_path_for_container_path(
        &self,
        container_path: &std::path::Path,
        writable: bool,
    ) -> Option<std::path::PathBuf> {
        host_path_for_mounts(
            &self.volumes,
            self.anonymous_volumes.iter().map(String::as_str).chain(
                self.named_ignore_volumes
                    .iter()
                    .map(|volume| volume.container_path.as_str()),
            ),
            container_path,
            writable,
        )
    }

    pub(crate) fn path_is_mounted(
        &self,
        host_path: &std::path::Path,
        container_path: &std::path::Path,
        writable: bool,
    ) -> bool {
        self.host_path_for_container_path(container_path, writable)
            .is_some_and(|mapped| mapped == host_path)
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
    fn docker_env_args_keeps_order_hides_inherited_values_and_takes_the_first_key() {
        let inherit = |key: &str, value: &str| EnvEntry::Inherit {
            key: key.to_string(),
            value: value.to_string(),
        };
        let literal = |key: &str, value: &str| EnvEntry::Literal {
            key: key.to_string(),
            value: value.to_string(),
        };
        // (entries, expected argv, expected inherited pairs, values that must not reach argv)
        type EnvCase = (
            Vec<EnvEntry>,
            &'static [&'static str],
            &'static [(&'static str, &'static str)],
            &'static [&'static str],
        );
        let cases: [EnvCase; 5] = [
            (
                vec![inherit("GH_TOKEN", "ghp_secret")],
                &["-e", "GH_TOKEN"],
                &[("GH_TOKEN", "ghp_secret")],
                &["ghp_secret"],
            ),
            (
                vec![literal("TERM", "xterm-256color")],
                &["-e", "TERM=xterm-256color"],
                &[],
                &[],
            ),
            (
                vec![
                    inherit("SECRET", "s3cr3t"),
                    literal("TERM", "xterm"),
                    inherit("TOKEN", "tok"),
                ],
                &["-e", "SECRET", "-e", "TERM=xterm", "-e", "TOKEN"],
                &[("SECRET", "s3cr3t"), ("TOKEN", "tok")],
                &["s3cr3t"],
            ),
            (vec![], &[], &[], &[]),
            (
                vec![
                    inherit("GH_TOKEN", "ghp_first"),
                    literal("GH_TOKEN", "literal_should_be_skipped"),
                    inherit("OTHER", "kept"),
                ],
                &["-e", "GH_TOKEN", "-e", "OTHER"],
                &[("GH_TOKEN", "ghp_first"), ("OTHER", "kept")],
                &["literal_should_be_skipped"],
            ),
        ];
        for (entries, expected_argv, expected_inherit, secrets) in cases {
            let (argv, inherited) = docker_env_args(&entries);
            assert_eq!(argv, expected_argv);
            let expected_inherit: Vec<(String, String)> = expected_inherit
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            assert_eq!(inherited, expected_inherit);
            for secret in secrets {
                assert!(
                    !argv.iter().any(|a| a.contains(secret)),
                    "{secret} leaked into argv"
                );
            }
        }
    }

    #[test]
    fn container_path_mapping_respects_shadow_volumes() {
        let mut config = ContainerConfig::default();
        config.volumes.push(VolumeMount {
            host_path: "/host/project".to_string(),
            container_path: "/workspace/project".to_string(),
            read_only: false,
        });
        let source = std::path::Path::new("/workspace/project/src/lib.rs");
        assert_eq!(
            config.host_path_for_container_path(source, false),
            Some(std::path::PathBuf::from("/host/project/src/lib.rs"))
        );
        assert_eq!(
            config.host_path_for_container_path(
                std::path::Path::new("/workspace/project/../other/file.jsonl"),
                false
            ),
            None,
            "a traversal component must not resolve outside the matched bind"
        );

        let settings = std::path::Path::new("/workspace/project/.prime/agent/settings.json");
        config
            .anonymous_volumes
            .push("/workspace/project/.prime".to_string());
        assert_eq!(config.host_path_for_container_path(settings, false), None);

        config.anonymous_volumes.clear();
        config.named_ignore_volumes.push(NamedVolumeMount {
            volume_name: "aoe-prime-shadow".to_string(),
            container_path: "/workspace/project/.prime".to_string(),
        });
        assert_eq!(config.host_path_for_container_path(settings, false), None);
        assert_eq!(
            config.host_path_for_container_path(source, false),
            Some(std::path::PathBuf::from("/host/project/src/lib.rs"))
        );
    }
}
