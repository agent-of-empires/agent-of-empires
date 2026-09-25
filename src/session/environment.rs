//! Environment variable helpers for session instances.

use super::config::SandboxConfig;
use super::instance::SandboxInfo;
use crate::containers::container_interface::EnvEntry;

/// Terminal environment variables that are always passed through for proper UI/theming
pub(crate) const DEFAULT_TERMINAL_ENV_VARS: &[&str] =
    &["TERM", "COLORTERM", "FORCE_COLOR", "NO_COLOR"];

/// Vertex provider env vars auto-forwarded into sandbox containers when `CLAUDE_CODE_USE_VERTEX` is
/// set on the host.
pub(crate) const AUTO_FORWARD_VERTEX_ENV_VARS: &[&str] = &[
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_VERTEX_REGION",
    "CLAUDE_CODE_USE_VERTEX",
    "CLOUD_ML_REGION",
];

/// Returns true when `CLAUDE_CODE_USE_VERTEX` is set on the host to a non-empty value.
pub(crate) fn host_vertex_enabled() -> bool {
    std::env::var("CLAUDE_CODE_USE_VERTEX")
        .ok()
        .is_some_and(|v| !v.is_empty())
}

/// Returns the user's preferred shell from `$SHELL`, falling back to `bash`.
pub(crate) fn user_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "bash".to_string())
}

/// Desktop and session environment variables a user's graphical login sets but that tmux does not
/// reliably carry into a `new-session`.
const FORWARDED_DESKTOP_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
];

/// Why the wholesale passthrough ([`inherited_host_env`] with `session.inherit_host_environment`
/// on) refuses a key, or `None` when it may be forwarded.
fn passthrough_denyreason(key: &str) -> Option<&'static str> {
    if !is_valid_env_key(key) {
        return Some("not a valid environment variable name");
    }
    if key.starts_with("AOE_") || key.starts_with("AGENT_OF_EMPIRES_") {
        return Some("aoe-internal wiring or credential");
    }
    if key == "TERM" {
        return Some("terminal type is owned by tmux and the spawn allowlists");
    }
    None
}

/// The environment a host session inherits from aoe, as `(KEY, VALUE)` pairs.
pub(crate) fn inherited_host_env(profile: &str) -> Vec<(String, String)> {
    let passthrough = super::config::profile_config::resolve_config_or_warn(
        &super::config::effective_profile(profile),
    )
    .session
    .inherit_host_environment;
    let vars = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
    inherited_host_env_from(vars, passthrough)
}

/// Pure core of [`inherited_host_env`], split out so the filtering is unit-tested without mutating
/// the process environment.
fn inherited_host_env_from<I>(vars: I, passthrough: bool) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let keep = |key: &str| {
        if passthrough {
            passthrough_denyreason(key).is_none()
        } else {
            key.starts_with("XDG_") || FORWARDED_DESKTOP_VARS.contains(&key)
        }
    };
    let mut pairs: Vec<(String, String)> = vars
        .into_iter()
        .filter(|(key, value)| !value.is_empty() && keep(key))
        .collect();
    pairs.sort();
    pairs
}

/// Shells whose quoting rules are incompatible with POSIX `'\''` escaping.
const NON_POSIX_SHELLS: &[&str] = &["fish", "nu", "nushell", "pwsh", "powershell"];

/// Shells we can safely launch with a `-l` login flag.
const LOGIN_FLAG_SHELLS: &[&str] = &[
    "bash", "zsh", "sh", "ash", "ksh", "mksh", "dash", "fish", "csh", "tcsh",
];

/// The `LOGIN_FLAG_SHELLS` basenames as one POSIX `case` pattern
/// (`bash|zsh|...`), for embedding in a generated shell script.
pub(crate) fn login_flag_shell_case_pattern() -> String {
    LOGIN_FLAG_SHELLS.join("|")
}

/// Every shell basename this codebase recognizes, as one POSIX `case` pattern,
/// whether or not `-l` applies to it.
pub(crate) fn known_shell_case_pattern() -> String {
    let mut names: Vec<&str> = Vec::with_capacity(LOGIN_FLAG_SHELLS.len() + NON_POSIX_SHELLS.len());
    for name in LOGIN_FLAG_SHELLS.iter().chain(NON_POSIX_SHELLS) {
        if !names.contains(name) {
            names.push(name);
        }
    }
    names.join("|")
}

/// Build the tmux pane command that launches `shell` as a login+interactive shell, so it sources
/// the user's profile and rc files (`~/.zprofile`, `~/.zshrc`, oh-my-zsh, Homebrew/nvm PATH setup)
/// exactly as a native terminal would.
pub(crate) fn login_shell_command(shell: &str) -> String {
    let escaped = shell_escape(shell);
    if LOGIN_FLAG_SHELLS.contains(&shell_basename(shell)) {
        format!("{escaped} -l")
    } else {
        escaped
    }
}

/// Like [`user_shell`], but falls back to `bash` when the user's shell is non-POSIX (e.g. fish,
/// nushell, pwsh).
pub(crate) fn user_posix_shell() -> String {
    let shell = user_shell();
    if NON_POSIX_SHELLS.contains(&shell_basename(&shell)) {
        "bash".to_string()
    } else {
        shell
    }
}

fn shell_basename(shell: &str) -> &str {
    std::path::Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(shell)
}

/// Shell-escape a value for safe interpolation into a shell command string.
pub(crate) fn shell_escape(val: &str) -> String {
    let val = val.replace('\n', "\\n").replace('\r', "\\r");
    let escaped = val.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

/// Quote one POSIX script word without changing its bytes.
pub(crate) fn shell_escape_script_word(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A pane-visible context notice. Data cannot inject terminal controls or shell
/// syntax, and printf treats percent signs and backslashes as literal content.
pub(crate) fn native_context_notice_command(message: &str) -> String {
    let safe: std::borrow::Cow<'_, str> = if message.chars().any(char::is_control) {
        let mut escaped = String::with_capacity(message.len());
        for character in message.chars() {
            if character.is_control() {
                escaped.extend(character.escape_default());
            } else {
                escaped.push(character);
            }
        }
        std::borrow::Cow::Owned(escaped)
    } else {
        std::borrow::Cow::Borrowed(message)
    };
    format!("printf '%s\\n' {}", shell_escape(&safe))
}

/// Resolve a session's sandbox environment entries to concrete `(KEY, VALUE)` pairs on the host,
/// for feeding into a host-side hook's process environment (so a `before_start` hook can read a
/// per-session `$TEST_VAR`).
pub(crate) fn session_host_env_pairs(
    profile: &str,
    project_path: &std::path::Path,
    sandbox_info: &SandboxInfo,
) -> Vec<(String, String)> {
    let resolved_profile = super::config::effective_profile(profile);
    let trusted = super::config::profile_config::resolve_config_or_warn(&resolved_profile)
        .sandbox
        .environment;
    let entries = match sandbox_info.extra_env.as_deref() {
        None => trusted,
        Some(extra) => {
            let repo_aware = super::config::repo_config::resolve_config_with_repo_or_warn(
                &resolved_profile,
                project_path,
            )
            .sandbox
            .environment;
            host_hook_entries(extra, &trusted, &repo_aware)
        }
    };
    resolve_hook_env_pairs(&entries)
}

/// Filter a session's `extra_env` down to the entries safe to expose to a host hook: everything
/// except entries the repo contributed (present in the repo-aware config but not in the
/// profile/global `trusted` baseline).
fn host_hook_entries(extra: &[String], trusted: &[String], repo_aware: &[String]) -> Vec<String> {
    let trusted: std::collections::HashSet<&str> = trusted.iter().map(String::as_str).collect();
    let repo_contributed: std::collections::HashSet<&str> = repo_aware
        .iter()
        .map(String::as_str)
        .filter(|e| !trusted.contains(e))
        .collect();
    extra
        .iter()
        .filter(|e| !repo_contributed.contains(e.as_str()))
        .cloned()
        .collect()
}

/// Resolve `sandbox.environment` entries to concrete host `(KEY, VALUE)` pairs for a `before_start`
/// host hook (the pure core of [`session_host_env_pairs`], split out so it can be tested without
/// touching config on disk).
fn resolve_hook_env_pairs(entries: &[String]) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut pairs = Vec::new();
    for entry in entries {
        let (key, value) = match entry.split_once('=') {
            Some((k, v)) => (k.to_string(), resolve_env_value(v)),
            None => (entry.clone(), std::env::var(entry).ok()),
        };
        // A malformed key would fail at `Command::envs` when the hook spawns;
        // skip it here (with a warning) rather than aborting the launch.
        if !is_valid_env_key(&key) {
            tracing::warn!(target: "session.create", "invalid env key '{}' for host hook; skipping", key);
            continue;
        }
        if let Some(v) = value {
            if seen.insert(key.clone()) {
                pairs.push((key, v));
            }
        }
    }
    pairs
}

/// Drop every static `environment` entry whose key was minted by `host_hooks.before_session`, so
/// OMP pre-launch routing resolves the same minted-wins environment that the pane later loads from
/// its protected file.
pub(crate) fn drop_shadowed_host_entries(
    entries: Vec<String>,
    minted: &[(String, String)],
) -> Vec<String> {
    if minted.is_empty() {
        return entries;
    }
    let minted_keys: std::collections::HashSet<&str> =
        minted.iter().map(|(k, _)| k.as_str()).collect();
    entries
        .into_iter()
        .filter(|entry| {
            let key = entry.split_once('=').map(|(k, _)| k).unwrap_or(entry);
            !minted_keys.contains(key)
        })
        .collect()
}

/// True when `key` is a valid environment variable name: an ASCII letter or `_` first, then ASCII
/// alphanumerics or `_`.
pub(crate) fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn resolve_host_environment_value(
    entries: &[String],
    target_key: &str,
) -> Option<String> {
    let mut resolved_value = None;
    for entry in entries {
        if let Some((key, value)) = entry.split_once('=') {
            if key == target_key {
                if let Some(value) = resolve_env_value(value) {
                    resolved_value = Some(value);
                }
            }
        } else if entry == target_key {
            match std::env::var(entry) {
                Ok(value) => resolved_value = Some(value),
                Err(_) => {
                    tracing::warn!("host environment variable {} is not set; skipping", entry)
                }
            }
        }
    }
    resolved_value
}

/// Resolve trusted global/profile `environment` entries for a host-side agent process.
pub(crate) fn resolve_host_environment_pairs(entries: &[String]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for entry in entries {
        let (key, value) = match entry.split_once('=') {
            Some((key, value)) => (key.to_string(), resolve_env_value(value)),
            None => {
                // Bare key passthrough: unset values leave the same warning
                // breadcrumb on every launch surface.
                let resolved = std::env::var(entry);
                if resolved.is_err() {
                    tracing::warn!(
                        target: "session.create",
                        "host environment variable {} is not set; skipping",
                        entry
                    );
                }
                (entry.clone(), resolved.ok())
            }
        };
        if !is_valid_env_key(&key) {
            tracing::warn!(
                target: "session.create",
                "invalid host environment key '{}'; skipping",
                key
            );
            continue;
        }
        if let Some(value) = value {
            pairs.retain(|(existing, _)| existing != &key);
            pairs.push((key, value));
        }
    }
    pairs
}

/// Resolve an environment value.
pub(crate) fn resolve_env_value(val: &str) -> Option<String> {
    if let Some(rest) = val.strip_prefix("$$") {
        Some(format!("${}", rest))
    } else if let Some(var_name) = val.strip_prefix('$') {
        match std::env::var(var_name) {
            Ok(v) => Some(v),
            Err(_) => {
                tracing::warn!(target: "session.create",
                    "Environment variable ${} is not set on host, skipping",
                    var_name
                );
                None
            }
        }
    } else {
        Some(val.to_string())
    }
}

/// Validate every entry in a list and return any warnings.
pub fn validate_env_entries<I, S>(entries: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    entries
        .into_iter()
        .filter_map(|e| {
            let s = e.as_ref();
            let key = s.split_once('=').map(|(k, _)| k).unwrap_or(s);
            if DEFAULT_TERMINAL_ENV_VARS.contains(&key) {
                None
            } else {
                validate_env_entry(s)
            }
        })
        .collect()
}

/// Validate an env entry string and return a warning message if it references a host variable that
/// doesn't exist.
pub fn validate_env_entry(entry: &str) -> Option<String> {
    let key = entry.split_once('=').map(|(key, _)| key).unwrap_or(entry);
    if !is_valid_env_key(key) {
        return Some(format!(
            "Warning: invalid environment key '{}'; skipping",
            key
        ));
    }
    let Some((_, value)) = entry.split_once('=') else {
        return std::env::var(entry).is_err().then(|| {
            format!(
                "Warning: {} is not set on the host, so the value will be empty in the container",
                entry
            )
        });
    };
    if value.starts_with("$$") {
        return None;
    }
    match value.strip_prefix('$') {
        Some("") => Some("Warning: bare '$' in value has no variable name".to_string()),
        Some(var_name) if resolve_env_value(value).is_none() => Some(format!(
            "Warning: ${} is not set on the host, so the value will be empty in the container",
            var_name
        )),
        _ => None,
    }
}

/// Collect all environment entries from defaults, global config, and per-session extras.
pub(crate) fn collect_environment(
    sandbox_config: &SandboxConfig,
    sandbox_info: &SandboxInfo,
) -> Vec<EnvEntry> {
    let mut seen_keys = std::collections::HashSet::new();
    let mut result = Vec::new();

    // When per-session extra_env is present, it is the authoritative env list (the TUI seeds it
    // from config.sandbox.environment and the user may have added, edited, or removed entries).
    let entries: &[String] = sandbox_info
        .extra_env
        .as_deref()
        .unwrap_or(&sandbox_config.environment);

    // Terminal defaults, plus Vertex provider vars when Vertex is enabled on the host. A key is
    // claimed even when unset on the host, so later entries cannot supply it.
    let vertex: &[&str] = if host_vertex_enabled() {
        AUTO_FORWARD_VERTEX_ENV_VARS
    } else {
        &[]
    };
    for &key in DEFAULT_TERMINAL_ENV_VARS.iter().chain(vertex) {
        if seen_keys.insert(key.to_string()) {
            if let Ok(value) = std::env::var(key) {
                result.push(EnvEntry::Inherit {
                    key: key.to_string(),
                    value,
                });
            }
        }
    }

    // Host-minted `before_start` values travel via the process environment, never argv.
    for (key, value) in &sandbox_info.before_start_env {
        if !is_valid_env_key(key) {
            tracing::warn!(target: "session.create", "invalid before_start environment key '{}'; skipping", key);
            continue;
        }
        if seen_keys.insert(key.clone()) {
            result.push(EnvEntry::Inherit {
                key: key.clone(),
                value: value.clone(),
            });
        }
    }

    for entry in entries {
        let (key, value) = match entry.split_once('=') {
            Some((key, value)) => (key, Some(value)),
            None => (entry.as_str(), None),
        };
        if !is_valid_env_key(key) {
            tracing::warn!(target: "session.create", "invalid sandbox environment key '{}'; skipping", key);
            continue;
        }
        if !seen_keys.insert(key.to_string()) {
            continue;
        }
        let key = key.to_string();
        let resolved = match value {
            Some(value) => match value.strip_prefix("$$") {
                Some(rest) => Some(EnvEntry::Literal {
                    key,
                    value: format!("${rest}"),
                }),
                None if value.starts_with('$') => {
                    resolve_env_value(value).map(|value| EnvEntry::Inherit { key, value })
                }
                None => Some(EnvEntry::Literal {
                    key,
                    value: value.to_string(),
                }),
            },
            None => match std::env::var(&key) {
                Ok(value) => Some(EnvEntry::Inherit { key, value }),
                Err(_) => {
                    tracing::warn!(target: "session.create",
                        "Environment variable {} is not set on host, skipping",
                        key
                    );
                    None
                }
            },
        };
        result.extend(resolved);
    }

    // Git's safe-directory check fails when the container user does not own the mounted files.
    for (key, value) in [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "safe.directory"),
        ("GIT_CONFIG_VALUE_0", "*"),
    ] {
        if seen_keys.insert(key.to_string()) {
            result.push(EnvEntry::Literal {
                key: key.to_string(),
                value: value.to_string(),
            });
        }
    }

    result
}

/// Resolve the effective sandbox config by merging global + the given profile + repo.
pub(crate) fn resolved_sandbox_config(
    profile: &str,
    project_path: &std::path::Path,
) -> super::config::SandboxConfig {
    let resolved = super::config::effective_profile(profile);
    super::config::repo_config::resolve_config_with_repo_or_warn(&resolved, project_path).sandbox
}

/// Environment transport for a sandboxed `docker exec` pane.
pub(crate) struct DockerExecEnv {
    /// Runtime arguments naming the inherited env-file descriptor.
    pub docker_args: String,
    /// Concrete target-container values for the protected env-file.
    pub env: Vec<(String, String)>,
}

pub(crate) const CONTAINER_EXEC_ENV_FD: u8 = 9;
pub(crate) const CONTAINER_EXEC_ENV_PATH: &str = "/dev/fd/9";

/// Build docker exec environment transport from config and optional
/// per-session extra entries.
#[cfg(test)]
pub(crate) fn build_docker_env_args(
    profile: &str,
    sandbox: &SandboxInfo,
    project_path: &std::path::Path,
) -> DockerExecEnv {
    build_docker_env_args_with_managed_codex_home(profile, sandbox, project_path, None)
}

/// Build docker exec environment flags and add AoE's managed Codex home when
/// the session does not explicitly configure `CODEX_HOME`.
pub(crate) fn build_docker_env_args_with_managed_codex_home(
    profile: &str,
    sandbox: &SandboxInfo,
    project_path: &std::path::Path,
    managed_codex_home: Option<&str>,
) -> DockerExecEnv {
    let sandbox_config = resolved_sandbox_config(profile, project_path);
    docker_exec_environment(sandbox, &sandbox_config, managed_codex_home)
}

pub(crate) fn docker_exec_environment(
    sandbox: &SandboxInfo,
    sandbox_config: &SandboxConfig,
    managed_codex_home: Option<&str>,
) -> DockerExecEnv {
    tracing::debug!(target: "session.create",
        "build_docker_env_args: configured_entries={}, extra_entries={}",
        sandbox_config.environment.len(),
        sandbox.extra_env.as_ref().map_or(0, Vec::len)
    );

    let mut env_entries = collect_environment(sandbox_config, sandbox);
    if let Some(codex_home) = managed_codex_home {
        if !env_entries.iter().any(|entry| entry.key() == "CODEX_HOME") {
            env_entries.push(EnvEntry::Literal {
                key: "CODEX_HOME".to_string(),
                value: codex_home.to_string(),
            });
        }
    }

    tracing::debug!(target: "session.create",
        "build_docker_env_args: resolved {} env entries",
        env_entries.len()
    );
    for entry in &env_entries {
        tracing::debug!(target: "session.create", "  env: {}=<set>", entry.key());
    }

    let env = env_entries
        .iter()
        .map(|entry| (entry.key().to_string(), entry.value().to_string()))
        .collect::<Vec<_>>();
    let docker_args = if env.is_empty() {
        String::new()
    } else {
        format!("--env-file {CONTAINER_EXEC_ENV_PATH}")
    };

    DockerExecEnv { docker_args, env }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::{isolate_app_dir, isolate_home, EnvGuard};
    use serial_test::serial;
    use std::path::Path;

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn strings(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|e| e.to_string()).collect()
    }

    fn sandbox(extra_env: Option<&[&str]>) -> SandboxInfo {
        SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: extra_env.map(strings),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        }
    }

    fn config(environment: &[&str]) -> SandboxConfig {
        SandboxConfig {
            environment: strings(environment),
            ..Default::default()
        }
    }

    /// `(value, inherited)` for every entry with `key`.
    fn lookup(entries: &[EnvEntry], key: &str) -> Vec<(String, bool)> {
        entries
            .iter()
            .filter(|e| e.key() == key)
            .map(|e| (e.value().to_string(), matches!(e, EnvEntry::Inherit { .. })))
            .collect()
    }

    #[test]
    fn inherited_host_env_filtering() {
        let vars = owned(&[
            ("DISPLAY", ":0"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("WAYLAND_DISPLAY", ""),
            ("PATH", "/usr/bin"),
            ("GOPATH", "/home/me/go"),
            ("AOE_TOKEN", "secret"),
            ("AGENT_OF_EMPIRES_DEBUG", "1"),
            ("TERM", "dumb"),
            ("1BAD", "x"),
        ]);
        assert_eq!(
            inherited_host_env_from(vars.clone(), false),
            owned(&[
                ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
                ("DISPLAY", ":0"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ])
        );
        assert_eq!(
            inherited_host_env_from(vars, true),
            owned(&[
                ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
                ("DISPLAY", ":0"),
                ("GOPATH", "/home/me/go"),
                ("PATH", "/usr/bin"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ])
        );
        for key in ["AOE_ACP_SOCKET", "AGENT_OF_EMPIRES_PROFILE", "", "HAS-DASH"] {
            assert!(passthrough_denyreason(key).is_some(), "{key:?}");
        }
    }

    #[test]
    #[serial]
    fn inherited_host_env_reads_the_setting_from_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let _env = EnvGuard::set(&[("DISPLAY", ":7"), ("ENVTEST_CUSTOM_VAR", "custom-value")]);
        let config_path = crate::session::config::config_path().expect("config path");
        std::fs::create_dir_all(config_path.parent().expect("app dir")).expect("app dir");
        let custom = |env: &[(String, String)]| {
            env.iter()
                .find(|(k, _)| k == "ENVTEST_CUSTOM_VAR")
                .map(|(_, v)| v.clone())
        };

        std::fs::write(&config_path, "").expect("write config");
        let default = inherited_host_env("");
        assert!(default.iter().any(|(k, _)| k == "DISPLAY"), "{default:?}");
        assert_eq!(custom(&default), None);

        std::fs::write(&config_path, "[session]\ninherit_host_environment = true\n")
            .expect("write config");
        assert_eq!(
            custom(&inherited_host_env("")).as_deref(),
            Some("custom-value")
        );
    }

    #[test]
    fn login_shell_command_flags_known_shells_only() {
        for (shell, want) in [
            ("/bin/zsh", "'/bin/zsh' -l"),
            ("/opt/homebrew/bin/fish", "'/opt/homebrew/bin/fish' -l"),
            ("/usr/bin/nu", "'/usr/bin/nu'"),
            ("/usr/bin/pwsh", "'/usr/bin/pwsh'"),
        ] {
            assert_eq!(login_shell_command(shell), want);
        }
    }

    #[test]
    #[serial(shell_env)]
    fn user_shell_resolution() {
        for (shell, user, posix) in [
            (Some("/bin/zsh"), "/bin/zsh", "/bin/zsh"),
            (Some("  "), "bash", "bash"),
            (None, "bash", "bash"),
            (Some("/usr/bin/fish"), "/usr/bin/fish", "bash"),
            (Some("/usr/bin/nu"), "/usr/bin/nu", "bash"),
        ] {
            let _shell = match shell {
                Some(shell) => EnvGuard::set(&[("SHELL", shell)]),
                None => EnvGuard::unset(&["SHELL"]),
            };
            assert_eq!(user_shell(), user);
            assert_eq!(user_posix_shell(), posix);
        }
    }

    // Regression: without `extra_env`, docker env comes from the session's profile, not the
    // global default profile.
    #[test]
    #[serial]
    fn docker_env_args_use_passed_profile_not_global_default() {
        let temp_home = tempfile::TempDir::new().unwrap();
        let _home_guard = isolate_home(temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let app_dir = temp_home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let app_dir = temp_home.path().join(crate::session::APP_DIR_NAME_OTHER);
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"default_profile = "default""#,
        )
        .unwrap();
        for (profile, token) in [("default", "read_only_token"), ("personal", "write_token")] {
            let dir = app_dir.join("profiles").join(profile);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("config.toml"),
                format!("[sandbox]\nenvironment = [\"GH_TOKEN={token}\"]\n"),
            )
            .unwrap();
        }

        let sandbox = sandbox(None);
        let project_path = temp_home.path().join("nonexistent_project");
        for (profile, token) in [
            ("personal", "write_token"),
            ("default", "read_only_token"),
            ("", "read_only_token"),
        ] {
            let result = build_docker_env_args(profile, &sandbox, &project_path);
            assert_eq!(result.docker_args, "--env-file /dev/fd/9");
            let expected = ("GH_TOKEN".to_string(), token.to_string());
            assert!(result.env.contains(&expected), "{profile:?}");
        }
    }

    #[test]
    #[serial]
    fn docker_env_args_ignore_repo_host_passthrough() {
        let temp_home = tempfile::TempDir::new().unwrap();
        let _home_guard = isolate_home(temp_home.path());
        let _env = EnvGuard::set(&[
            ("AOE_TEST_REPO_SECRET_3710", "repo-secret"),
            ("AOE_TEST_PROFILE_PT_3710", "profile-value"),
        ]);
        let profile: crate::session::ProfileConfig = serde_json::from_value(serde_json::json!({
            "sandbox": {"environment": ["PROFILE_PT=$AOE_TEST_PROFILE_PT_3710"]}
        }))
        .unwrap();
        crate::session::save_profile_config("default", &profile).unwrap();

        let project = temp_home.path().join("project");
        std::fs::create_dir_all(project.join(".agent-of-empires")).unwrap();
        std::fs::write(
            project.join(".agent-of-empires/config.toml"),
            "[sandbox]\nenvironment = [\"AOE_TEST_REPO_SECRET_3710\", \"LEAK=$AOE_TEST_REPO_SECRET_3710\"]\n",
        )
        .unwrap();

        let env = build_docker_env_args("default", &sandbox(None), &project).env;
        assert!(
            !env.iter().any(|(_, v)| v == "repo-secret"),
            "repo config resolved a host variable"
        );
        assert!(env.contains(&("PROFILE_PT".to_string(), "profile-value".to_string())));
    }

    #[test]
    #[serial]
    fn docker_env_values_never_reach_argv() {
        let _app_guard = isolate_app_dir();
        let _env = EnvGuard::set(&[
            ("AOE_TEST_TOKEN", "secret123"),
            ("AOE_TEST_SOURCE", "secret456"),
            ("AOE_TEST_BARE", "barevalue"),
        ]);
        let sandbox = sandbox(Some(&[
            "AOE_TEST_TOKEN=$AOE_TEST_TOKEN",
            "MY_MAPPED=$AOE_TEST_SOURCE",
            "AOE_TEST_BARE",
            "MY_LITERAL=literal-secret",
        ]));
        let result = build_docker_env_args("", &sandbox, Path::new("/nonexistent"));
        assert_eq!(result.docker_args, "--env-file /dev/fd/9");
        for pair in owned(&[
            ("AOE_TEST_TOKEN", "secret123"),
            ("MY_MAPPED", "secret456"),
            ("AOE_TEST_BARE", "barevalue"),
            ("MY_LITERAL", "literal-secret"),
        ]) {
            assert!(result.env.contains(&pair), "{pair:?}");
        }
    }

    #[test]
    fn managed_codex_home_is_passed_to_exec_unless_overridden() {
        let _app_guard = isolate_app_dir();
        let managed_home = "/root/.codex/codex-upgrade-test";
        let custom: &[&str] = &["CODEX_HOME=/root/custom-codex"];
        for (extra_env, expected_home) in
            [(None, managed_home), (Some(custom), "/root/custom-codex")]
        {
            let result = build_docker_env_args_with_managed_codex_home(
                "",
                &sandbox(extra_env),
                Path::new("/nonexistent"),
                Some(managed_home),
            );
            let homes: Vec<&str> = result
                .env
                .iter()
                .filter(|(k, _)| k == "CODEX_HOME")
                .map(|(_, v)| v.as_str())
                .collect();
            assert_eq!(homes, [expected_home]);
        }
    }

    #[test]
    fn shell_escape_quotes_and_metacharacters() {
        let cases = [
            ("hello", "'hello'"),
            ("Don't do that", "'Don'\\''t do that'"),
            ("say \"hello\"", "'say \"hello\"'"),
            ("path\\to\\file", "'path\\to\\file'"),
            ("$HOME/path", "'$HOME/path'"),
            ("run `cmd`", "'run `cmd`'"),
            ("hello!", "'hello!'"),
            ("line1\nline2", "'line1\\nline2'"),
            ("line1\r\nline2", "'line1\\r\\nline2'"),
            ("He said \"don't\"", "'He said \"don'\\''t\"'"),
        ];
        for (input, expected) in cases {
            assert_eq!(shell_escape(input), expected, "shell_escape({input:?})");
        }
    }

    #[test]
    #[serial]
    fn host_environment_grammar() {
        let _env = EnvGuard::set(&[
            ("AOE_TEST_HOST_REF", "from-host"),
            ("AOE_TEST_HOST_BARE", "bare"),
        ]);
        let _unset = EnvGuard::unset(&["AOE_TEST_HOST_MISSING"]);
        let entries = strings(&[
            "CODEX_HOME=/first",
            "FROM_HOST=$AOE_TEST_HOST_REF",
            "ESCAPED=$$LIT",
            "AOE_TEST_HOST_BARE",
            "MISSING=$AOE_TEST_HOST_MISSING",
            "1BAD=x",
            "CODEX_HOME=/second",
            "CODEX_HOME=$AOE_TEST_HOST_MISSING",
            "FROM_HOST=second",
        ]);

        // Host agent env: the last resolvable entry for a key wins and moves to the end.
        assert_eq!(
            resolve_host_environment_pairs(&entries),
            owned(&[
                ("ESCAPED", "$LIT"),
                ("AOE_TEST_HOST_BARE", "bare"),
                ("CODEX_HOME", "/second"),
                ("FROM_HOST", "second"),
            ])
        );
        assert_eq!(
            resolve_host_environment_value(&entries, "CODEX_HOME").as_deref(),
            Some("/second")
        );
        assert_eq!(
            resolve_host_environment_value(&entries[..2], "FROM_HOST").as_deref(),
            Some("from-host")
        );

        // Host hooks: the first entry for a key wins; invalid keys are skipped.
        let mut hook_entries = entries.clone();
        hook_entries.extend(strings(&["HAS SPACE=y", "=novalue", "_OK=2"]));
        assert_eq!(
            resolve_hook_env_pairs(&hook_entries),
            owned(&[
                ("CODEX_HOME", "/first"),
                ("FROM_HOST", "from-host"),
                ("ESCAPED", "$LIT"),
                ("AOE_TEST_HOST_BARE", "bare"),
                ("_OK", "2"),
            ])
        );
    }

    #[test]
    fn drop_shadowed_host_entries_matches_whole_keys() {
        let entries = strings(&[
            "CLAUDE_CONFIG_DIR=/stale",
            "KEEP_LITERAL=keep",
            "ANTHROPIC_BASE_URL=$SOME_REF",
            "TERM",
            "FOOBAR=2",
        ]);
        assert_eq!(drop_shadowed_host_entries(entries.clone(), &[]), entries);
        let minted = owned(&[
            ("CLAUDE_CONFIG_DIR", "/fresh"),
            ("ANTHROPIC_BASE_URL", "http://x"),
            ("TERM", "xterm"),
            ("FOO", "minted"),
        ]);
        assert_eq!(
            drop_shadowed_host_entries(entries, &minted),
            strings(&["KEEP_LITERAL=keep", "FOOBAR=2"])
        );
    }

    #[test]
    fn host_hook_env_excludes_repo_contributed_entries() {
        let extra = strings(&["TEST_VAR=foo", "NODE_ENV=test", "SHARED=keep"]);
        let trusted = strings(&["SHARED=keep"]);
        let repo_aware = strings(&["NODE_ENV=test", "SHARED=keep"]);
        assert_eq!(
            host_hook_entries(&extra, &trusted, &repo_aware),
            strings(&["TEST_VAR=foo", "SHARED=keep"])
        );

        let _app_guard = isolate_app_dir();
        let tmp = tempfile::tempdir().unwrap();
        let info = sandbox(Some(&["TEST_VAR=foo", "OTHER=bar"]));
        assert_eq!(
            session_host_env_pairs("any-profile", tmp.path(), &info),
            owned(&[("TEST_VAR", "foo"), ("OTHER", "bar")])
        );
    }

    #[test]
    #[serial]
    fn collect_environment_entry_grammar() {
        let _env = EnvGuard::set(&[
            ("AOE_TEST_ENV_PT", "test_value"),
            ("AOE_TEST_HOST_REF", "host_val"),
        ]);
        let config = config(&[
            "AOE_TEST_ENV_PT",
            "MY_KEY=my_value",
            "INJECTED=$AOE_TEST_HOST_REF",
            "ESCAPED=$$LITERAL",
            "GH_TOKEN=stale_literal",
            "CFG; touch /tmp/cfg_injected; #=secret",
        ]);
        let mut info = sandbox(None);
        info.before_start_env = owned(&[
            ("GH_TOKEN", "ghs_fresh"),
            ("HOOK$(touch /tmp/hook_injected)", "secret"),
        ]);

        let result = collect_environment(&config, &info);
        for (key, value, inherited) in [
            ("AOE_TEST_ENV_PT", "test_value", true),
            ("MY_KEY", "my_value", false),
            ("INJECTED", "host_val", true),
            ("ESCAPED", "$LITERAL", false),
            ("GH_TOKEN", "ghs_fresh", true),
            ("GIT_CONFIG_COUNT", "1", false),
            ("GIT_CONFIG_KEY_0", "safe.directory", false),
            ("GIT_CONFIG_VALUE_0", "*", false),
        ] {
            assert_eq!(
                lookup(&result, key),
                [(value.to_string(), inherited)],
                "{key}"
            );
        }
        assert!(!result.iter().any(|e| e.key().contains("touch")));

        // Per-session `extra_env` replaces the configured list and can override git defaults.
        info.extra_env = Some(strings(&[
            "MY_KEY=from_session",
            "GIT_CONFIG_COUNT=2",
            "GIT_CONFIG_VALUE_0=/workspace/custom",
            "EXTRA`touch /tmp/extra_injected`=secret",
        ]));
        let result = collect_environment(&config, &info);
        for (key, value) in [
            ("MY_KEY", "from_session"),
            ("GIT_CONFIG_COUNT", "2"),
            ("GIT_CONFIG_VALUE_0", "/workspace/custom"),
        ] {
            assert_eq!(lookup(&result, key), [(value.to_string(), false)], "{key}");
        }
        assert!(lookup(&result, "INJECTED").is_empty());
        assert!(!result.iter().any(|e| e.key().contains("touch")));
    }

    #[test]
    #[serial]
    fn collect_environment_auto_forwards_vertex_vars_only_when_enabled() {
        for (flag, forwarded) in [(Some("1"), true), (Some(""), false), (None, false)] {
            let _env = EnvGuard::set(&[
                ("ANTHROPIC_VERTEX_PROJECT_ID", "my-proj"),
                ("CLOUD_ML_REGION", "us-east5"),
                ("ANTHROPIC_API_KEY", "sk-host-key"),
            ]);
            let _flag = match flag {
                Some(flag) => EnvGuard::set(&[("CLAUDE_CODE_USE_VERTEX", flag)]),
                None => EnvGuard::unset(&["CLAUDE_CODE_USE_VERTEX"]),
            };
            let result =
                collect_environment(&config(&["ANTHROPIC_VERTEX_PROJECT_ID"]), &sandbox(None));
            assert_eq!(
                lookup(&result, "ANTHROPIC_VERTEX_PROJECT_ID"),
                [("my-proj".to_string(), true)],
                "never duplicated"
            );
            assert_eq!(
                lookup(&result, "CLOUD_ML_REGION").len(),
                usize::from(forwarded),
                "{flag:?}"
            );
            assert!(
                lookup(&result, "ANTHROPIC_API_KEY").is_empty(),
                "never auto-forwarded"
            );
        }
    }

    #[test]
    #[serial]
    fn validate_env_entries_warns_only_for_unresolvable_entries() {
        let _env = EnvGuard::set(&[("AOE_TEST_VALIDATE_PRESENT", "exists")]);
        let _unset = EnvGuard::unset(&["AOE_TEST_VALIDATE_MISSING"]);
        for (entry, warns) in [
            ("AOE_TEST_VALIDATE_PRESENT", false),
            ("MY_KEY=$AOE_TEST_VALIDATE_PRESENT", false),
            ("MY_KEY=some_literal", false),
            ("MY_KEY=$$ESCAPED", false),
            ("AOE_TEST_VALIDATE_MISSING", true),
            ("MY_KEY=$AOE_TEST_VALIDATE_MISSING", true),
        ] {
            let warning = validate_env_entry(entry);
            assert_eq!(warning.is_some(), warns, "{entry}");
            if let Some(warning) = warning {
                assert!(warning.contains("AOE_TEST_VALIDATE_MISSING"), "{warning}");
            }
        }
        let warnings = validate_env_entries([
            "A=$AOE_TEST_VALIDATE_MISSING",
            "OK=fine",
            "B=$AOE_TEST_VALIDATE_MISSING",
        ]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(validate_env_entries(Vec::<String>::new()).is_empty());
    }

    #[test]
    #[serial(shell_env)]
    fn validate_env_entries_skips_default_terminal_vars_when_unset() {
        let _env = EnvGuard::unset(DEFAULT_TERMINAL_ENV_VARS);
        assert!(validate_env_entries(DEFAULT_TERMINAL_ENV_VARS).is_empty());
    }
}
