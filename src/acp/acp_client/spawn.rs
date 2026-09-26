//! `SpawnConfig` and agent subprocess spawning, including the environment
//! filter that decides which host variables the agent sees.

use crate::acp::agent_registry::AgentSpec;
use crate::session::SandboxInfo;
use agent_client_protocol::schema::v1::McpServer;
use std::path::PathBuf;
use std::process::Stdio;
use tracing::{debug, info, warn};

use super::errors::AcpError;
use super::resolve_command::resolve_agent_command;

#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Registry key of the resolved ACP backend; selects the `AgentProfile`.
    pub agent_key: String,
    /// The session's logical tool, fixed for its lifetime even when
    /// `agent_key` differs (override, `switch-agent`). Feeds `AOE_TOOL` for
    /// `host_hooks.before_session`.
    pub tool: String,
    pub spec: AgentSpec,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    /// Request-sourced provider env, filtered by `provider_env_denyreason`.
    pub provider_env: Vec<(String, String)>,
    /// Trusted operator `environment` entries, empty for sandboxed agents.
    /// Wins over `provider_env` on a shared key.
    pub host_environment: Vec<(String, String)>,
    /// Applied through the `thought_level` option on every establish path so
    /// a pinned effort survives respawns.
    pub default_effort: Option<String>,
    /// The effort was set per session, not inherited from the model's profile
    /// default, so a model change must not re-resolve it.
    pub default_effort_explicit: bool,
    /// Applied strictly through a `category:"mode"` option on fresh sessions.
    pub default_mode: Option<String>,
    /// `Instance.agent_model`, re-asserted through a `category:"model"` option
    /// after every establish and reset, for adapters that ignore
    /// `AOE_AGENT_MODEL`. Skipped when already current.
    pub default_model: Option<String>,
    /// Runner socket; `None` spawns the agent over in-proc stdio.
    pub socket_path: Option<PathBuf>,
    /// Loaded via `session/load` when the agent supports it.
    pub stored_acp_session_id: Option<String>,
    /// Parent id for a structured `session/fork` (`Instance.fork_pending`).
    pub fork_from: Option<String>,
    pub sandbox_info: Option<SandboxInfo>,
    /// Resolves profile-level `sandbox.environment` and settings.
    pub source_profile: Option<String>,
    /// From `<app_dir>/mcp.json`; gated against agent capabilities later.
    pub mcp_servers: Vec<McpServer>,
    /// Seed an empty event store from the load replay instead of suppressing
    /// it (first spawn of an imported session, #2276).
    pub seed_history_replay: bool,
    /// Exported as `AOE_ARTIFACT_DIR` to host agents (#2587).
    pub artifact_dir: Option<PathBuf>,
    /// `(wrapper, base)` when an `agent_detect_as` wrapper runs its base
    /// adapter instead (#3422).
    pub wrapper_substitution: Option<(String, String)>,
    /// Lifecycle epoch stamped on the runner's registry record.
    pub generation: u64,
    pub claude_store_pin: Option<crate::session::capture::ClaudeStorePin>,
}

/// Request-sourced `provider_env` keys: the shared deny policy, plus Claude's
/// store routing, which the session's conversation pins.
pub(super) fn request_env_denyreason(key: &str) -> Option<&'static str> {
    provider_env_denyreason(key).or_else(|| {
        (key == "CLAUDE_CONFIG_DIR").then_some("Claude store routing, pinned by the session")
    })
}

/// Request-sourced keys may not redirect infrastructure the operator env
/// controls or hook the dynamic linker. Provider auth keys are allowed.
pub(super) fn provider_env_denyreason(key: &str) -> Option<&'static str> {
    const INFRA_KEYS: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];
    if key.is_empty() {
        Some("empty key")
    } else if key == "AOE_TOKEN" {
        Some("aoe auth token, must not reach the agent")
    } else if INFRA_KEYS.contains(&key) {
        Some("infrastructure key, controlled by operator env")
    } else if key.starts_with("LD_") || key.starts_with("DYLD_") {
        Some("dynamic linker hook, would alter child binary load")
    } else {
        None
    }
}

/// Trusted operator config may set infrastructure keys (as a terminal pane
/// can); only aoe's token and the daemon to runner carrier are banned. Shared
/// with the runner's spawn so the policies cannot drift.
pub(crate) fn host_environment_denyreason(key: &str) -> Option<&'static str> {
    if !crate::session::environment::is_valid_env_key(key) {
        Some("not a valid environment variable name")
    } else if key == "AOE_TOKEN" {
        Some("aoe auth token, must not reach the agent")
    } else if key == crate::process::runner::ACP_AGENT_ENV {
        Some("reserved structured-worker environment carrier")
    } else {
        None
    }
}

/// Redact unambiguous secret shapes from agent stderr before logging.
pub(super) fn scrub_stderr_secrets(line: &str) -> std::borrow::Cow<'_, str> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"\b(sk-(?:ant-)?[A-Za-z0-9_\-]{16,}|ghp_[A-Za-z0-9]{16,}|gho_[A-Za-z0-9]{16,}|github_pat_[A-Za-z0-9_]{16,}|AKIA[A-Z0-9]{16}|Bearer\s+[A-Za-z0-9_.\-]{20,})",
        )
        .expect("static secret-scrub regex must compile")
    });
    re.replace_all(line, "<redacted-secret>")
}

/// Forwarded to every host-side agent on both spawn paths. Provider
/// credentials come only from the adapter allowlist or session config.
pub(super) const ALWAYS_FORWARD_ENV: &[&str] = &[
    "PATH",
    "HOME",
    // Drives `get_app_dir()` on Linux; a mismatch puts the runner's registry
    // record where the daemon never looks (#1383).
    "XDG_CONFIG_HOME",
    "LANG",
    "LC_ALL",
    "TERM",
    "USER",
    // Lets the agent's git authenticate over SSH (#2691).
    "SSH_AUTH_SOCK",
];

/// The inherited host environment (desktop vars etc., #3262), applied first so
/// every later layer wins on a shared key. Sandboxed agents get none.
pub(super) fn inherited_host_env_pairs(config: &SpawnConfig) -> Vec<(String, String)> {
    if config.sandbox_info.is_some() {
        return Vec::new();
    }
    let profile = config.source_profile.as_deref().unwrap_or_default();
    crate::session::environment::inherited_host_env(profile)
}

/// Path-valued allowlist entries: valid on the host, meaningless in a
/// container. A path-valued key added to `env_allowlist_for` belongs here.
pub(super) fn is_host_only_path_env(key: &str) -> bool {
    matches!(
        key,
        "CLAUDE_CONFIG_DIR"
            | "CODEX_HOME"
            | "GOOGLE_APPLICATION_CREDENTIALS"
            | "AWS_CONFIG_FILE"
            | "AWS_SHARED_CREDENTIALS_FILE"
            | "AWS_WEB_IDENTITY_TOKEN_FILE"
            | "AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE"
    )
}

/// The adapter's `env_allowlist` (#3238) resolved against the live env, with
/// denied keys dropped. Shared by all three spawn paths.
pub(super) fn allowlisted_env_pairs(config: &SpawnConfig) -> Vec<(String, String)> {
    let Some(allowlist) = config.spec.env_allowlist.as_ref() else {
        return Vec::new();
    };
    let mut pairs = Vec::new();
    for name in allowlist {
        if let Some(reason) = provider_env_denyreason(name) {
            warn!(target: "acp", key = %name, reason, "ignoring env allowlist entry");
            continue;
        }
        if let Ok(value) = std::env::var(name) {
            pairs.push((name.clone(), value));
        }
    }
    pairs
}

/// Key names applied per layer, for the spawn log. Values are never logged.
#[derive(Debug, Default)]
pub(super) struct EnvKeys {
    inherited: Vec<String>,
    forwarded: Vec<String>,
    provider: Vec<String>,
    host: Vec<String>,
}

/// The env layers both spawn paths share, in precedence order, lowest first.
/// `extra_path_dirs` are prepended to PATH so the adapter's own `node` lookups
/// match its install. Neither `env_clear` nor the layers unique to one path
/// (the stdio `host_environment`, the runner's own PATH chain) belong here.
pub(super) fn apply_env_filter(
    cmd: &mut std::process::Command,
    config: &SpawnConfig,
    extra_path_dirs: &[PathBuf],
) -> EnvKeys {
    let mut keys = EnvKeys::default();
    for (key, value) in inherited_host_env_pairs(config) {
        cmd.env(&key, value);
        keys.inherited.push(key);
    }
    for &name in ALWAYS_FORWARD_ENV {
        let Ok(mut value) = std::env::var(name) else {
            continue;
        };
        if name == "PATH" && !extra_path_dirs.is_empty() {
            value = prepend_path_dirs(std::ffi::OsStr::new(&value), extra_path_dirs)
                .to_string_lossy()
                .into_owned();
        }
        cmd.env(name, value);
        keys.forwarded.push(name.to_string());
    }
    for (key, value) in allowlisted_env_pairs(config) {
        cmd.env(&key, value);
        keys.forwarded.push(key);
    }
    for (key, value) in &config.provider_env {
        if let Some(reason) = request_env_denyreason(key) {
            warn!(target: "acp", key = %key, reason, "rejecting provider_env override of protected key");
            continue;
        }
        cmd.env(key, value);
        keys.provider.push(key.clone());
    }
    keys
}

/// `dirs` first, then `path`, dropping any already present.
pub(super) fn prepend_path_dirs(path: &std::ffi::OsStr, dirs: &[PathBuf]) -> std::ffi::OsString {
    let existing: Vec<PathBuf> = std::env::split_paths(path).collect();
    let mut chain: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        if !existing.contains(dir) && !chain.contains(dir) {
            chain.push(dir.clone());
        }
    }
    chain.extend(existing);
    std::env::join_paths(&chain).unwrap_or_else(|_| path.to_os_string())
}

fn apply_stdio_env(
    cmd: &mut tokio::process::Command,
    config: &SpawnConfig,
    extra_path_dirs: &[PathBuf],
) -> EnvKeys {
    cmd.env_clear();
    let mut keys = apply_env_filter(cmd.as_std_mut(), config, extra_path_dirs);
    // Last, so trusted operator config outranks request-sourced env. The
    // runner path carries these on `ACP_AGENT_ENV` instead.
    for (key, value) in &config.host_environment {
        if let Some(reason) = host_environment_denyreason(key) {
            warn!(target: "acp", key = %key, reason, "rejecting configured host environment key");
            continue;
        }
        cmd.env(key, value);
        keys.host.push(key.clone());
    }
    if let Some(socket_path) = &config.socket_path {
        cmd.env("AOE_ACP_SOCKET", socket_path);
    }
    keys
}

pub(super) fn native_store_snapshot(
    config: &SpawnConfig,
    command: &std::process::Command,
    overrides: &[(String, String)],
) -> Option<crate::session::ExecutionBinding> {
    if config.sandbox_info.is_some()
        || !matches!(config.agent_key.as_str(), "claude" | "claude-code")
    {
        return None;
    }
    let value = |name: &str| {
        overrides
            .iter()
            .rev()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
            .or_else(|| {
                command
                    .get_envs()
                    .find(|(key, _)| *key == name)
                    .and_then(|(_, value)| value?.to_str().map(str::to_owned))
            })
            .filter(|value| !value.is_empty())
    };
    let cwd = crate::session::capture::canonicalize_allowing_missing_leaf(&config.cwd)?;
    let exported = value("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    let home = value("HOME").map(PathBuf::from);
    let root = exported
        .clone()
        .or_else(|| home.as_ref().map(|home| home.join(".claude")))?;
    let root = if root.is_absolute() {
        root
    } else {
        cwd.join(root)
    };
    let exported_default_store = exported.is_some()
        && home
            .as_ref()
            .is_some_and(|home| crate::session::capture::is_default_claude_store(&root, home));
    Some(crate::session::ExecutionBinding {
        agent: "claude".into(),
        stores: vec![crate::session::capture::canonicalize_allowing_missing_leaf(
            &root,
        )?],
        configuration: Vec::new(),
        exported_default_store,
        cwd,
        filesystem: "host".into(),
        cwd_filesystem: "host".into(),
    })
}

pub(super) fn spawn_subprocess(
    config: &SpawnConfig,
) -> Result<
    (
        tokio::process::Child,
        Option<crate::session::ExecutionBinding>,
    ),
    AcpError,
> {
    // The daemon's PATH is frozen at launch, so resolve against known
    // node-manager dirs too (#1048).
    let app_dir = crate::session::get_app_dir().ok();
    let resolved = resolve_agent_command(&config.spec.command, app_dir.as_deref());
    let (spawn_command, extra_path_dirs) = match &resolved {
        Some(r) => (
            r.path.to_string_lossy().into_owned(),
            r.prepend_paths.clone(),
        ),
        None => (config.spec.command.clone(), Vec::new()),
    };

    let mut cmd = tokio::process::Command::new(&spawn_command);
    cmd.args(&config.spec.args)
        .current_dir(&config.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let keys = apply_stdio_env(&mut cmd, config, &extra_path_dirs);

    info!(
        target: "acp.protocol.spawn",
        command = %config.spec.command,
        resolved = %spawn_command,
        args = ?config.spec.args,
        cwd = %config.cwd.display(),
        transport = if config.socket_path.is_some() { "socket" } else { "stdio" },
        socket = ?config.socket_path,
        env_forwarded = ?keys.forwarded,
        env_inherited = ?keys.inherited,
        provider_env = ?keys.provider,
        host_environment = ?keys.host,
        "spawning ACP agent subprocess"
    );

    let native_store = native_store_snapshot(config, cmd.as_std(), &[]);
    let mut child = cmd.spawn().map_err(|e| {
        warn!(
            target: "acp.protocol.spawn",
            command = %config.spec.command,
            resolved = %spawn_command,
            "spawn failed: {e}"
        );
        // ENOENT is ambiguous: a missing cwd must classify as
        // ProjectPathMissing (#1089) before the unresolved-binary hint (#1048).
        if e.kind() == std::io::ErrorKind::NotFound && config.cwd.exists() && resolved.is_none() {
            AcpError::missing_binary_spawn_error(&e, &config.spec.command)
        } else {
            AcpError::classify_spawn_error(e, &config.cwd, &spawn_command)
        }
    })?;

    let pid = child.id();
    info!(
        target: "acp.protocol.spawn",
        command = %config.spec.command,
        pid = ?pid,
        "ACP agent subprocess started"
    );
    match child.stderr.take() {
        Some(stderr) => drain_stderr(stderr, config.spec.command.clone(), pid),
        None => warn!(
            target: "acp.protocol.spawn",
            command = %config.spec.command,
            pid = ?pid,
            "child has no stderr handle; agent crashes will be silent"
        ),
    }
    Ok((child, native_store))
}

/// An undrained stderr pipe fills and blocks the agent, which looks like a
/// wedged handshake.
fn drain_stderr(stderr: tokio::process::ChildStderr, command: String, pid: Option<u32>) {
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let mut reader = BufReader::new(stderr).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => debug!(
                    target: "acp.protocol.stderr",
                    command = %command,
                    pid = ?pid,
                    "{}",
                    scrub_stderr_secrets(&line),
                ),
                Ok(None) => {
                    debug!(target: "acp.protocol.stderr", command = %command, pid = ?pid, "stderr EOF");
                    break;
                }
                Err(e) => {
                    warn!(target: "acp.protocol.stderr", command = %command, pid = ?pid, "stderr read error: {e}");
                    break;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::{env_test_spawn_config, reset_fake_spawn_config};
    use crate::acp::acp_client::AcpClient;
    use crate::acp::state::{AcpSessionId, Event};
    use std::collections::HashMap;

    fn applied_env(config: &SpawnConfig) -> HashMap<String, String> {
        let mut cmd = std::process::Command::new("/bin/true");
        cmd.env_clear();
        apply_env_filter(&mut cmd, config, &[]);
        cmd.get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_classifies_missing_binary_and_missing_cwd() {
        let mut config = env_test_spawn_config(std::env::temp_dir());
        config.spec.command = "/nonexistent/agent/binary/aoe-test".into();
        let result = AcpClient::spawn(config, AcpSessionId("s-1".into())).await;
        assert!(matches!(result, Err(AcpError::Spawn(_))));

        // #1089: a renamed project path is typed, not a bare ENOENT.
        let missing =
            std::env::temp_dir().join(format!("aoe-test-missing-cwd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        let mut config = env_test_spawn_config(missing.clone());
        config.spec.command = "/bin/true".into();
        match AcpClient::spawn(config, AcpSessionId("s-1".into())).await {
            Err(AcpError::ProjectPathMissing { path }) => assert_eq!(path, missing),
            Err(other) => panic!("expected ProjectPathMissing, got {other:?}"),
            Ok(_) => panic!("expected ProjectPathMissing, got Ok"),
        }
    }

    /// The worker's handoff binding records an exported default store the way the
    /// terminal launch does, so an explicit selection keeps matching across surfaces.
    #[test]
    fn claude_snapshot_records_only_an_exported_default_store() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let config = env_test_spawn_config(home.clone());
        let command = std::process::Command::new("true");
        let default = home.join(".claude");
        let custom = home.join("custom");
        for (exported, expected) in [
            (None, false),
            (Some(&default), true),
            (Some(&custom), false),
        ] {
            let mut overrides = vec![("HOME".to_string(), home.display().to_string())];
            overrides.extend(
                exported.map(|dir| ("CLAUDE_CONFIG_DIR".to_string(), dir.display().to_string())),
            );
            let snapshot = native_store_snapshot(&config, &command, &overrides).unwrap();
            assert_eq!(
                snapshot.exported_default_store, expected,
                "exported={exported:?}"
            );
        }
    }

    #[test]
    fn env_deny_policies() {
        for (key, provider_denied, host_denied) in [
            ("AOE_TOKEN", true, true),
            ("PATH", true, false),
            ("HOME", true, false),
            ("LD_PRELOAD", true, false),
            ("LD_LIBRARY_PATH", true, false),
            ("DYLD_INSERT_LIBRARIES", true, false),
            ("", true, true),
            ("ANTHROPIC_API_KEY", false, false),
            ("CLAUDE_CODE_OAUTH_TOKEN", false, false),
            ("OPENAI_API_KEY", false, false),
            ("MY_CUSTOM_VAR", false, false),
            ("XDG_CONFIG_HOME", false, false),
            ("CODEX_HOME", false, false),
            ("1BAD", false, true),
            ("HAS-DASH", false, true),
        ] {
            assert_eq!(
                provider_env_denyreason(key).is_some(),
                provider_denied,
                "{key}"
            );
            assert_eq!(
                host_environment_denyreason(key).is_some(),
                host_denied,
                "{key}"
            );
        }
        assert!(host_environment_denyreason(crate::process::runner::ACP_AGENT_ENV).is_some());
        // #2691: both spawn paths read this one list.
        assert!(ALWAYS_FORWARD_ENV.contains(&"SSH_AUTH_SOCK"));
    }

    /// #3262: structured-view agents get the desktop env; sandboxed ones
    /// get `sandbox.environment` instead.
    #[test]
    #[serial_test::serial]
    fn apply_env_filter_forwards_desktop_env_to_host_agents_only() {
        let tmp = tempfile::tempdir().unwrap();
        let _app_dir = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let desktop = [
            ("DISPLAY", ":99"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        ];
        let _env = crate::session::test_support::EnvGuard::set(&desktop);
        let mut config = env_test_spawn_config(tmp.path().to_path_buf());
        let applied = applied_env(&config);
        for (key, expected) in desktop {
            assert_eq!(
                applied.get(key).map(String::as_str),
                Some(expected),
                "{key}"
            );
        }
        config.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: "alpine:latest".into(),
            container_name: "aoe-sandbox-envtest".into(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        });
        assert!(inherited_host_env_pairs(&config).is_empty());
    }

    /// #3238: each adapter receives only its own allowlisted credentials, and
    /// the allowlist cannot smuggle denied keys.
    #[test]
    #[serial_test::serial]
    fn apply_env_filter_applies_agent_env_allowlist() {
        let tmp = tempfile::tempdir().unwrap();
        let _app_dir = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("ANTHROPIC_API_KEY", "sk-anthropic"),
            ("OPENAI_API_KEY", "sk-openai"),
            ("GOOGLE_GENERATIVE_AI_API_KEY", "ai-google"),
            ("GEMINI_API_KEY", "ai-gemini-cli-key"),
            ("PRIME_API_KEY", "pk-prime"),
            ("AOE_TEST_UNLISTED_SENTINEL", "leak"),
            ("AOE_TOKEN", "daemon-secret"),
            ("LD_PRELOAD", "/tmp/evil.so"),
            ("CLAUDE_CONFIG_DIR", "/operator/claude"),
        ]);
        let reg = crate::acp::agent_registry::AgentRegistry::with_defaults();
        let mut config = env_test_spawn_config(tmp.path().to_path_buf());
        let denied_allowlist = AgentSpec {
            env_allowlist: Some(vec![
                "OPENAI_API_KEY".into(),
                "AOE_TOKEN".into(),
                "LD_PRELOAD".into(),
            ]),
            ..config.spec.clone()
        };
        let custom = crate::acp::AgentSpec::from_acp_cmd("custom", "/bin/true").unwrap();
        let cases: [(AgentSpec, &[(&str, &str)], &[&str]); 6] = [
            (
                reg.get("claude-code").unwrap().clone(),
                &[("CLAUDE_CONFIG_DIR", "/operator/claude")],
                &["OPENAI_API_KEY"],
            ),
            (
                reg.get("aoe-agent").unwrap().clone(),
                &[
                    ("ANTHROPIC_API_KEY", "sk-anthropic"),
                    ("OPENAI_API_KEY", "sk-openai"),
                    ("GOOGLE_GENERATIVE_AI_API_KEY", "ai-google"),
                ],
                &["GEMINI_API_KEY"],
            ),
            (
                reg.get("codex").unwrap().clone(),
                &[("OPENAI_API_KEY", "sk-openai")],
                &["ANTHROPIC_API_KEY"],
            ),
            // #3702
            (
                reg.get("prime-agent").unwrap().clone(),
                &[("PRIME_API_KEY", "pk-prime")],
                &["AOE_TEST_UNLISTED_SENTINEL"],
            ),
            (custom, &[], &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"]),
            (
                denied_allowlist,
                &[("OPENAI_API_KEY", "sk-openai")],
                &["AOE_TOKEN", "LD_PRELOAD"],
            ),
        ];
        for (spec, present, absent) in cases {
            config.spec = spec;
            let applied = applied_env(&config);
            for (key, value) in present {
                assert_eq!(applied.get(*key).map(String::as_str), Some(*value), "{key}");
            }
            for key in absent {
                assert!(!applied.contains_key(*key), "{key} leaked: {applied:#?}");
            }
        }

        // A request cannot move a worker off the store its conversation pins.
        config.spec = crate::acp::AgentSpec::from_acp_cmd("custom", "/bin/true").unwrap();
        config.provider_env = vec![
            ("CLAUDE_CONFIG_DIR".into(), "/request".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-request".into()),
        ];
        let applied = applied_env(&config);
        assert_eq!(
            applied.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-request")
        );
        assert!(!applied.contains_key("CLAUDE_CONFIG_DIR"), "{applied:#?}");
    }

    #[test]
    fn scrub_stderr_secrets_cases() {
        for input in [
            "auth failed: sk-ant-abcdefghijklmnop1234567890",
            "Bearer abcdefghijklmnop1234567890.signature",
            "GitHub PAT: ghp_abcdefghijklmnop1234567890",
            "legacy fine grained: github_pat_abcdefghijklmnop1234",
            "AWS: AKIAIOSFODNN7EXAMPLE",
        ] {
            assert!(
                scrub_stderr_secrets(input).contains("<redacted-secret>"),
                "{input}"
            );
        }
        for line in [
            "agent connected at /tmp/aoe.sock",
            "session/initialize ok, capabilities: load_session=true",
            "user prompt: please refactor src/main.rs to use anyhow",
            "the variable sk-test is fine",
        ] {
            assert_eq!(scrub_stderr_secrets(line), line);
        }
    }

    /// A stdio agent that answers `initialize` with `load_session`, and `reply`
    /// for its matching method.
    #[cfg(unix)]
    async fn scripted_agent(
        dir: &std::path::Path,
        load_session: bool,
        replies: &[(&str, &str)],
        stored: Option<&str>,
    ) -> AcpClient {
        let mut cases = format!(
            "    *'\"method\":\"initialize\"'*)\n      printf '{{\"jsonrpc\":\"2.0\",\"id\":%s,\"result\":{{\"protocolVersion\":1,\"agentCapabilities\":{{\"loadSession\":{load_session}}}}}}}\\n' \"$id\" ;;\n"
        );
        for (method, body) in replies {
            cases.push_str(&format!(
                "    *'\"method\":\"{method}\"'*)\n      printf '{{\"jsonrpc\":\"2.0\",\"id\":%s,{body}}}\\n' \"$id\" ;;\n"
            ));
        }
        let script = format!(
            "#!/bin/sh\nwhile IFS= read -r line; do\n  id=$(printf '%s' \"$line\" | sed -En 's/.*\"id\":(\"[^\"]*\"|[0-9]+).*/\\1/p')\n  case $line in\n{cases}  esac\ndone\n"
        );
        let path = dir.join("agent.sh");
        std::fs::write(&path, script).unwrap();
        let cwd = dir.join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();
        let mut config = reset_fake_spawn_config(&path, &cwd);
        config.stored_acp_session_id = stored.map(Into::into);
        AcpClient::spawn(config, AcpSessionId("scripted".into()))
            .await
            .expect("spawn fake agent")
    }

    /// Events until the channel closes, as `kind[:detail]` strings; a startup
    /// error fails the test.
    #[cfg(unix)]
    async fn terminal_events(client: &mut AcpClient) -> Vec<String> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut kinds = Vec::new();
        while let Some(event) = tokio::time::timeout_at(deadline, client.next_event())
            .await
            .expect("timed out waiting for the connection to end")
        {
            match event {
                Event::SessionContextReset { reason } => kinds.push(format!("reset:{reason}")),
                Event::RateLimit { info } => kinds.push(format!("rate_limit:{}", info.kind)),
                Event::Stopped { reason } => kinds.push(format!("stopped:{reason}")),
                Event::AgentStartupError { message } => {
                    panic!("recoverable failure surfaced a startup error: {message}")
                }
                _ => {}
            }
        }
        kinds
    }

    /// #3560: a prompt rejected because the agent dropped the resumed session
    /// resets context before the connection ends on a soft stop.
    #[cfg(unix)]
    #[tokio::test]
    async fn recoverable_startup_failures_do_not_surface_startup_errors() {
        let _env = crate::session::test_support::EnvGuard::read_lock();
        let dir = tempfile::tempdir().unwrap();
        let mut client = scripted_agent(
            dir.path(),
            true,
            &[
                ("session/load", r#""result":{}"#),
                ("session/new", r#""result":{"sessionId":"sid-1"}"#),
                (
                    "session/prompt",
                    r#""error":{"code":-32602,"message":"Unsupported ACP session"}"#,
                ),
            ],
            Some("sid-stored"),
        )
        .await;
        client.send_prompt("continue", &[]).await.unwrap();
        let kinds = terminal_events(&mut client).await;
        assert_eq!(kinds.len(), 2, "{kinds:?}");
        assert!(
            kinds[0].contains("resumed session no longer available"),
            "{kinds:?}"
        );
        assert_eq!(kinds[1], "stopped:stored_session_rejected");
        let _ = client.shutdown().await;

        // #3514: a limit hit at `session/new` parks the session instead of
        // failing startup and burning the restart budget.
        {
            let dir = tempfile::tempdir().unwrap();
            let mut client = scripted_agent(
                dir.path(),
                false,
                &[(
                    "session/new",
                    r#""error":{"code":-32603,"message":"Internal error","data":{"details":"You have hit your limit","errorKind":"rate_limit"}}"#,
                )],
                None,
            )
            .await;
            let kinds = terminal_events(&mut client).await;
            assert_eq!(kinds, ["rate_limit:rate_limit", "stopped:rate_limited"]);
            let _ = client.shutdown().await;
        }
    }
}
