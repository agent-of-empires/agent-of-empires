//! Runtime endpoints, inspected mounts, and transport for a checked launch.

use serde_json::Value;

use super::runtime::RuntimeKind;
use super::ContainerRuntime;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RuntimeExecutionSnapshot {
    pub(crate) kind: crate::session::ContainerRuntimeName,
    pub(crate) program: std::path::PathBuf,
    pub(crate) cwd: std::path::PathBuf,
    pub(crate) endpoint: String,
    pub(crate) local_mounts: bool,
    pub(crate) routing: Vec<(String, Option<String>)>,
    /// Frozen transport flags for the inspected Docker endpoint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) global_arguments: Vec<String>,
}

const RUNTIME_ROUTING_KEYS: &[&str] = &[
    "HOME",
    "PATH",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_RUNTIME_DIR",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_TLS",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
    "CONTAINER_HOST",
    "CONTAINER_CONNECTION",
    "CONTAINER_SSHKEY",
    "CONTAINERS_CONF",
    "CONTAINERS_CONF_OVERRIDE",
    "CONTAINERS_CONF_MODULES",
    "CONTAINERS_STORAGE_CONF",
    "SSH_AUTH_SOCK",
];

impl RuntimeExecutionSnapshot {
    fn runtime(&self) -> ContainerRuntime {
        match self.kind {
            crate::session::ContainerRuntimeName::Docker => ContainerRuntime::docker(),
            crate::session::ContainerRuntimeName::Podman => ContainerRuntime::podman(),
            crate::session::ContainerRuntimeName::AppleContainer => {
                ContainerRuntime::apple_container()
            }
        }
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.routing
            .iter()
            .find(|(name, _)| name == key)
            .and_then(|(_, value)| value.as_deref())
            .filter(|value| !value.is_empty())
    }

    fn set(&mut self, key: &str, value: Option<String>) {
        let entry = self
            .routing
            .iter_mut()
            .find(|(name, _)| name == key)
            .expect("runtime routing key is declared");
        entry.1 = value;
    }

    pub(crate) fn command(&self, args: &[String]) -> std::process::Command {
        let mut command = std::process::Command::new(&self.program);
        command
            .current_dir(&self.cwd)
            .args(&self.global_arguments)
            .args(args);
        for (key, value) in &self.routing {
            if let Some(value) = value {
                command.env(key, value);
            } else {
                command.env_remove(key);
            }
        }
        command
    }

    fn probe(&self, args: &[&str]) -> anyhow::Result<Vec<u8>> {
        let args = args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>();
        let output = self.runtime().base.probe_output(&mut self.command(&args))?;
        anyhow::ensure!(
            output.status.success(),
            "runtime context probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        anyhow::ensure!(
            output.stdout.len() <= 2 * 1024 * 1024,
            "runtime context probe exceeded its size limit"
        );
        Ok(output.stdout)
    }

    fn probe_json(&self, args: &[&str]) -> anyhow::Result<Value> {
        Ok(serde_json::from_slice(&self.probe(args)?)?)
    }

    fn validate_endpoint(endpoint: &str) -> anyhow::Result<()> {
        let authority = endpoint
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next().unwrap_or_default());
        anyhow::ensure!(!authority.and_then(|authority| authority.split_once('@'))
            .is_some_and(|(user, _)| user.contains(':')),
            "managed runtime endpoints cannot embed credentials; use a configured transport identity");
        anyhow::ensure!(
            !endpoint.contains(['\n', '\r', '\0']),
            "invalid runtime endpoint"
        );
        Ok(())
    }

    pub(crate) fn capture(runtime: &ContainerRuntime) -> anyhow::Result<Self> {
        use crate::session::ContainerRuntimeName as Name;
        let routing = RUNTIME_ROUTING_KEYS
            .iter()
            .map(|key| {
                let value = std::env::var_os(key)
                    .map(|value| {
                        value.into_string().map_err(|_| {
                            anyhow::anyhow!("runtime routing variable {key} is not UTF-8")
                        })
                    })
                    .transpose()?;
                Ok(((*key).to_owned(), value))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let path = routing
            .iter()
            .find(|(key, _)| key == "PATH")
            .and_then(|(_, value)| value.as_deref());
        let cwd = std::env::current_dir()?;
        let program = which::which_in(runtime.base.binary, path, &cwd)?;
        anyhow::ensure!(
            program.to_str().is_some(),
            "runtime executable path is not UTF-8"
        );
        let mut snapshot = Self {
            kind: match runtime.kind {
                RuntimeKind::Docker => Name::Docker,
                RuntimeKind::Podman => Name::Podman,
                RuntimeKind::AppleContainer => Name::AppleContainer,
            },
            program,
            cwd,
            endpoint: String::new(),
            local_mounts: false,
            routing,
            global_arguments: Vec::new(),
        };
        match snapshot.kind {
            Name::Docker => {
                if snapshot.value("DOCKER_CONTEXT").is_none()
                    && snapshot.value("DOCKER_HOST").is_some()
                {
                    snapshot.endpoint = snapshot.value("DOCKER_HOST").unwrap().to_owned();
                } else {
                    let context =
                        snapshot.probe_json(&["context", "inspect", "--format", "{{json .}}"])?;
                    snapshot.endpoint = context
                        .pointer("/Endpoints/docker/Host")
                        .and_then(Value::as_str)
                        .ok_or_else(|| anyhow::anyhow!("Docker context has no resolved endpoint"))?
                        .to_owned();
                    if context.get("Name").and_then(Value::as_str) != Some("default") {
                        snapshot.set("DOCKER_TLS", None);
                        snapshot.set("DOCKER_TLS_VERIFY", None);
                        snapshot.set("DOCKER_CERT_PATH", None);
                        let material = context.pointer("/TLSMaterial/docker");
                        let files = material.and_then(Value::as_array);
                        let skip = context
                            .pointer("/Endpoints/docker/SkipTLSVerify")
                            .and_then(Value::as_bool)
                            == Some(true);
                        if material.is_some() || skip {
                            snapshot.global_arguments.push("--tls".to_owned());
                            snapshot
                                .global_arguments
                                .push(format!("--tlsverify={}", !skip));
                            for (flag, file) in [
                                ("--tlscacert", "ca.pem"),
                                ("--tlscert", "cert.pem"),
                                ("--tlskey", "key.pem"),
                            ] {
                                let declared = files.is_some_and(|files| {
                                    files.iter().any(|entry| entry.as_str() == Some(file))
                                });
                                let path = if declared {
                                    let directory = context
                                        .pointer("/Storage/TLSPath")
                                        .and_then(Value::as_str)
                                        .ok_or_else(|| {
                                            anyhow::anyhow!(
                                                "Docker context has no TLS storage path"
                                            )
                                        })?;
                                    let path =
                                        std::path::Path::new(directory).join("docker").join(file);
                                    anyhow::ensure!(
                                        path.is_file(),
                                        "Docker context TLS file is missing: {}",
                                        path.display()
                                    );
                                    path.to_str()
                                        .ok_or_else(|| {
                                            anyhow::anyhow!("Docker TLS path is not UTF-8")
                                        })?
                                        .to_owned()
                                } else {
                                    // Empty paths disable Docker's ambient certificate defaults.
                                    String::new()
                                };
                                snapshot.global_arguments.push(format!("{flag}={path}"));
                            }
                        }
                    }
                }
                Self::validate_endpoint(&snapshot.endpoint)?;
                snapshot.set("DOCKER_HOST", Some(snapshot.endpoint.clone()));
                snapshot.set("DOCKER_CONTEXT", None);
                snapshot.local_mounts =
                    snapshot
                        .endpoint
                        .strip_prefix("unix://")
                        .is_some_and(|socket| {
                            let socket = std::path::Path::new(socket);
                            ["/var/run/docker.sock", "/run/docker.sock"]
                                .iter()
                                .any(|known| socket == std::path::Path::new(known))
                                || snapshot.value("XDG_RUNTIME_DIR").is_some_and(|dir| {
                                    socket == std::path::Path::new(dir).join("docker.sock")
                                })
                                || snapshot.value("HOME").is_some_and(|home| {
                                    [".docker/run/docker.sock", ".docker/desktop/docker.sock"]
                                        .iter()
                                        .any(|suffix| {
                                            socket == std::path::Path::new(home).join(suffix)
                                        })
                                })
                        });
            }
            Name::Podman => {
                let remote = snapshot
                    .probe_json(&["info", "--format", "{{json .Host.ServiceIsRemote}}"])?
                    .as_bool()
                    .ok_or_else(|| {
                        anyhow::anyhow!("Podman did not establish transport locality")
                    })?;
                if !remote {
                    snapshot.endpoint = "local://podman".into();
                    snapshot.local_mounts = true;
                } else {
                    if let Some(endpoint) = snapshot.value("CONTAINER_HOST") {
                        snapshot.endpoint = endpoint.to_owned();
                    } else {
                        let connections = snapshot.probe_json(&[
                            "system",
                            "connection",
                            "list",
                            "--format",
                            "json",
                        ])?;
                        let selected = connections
                            .as_array()
                            .and_then(|connections| {
                                connections.iter().find(|connection| {
                                    match snapshot.value("CONTAINER_CONNECTION") {
                                        Some(name) => {
                                            connection.get("Name").and_then(Value::as_str)
                                                == Some(name)
                                        }
                                        None => {
                                            connection.get("Default").and_then(Value::as_bool)
                                                == Some(true)
                                        }
                                    }
                                })
                            })
                            .ok_or_else(|| {
                                anyhow::anyhow!("Podman connection endpoint is not established")
                            })?;
                        snapshot.endpoint = selected
                            .get("URI")
                            .and_then(Value::as_str)
                            .ok_or_else(|| anyhow::anyhow!("Podman connection has no URI"))?
                            .to_owned();
                        if snapshot.value("CONTAINER_SSHKEY").is_none() {
                            snapshot.set(
                                "CONTAINER_SSHKEY",
                                selected
                                    .get("Identity")
                                    .and_then(Value::as_str)
                                    .filter(|value| !value.is_empty())
                                    .map(str::to_owned),
                            );
                        }
                    }
                    Self::validate_endpoint(&snapshot.endpoint)?;
                    snapshot.set("CONTAINER_HOST", Some(snapshot.endpoint.clone()));
                    snapshot.set("CONTAINER_CONNECTION", None);
                }
            }
            Name::AppleContainer => {
                snapshot.endpoint = "local://apple-container".into();
                snapshot.local_mounts = true;
            }
        }
        Ok(snapshot)
    }

    pub(crate) fn exec(&self, name: &str, cwd: &str, args: &[String]) -> std::process::Command {
        let argv = self.runtime().build_exec_argv(name, cwd, args);
        self.command(&argv[1..])
    }

    pub(crate) fn exec_shell_command(
        &self,
        name: &str,
        options: Option<&str>,
        command: &str,
    ) -> String {
        let runtime = self.runtime();
        let command = runtime.exec_command(name, options, command);
        format!(
            "{}{}{}",
            crate::session::environment::shell_escape(
                self.program.to_str().expect("validated runtime path")
            ),
            self.global_arguments
                .iter()
                .map(|arg| format!(
                    " {}",
                    crate::session::environment::shell_escape_script_word(arg)
                ))
                .collect::<String>(),
            command
                .strip_prefix(runtime.base.binary)
                .expect("runtime command starts with its binary")
        )
    }

    pub(crate) fn canonical_path(
        &self,
        name: &str,
        path: &std::path::Path,
    ) -> anyhow::Result<std::path::PathBuf> {
        anyhow::ensure!(path.is_absolute(), "container path must be absolute");
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("container path is not UTF-8"))?;
        let script = r#"PATH=/usr/bin:/bin; export PATH
if [ -e "$1" ] || [ -L "$1" ]; then exec readlink -f -- "$1"; fi
p=$1; suffix=
while [ ! -d "$p" ]; do
  [ ! -L "$p" ] || exit 1
  suffix=/${p##*/}$suffix
  p=${p%/*}; [ -n "$p" ] || p=/
done
cd -P -- "$p" || exit 1
printf '%s%s\n' "$PWD" "$suffix""#;
        let args = ["/bin/sh", "-c", script, "aoe-path", path].map(str::to_owned);
        let output = self
            .runtime()
            .base
            .probe_output(&mut self.exec(name, "/", &args))?;
        anyhow::ensure!(
            output.status.success(),
            "container path could not be resolved"
        );
        let output = String::from_utf8(output.stdout)?;
        let path = std::path::PathBuf::from(output.strip_suffix('\n').unwrap_or(&output));
        anyhow::ensure!(
            path.is_absolute(),
            "container path resolver returned a relative path"
        );
        Ok(path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ContainerExecutionSnapshot {
    pub(crate) runtime: RuntimeExecutionSnapshot,
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) mounts: Vec<super::VolumeMount>,
    pub(crate) shadow_mounts: Vec<String>,
}

impl ContainerExecutionSnapshot {
    pub(crate) fn capture(runtime: RuntimeExecutionSnapshot, name: &str) -> anyhow::Result<Self> {
        use crate::session::ContainerRuntimeName as Name;
        let inspected = match runtime.kind {
            Name::Docker | Name::Podman => runtime.probe_json(&[
                "container",
                "inspect",
                "--format",
                "{\"id\":{{json .Id}},\"mounts\":{{json .Mounts}}}",
                name,
            ])?,
            Name::AppleContainer => runtime.probe_json(&["inspect", name])?,
        };
        let (id, mounts) = match runtime.kind {
            Name::Docker | Name::Podman => (inspected.get("id"), inspected.get("mounts")),
            Name::AppleContainer => (
                inspected
                    .pointer("/0/id")
                    .or_else(|| inspected.pointer("/0/configuration/id")),
                inspected.pointer("/0/configuration/mounts"),
            ),
        };
        let id = id
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("container inspect has no identity"))?
            .to_owned();
        let mounts = mounts
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("container inspect has no mount snapshot"))?;
        let mut binds = Vec::new();
        let mut shadows = Vec::new();
        for mount in mounts {
            let apple = runtime.kind == Name::AppleContainer;
            let destination = mount
                .get(if apple { "destination" } else { "Destination" })
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("inspected mount has no destination"))?;
            anyhow::ensure!(
                std::path::Path::new(destination).is_absolute(),
                "inspected mount destination is not absolute"
            );
            let bind = if apple {
                mount.pointer("/type/virtiofs").is_some()
            } else {
                mount.get("Type").and_then(Value::as_str) == Some("bind")
            };
            if bind {
                let source = mount
                    .get(if apple { "source" } else { "Source" })
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("inspected bind has no source"))?;
                anyhow::ensure!(
                    std::path::Path::new(source).is_absolute(),
                    "inspected bind source is not absolute"
                );
                let read_only = if apple {
                    mount
                        .get("options")
                        .and_then(Value::as_array)
                        .ok_or_else(|| anyhow::anyhow!("inspected share has no mount options"))?
                        .iter()
                        .any(|option| option.as_str() == Some("ro"))
                } else {
                    !mount
                        .get("RW")
                        .and_then(Value::as_bool)
                        .ok_or_else(|| anyhow::anyhow!("inspected bind has no access mode"))?
                };
                binds.push(super::VolumeMount {
                    host_path: source.into(),
                    container_path: destination.into(),
                    read_only,
                });
            } else {
                shadows.push(destination.into());
            }
        }
        Ok(Self {
            runtime,
            name: name.into(),
            id,
            mounts: binds,
            shadow_mounts: shadows,
        })
    }

    fn mapped_path(&self, path: &std::path::Path, writable: bool) -> Option<std::path::PathBuf> {
        super::container_interface::host_path_for_mounts(
            &self.mounts,
            self.shadow_mounts.iter().map(String::as_str),
            path,
            writable,
        )
    }

    pub(crate) fn host_path(
        &self,
        path: &std::path::Path,
        writable: bool,
    ) -> Option<std::path::PathBuf> {
        if !self.runtime.local_mounts {
            return None;
        }
        self.mapped_path(path, writable)
    }

    pub(crate) fn physical_path(&self, path: &std::path::Path) -> (String, std::path::PathBuf) {
        if let Some(mapped) = self.mapped_path(path, true) {
            if self.runtime.local_mounts {
                let canonical = std::fs::canonicalize(&mapped)
                    .unwrap_or_else(|_| crate::git::template::lexical_normalize(&mapped));
                return ("host".into(), canonical);
            }
            return (
                format!("runtime:{:?}:{}", self.runtime.kind, self.runtime.endpoint),
                crate::git::template::lexical_normalize(&mapped),
            );
        }
        (
            format!(
                "container:{:?}:{}:{}",
                self.runtime.kind, self.runtime.endpoint, self.id
            ),
            path.to_path_buf(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::ContainerRuntimeName;
    use std::os::unix::fs::PermissionsExt;

    fn snapshot(local_mounts: bool) -> ContainerExecutionSnapshot {
        ContainerExecutionSnapshot {
            runtime: RuntimeExecutionSnapshot {
                kind: ContainerRuntimeName::Docker,
                program: std::path::PathBuf::from("docker"),
                cwd: std::path::PathBuf::from("/tmp"),
                endpoint: String::new(),
                local_mounts,
                routing: Vec::new(),
                global_arguments: Vec::new(),
            },
            name: "aoe-test".into(),
            id: "container-id".into(),
            mounts: vec![super::super::VolumeMount {
                host_path: "/host/project".into(),
                container_path: "/workspace/project".into(),
                read_only: false,
            }],
            shadow_mounts: Vec::new(),
        }
    }

    #[test]
    fn host_path_requires_local_mounts() {
        let target = std::path::Path::new("/workspace/project/src/main.rs");
        assert_eq!(
            snapshot(true).host_path(target, false),
            Some(std::path::PathBuf::from("/host/project/src/main.rs"))
        );
        assert_eq!(snapshot(false).host_path(target, false), None);
    }

    #[test]
    fn physical_path_names_remote_and_local_projections() {
        let (domain, _) =
            snapshot(true).physical_path(std::path::Path::new("/workspace/project/src/main.rs"));
        assert_eq!(domain, "host");
        let (domain, path) =
            snapshot(false).physical_path(std::path::Path::new("/workspace/project/src/main.rs"));
        assert!(domain.starts_with("runtime:"));
        assert_eq!(path, std::path::PathBuf::from("/host/project/src/main.rs"));
        let (domain, path) =
            snapshot(true).physical_path(std::path::Path::new("/unmapped/file.txt"));
        assert!(domain.starts_with("container:"));
        assert_eq!(path, std::path::PathBuf::from("/unmapped/file.txt"));
    }

    /// A fake `docker` binary plus a hermetic environment let `capture`
    /// exercise the real probe path: a non-default context must freeze its
    /// TLS setup into `global_arguments` instead of losing it to the
    /// environment strip, and the flags must land on every command.
    #[test]
    #[serial_test::serial]
    fn capture_freezes_tls_flags_for_a_non_default_context() {
        use crate::session::test_support::EnvGuard;
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let docker = bin.join("docker");
        std::fs::write(
            &docker,
            "#!/bin/sh\nif [ \"$1\" = context ]; then printf '%s' \"$FIXTURE_CONTEXT\"; else printf '%s\\0' \"$@\"; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let metadata = temp.path().join("metadata\r\n'quoted");
        let tls_dir = metadata.join("docker");
        std::fs::create_dir_all(&tls_dir).unwrap();
        for file in ["ca.pem", "cert.pem", "key.pem"] {
            std::fs::write(tls_dir.join(file), b"fixture").unwrap();
        }
        let context = serde_json::json!({
            "Name": "tlsctx",
            "Endpoints": {"docker": {"Host": "tcp://127.0.0.1:2375", "SkipTLSVerify": false}},
            "TLSMaterial": {"docker": ["ca.pem", "cert.pem", "key.pem"]},
            "Storage": {"TLSPath": metadata},
        })
        .to_string();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _env = EnvGuard::set(&[
            ("PATH", path.as_str()),
            ("DOCKER_CONTEXT", "tlsctx"),
            ("HOME", temp.path().to_str().unwrap()),
            ("FIXTURE_CONTEXT", context.as_str()),
        ]);
        let _clear = EnvGuard::unset(&[
            "DOCKER_HOST",
            "DOCKER_CONFIG",
            "DOCKER_TLS",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ]);

        let snapshot = RuntimeExecutionSnapshot::capture(&ContainerRuntime::docker()).unwrap();
        assert_eq!(
            snapshot.global_arguments,
            vec![
                "--tls".to_owned(),
                "--tlsverify=true".to_owned(),
                format!("--tlscacert={}", expect_path(&tls_dir, "ca.pem")),
                format!("--tlscert={}", expect_path(&tls_dir, "cert.pem")),
                format!("--tlskey={}", expect_path(&tls_dir, "key.pem")),
            ],
            "the context's TLS setup must be frozen, not read from ambient env"
        );
        let command = snapshot.command(&["ps".to_owned()]);
        assert_eq!(command.get_args().next().unwrap(), "--tls");
        let direct = snapshot
            .exec("fixture", "/", &["true".into()])
            .output()
            .unwrap();
        assert!(direct.status.success());
        let shell = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(snapshot.exec_shell_command("fixture", None, "true"))
            .output()
            .unwrap();
        assert!(shell.status.success());
        let tls_arguments = |output: &[u8]| {
            output
                .split(|byte| *byte == 0)
                .filter(|arg| arg.starts_with(b"--tls"))
                .map(<[u8]>::to_vec)
                .collect::<Vec<_>>()
        };
        assert_eq!(tls_arguments(&shell.stdout), tls_arguments(&direct.stdout));
    }

    fn expect_path(tls_dir: &std::path::Path, file: &str) -> String {
        tls_dir.join(file).to_str().unwrap().to_owned()
    }

    #[test]
    #[serial_test::serial]
    fn capture_keeps_tls_for_skip_tls_verify_without_material() {
        use crate::session::test_support::EnvGuard;
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let docker = bin.join("docker");
        std::fs::write(
            &docker,
            "#!/bin/sh\nif [ \"$1\" = context ]; then printf '%s' \"$FIXTURE_CONTEXT\"; else echo '{}'; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let context = serde_json::json!({
            "Name": "plain",
            "Endpoints": {"docker": {"Host": "tcp://127.0.0.1:2375", "SkipTLSVerify": true}},
            "Storage": {"TLSPath": temp.path().join("metadata")},
        })
        .to_string();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _env = EnvGuard::set(&[
            ("PATH", path.as_str()),
            ("DOCKER_CONTEXT", "plain"),
            ("HOME", temp.path().to_str().unwrap()),
            ("FIXTURE_CONTEXT", context.as_str()),
        ]);
        let _clear = EnvGuard::unset(&[
            "DOCKER_HOST",
            "DOCKER_CONFIG",
            "DOCKER_TLS",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ]);

        let snapshot = RuntimeExecutionSnapshot::capture(&ContainerRuntime::docker()).unwrap();

        assert_eq!(
            snapshot.global_arguments,
            vec![
                "--tls".to_owned(),
                "--tlsverify=false".to_owned(),
                "--tlscacert=".to_owned(),
                "--tlscert=".to_owned(),
                "--tlskey=".to_owned(),
            ],
            "SkipTLSVerify must keep the endpoint on TLS instead of downgrading to HTTP"
        );
    }

    #[test]
    #[serial_test::serial]
    fn capture_rejects_a_declared_but_missing_tls_file() {
        use crate::session::test_support::EnvGuard;
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let docker = bin.join("docker");
        std::fs::write(
            &docker,
            "#!/bin/sh\nif [ \"$1\" = context ]; then printf '%s' \"$FIXTURE_CONTEXT\"; else echo '{}'; fi\n",
        )
        .unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::create_dir_all(temp.path().join("metadata/docker")).unwrap();
        std::fs::write(temp.path().join("metadata/docker/ca.pem"), b"fixture").unwrap();
        let context = serde_json::json!({
            "Name": "broken",
            "Endpoints": {"docker": {"Host": "tcp://127.0.0.1:2375", "SkipTLSVerify": false}},
            "TLSMaterial": {"docker": ["ca.pem", "cert.pem"]},
            "Storage": {"TLSPath": temp.path().join("metadata")},
        })
        .to_string();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let _env = EnvGuard::set(&[
            ("PATH", path.as_str()),
            ("DOCKER_CONTEXT", "broken"),
            ("HOME", temp.path().to_str().unwrap()),
            ("FIXTURE_CONTEXT", context.as_str()),
        ]);
        let _clear = EnvGuard::unset(&["DOCKER_HOST", "DOCKER_CONFIG"]);

        let error = RuntimeExecutionSnapshot::capture(&ContainerRuntime::docker())
            .expect_err("a declared but missing certificate must fail the capture");

        assert!(
            error.to_string().contains("TLS file is missing"),
            "the refusal must name the missing file: {error}"
        );
    }
}
