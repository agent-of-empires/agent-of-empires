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
    /// Build a `SessionSandbox` + `SandboxPathMap` from a `SandboxInfo`
    /// and the session's host-side project_path. Path-map entries
    /// cover only the workspace volume(s) the container was built
    /// with; see `docs/acp.md` for the known-limitations note on
    /// agent-config and `extra_volumes`.
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
        // The workdir must be what the container was actually created with, not
        // a live recompute. `compute_volume_paths` resolves the worktree's git
        // linkage and silently collapses to `/workspace/<basename>` once that
        // linkage breaks on the host (and it ignores multi-repo `workspace_info`),
        // which would `docker exec -w` into a path the container never mounted
        // (#2414). Prefer the create-time-pinned value, then the live container's
        // own `WorkingDir`, and only fall back to the recompute when neither is
        // available (no container created yet). The mount map stays the computed
        // project volumes; making it workspace-complete needs `workspace_info`,
        // which the reattach path does not carry (tracked separately).
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

    /// Re-resolve env entries for this session's sandbox. Called on every
    /// `terminal/create` so rotated host values (e.g. refreshed tokens)
    /// reach the agent's shell commands without requiring a container
    /// recreate.
    ///
    /// A missing `source_profile` only happens for legacy `WorkerRecord`
    /// entries written before the field was persisted. Warns once per
    /// call rather than failing, since refusing resolution would break
    /// `terminal/create` for sessions that are otherwise healthy.
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

/// Result of constructing the `docker exec` argv for a sandboxed structured view
/// spawn. `docker_binary` is argv[0] (the docker/podman runtime);
/// `docker_args` is everything after it (including the container name
/// and the in-container agent argv). `inherit_env` is the set of
/// (key, value) pairs the parent process must export so docker can
/// forward them via the matching `-e KEY` flags already in `docker_args`.
pub(super) struct SandboxArgv {
    pub(super) docker_binary: String,
    pub(super) docker_args: Vec<String>,
    pub(super) inherit_env: Vec<(String, String)>,
}

/// Build the `docker exec` argv for a sandboxed structured view spawn. The
/// resulting command is what the runner executes; docker proxies the
/// agent's stdio across the container boundary. Mirrors the tmux
/// view's env handling so the same `sandbox.environment` and
/// `extra_env` entries take effect.
///
/// `container_workdir` is the in-container working directory for the
/// session, pre-computed by `SessionSandbox::from_info` and passed
/// through to avoid re-running `compute_volume_paths`.
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
    // The session artifact dir is bind-mounted at the fixed container path by
    // build_container_config; export it so the agent writes viewable artifacts
    // there. See #2587.
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
    // `collect_environment` already dedupes by key, so the entry list is
    // unique. We still track `seen_keys` so the explicit auth sources below
    // cannot override sandbox configuration.
    let mut seen_keys: std::collections::HashSet<String> =
        env_entries.iter().map(|e| e.key().to_string()).collect();
    let (env_argv, inherit_pairs) = docker_env_args(&env_entries);
    docker_args.extend(env_argv);
    let mut inherit_env: Vec<(String, String)> = inherit_pairs;

    // Auth sources are claimed highest-priority first, because `seen_keys` is
    // first-claim-wins. The order mirrors the non-sandboxed paths, where the
    // per-request `provider_env` is applied last and so wins a shared key over
    // the ambient host keys: request auth (`provider_env`) > per-adapter
    // allowlist. The sandbox env list (`collect_environment` above) is claimed
    // before both, matching the operator-config precedence on the host paths.

    // Per-spawn provider_env entries (the request's auth payload).
    for (key, value) in &config.provider_env {
        if provider_env_denyreason(key).is_some() {
            continue;
        }
        if seen_keys.insert(key.clone()) {
            docker_args.push("-e".into());
            docker_args.push(key.clone());
            inherit_env.push((key.clone(), value.clone()));
        }
    }

    // Per-adapter env allowlist (#3238). The same keys `apply_env_filter`
    // forwards on the non-sandboxed paths must also cross the container
    // boundary, or a sandboxed non-Claude session silently loses its
    // provider auth (the #3238 symptom). docker only forwards a name handed
    // to it via `-e`, so each allowlisted host value is set on the runner
    // (`inherit_env`) and named with `-e KEY`.
    //
    // Value-typed entries only: a host path names nothing inside the container,
    // so forwarding it points the adapter at a directory that does not exist
    // instead of the one `AGENT_CONFIG_MOUNTS` bind-mounts at the canonical
    // container path. These keys stay host-only and keep flowing on the two
    // non-sandboxed spawn paths, where they are the point.
    for (key, value) in allowlisted_env_pairs(config) {
        if is_host_only_path_env(&key) {
            continue;
        }
        if seen_keys.insert(key.clone()) {
            docker_args.push("-e".into());
            docker_args.push(key.clone());
            inherit_env.push((key, value));
        }
    }

    // Model override (AOE_AGENT_MODEL): the supervisor folds the
    // requested model into provider_env above, so it's already covered.

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

/// The `cwd` to send on `session/new` / `session/load` / `session/fork`.
///
/// A sandboxed agent runs inside the container (via `docker exec`), so it
/// must be given the container workdir, not the host project path; the host
/// path does not exist in the container and the agent rejects it with
/// "'cwd' does not exist on the machine running the agent" (#2871). The
/// container workdir is the create-time-pinned value resolved by
/// `SessionSandbox::from_info`. Non-sandbox sessions keep the host `cwd`.
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
    use crate::acp::agent_registry::AgentSpec;

    /// Regression for issue #2414 on the structured-view path: `from_info`
    /// must use the create-time-pinned `SandboxInfo::container_workdir`, not a
    /// live recompute. With the worktree's git linkage broken,
    /// `compute_volume_paths` collapses to `/workspace/<basename>` (a path the
    /// container never mounted), so the pin has to win.
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

        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-pinned1".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: Some("/workspace/repo-worktrees/feature".into()),
        };

        // Pin present, so no live inspect is attempted; the pinned value is used.
        let (resources, _map) = SessionSandbox::from_info(&sandbox, &worktree, None).unwrap();
        assert_eq!(
            resources.container_workdir,
            PathBuf::from("/workspace/repo-worktrees/feature"),
        );

        // The pin is load-bearing: the live recompute on this orphaned worktree
        // collapses to the basename, which is the path that never got mounted.
        let (_volumes, computed) = crate::session::config::container_config::compute_volume_paths(
            &worktree,
            &worktree.to_string_lossy(),
        )
        .unwrap();
        assert_eq!(computed, "/workspace/feature");
    }

    /// #2871: a sandboxed agent runs in-container, so session/new|load|fork
    /// must carry the container workdir, not the host path (which does not
    /// exist inside the container, worktree `..` or not). Non-sandbox
    /// sessions keep the host cwd.
    #[test]
    fn agent_request_cwd_prefers_container_workdir_when_sandboxed() {
        let host = PathBuf::from(
            "/Users/nbrake/scm/agent-of-empires/../agent-of-empires-worktrees/bohemians",
        );
        let container = PathBuf::from("/workspace/bohemians");

        assert_eq!(
            agent_request_cwd(Some(container.as_path()), &host),
            container,
            "sandboxed request must use the container workdir"
        );
        assert_eq!(
            agent_request_cwd(None, &host),
            host,
            "non-sandbox request must use the host cwd unchanged"
        );
    }

    /// Sandboxed structured view spawn must wrap the agent command in
    /// `docker exec` argv with `-i`, the container workdir, an `-e`
    /// flag per env entry, then the container name, then the agent
    /// argv. The docker binary must be argv[0]. Mirrors the tmux
    /// view's wrap so the same `claude-agent-acp` invocation
    /// goes inside the container instead of running on the host.
    #[test]
    fn build_sandbox_docker_argv_wraps_agent_in_docker_exec() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-abc12345".into(),
            extra_env: Some(vec!["MY_LITERAL=hello".into()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let config = SpawnConfig {
            wrapper_substitution: None,
            agent_key: "claude".into(),
            tool: "claude".into(),
            spec: AgentSpec {
                command: "claude-agent-acp".into(),
                args: vec!["--stdio".into()],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd,
            additional_dirs: vec![],
            provider_env: vec![],
            host_environment: vec![],
            default_effort: None,
            default_mode: None,
            socket_path: None,
            stored_acp_session_id: None,
            fork_from: None,
            seed_history_replay: false,
            artifact_dir: None,
            sandbox_info: Some(sandbox.clone()),
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let argv = build_sandbox_docker_argv(&config, &sandbox, "/workspace/proj")
            .expect("docker argv built");
        assert!(
            argv.docker_binary == "docker" || argv.docker_binary == "podman",
            "expected docker/podman binary, got {:?}",
            argv.docker_binary
        );
        assert_eq!(argv.docker_args[0], "exec");
        assert_eq!(argv.docker_args[1], "-i");
        assert_eq!(argv.docker_args[2], "-w");
        let cn_idx = argv
            .docker_args
            .iter()
            .position(|a| a == "aoe-sandbox-abc12345")
            .expect("container name in argv");
        let cmd_idx = cn_idx + 1;
        assert_eq!(argv.docker_args[cmd_idx], "claude-agent-acp");
        assert_eq!(argv.docker_args[cmd_idx + 1], "--stdio");
        // Literal env entry lands as `-e KEY=VALUE`.
        assert!(
            argv.docker_args.iter().any(|a| a == "MY_LITERAL=hello"),
            "literal env entry must be propagated as `-e KEY=VALUE`"
        );
        // The literal entry's KEY=VALUE form must NOT also appear in
        // `inherit_env` (that vec is for Inherit-style entries whose
        // value comes from the parent process env, not for literals).
        assert!(
            !argv.inherit_env.iter().any(|(k, _)| k == "MY_LITERAL"),
            "literal entries must not duplicate into inherit_env"
        );
    }

    /// Inherit-style env entries (provider auth keys) must lower into a
    /// pair of `-e KEY` (key only) in docker_args plus a `(KEY, VALUE)`
    /// pair in inherit_env so the runner can re-export the value and
    /// docker can forward it into the container.
    #[test]
    fn build_sandbox_docker_argv_inherit_env_shape() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-abc12345".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let config = SpawnConfig {
            wrapper_substitution: None,
            agent_key: "claude".into(),
            tool: "claude".into(),
            spec: AgentSpec {
                command: "claude-agent-acp".into(),
                args: vec![],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd,
            additional_dirs: vec![],
            // Per-spawn provider_env entry: must end up Inherit-style.
            provider_env: vec![("ANTHROPIC_API_KEY".into(), "sk-test-value".into())],
            host_environment: vec![],
            default_effort: None,
            default_mode: None,
            socket_path: None,
            stored_acp_session_id: None,
            fork_from: None,
            seed_history_replay: false,
            artifact_dir: None,
            sandbox_info: Some(sandbox.clone()),
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let argv = build_sandbox_docker_argv(&config, &sandbox, "/workspace/proj")
            .expect("docker argv built");
        // The `-e KEY` flag (without value) must appear consecutively.
        let key_flag_idx = argv
            .docker_args
            .windows(2)
            .position(|w| w[0] == "-e" && w[1] == "ANTHROPIC_API_KEY")
            .expect("ANTHROPIC_API_KEY -e flag must be present");
        // Value-typed forms like `-e ANTHROPIC_API_KEY=...` must NOT
        // appear; that would leak the secret into argv.
        assert!(
            !argv
                .docker_args
                .iter()
                .any(|a| a.starts_with("ANTHROPIC_API_KEY=")),
            "secret must not appear as `KEY=VALUE` in argv (slot {key_flag_idx})"
        );
        // The value must travel via inherit_env so the parent process
        // sets it before exec-ing docker.
        assert_eq!(
            argv.inherit_env
                .iter()
                .find(|(k, _)| k == "ANTHROPIC_API_KEY")
                .map(|(_, v)| v.as_str()),
            Some("sk-test-value"),
        );
    }

    /// A host filesystem path is not a credential, so it must NOT be
    /// auto-forwarded into the container even when set on the host and even
    /// when the adapter's `env_allowlist` names it (#3238). The path resolves
    /// to nothing inside the container, so forwarding it points the adapter
    /// away from the config dir `AGENT_CONFIG_MOUNTS` bind-mounts at the
    /// canonical container location. `CLAUDE_CONFIG_DIR` established the rule;
    /// `CODEX_HOME` and `GOOGLE_APPLICATION_CREDENTIALS` reach the same
    /// function through the per-adapter allowlists.
    ///
    /// Tagged `#[serial]` because the test mutates the process-wide
    /// env; parallel readers of `std::env::var` would race.
    #[test]
    #[serial_test::serial]
    fn build_sandbox_docker_argv_drops_host_only_path_env() {
        // Set the vars to simulate the host having them; the function under
        // test must still skip every one. `OPENAI_API_KEY` rides along as the
        // control: a value-typed allowlist entry alongside them must still
        // cross, or the assertion below would pass on a function that forwards
        // nothing at all.
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("CLAUDE_CONFIG_DIR", "/Users/operator/.claude"),
            ("CODEX_HOME", "/Users/operator/.codex"),
            (
                "GOOGLE_APPLICATION_CREDENTIALS",
                "/Users/operator/gcp-key.json",
            ),
            ("OPENAI_API_KEY", "sk-test-value"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-cfgdir".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let config = SpawnConfig {
            wrapper_substitution: None,
            agent_key: "codex".into(),
            tool: "codex".into(),
            spec: AgentSpec {
                command: "codex-acp".into(),
                args: vec![],
                description: "test".into(),
                env_allowlist: Some(vec![
                    "CLAUDE_CONFIG_DIR".into(),
                    "CODEX_HOME".into(),
                    "GOOGLE_APPLICATION_CREDENTIALS".into(),
                    "OPENAI_API_KEY".into(),
                ]),
            },
            cwd: tmp.path().to_path_buf(),
            additional_dirs: vec![],
            provider_env: vec![],
            host_environment: vec![],
            default_effort: None,
            default_mode: None,
            socket_path: None,
            stored_acp_session_id: None,
            fork_from: None,
            seed_history_replay: false,
            artifact_dir: None,
            sandbox_info: Some(sandbox.clone()),
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let argv = build_sandbox_docker_argv(&config, &sandbox, "/workspace/proj")
            .expect("docker argv built");

        for key in [
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(
                !argv.docker_args.iter().any(|a| a == key),
                "{key} is a host path and must not be forwarded as `-e KEY`"
            );
            assert!(
                !argv
                    .docker_args
                    .iter()
                    .any(|a| a.starts_with(&format!("{key}="))),
                "{key} must not appear as a literal `KEY=VALUE` either"
            );
            assert!(
                !argv.inherit_env.iter().any(|(k, _)| k == key),
                "{key} must not land in inherit_env"
            );
        }
        assert_eq!(
            argv.inherit_env
                .iter()
                .find(|(k, _)| k == "OPENAI_API_KEY")
                .map(|(_, v)| v.as_str()),
            Some("sk-test-value"),
            "the value-typed allowlist entry must still cross the boundary"
        );
    }

    /// #3238 regression guard: the per-adapter `env_allowlist` must cross the
    /// container boundary for a sandboxed session. Before the fix,
    /// `build_sandbox_docker_argv` forwarded only a fixed Claude-key block plus
    /// `provider_env`, so an operator's `OPENAI_API_KEY` never reached a
    /// sandboxed `codex`/`aoe-agent` session and auth silently failed. The
    /// value must ride `inherit_env` (not `-e KEY=VALUE`, which would leak the
    /// secret into argv), and a denied key (`LD_PRELOAD`) must still be
    /// dropped even when allowlisted.
    #[test]
    #[serial_test::serial]
    fn build_sandbox_docker_argv_forwards_env_allowlist() {
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("OPENAI_API_KEY", "sk-openai"),
            ("ANTHROPIC_API_KEY", "sk-unrelated-anthropic"),
            ("LD_PRELOAD", "/tmp/evil.so"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-allowlist".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let mut config = env_test_spawn_config(tmp.path().to_path_buf());
        config.spec.command = "codex-acp".into();
        config.spec.env_allowlist = Some(vec!["OPENAI_API_KEY".into(), "LD_PRELOAD".into()]);
        config.sandbox_info = Some(sandbox.clone());

        let argv = build_sandbox_docker_argv(&config, &sandbox, "/workspace/proj")
            .expect("docker argv built");

        assert!(
            argv.docker_args
                .windows(2)
                .any(|w| w[0] == "-e" && w[1] == "OPENAI_API_KEY"),
            "allowlisted OPENAI_API_KEY must be named with `-e KEY`, got {:?}",
            argv.docker_args
        );
        assert_eq!(
            argv.inherit_env
                .iter()
                .find(|(k, _)| k == "OPENAI_API_KEY")
                .map(|(_, v)| v.as_str()),
            Some("sk-openai"),
            "the value must ride inherit_env so it stays out of argv"
        );
        assert!(
            !argv
                .docker_args
                .iter()
                .any(|a| a.starts_with("OPENAI_API_KEY=")),
            "secret must not appear as `KEY=VALUE` in argv"
        );
        assert!(
            !argv.docker_args.iter().any(|a| a == "LD_PRELOAD")
                && !argv.inherit_env.iter().any(|(k, _)| k == "LD_PRELOAD"),
            "a denied linker hook must not cross the boundary even when allowlisted, got {:?}",
            argv.docker_args
        );
        assert!(
            !argv.docker_args.iter().any(|a| a == "ANTHROPIC_API_KEY")
                && !argv
                    .inherit_env
                    .iter()
                    .any(|(k, _)| k == "ANTHROPIC_API_KEY"),
            "a credential outside this adapter's allowlist must not cross the boundary"
        );
    }

    /// The per-session `provider_env` auth payload must win a shared key over
    /// the adapter's ambient allowlist, matching the non-sandboxed paths (where
    /// `provider_env` is applied last). Before the ordering fix the sandbox
    /// claimed the host key first, so a session that selected a different
    /// Anthropic credential silently ran under the operator's ambient one
    /// inside the container.
    #[test]
    #[serial_test::serial]
    fn build_sandbox_docker_argv_provider_env_beats_ambient_host_key() {
        let _env = crate::session::test_support::EnvGuard::set(&[(
            "ANTHROPIC_API_KEY",
            "sk-host-ambient",
        )]);
        let tmp = tempfile::tempdir().unwrap();
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-precedence".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let mut config = env_test_spawn_config(tmp.path().to_path_buf());
        let reg = crate::acp::agent_registry::AgentRegistry::with_defaults();
        config.spec = reg.get("claude").expect("claude default").clone();
        config.provider_env = vec![("ANTHROPIC_API_KEY".into(), "sk-session-request".into())];
        config.sandbox_info = Some(sandbox.clone());

        let argv = build_sandbox_docker_argv(&config, &sandbox, "/workspace/proj")
            .expect("docker argv built");

        let values: Vec<&str> = argv
            .inherit_env
            .iter()
            .filter(|(k, _)| k == "ANTHROPIC_API_KEY")
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(
            values,
            vec!["sk-session-request"],
            "the request credential must win and be forwarded exactly once, got {:?}",
            argv.inherit_env
        );
    }
}
