//! The container a sandboxed session runs in, and the `docker exec` argv the
//! agent command is wrapped in.

use crate::acp::fs_handler::SandboxPathMap;
use crate::session::SandboxInfo;
use std::path::{Path, PathBuf};

use super::errors::AcpError;
use super::spawn::{
    allowlisted_env_pairs, is_host_only_path_env, provider_env_denyreason, SpawnConfig,
};

/// Sandbox handles a connection task needs to route ACP fs/* and
/// terminal/* requests across the container boundary.
#[derive(Debug, Clone)]
pub struct SessionSandbox {
    pub container_name: String,
    pub container_workdir: PathBuf,
    /// Snapshot of the session's sandbox info, used to re-resolve env on
    /// every `terminal/create` so the agent's shell commands see the same
    /// env entries (including any rotated host values) as the interactive
    /// tmux pane.
    pub sandbox_info: SandboxInfo,
    /// Profile the session was created under. Required for
    /// `resolved_sandbox_config` to pick up per-profile env overrides.
    pub source_profile: Option<String>,
    /// Host-side project path. `resolved_sandbox_config` walks up from
    /// here to find any repo-local config overrides.
    pub project_path: PathBuf,
}

impl SessionSandbox {
    /// Path-map entries cover only the workspace volume(s) the container was
    /// built with; see `docs/acp.md` for the agent-config and `extra_volumes`
    /// limitations.
    pub fn from_info(
        sandbox: &SandboxInfo,
        project_path: &Path,
        source_profile: Option<String>,
    ) -> Result<(Self, SandboxPathMap), AcpError> {
        let project_path_str = project_path.to_string_lossy().to_string();
        let (volumes, computed_workdir) =
            crate::session::config::container_config::compute_volume_paths(
                project_path,
                &project_path_str,
            )
            .map_err(|e| AcpError::Spawn(format!("compute container workdir: {e}")))?;
        // Must be what the container was created with. `compute_volume_paths`
        // collapses to `/workspace/<basename>` once the worktree's git linkage
        // breaks, naming a path the container never mounted (#2414), so prefer
        // the create-time pin, then the live container, then the recompute.
        let workdir = sandbox
            .container_workdir
            .clone()
            .or_else(|| {
                crate::containers::get_container_runtime()
                    .container_working_dir(&sandbox.container_name)
            })
            .unwrap_or(computed_workdir);
        let mounts: Vec<(PathBuf, PathBuf)> = volumes
            .into_iter()
            .map(|v| (PathBuf::from(v.container_path), PathBuf::from(v.host_path)))
            .collect();
        Ok((
            Self {
                container_name: sandbox.container_name.clone(),
                container_workdir: PathBuf::from(workdir),
                sandbox_info: sandbox.clone(),
                source_profile,
                project_path: project_path.to_path_buf(),
            },
            SandboxPathMap::new(mounts),
        ))
    }

    /// Re-resolved on every `terminal/create` so rotated host values reach the
    /// agent's shell commands without a container recreate. A missing
    /// `source_profile` (legacy `WorkerRecord`) warns rather than failing, which
    /// would break `terminal/create` for an otherwise healthy session.
    pub fn current_env_entries(&self) -> Vec<crate::containers::container_interface::EnvEntry> {
        let profile = match self.source_profile.as_deref() {
            Some(p) => p,
            None => {
                tracing::warn!(
                    target: "acp.terminal",
                    container = %self.container_name,
                    "SessionSandbox has no source_profile (likely a legacy WorkerRecord); \
                     resolving terminal/create env against the global default profile"
                );
                ""
            }
        };
        let sandbox_config =
            crate::session::environment::resolved_sandbox_config(profile, &self.project_path);
        crate::session::environment::collect_environment(&sandbox_config, &self.sandbox_info)
    }
}

/// `docker_binary` is `argv[0]` (docker/podman). Export `inherit_env` so the
/// `-e KEY` flags in `docker_args` forward those values into the container.
pub(super) struct SandboxArgv {
    pub(super) docker_binary: String,
    pub(super) docker_args: Vec<String>,
    pub(super) inherit_env: Vec<(String, String)>,
}

/// Docker proxies the agent's stdio across the container boundary. Mirrors the
/// tmux view's env handling so the same `sandbox.environment` and `extra_env`
/// entries take effect. `container_workdir` comes pre-computed from
/// `SessionSandbox::from_info`.
pub(super) fn build_sandbox_docker_argv(
    config: &SpawnConfig,
    sandbox: &SandboxInfo,
    container_workdir: &str,
) -> Result<SandboxArgv, AcpError> {
    use crate::containers::container_interface::docker_env_args;

    let runtime = crate::containers::get_container_runtime();
    let docker_binary = runtime.base.binary.to_string();

    let project_path = config.cwd.as_path();
    let profile_for_env = config.source_profile.as_deref().unwrap_or("");
    let sandbox_config =
        crate::session::environment::resolved_sandbox_config(profile_for_env, project_path);
    let mut env_entries =
        crate::session::environment::collect_environment(&sandbox_config, sandbox);
    // Bind-mounted at the fixed container path by build_container_config;
    // export it so the agent writes viewable artifacts there (#2587).
    env_entries.push(crate::containers::EnvEntry::Literal {
        key: crate::session::artifacts::ARTIFACT_DIR_ENV.to_string(),
        value: crate::session::artifacts::CONTAINER_ARTIFACT_DIR.to_string(),
    });

    let mut docker_args: Vec<String> = vec![
        "exec".into(),
        "-i".into(),
        "-w".into(),
        container_workdir.to_string(),
    ];
    // First claim wins, so the sandbox config below outranks both auth sources.
    let mut seen_keys: std::collections::HashSet<String> =
        env_entries.iter().map(|e| e.key().to_string()).collect();
    let (env_argv, inherit_pairs) = docker_env_args(&env_entries);
    docker_args.extend(env_argv);
    let mut inherit_env: Vec<(String, String)> = inherit_pairs;

    // The request's auth payload (`provider_env`) is claimed ahead of the
    // per-adapter allowlist (#3238), mirroring the non-sandboxed paths where
    // `provider_env` is applied last and so wins a shared key. Denied keys and
    // host-only paths (which name nothing inside the container) never cross.
    // docker forwards only a name handed to it via `-e`, so the value is set on
    // the runner via `inherit_env` and the key alone goes in the argv.
    let request_auth = config
        .provider_env
        .iter()
        .filter(|&(key, _)| provider_env_denyreason(key).is_none())
        .cloned();
    let adapter_allowlist = allowlisted_env_pairs(config)
        .into_iter()
        .filter(|(key, _)| !is_host_only_path_env(key));
    for (key, value) in request_auth.chain(adapter_allowlist) {
        if seen_keys.insert(key.clone()) {
            docker_args.push("-e".into());
            docker_args.push(key.clone());
            inherit_env.push((key, value));
        }
    }

    docker_args.push(sandbox.container_name.clone());
    docker_args.push(config.spec.command.clone());
    for a in &config.spec.args {
        docker_args.push(a.clone());
    }

    Ok(SandboxArgv {
        docker_binary,
        docker_args,
        inherit_env,
    })
}

/// The `cwd` for `session/new` / `session/load` / `session/fork`. A sandboxed
/// agent runs in the container, where the host project path does not exist and
/// is rejected as "'cwd' does not exist on the machine running the agent"
/// (#2871). Non-sandbox sessions keep the host `cwd`.
pub(super) fn agent_request_cwd(
    container_workdir: Option<&std::path::Path>,
    host_cwd: &std::path::Path,
) -> PathBuf {
    container_workdir.unwrap_or(host_cwd).to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::env_test_spawn_config;

    fn sandbox(container_name: &str, container_workdir: Option<&str>) -> SandboxInfo {
        SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: container_name.into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: container_workdir.map(str::to_string),
        }
    }

    /// Sandboxed spawn config for `container`, wrapping the default host one.
    fn sandbox_config(cwd: &std::path::Path, info: &SandboxInfo) -> SpawnConfig {
        let mut config = env_test_spawn_config(cwd.to_path_buf());
        config.sandbox_info = Some(info.clone());
        config
    }

    /// #2414: the create-time pin must beat a live recompute, which collapses
    /// to `/workspace/<basename>` once the worktree's git linkage breaks.
    #[test]
    fn from_info_prefers_pinned_workdir_over_live_recompute() {
        let tmp = tempfile::tempdir().unwrap();
        // Orphaned worktree: a `.git` file whose gitdir points nowhere.
        let worktree = tmp.path().join("repo-worktrees").join("feature");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(
            worktree.join(".git"),
            "gitdir: ../../does-not-exist/.git/worktrees/feature\n",
        )
        .unwrap();
        let info = sandbox(
            "aoe-sandbox-pinned1",
            Some("/workspace/repo-worktrees/feature"),
        );

        let (resources, _map) = SessionSandbox::from_info(&info, &worktree, None).unwrap();
        assert_eq!(
            resources.container_workdir,
            PathBuf::from("/workspace/repo-worktrees/feature"),
        );

        // The pin is load-bearing: the recompute names a path never mounted.
        let (_volumes, computed) = crate::session::config::container_config::compute_volume_paths(
            &worktree,
            &worktree.to_string_lossy(),
        )
        .unwrap();
        assert_eq!(computed, "/workspace/feature");
    }

    /// #2871: a sandboxed agent runs in-container, so session/new|load|fork
    /// must carry the container workdir, not the host path.
    #[test]
    fn agent_request_cwd_prefers_container_workdir_when_sandboxed() {
        let host = PathBuf::from("/scm/aoe/../aoe-worktrees/bohemians");
        let container = PathBuf::from("/workspace/bohemians");
        assert_eq!(
            agent_request_cwd(Some(container.as_path()), &host),
            container
        );
        assert_eq!(agent_request_cwd(None, &host), host);
    }

    /// The agent command is wrapped as `docker exec -i -w <workdir> ... <container>
    /// <argv>`, with literal env entries inlined as `-e KEY=VALUE` and never
    /// duplicated into `inherit_env` (which is for parent-process values).
    #[test]
    fn build_sandbox_docker_argv_wraps_agent_in_docker_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let mut info = sandbox("aoe-sandbox-abc12345", None);
        info.extra_env = Some(vec!["MY_LITERAL=hello".into()]);
        let mut config = sandbox_config(tmp.path(), &info);
        config.spec.args = vec!["--stdio".into()];

        let argv = build_sandbox_docker_argv(&config, &info, "/workspace/proj").unwrap();

        assert!(
            argv.docker_binary == "docker" || argv.docker_binary == "podman",
            "{:?}",
            argv.docker_binary
        );
        assert_eq!(argv.docker_args[..3], ["exec", "-i", "-w"]);
        let container = argv
            .docker_args
            .iter()
            .position(|a| a == "aoe-sandbox-abc12345")
            .expect("container name in argv");
        assert_eq!(
            argv.docker_args[container + 1..],
            ["claude-agent-acp", "--stdio"]
        );
        assert!(argv.docker_args.iter().any(|a| a == "MY_LITERAL=hello"));
        assert!(!argv.inherit_env.iter().any(|(k, _)| k == "MY_LITERAL"));
    }

    /// A credential must reach the container as `-e KEY` plus an `inherit_env`
    /// pair, never as `-e KEY=VALUE`, which would leak the secret into argv.
    #[test]
    fn build_sandbox_docker_argv_inherit_env_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let info = sandbox("aoe-sandbox-abc12345", None);
        let mut config = sandbox_config(tmp.path(), &info);
        config.provider_env = vec![("ANTHROPIC_API_KEY".into(), "sk-test-value".into())];

        let argv = build_sandbox_docker_argv(&config, &info, "/workspace/proj").unwrap();

        assert_named_without_value(&argv, "ANTHROPIC_API_KEY", "sk-test-value");
    }

    /// `-e KEY` names the credential, the value rides `inherit_env`, and the
    /// secret never appears in argv.
    fn assert_named_without_value(argv: &SandboxArgv, key: &str, value: &str) {
        assert!(
            argv.docker_args
                .windows(2)
                .any(|w| w[0] == "-e" && w[1] == key),
            "{key} must be named with `-e KEY`, got {:?}",
            argv.docker_args
        );
        assert!(
            !argv
                .docker_args
                .iter()
                .any(|a| a.starts_with(&format!("{key}="))),
            "{key} must not appear as `KEY=VALUE` in argv"
        );
        assert_eq!(
            argv.inherit_env
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str()),
            Some(value),
        );
    }

    fn assert_absent(argv: &SandboxArgv, key: &str) {
        assert!(
            !argv.docker_args.iter().any(|a| a == key)
                && !argv
                    .docker_args
                    .iter()
                    .any(|a| a.starts_with(&format!("{key}=")))
                && !argv.inherit_env.iter().any(|(k, _)| k == key),
            "{key} must not cross the container boundary, got {:?}",
            argv.docker_args
        );
    }

    /// #3238: the per-adapter `env_allowlist` must cross the boundary, but a
    /// host filesystem path resolves to nothing inside the container and a
    /// denied linker hook must be dropped even when allowlisted. `serial`
    /// because the cases set process-wide env.
    #[test]
    #[serial_test::serial]
    fn build_sandbox_docker_argv_applies_env_allowlist() {
        const HOST_ONLY: [&str; 7] = [
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "AWS_CONFIG_FILE",
            "AWS_SHARED_CREDENTIALS_FILE",
            "AWS_WEB_IDENTITY_TOKEN_FILE",
            "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE",
        ];
        let mut env: Vec<(&str, &str)> = HOST_ONLY.iter().map(|k| (*k, "/host/path")).collect();
        env.extend([
            ("OPENAI_API_KEY", "sk-openai"),
            ("ANTHROPIC_API_KEY", "sk-unrelated-anthropic"),
            ("LD_PRELOAD", "/tmp/evil.so"),
        ]);
        let _env = crate::session::test_support::EnvGuard::set(&env);

        let tmp = tempfile::tempdir().unwrap();
        let info = sandbox("aoe-sandbox-allowlist", None);
        let mut config = sandbox_config(tmp.path(), &info);
        config.spec.command = "codex-acp".into();
        let mut allowlist: Vec<String> = HOST_ONLY.iter().map(|k| (*k).to_string()).collect();
        allowlist.extend(["OPENAI_API_KEY".into(), "LD_PRELOAD".into()]);
        config.spec.env_allowlist = Some(allowlist);

        let argv = build_sandbox_docker_argv(&config, &info, "/workspace/proj").unwrap();

        // The value-typed allowlist entry crosses, so the assertions below
        // cannot pass on a function that forwards nothing at all.
        assert_named_without_value(&argv, "OPENAI_API_KEY", "sk-openai");
        for key in HOST_ONLY {
            assert_absent(&argv, key);
        }
        assert_absent(&argv, "LD_PRELOAD");
        assert_absent(&argv, "ANTHROPIC_API_KEY");
    }

    /// The request's `provider_env` must win a shared key over the adapter's
    /// ambient allowlist, matching the non-sandboxed paths. Before the ordering
    /// fix a session that selected its own credential silently ran under the
    /// operator's ambient one inside the container.
    #[test]
    #[serial_test::serial]
    fn build_sandbox_docker_argv_provider_env_beats_ambient_host_key() {
        let _env = crate::session::test_support::EnvGuard::set(&[(
            "ANTHROPIC_API_KEY",
            "sk-host-ambient",
        )]);
        let tmp = tempfile::tempdir().unwrap();
        let info = sandbox("aoe-sandbox-precedence", None);
        let mut config = sandbox_config(tmp.path(), &info);
        let reg = crate::acp::agent_registry::AgentRegistry::with_defaults();
        config.spec = reg.get("claude").expect("claude default").clone();
        config.provider_env = vec![("ANTHROPIC_API_KEY".into(), "sk-session-request".into())];

        let argv = build_sandbox_docker_argv(&config, &info, "/workspace/proj").unwrap();

        let values: Vec<&str> = argv
            .inherit_env
            .iter()
            .filter(|(k, _)| k == "ANTHROPIC_API_KEY")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            values,
            vec!["sk-session-request"],
            "the request credential must win and be forwarded exactly once"
        );
    }
}
