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

/// Configuration for spawning an ACP agent.
#[derive(Debug, Clone)]
pub struct SpawnConfig {
    /// Registry key of the agent (e.g. `"claude"`, `"codex"`,
    /// `"opencode"`). Used to resolve the static `AgentProfile` that
    /// gates server-side claude-specific event synthesis and routes
    /// per-agent slash commands. Defaults to `"claude"` for legacy
    /// callers; the supervisor passes the real key when it spawns.
    pub agent_key: String,
    /// The logical session tool, as it would appear on `Instance.tool` for a
    /// terminal-view session. Distinct from `agent_key`: `agent_key` is the
    /// resolved ACP backend (which can differ from the tool on an override, a
    /// no-ACP-command custom agent, or after `switch-agent`), while `tool`
    /// stays fixed for the session's lifetime, including across a respawn
    /// that clones this `SpawnConfig` directly. `host_hooks.before_session`'s
    /// `AOE_TOOL` uses this field so a tool-scoped hook picks the same
    /// environment in structured view that it would in the terminal view.
    pub tool: String,
    pub spec: AgentSpec,
    pub cwd: PathBuf,
    pub additional_dirs: Vec<PathBuf>,
    /// Provider env vars to forward (after applying the agent's allowlist).
    pub provider_env: Vec<(String, String)>,
    /// Trusted global/profile `environment` entries ("Host Environment"),
    /// already resolved to concrete pairs, to apply to the agent process the
    /// way a terminal-view pane command applies them. The caller decides what
    /// belongs here: the supervisor leaves it empty for sandboxed agents,
    /// whose environment comes from `sandbox.environment` instead.
    ///
    /// Kept separate from `provider_env`, which is request-sourced: it carries
    /// a stricter denylist and loses to these entries on a shared key.
    pub host_environment: Vec<(String, String)>,
    /// Optional reasoning effort to apply through the adapter's
    /// `thought_level` config option after the handshake, on a fresh session
    /// (`session/new`, `session/fork`) and on a resumed one (`session/load`)
    /// alike, so a session's pinned effort survives a worker respawn.
    pub default_effort: Option<String>,
    /// Optional default mode to apply on fresh ACP sessions through the
    /// adapter's `category:"mode"` config option. Applied strictly: a value
    /// the agent does not advertise no-ops with a warning.
    pub default_mode: Option<String>,
    /// Reserved for a future agent-in-container that natively speaks
    /// the socket transport. The current structured view sandbox path runs
    /// `docker exec` from the host-side runner (which already holds the
    /// daemon↔runner socket) and proxies the agent's stdio across the
    /// container boundary, so no bind-mount is needed today.
    pub socket_path: Option<PathBuf>,
    /// ACP session id from a previous run, captured during the last
    /// `session/new` and persisted on `Instance.acp_session_id`.
    /// When `Some` and the agent advertises
    /// `agent_capabilities.load_session = true`, the connection task
    /// sends `LoadSessionRequest` instead of `NewSessionRequest`. On
    /// load failure the task falls back to `session/new` and emits a
    /// `SessionContextReset` event.
    pub stored_acp_session_id: Option<String>,
    /// When `Some`, this spawn is a structured fork: instead of `session/new`
    /// or `session/load`, the connection task sends `session/fork` with this
    /// parent ACP session id (provided the agent advertises the fork
    /// capability). The adapter mints a new child id, captured via
    /// `AcpSessionAssigned` and persisted on `Instance.acp_session_id`.
    /// Sourced from `Instance.fork_pending`.
    pub fork_from: Option<String>,
    /// When `Some`, the agent runs inside the named Docker container.
    /// Daemon-side spawn wraps the argv in `docker exec` and the
    /// fs/terminal handlers route across the container boundary using
    /// the container_workdir / mount map.
    pub sandbox_info: Option<SandboxInfo>,
    /// Source profile of the session. Used together with `sandbox_info`
    /// to resolve profile-level `sandbox.environment` entries so the
    /// structured view sandbox env mirrors the tmux view. `None` for
    /// non-sandboxed sessions.
    pub source_profile: Option<String>,
    /// MCP servers to forward to the agent on `session/new` and
    /// `session/load`, resolved from the global `<app_dir>/mcp.json` by the
    /// supervisor. Capability gating (dropping `http`/`sse` the agent did not
    /// advertise) happens later, against the `initialize` response. Empty when
    /// no config file exists, which preserves pre-feature behavior.
    pub mcp_servers: Vec<McpServer>,
    /// When true and this spawn resumes via `session/load`, seed the event
    /// store from the agent's history replay instead of suppressing it.
    /// Set for the first spawn of an imported Claude session whose store is
    /// empty; false for normal reattach (the transcript is already stored,
    /// so re-ingesting would duplicate-key panic). See #2276.
    pub seed_history_replay: bool,
    /// Host path of the session's managed artifact directory, exported to a
    /// local agent via `AOE_ARTIFACT_DIR`. A sandboxed agent instead sees the
    /// fixed container mount, so this host path is only used when
    /// `sandbox_info` is `None`. `None` disables the export. See #2587.
    pub artifact_dir: Option<PathBuf>,
    /// Set when this launch runs an `agent_detect_as` wrapper's base
    /// adapter instead of the wrapper itself (#3422): `(wrapper, base)`.
    /// Watchdog respawns reuse a cloned `SpawnConfig`, so they re-emit the
    /// same substitution warning the initial spawn logged, one line per
    /// launch.
    pub wrapper_substitution: Option<(String, String)>,
}

/// Reject `provider_env` request entries whose key would either escape
/// the agent sandbox (PATH, HOME, etc.; `always_forward` already wires
/// those from the operator's environment) or hijack the dynamic linker
/// (LD_PRELOAD, DYLD_INSERT_LIBRARIES, etc.) to run arbitrary code in
/// the child. Provider auth keys (`ANTHROPIC_API_KEY`, etc.) are
/// deliberately NOT on the denylist because per-session provider auth
/// is the legitimate use case for `provider_env`.
///
/// Returns `Some(reason)` if the key is rejected, `None` if it's safe
/// to forward. The reason string is logged as a structured field.
pub(super) fn provider_env_denyreason(key: &str) -> Option<&'static str> {
    if key.is_empty() {
        return Some("empty key");
    }
    if key == "AOE_TOKEN" {
        return Some("aoe auth token, must not reach the agent");
    }
    // Infrastructure / locale keys that `always_forward` already wires
    // from the parent env. Letting `provider_env` override them lets the
    // request point the agent's binary lookup or home tree at an
    // attacker-controlled location.
    const INFRA_KEYS: &[&str] = &["PATH", "HOME", "USER", "LANG", "LC_ALL", "TERM"];
    if INFRA_KEYS.contains(&key) {
        return Some("infrastructure key, controlled by operator env");
    }
    // Dynamic linker hooks: glibc `LD_*` and macOS `DYLD_*`. Overriding
    // these causes the child process to load attacker-chosen shared
    // objects before main(), bypassing the agent binary entirely.
    if key.starts_with("LD_") || key.starts_with("DYLD_") {
        return Some("dynamic linker hook, would alter child binary load");
    }
    None
}

/// Keys that configured host environment (`Config.environment`) must never
/// inject into an ACP agent. Unlike request-sourced `provider_env`, this list
/// deliberately does NOT cover infrastructure keys: `environment` is trusted
/// operator config (repo config cannot contribute it), and a terminal-view
/// pane already lets it override HOME/PATH via the shell assignment prefix.
/// Forwarding the same set to a structured worker is the parity this exists
/// for; what stays banned is aoe's own auth token and the reserved
/// daemon->runner carrier.
///
/// Shared with the runner side (`crate::process::runner::spawn_agent`) so the
/// two spawn paths cannot drift on policy.
pub(crate) fn host_environment_denyreason(key: &str) -> Option<&'static str> {
    if !crate::session::environment::is_valid_env_key(key) {
        return Some("not a valid environment variable name");
    }
    if key == "AOE_TOKEN" {
        return Some("aoe auth token, must not reach the agent");
    }
    if key == crate::process::runner::ACP_AGENT_ENV {
        return Some("reserved structured-worker environment carrier");
    }
    None
}

/// Scrub well-known secret patterns from agent stderr before it lands in
/// `debug.log`. Conservative; only redacts strings that unambiguously
/// signal a secret via prefix (Anthropic `sk-`, GitHub `ghp_`,
/// `Bearer <token>`, etc.). Catches the common case where an adapter
/// prints "auth failed: api_key=sk-ant-..."; will not catch a hand-rolled
/// secret with no recognisable shape. Users sharing logs in bug reports
/// should still scan them; see docs/acp.md#sharing-debug-logs.
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

/// Infrastructure variables forwarded from the operator environment to every
/// host-side agent, on both the detached-runner path (`apply_env_filter`) and
/// the in-proc stdio path (`spawn_subprocess`). Provider credentials come only
/// from the adapter's `env_allowlist` or explicit session configuration.
pub(super) const ALWAYS_FORWARD_ENV: &[&str] = &[
    "PATH",
    "HOME",
    // XDG_CONFIG_HOME drives `get_app_dir()` on Linux (see
    // src/session/mod.rs). Without forwarding, the runner falls
    // back to `$HOME/.config/agent-of-empires[-dev]`, which
    // diverges from the daemon when the operator (or live test
    // harness) has set XDG_CONFIG_HOME to a non-default value.
    // The runner then writes its WorkerRecord to a path the
    // daemon never reads, the daemon's `reap_user_stopped`
    // observes the registry as missing on the next tick, emits
    // `Stopped { user_stopped }`, and respawns, turning a fine
    // worker into a respawn loop. See #1383 (CI Linux live
    // specs under an isolated $XDG_CONFIG_HOME).
    "XDG_CONFIG_HOME",
    "LANG",
    "LC_ALL",
    "TERM",
    "USER",
    // Path to the operator's ssh-agent socket. Forwarding it lets the
    // agent's git subprocess authenticate over SSH; without it, git SSH
    // has no agent to connect to (most visible on Linux, where the socket
    // lives in the environment). The value is a socket path, not a secret;
    // the security lives in the ssh-agent behind it. See #2691.
    "SSH_AUTH_SOCK",
];

/// The inherited host environment layer for a structured-view agent, applied
/// under [`ALWAYS_FORWARD_ENV`] on both spawn paths.
///
/// This is the fix for #3262. #3079 added desktop-env forwarding for the tmux
/// paths only, so an agent in the structured view still got `env_clear()` plus
/// the fixed base allowlist and never saw `DISPLAY`: the very
/// symptom #3075 reported, still live for anyone driving aoe from the browser.
/// Routing both views through
/// [`crate::session::environment::inherited_host_env`] is what keeps them from
/// drifting again.
///
/// Applied first, so `ALWAYS_FORWARD_ENV` (and its `PATH` prepend), the agent
/// allowlist, `provider_env`, and the operator's `environment` list all still
/// win on a shared key.
/// Returns pairs rather than taking a `Command` because the two spawn sites use
/// different `Command` types (`std` on the runner path, `tokio` in-proc).
pub(super) fn inherited_host_env_pairs(config: &SpawnConfig) -> Vec<(String, String)> {
    // A sandboxed agent's environment is `sandbox.environment` by contract; the
    // host's desktop and toolchain vars mean nothing inside the container.
    if config.sandbox_info.is_some() {
        return Vec::new();
    }
    let profile = config.source_profile.as_deref().unwrap_or_default();
    crate::session::environment::inherited_host_env(profile)
}

/// Allowlist entries whose value is a host filesystem path rather than a
/// credential. They are legitimate on the two host spawn paths and must not
/// cross into a container: the path names nothing there, so forwarding it
/// points the adapter away from the config dir `AGENT_CONFIG_MOUNTS` mounts
/// at the canonical container location. `CLAUDE_CONFIG_DIR` is the case that
/// established the rule; the other two arrived with the per-adapter allowlists
/// in #3238. Adding a path-valued key to `env_allowlist_for` means adding it
/// here too.
pub(super) fn is_host_only_path_env(key: &str) -> bool {
    matches!(
        key,
        "CLAUDE_CONFIG_DIR" | "CODEX_HOME" | "GOOGLE_APPLICATION_CREDENTIALS"
    )
}

/// Resolve the adapter's `env_allowlist` (#3238) against the operator's live
/// environment, dropping any key the provider-env deny policy rejects
/// (`AOE_TOKEN`, infra keys, `LD_*`/`DYLD_*` linker hooks). Shared by all
/// three spawn paths (detached runner, in-proc stdio, and the docker-exec
/// sandbox wrap) so the forwarded set and the deny posture cannot drift
/// between them. Returns `(key, value)` for each allowlisted key present in
/// the host env, warning on a rejected one. It deliberately does not cover
/// the daemon->runner carrier `AOE_ACP_AGENT_ENV`; that is
/// `host_environment_denyreason`'s job, and the runner clears the carrier
/// before exec regardless.
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

/// Apply the env_clear + allowlist + provider_env filtering used by both
/// the detached-runner path and the in-proc stdio path. Pulled out so
/// the two spawn sites share the same security posture.
pub(super) fn apply_env_filter(cmd: &mut std::process::Command, config: &SpawnConfig) {
    for (key, value) in inherited_host_env_pairs(config) {
        cmd.env(key, value);
    }
    for name in ALWAYS_FORWARD_ENV {
        if let Ok(value) = std::env::var(name) {
            cmd.env(name, value);
        }
    }
    for (key, value) in allowlisted_env_pairs(config) {
        cmd.env(key, value);
    }
    for (key, value) in &config.provider_env {
        if provider_env_denyreason(key).is_some() {
            continue;
        }
        cmd.env(key, value);
    }
}

pub(super) fn spawn_subprocess(config: &SpawnConfig) -> Result<tokio::process::Child, AcpError> {
    // Resolve bare command names against PATH + known node-manager dirs.
    // `aoe serve` captures PATH at daemon-launch time and freezes it for
    // its lifetime; without this, a `nvm use` after launch leaves the
    // adapter installed but unreachable. See #1048.
    let app_dir = crate::session::get_app_dir().ok();
    let resolved = resolve_agent_command(&config.spec.command, app_dir.as_deref());
    let (spawn_command, extra_path_dirs): (String, Vec<std::path::PathBuf>) = match &resolved {
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

    // Env: clear, then forward shared infrastructure plus the selected
    // adapter's provider allowlist. AOE_TOKEN must NEVER reach the agent. The
    // shared helper keeps the runner and in-proc paths from drifting.
    cmd.env_clear();
    // Under the allowlist, so ALWAYS_FORWARD_ENV's PATH prepend still wins.
    // Same layer the runner path applies in `apply_env_filter`; see #3262.
    let mut inherited_keys: Vec<String> = Vec::new();
    for (key, value) in inherited_host_env_pairs(config) {
        cmd.env(&key, value);
        inherited_keys.push(key);
    }
    let mut forwarded_keys: Vec<&str> = Vec::new();
    for &name in ALWAYS_FORWARD_ENV {
        if let Ok(mut value) = std::env::var(name) {
            // Prepend the resolved bin dir to PATH so the adapter's own
            // `node`/`npx` lookups land in the same node install as the
            // adapter itself, not whatever node happens to be on the
            // daemon's frozen PATH.
            if name == "PATH" && !extra_path_dirs.is_empty() {
                let existing: Vec<std::path::PathBuf> = std::env::split_paths(&value).collect();
                let mut chain: Vec<std::path::PathBuf> = Vec::new();
                for dir in &extra_path_dirs {
                    if !existing.contains(dir) && !chain.contains(dir) {
                        chain.push(dir.clone());
                    }
                }
                chain.extend(existing);
                if let Ok(joined) = std::env::join_paths(&chain) {
                    value = joined.to_string_lossy().into_owned();
                }
            }
            cmd.env(name, value);
            forwarded_keys.push(name);
        }
    }
    // Same allowlist + deny posture as the detached-runner path, via the
    // shared helper so the two spawn sites cannot drift (#3238).
    let allowlisted = allowlisted_env_pairs(config);
    for (key, value) in &allowlisted {
        cmd.env(key, value);
        forwarded_keys.push(key.as_str());
    }
    let mut provider_keys: Vec<&str> = Vec::new();
    for (key, value) in &config.provider_env {
        if let Some(reason) = provider_env_denyreason(key) {
            warn!(
                target: "acp",
                key = %key,
                reason,
                "rejecting provider_env override of protected key",
            );
            continue;
        }
        cmd.env(key, value);
        provider_keys.push(key.as_str());
    }
    // Applied last so trusted operator config outranks the request-sourced
    // `provider_env` on a shared key. The detached-runner path has the same
    // precedence for free (the runner overrides its inherited env when it
    // spawns the adapter), and the two must agree.
    let mut host_env_keys: Vec<&str> = Vec::new();
    for (key, value) in &config.host_environment {
        if let Some(reason) = host_environment_denyreason(key) {
            warn!(
                target: "acp",
                key = %key,
                reason,
                "rejecting configured host environment key",
            );
            continue;
        }
        cmd.env(key, value);
        host_env_keys.push(key.as_str());
    }

    // Socket-transport agents need to know where to connect. Pass the
    // path via env so the agent's bootstrap can `connect()` to it
    // instead of falling back to stdio.
    if let Some(socket_path) = &config.socket_path {
        cmd.env("AOE_ACP_SOCKET", socket_path);
    }

    info!(
        target: "acp.protocol.spawn",
        command = %config.spec.command,
        resolved = %spawn_command,
        args = ?config.spec.args,
        cwd = %config.cwd.display(),
        transport = if config.socket_path.is_some() { "socket" } else { "stdio" },
        socket = ?config.socket_path,
        env_forwarded = ?forwarded_keys,
        // Key names only, like every other env field here: the whole point of
        // the layer is that it can carry the operator's secrets under
        // `session.inherit_host_environment`.
        env_inherited = ?inherited_keys,
        provider_env = ?provider_keys,
        host_environment = ?host_env_keys,
        "spawning ACP agent subprocess"
    );

    let mut child = cmd.spawn().map_err(|e| {
        warn!(
            target: "acp.protocol.spawn",
            command = %config.spec.command,
            resolved = %spawn_command,
            "spawn failed: {e}"
        );
        // POSIX ENOENT on `Command::spawn` is ambiguous: missing binary,
        // missing cwd, or missing interpreter all surface as the same
        // libc error. Order matters here:
        //   1. cwd missing → ProjectPathMissing (so the UI renders the
        //      "restore or rebind project_path" banner, not the
        //      install-adapter copy). See #1089.
        //   2. bare-command ENOENT with no PATH resolution → enriched
        //      Spawn message hinting at the frozen-PATH cause. See #1048.
        //   3. fallback → generic Spawn classification.
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

    // Drain stderr line-by-line into the tracing log. Without this the
    // child's stderr pipe fills up at ~64KB and the agent blocks on
    // write, looking like a wedged ACP handshake. Logging every line
    // also gives us a record of what the adapter said before it died.
    if let Some(stderr) = child.stderr.take() {
        let command_label = config.spec.command.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stderr).lines();
            loop {
                match reader.next_line().await {
                    Ok(Some(line)) => {
                        debug!(
                            target: "acp.protocol.stderr",
                            command = %command_label,
                            pid = ?pid,
                            "{}",
                            scrub_stderr_secrets(&line),
                        );
                    }
                    Ok(None) => {
                        debug!(
                            target: "acp.protocol.stderr",
                            command = %command_label,
                            pid = ?pid,
                            "stderr EOF"
                        );
                        break;
                    }
                    Err(e) => {
                        warn!(
                            target: "acp.protocol.stderr",
                            command = %command_label,
                            pid = ?pid,
                            "stderr read error: {e}"
                        );
                        break;
                    }
                }
            }
        });
    } else {
        warn!(
            target: "acp.protocol.spawn",
            command = %config.spec.command,
            pid = ?pid,
            "child has no stderr handle; agent crashes will be silent"
        );
    }

    Ok(child)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::acp_client::test_helpers::env_test_spawn_config;
    use crate::acp::acp_client::AcpClient;
    use crate::acp::state::AcpSessionId;

    #[tokio::test]
    #[serial_test::serial]
    async fn spawn_with_nonexistent_command_errors_cleanly() {
        let config = SpawnConfig {
            wrapper_substitution: None,
            agent_key: "claude".into(),
            tool: "claude".into(),
            spec: AgentSpec {
                command: "/nonexistent/agent/binary/aoe-test".into(),
                args: vec![],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd: std::env::temp_dir(),
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
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let result = AcpClient::spawn(config, AcpSessionId("s-1".into())).await;
        assert!(matches!(result, Err(AcpError::Spawn(_))));
    }

    /// Pre-flight cwd check: when `project_path` was renamed out from
    /// under the session, the supervisor's spawn fails with a typed
    /// `ProjectPathMissing` instead of a bare ENOENT-mapped `Spawn`.
    /// See #1089.
    #[tokio::test]
    async fn spawn_returns_project_path_missing_when_cwd_does_not_exist() {
        let missing =
            std::env::temp_dir().join(format!("aoe-test-missing-cwd-{}", std::process::id()));
        // Ensure the path truly does not exist.
        let _ = std::fs::remove_dir_all(&missing);
        let config = SpawnConfig {
            wrapper_substitution: None,
            agent_key: "claude".into(),
            tool: "claude".into(),
            spec: AgentSpec {
                command: "/bin/true".into(),
                args: vec![],
                description: "test".into(),
                env_allowlist: None,
            },
            cwd: missing.clone(),
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
            sandbox_info: None,
            source_profile: None,
            mcp_servers: Vec::new(),
        };
        let result = AcpClient::spawn(config, AcpSessionId("s-1".into())).await;
        match result {
            Err(AcpError::ProjectPathMissing { path }) => assert_eq!(path, missing),
            Err(other) => panic!("expected ProjectPathMissing, got {other:?}"),
            Ok(_) => panic!("expected ProjectPathMissing, got Ok"),
        }
    }

    #[test]
    fn provider_env_denyreason_blocks_infra_and_linker_keys() {
        assert!(provider_env_denyreason("AOE_TOKEN").is_some());
        assert!(provider_env_denyreason("PATH").is_some());
        assert!(provider_env_denyreason("HOME").is_some());
        assert!(provider_env_denyreason("LD_PRELOAD").is_some());
        assert!(provider_env_denyreason("LD_LIBRARY_PATH").is_some());
        assert!(provider_env_denyreason("DYLD_INSERT_LIBRARIES").is_some());
        assert!(provider_env_denyreason("").is_some());
    }

    #[test]
    fn provider_env_denyreason_allows_provider_auth_keys() {
        // The legitimate use case: per-session auth override.
        assert!(provider_env_denyreason("ANTHROPIC_API_KEY").is_none());
        assert!(provider_env_denyreason("CLAUDE_CODE_OAUTH_TOKEN").is_none());
        assert!(provider_env_denyreason("OPENAI_API_KEY").is_none());
        assert!(provider_env_denyreason("AOE_AGENT_MODEL").is_none());
        // Custom provider keys should pass through.
        assert!(provider_env_denyreason("MY_CUSTOM_VAR").is_none());
    }

    #[test]
    fn host_environment_denyreason_blocks_token_carrier_and_invalid_keys() {
        // aoe's own auth token and the reserved daemon->runner carrier must
        // never ride the trusted `Config.environment` list into an agent.
        assert!(host_environment_denyreason("AOE_TOKEN").is_some());
        assert!(host_environment_denyreason(crate::process::runner::ACP_AGENT_ENV).is_some());
        // Malformed keys are rejected before they reach `Command::env`.
        assert!(host_environment_denyreason("").is_some());
        assert!(host_environment_denyreason("1BAD").is_some());
        assert!(host_environment_denyreason("HAS-DASH").is_some());
    }

    #[test]
    fn host_environment_denyreason_allows_infra_and_config_keys() {
        // The whole point of Host Environment: unlike request-sourced
        // `provider_env`, trusted operator config MAY set HOME / PATH /
        // XDG_CONFIG_HOME (the terminal-view prefix already can), plus the
        // motivating `CODEX_HOME` and arbitrary custom keys.
        assert!(host_environment_denyreason("HOME").is_none());
        assert!(host_environment_denyreason("PATH").is_none());
        assert!(host_environment_denyreason("XDG_CONFIG_HOME").is_none());
        assert!(host_environment_denyreason("CODEX_HOME").is_none());
        assert!(host_environment_denyreason("GIT_CONFIG_GLOBAL").is_none());
        assert!(host_environment_denyreason("MY_CUSTOM_VAR").is_none());
    }

    #[test]
    fn always_forward_env_includes_ssh_auth_sock() {
        // Regression guard for #2691: without SSH_AUTH_SOCK in the shared
        // forward list, git-over-SSH has no ssh-agent socket to reach.
        // Both spawn paths (`apply_env_filter`, `spawn_subprocess`) read
        // this one const, so its membership is also the parity guarantee
        // between the runner path and the in-proc stdio path.
        assert!(ALWAYS_FORWARD_ENV.contains(&"SSH_AUTH_SOCK"));
    }

    /// Regression test for #3262. The structured view spawns its agent with
    /// `env_clear()` plus `ALWAYS_FORWARD_ENV`, and #3079 wired desktop-env
    /// forwarding into the tmux paths only. So a browser-view agent still had
    /// no `DISPLAY` and could not open an OIDC login, the original #3075
    /// symptom. Before the fix this asserted set was exactly
    /// `{CLAUDE_CONFIG_DIR, HOME, PATH, TERM}`.
    ///
    /// `#[serial]` because it mutates the process-wide env, which parallel
    /// readers of `std::env::var` would race.
    #[test]
    #[serial_test::serial]
    fn apply_env_filter_forwards_desktop_env_to_structured_view_agents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // An isolated app dir keeps the operator's real env snapshot (and
        // `inherit_host_environment` setting) out of the assertion.
        let _app_dir = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("DISPLAY", ":99"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        ]);

        let config = env_test_spawn_config(tmp.path().to_path_buf());
        let mut cmd = std::process::Command::new("/bin/true");
        cmd.env_clear();
        apply_env_filter(&mut cmd, &config);

        let applied: std::collections::HashMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        for (key, expected) in [
            ("DISPLAY", ":99"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
        ] {
            assert_eq!(
                applied.get(key).map(String::as_str),
                Some(expected),
                "{key} must reach a structured-view agent, got {applied:#?}"
            );
        }
    }

    /// #3238: `AgentSpec.env_allowlist` populated by `with_defaults` must
    /// reach the agent via `apply_env_filter`. Uses the `aoe-agent` spec (the
    /// one that would silently no-op if we keyed `env_allowlist_for` on
    /// `spec.command`, which is placeholder-templated, instead of the binary
    /// token).
    /// A negative case rides along: `GEMINI_API_KEY` is set but not in the
    /// AI-SDK-based `aoe-agent`'s allowlist, so it must NOT be forwarded.
    #[test]
    #[serial_test::serial]
    fn apply_env_filter_forwards_agent_env_allowlist() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("ANTHROPIC_API_KEY", "sk-anthropic"),
            ("OPENAI_API_KEY", "sk-openai"),
            ("GOOGLE_GENERATIVE_AI_API_KEY", "ai-google"),
            ("GEMINI_API_KEY", "ai-gemini-cli-key"),
        ]);

        let mut config = env_test_spawn_config(tmp.path().to_path_buf());
        let reg = crate::acp::agent_registry::AgentRegistry::with_defaults();
        config.spec = reg.get("aoe-agent").expect("aoe-agent default").clone();

        let mut cmd = std::process::Command::new("/bin/true");
        cmd.env_clear();
        apply_env_filter(&mut cmd, &config);

        let applied: std::collections::HashMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(
            applied.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-anthropic")
        );
        assert_eq!(
            applied.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-openai")
        );
        assert_eq!(
            applied
                .get("GOOGLE_GENERATIVE_AI_API_KEY")
                .map(String::as_str),
            Some("ai-google")
        );
        assert!(
            !applied.contains_key("GEMINI_API_KEY"),
            "aoe-agent (AI-SDK) must not receive the gemini-CLI-native key, got {applied:#?}"
        );

        config.spec = reg.get("codex").expect("codex default").clone();
        let mut codex_cmd = std::process::Command::new("/bin/true");
        codex_cmd.env_clear();
        apply_env_filter(&mut codex_cmd, &config);
        let codex_env: std::collections::HashMap<String, String> = codex_cmd
            .get_envs()
            .filter_map(|(key, value)| {
                Some((
                    key.to_string_lossy().into_owned(),
                    value?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(
            codex_env.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-openai")
        );
        assert!(
            !codex_env.contains_key("ANTHROPIC_API_KEY"),
            "Codex must not receive another adapter's ambient credential, got {codex_env:#?}"
        );

        // A custom `agent_acp_cmd` adapter carries no allowlist at all, so it
        // gets no ambient provider credential: the whole point of moving the
        // Claude keys off `ALWAYS_FORWARD_ENV`.
        config.spec = crate::acp::AgentSpec::from_acp_cmd("custom", "/bin/true").expect("spec");
        let mut custom_cmd = std::process::Command::new("/bin/true");
        custom_cmd.env_clear();
        apply_env_filter(&mut custom_cmd, &config);
        let custom_keys: Vec<String> = custom_cmd
            .get_envs()
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        for key in ["ANTHROPIC_API_KEY", "OPENAI_API_KEY"] {
            assert!(
                !custom_keys.iter().any(|k| k == key),
                "a custom adapter must not receive the ambient {key}, got {custom_keys:?}"
            );
        }
    }

    /// #3238 security posture: `spec.env_allowlist` (which a custom-agent
    /// definition or an edited config could populate) must not become a
    /// smuggling channel. Every entry runs through `provider_env_denyreason`,
    /// so `AOE_TOKEN` and `LD_*`/`DYLD_*` linker hooks are dropped even when
    /// the operator's environment has them set, while a legitimate provider
    /// key still forwards. The deny predicate the fix routes through had
    /// positive coverage only.
    #[test]
    #[serial_test::serial]
    fn apply_env_filter_drops_denied_allowlist_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("OPENAI_API_KEY", "sk-openai"),
            ("AOE_TOKEN", "daemon-secret"),
            ("LD_PRELOAD", "/tmp/evil.so"),
        ]);

        let mut config = env_test_spawn_config(tmp.path().to_path_buf());
        config.spec.env_allowlist = Some(vec![
            "OPENAI_API_KEY".into(),
            "AOE_TOKEN".into(),
            "LD_PRELOAD".into(),
        ]);

        let mut cmd = std::process::Command::new("/bin/true");
        cmd.env_clear();
        apply_env_filter(&mut cmd, &config);

        let applied: std::collections::HashMap<String, String> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert_eq!(
            applied.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-openai"),
            "a legitimately allowlisted provider key must forward"
        );
        assert!(
            !applied.contains_key("AOE_TOKEN"),
            "the daemon auth token must never reach the agent, got {applied:#?}"
        );
        assert!(
            !applied.contains_key("LD_PRELOAD"),
            "a linker hook must be denied even when allowlisted, got {applied:#?}"
        );
    }

    /// A sandboxed agent's environment is `sandbox.environment` by contract:
    /// host desktop vars mean nothing inside the container, and forwarding them
    /// would silently widen what the sandbox exposes.
    #[test]
    #[serial_test::serial]
    fn inherited_host_env_pairs_skips_sandboxed_agents() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let _env = crate::session::test_support::EnvGuard::set(&[("DISPLAY", ":99")]);

        let mut config = env_test_spawn_config(tmp.path().to_path_buf());
        assert!(
            inherited_host_env_pairs(&config)
                .iter()
                .any(|(k, _)| k == "DISPLAY"),
            "host agents get the desktop env"
        );

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
        assert!(
            inherited_host_env_pairs(&config).is_empty(),
            "sandboxed agents get sandbox.environment instead"
        );
    }

    #[test]
    fn scrub_stderr_secrets_redacts_known_prefixes() {
        let cases = [
            ("auth failed: sk-ant-abcdefghijklmnop1234567890", true),
            ("Bearer abcdefghijklmnop1234567890.signature", true),
            ("GitHub PAT: ghp_abcdefghijklmnop1234567890", true),
            ("legacy fine grained: github_pat_abcdefghijklmnop1234", true),
            ("AWS: AKIAIOSFODNN7EXAMPLE", true),
        ];
        for (input, should_redact) in cases {
            let scrubbed = scrub_stderr_secrets(input);
            if should_redact {
                assert!(
                    scrubbed.contains("<redacted-secret>"),
                    "expected redaction in {input:?}, got {scrubbed:?}"
                );
            } else {
                assert_eq!(scrubbed, input);
            }
        }
    }

    #[test]
    fn scrub_stderr_secrets_leaves_innocuous_lines_alone() {
        // Common-case debug lines that must not get false-positive
        // redaction or the log loses diagnostic value.
        let lines = [
            "agent connected at /tmp/aoe.sock",
            "session/initialize ok, capabilities: load_session=true",
            "user prompt: please refactor src/main.rs to use anyhow",
            // Even though "sk-" appears, the literal isn't long enough
            // to match the secret regex.
            "the variable sk-test is fine",
        ];
        for line in lines {
            assert_eq!(scrub_stderr_secrets(line), line);
        }
    }
}
