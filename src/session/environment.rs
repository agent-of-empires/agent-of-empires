//! Environment variable helpers for session instances.
//!
//! Pure functions for building environment variable arguments used when
//! launching tools inside Docker containers.

use super::config::SandboxConfig;
use super::instance::SandboxInfo;
use crate::containers::container_interface::EnvEntry;

/// Keys whose values are safe to show in logs (not secrets).
const SAFE_ENV_KEYS: &[&str] = &[
    "TERM",
    "COLORTERM",
    "FORCE_COLOR",
    "NO_COLOR",
    "GIT_CONFIG_GLOBAL",
    "CLAUDE_CONFIG_DIR",
    "AOE_INSTANCE_ID",
];

/// Redact secret values from a command string for safe logging.
/// Replaces `-e KEY='value'` and `-e KEY=value` patterns with `-e KEY=<redacted>`,
/// and `export KEY='value'` patterns with `export KEY=<redacted>`,
/// except for known-safe keys (TERM, COLORTERM, GIT_CONFIG_GLOBAL, etc.).
pub(crate) fn redact_env_values(cmd: &str) -> String {
    let result = redact_docker_env_flags(cmd);
    redact_export_statements(&result)
}

/// Redact `-e KEY=VALUE` patterns in a command string.
fn redact_docker_env_flags(cmd: &str) -> String {
    let mut result = String::with_capacity(cmd.len());
    let mut remaining = cmd;

    while let Some(pos) = remaining.find("-e ") {
        result.push_str(&remaining[..pos]);
        remaining = &remaining[pos + 3..]; // skip past "-e "

        // Find the KEY before '='
        let eq_pos = remaining.find('=');
        // Find the boundary of this env arg: next " -e " or end of string
        let next_env = remaining.find(" -e ").unwrap_or(remaining.len());

        if let Some(eq_pos) = eq_pos {
            // Only treat as KEY=VALUE if '=' comes before the next '-e' boundary
            if eq_pos < next_env {
                let key = &remaining[..eq_pos];
                if key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    if SAFE_ENV_KEYS.contains(&key) {
                        result.push_str("-e ");
                        result.push_str(&remaining[..next_env]);
                    } else {
                        result.push_str("-e ");
                        result.push_str(key);
                        result.push_str("=<redacted>");
                    }
                    remaining = &remaining[next_env..];
                    continue;
                }
            }
        }

        // No '=' found or not a valid key; pass through as-is (e.g., `-e KEY` inherit form)
        result.push_str("-e ");
        result.push_str(&remaining[..next_env]);
        remaining = &remaining[next_env..];
    }
    result.push_str(remaining);
    result
}

/// Redact `export KEY='value'` and `export KEY=value` patterns in a command string.
fn redact_export_statements(cmd: &str) -> String {
    let mut result = String::with_capacity(cmd.len());
    let mut remaining = cmd;

    while let Some(pos) = remaining.find("export ") {
        result.push_str(&remaining[..pos]);
        remaining = &remaining[pos + 7..]; // skip past "export "

        // Find the boundary: next "; " or end of string
        let boundary = remaining.find("; ").unwrap_or(remaining.len());

        let eq_pos = remaining.find('=');
        if let Some(eq_pos) = eq_pos {
            if eq_pos < boundary {
                let key = &remaining[..eq_pos];
                if key.chars().all(|c| c.is_alphanumeric() || c == '_') {
                    if SAFE_ENV_KEYS.contains(&key) {
                        result.push_str("export ");
                        result.push_str(&remaining[..boundary]);
                    } else {
                        result.push_str("export ");
                        result.push_str(key);
                        result.push_str("=<redacted>");
                    }
                    remaining = &remaining[boundary..];
                    continue;
                }
            }
        }

        // No '=' or not a valid key; pass through
        result.push_str("export ");
        result.push_str(&remaining[..boundary]);
        remaining = &remaining[boundary..];
    }
    result.push_str(remaining);
    result
}

/// Terminal environment variables that are always passed through for proper UI/theming
pub(crate) const DEFAULT_TERMINAL_ENV_VARS: &[&str] =
    &["TERM", "COLORTERM", "FORCE_COLOR", "NO_COLOR"];

/// Vertex provider env vars auto-forwarded into sandbox containers when
/// `CLAUDE_CODE_USE_VERTEX` is set on the host. The flag itself is included
/// so the container sees a consistent state.
///
/// `ANTHROPIC_API_KEY` is intentionally not in this list: Vertex auth uses
/// GCP credentials, and force-forwarding the Anthropic API key would change
/// behavior for users who happen to have it on their shell for unrelated
/// reasons. Users who want it forwarded can add it to `sandbox.environment`
/// explicitly.
pub(crate) const AUTO_FORWARD_VERTEX_ENV_VARS: &[&str] = &[
    "ANTHROPIC_VERTEX_PROJECT_ID",
    "ANTHROPIC_VERTEX_REGION",
    "CLAUDE_CODE_USE_VERTEX",
    "CLOUD_ML_REGION",
];

/// Returns true when `CLAUDE_CODE_USE_VERTEX` is set on the host to a
/// non-empty value. An empty string is treated as unset to match how the
/// flag is conventionally interpreted.
pub(crate) fn host_vertex_enabled() -> bool {
    std::env::var("CLAUDE_CODE_USE_VERTEX")
        .ok()
        .is_some_and(|v| !v.is_empty())
}

/// Returns the user's preferred shell from `$SHELL`, falling back to `bash`.
///
/// Used for host-side command wrappers (agent launch, local hook execution)
/// so that the user's PATH and rc-file sourcing work correctly. Container
/// contexts should keep using a fixed shell since the user shell may not be
/// installed inside the image.
pub(crate) fn user_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "bash".to_string())
}

/// Desktop and session environment variables a user's graphical login sets but
/// that tmux does not reliably carry into a `new-session`. tmux's
/// `update-environment` only refreshes DISPLAY/SSH_*/XAUTHORITY/WINDOWID/
/// KRB5CCNAME (and removes any not present in the creating process); everything
/// else survives only if it was in the tmux server's frozen global environment.
/// In structured view the sessions are created by the `aoe serve` daemon, so
/// without explicit forwarding a browser launched from an agent (e.g. an OIDC
/// login) has no DISPLAY/XDG_RUNTIME_DIR/DBUS to reach the user's desktop
/// (#3075). Any `XDG_*` var is forwarded on top of this explicit list.
const FORWARDED_DESKTOP_VARS: &[&str] = &[
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
];

/// Why the wholesale passthrough ([`inherited_host_env`] with
/// `session.inherit_host_environment` on) refuses a key, or `None` when it may
/// be forwarded.
///
/// `AOE_`-prefixed keys are aoe's own per-process wiring and credentials
/// (`AOE_TOKEN`, `AOE_DAEMON_TOKEN`, `AOE_ACP_SOCKET`, the runner env carrier),
/// so the whole prefix is refused: passing them to an agent would either leak
/// aoe's own auth or point the agent at a socket that is not its own.
/// `AGENT_OF_EMPIRES_` is the same story under aoe's older prefix, and it is not
/// vestigial: `AGENT_OF_EMPIRES_DEBUG` still switches on debug logging, and the
/// detached ACP runner is itself an `aoe` process, so forwarding it would have
/// the runner start writing `debug.log` because of a var the operator exported
/// for their own shell. `TERM` is refused because tmux owns the pane's terminal
/// type (`default-terminal`) and a daemon's `TERM` is routinely absent or
/// `dumb`; forwarding that would degrade a pane the operator never asked to
/// degrade. The ACP paths forward `TERM` through their own allowlist, so nothing
/// loses it.
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
///
/// This is the single source for every host spawn path: tmux agent sessions and
/// host terminals set it via `new-session -e`, and the structured view applies
/// it to the agent process after its `env_clear()`. Keeping one resolver is what
/// stops the terminal and structured views drifting apart, which is the bug
/// #3079 shipped: it fixed the tmux paths and left the structured view forwarding
/// nothing, so a browser-view agent still had no `DISPLAY` (#3262).
///
/// Sourced from aoe's own process environment, so a session inherits whatever
/// aoe itself holds and nothing more. A daemon launched without the operator's
/// environment (a systemd unit with no `PassEnvironment` / `EnvironmentFile`,
/// say) has nothing to forward, and fixing that belongs to whatever starts the
/// daemon rather than to aoe: forwarding is a passthrough, not a store.
///
/// Which vars qualify depends on `session.inherit_host_environment`: off (the
/// default) forwards only the desktop/session vars a graphical login sets, on
/// forwards everything [`passthrough_denyreason`] permits.
///
/// `profile` selects the config layer, so a profile override wins over global.
pub(crate) fn inherited_host_env(profile: &str) -> Vec<(String, String)> {
    let passthrough =
        super::profile_config::resolve_config_or_warn(&super::config::effective_profile(profile))
            .session
            .inherit_host_environment;
    let vars = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)));
    inherited_host_env_from(vars, passthrough)
}

/// Pure core of [`inherited_host_env`], split out so the filtering is
/// unit-tested without mutating the process environment.
///
/// Empty values are dropped on purpose. `new-session -e KEY=` overrides the
/// tmux server's frozen base environment with an empty string, and that base
/// env is frequently the *good* one (the server was first started from the
/// user's graphical login while the current daemon is the impoverished side).
/// Forwarding `DISPLAY=` there would blank out a working display, so we only
/// add values aoe positively has and never clobber an inherited one with empty
/// (an empty desktop var is useless to a browser anyway).
///
/// Sorted so the emitted `-e` args and the applied process env are
/// deterministic.
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

/// Shells we can safely launch with a `-l` login flag. Others (nushell,
/// PowerShell) are launched plain; they still source their own interactive
/// config, and `-l` would either error or mean something different there.
const LOGIN_FLAG_SHELLS: &[&str] = &["bash", "zsh", "sh", "ksh", "dash", "fish", "csh", "tcsh"];

/// Build the tmux pane command that launches `shell` as a login+interactive
/// shell, so it sources the user's profile and rc files (`~/.zprofile`,
/// `~/.zshrc`, oh-my-zsh, Homebrew/nvm PATH setup) exactly as a native
/// terminal would. Login-capable shells get `-l`; others launch plain. The
/// path is shell-escaped for the tmux command parser.
pub(crate) fn login_shell_command(shell: &str) -> String {
    let basename = std::path::Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(shell);
    let escaped = shell_escape(shell);
    if LOGIN_FLAG_SHELLS.contains(&basename) {
        format!("{escaped} -l")
    } else {
        escaped
    }
}

/// Like [`user_shell`], but falls back to `bash` when the user's shell is
/// non-POSIX (e.g. fish, nushell, pwsh). Use this for command wrappers that
/// rely on POSIX single-quote escaping (`'\''`).
pub(crate) fn user_posix_shell() -> String {
    let shell = user_shell();
    let basename = std::path::Path::new(&shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&shell);
    if NON_POSIX_SHELLS.contains(&basename) {
        "bash".to_string()
    } else {
        shell
    }
}

/// Shell-escape a value for safe interpolation into a shell command string.
///
/// Uses single-quote escaping: inside single quotes ALL characters are literal
/// except `'` itself, which is escaped via the POSIX `'\''` technique. This is
/// the most robust approach -- it prevents expansion of `$`, `` ` ``, `\`, `!`,
/// and every other shell metacharacter in one shot.
///
/// Newlines and carriage returns are replaced with literal `\n` / `\r` text to
/// keep the command on a single line (required for tmux session commands).
pub(crate) fn shell_escape(val: &str) -> String {
    let val = val.replace('\n', "\\n").replace('\r', "\\r");
    let escaped = val.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

/// Build a shell-ready `KEY='value' KEY2='value2' ` prefix from a list of
/// environment entries, suitable for prepending to a host command line.
///
/// Entry grammar (identical to `sandbox.environment`):
/// - `KEY=value`: literal value, passed through verbatim.
/// - `KEY=$VAR`: read VAR from the host env at spawn time (skipped with a
///   warning if VAR is not set).
/// - `KEY=$$literal`: escape; emits `KEY='$literal'`.
/// - bare `KEY`: passthrough from the host env (skipped with a warning if
///   the var is not set).
///
/// Values are passed through `shell_escape` so spaces, quotes, and shell
/// metacharacters are preserved literally. Returns an empty string when
/// the entry list is empty so callers can format unconditionally.
///
/// Keys are not escapable here (a shell assignment needs a bare name on the
/// left), so an entry whose key is not a valid environment variable name is
/// skipped with a warning rather than concatenated into the command line. The
/// structured-view sibling [`resolve_host_environment_pairs`] applies the same
/// rule, so both views drop the same entries.
pub(crate) fn host_environment_prefix(entries: &[String]) -> String {
    let mut out = String::new();
    for entry in entries {
        let key = entry.split_once('=').map(|(key, _)| key).unwrap_or(entry);
        if !is_valid_env_key(key) {
            tracing::warn!(target: "session.create", "invalid host environment key '{}'; skipping", key);
            continue;
        }
        if let Some((key, value)) = entry.split_once('=') {
            let resolved = if let Some(rest) = value.strip_prefix("$$") {
                Some(format!("${}", rest))
            } else if value.starts_with('$') {
                match resolve_env_value(value) {
                    Some(v) => Some(v),
                    None => continue,
                }
            } else {
                Some(value.to_string())
            };
            if let Some(v) = resolved {
                out.push_str(&format!("{}={} ", key, shell_escape(&v)));
            }
        } else {
            // Bare key: passthrough from host env.
            match std::env::var(entry) {
                Ok(v) => out.push_str(&format!("{}={} ", entry, shell_escape(&v))),
                Err(_) => {
                    tracing::warn!(target: "session.create", "host environment variable {} is not set; skipping", entry)
                }
            }
        }
    }
    out
}

/// Resolve a session's sandbox environment entries to concrete `(KEY, VALUE)`
/// pairs on the host, for feeding into a host-side hook's process environment
/// (so a `before_start` hook can read a per-session `$TEST_VAR`).
///
/// Trust boundary: `before_start` hooks are profile/global only, so a repo's
/// `.agent-of-empires/config.toml` `sandbox.environment` must never reach host
/// execution (e.g. a repo setting `PATH`). Sources:
/// - With a per-session `extra_env`: use it, but drop any entry the repo
///   contributed. `extra_env` is seeded verbatim from the repo-aware config in
///   the new-session dialog, so a submitted override can still carry repo
///   entries; [`host_hook_entries`] filters those out. This is subtractive
///   only and does not affect the container's env (which keeps `extra_env`
///   verbatim via [`collect_environment`]).
/// - Without one: the profile/global `sandbox.environment` baseline.
///
/// Each entry is resolved to a plain host value via the shared grammar:
/// `KEY=value` is literal, `KEY=$VAR` reads the host env, `KEY=$$literal`
/// escapes a `$`, and a bare `KEY` passes through from the host env. Unset host
/// references and bare keys are skipped. Deduplicates by key (first wins).
pub(crate) fn session_host_env_pairs(
    profile: &str,
    project_path: &std::path::Path,
    sandbox_info: &SandboxInfo,
) -> Vec<(String, String)> {
    let resolved_profile = super::config::effective_profile(profile);
    let trusted = super::profile_config::resolve_config_or_warn(&resolved_profile)
        .sandbox
        .environment;
    let entries = match sandbox_info.extra_env.as_deref() {
        None => trusted,
        Some(extra) => {
            let repo_aware = super::repo_config::resolve_config_with_repo_or_warn(
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

/// Filter a session's `extra_env` down to the entries safe to expose to a host
/// hook: everything except entries the repo contributed (present in the
/// repo-aware config but not in the profile/global `trusted` baseline). Repo
/// entries are dropped, never added, so an untrusted repo cannot reach host
/// execution even when the user submits a per-session override seeded from the
/// repo-aware dialog. Pure, so it is unit-tested without touching disk.
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

/// Resolve `sandbox.environment` entries to concrete host `(KEY, VALUE)` pairs
/// for a `before_start` host hook (the pure core of [`session_host_env_pairs`],
/// split out so it can be tested without touching config on disk).
///
/// Duplicate keys resolve FIRST-wins here. The agent-side sibling,
/// `resolve_host_environment_pairs`, is deliberately LAST-wins to match the
/// terminal-view shell-assignment prefix; keep the two distinct so a future
/// edit does not copy one precedence rule onto the other.
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

/// Drop every `environment` entry whose key was minted by
/// `host_hooks.before_session`, so the minted value is the only source for that
/// key on a host launch.
///
/// Exists because the two channels have different binding strength in the
/// terminal view: the static list becomes a `KEY='v' ` shell-assignment prefix
/// on the pane command, which overrides whatever the pane inherited, while a
/// minted value arrives via `tmux new-session -e` (deliberately, so a secret
/// stays out of argv). Without this filter the stale config entry would win and
/// `before_session` would look like it silently did nothing.
///
/// Entry keys are read with the same grammar the resolvers use: the part before
/// the first `=`, or the whole entry for a bare passthrough key.
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

/// True when `key` is a valid environment variable name: an ASCII letter or `_`
/// first, then ASCII alphanumerics or `_`. Shared by the host-env resolver and
/// the `before_start` stdout parser so both reject the same malformed keys
/// before they reach `Command::envs`.
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

/// Resolve trusted global/profile `environment` entries for a host-side agent
/// process. Uses the same grammar as [`host_environment_prefix`], but returns
/// concrete pairs for `Command::env`. Later entries replace earlier entries,
/// matching the shell assignment behavior used by terminal sessions.
///
/// Repo configuration cannot contribute to `Config.environment`
/// (`REPO_OVERRIDABLE_SECTIONS` in `repo_config` excludes it); callers must
/// still keep these pairs out of sandboxed agents, whose environment is
/// controlled by `sandbox.environment` instead.
///
/// Serve-gated to match its only consumer, the structured-view supervisor.
#[cfg(feature = "serve")]
pub(crate) fn resolve_host_environment_pairs(entries: &[String]) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();
    for entry in entries {
        let (key, value) = match entry.split_once('=') {
            Some((key, value)) => (key.to_string(), resolve_env_value(value)),
            None => {
                // Bare key: passthrough from host env. Warn when it is unset, so a bare key that
                // silently does not forward leaves the same breadcrumb here as it does on the
                // terminal path in `host_environment_prefix`.
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

/// Resolve an environment value. If the value starts with `$`, read the
/// named variable from the host environment (use `$$` to escape a literal `$`).
/// Otherwise return the literal value.
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
///
/// Mirrors what `collect_environment` will silently drop at container
/// create or docker exec time, so callers can surface the same warnings
/// to the user via toast or stderr before the failure becomes invisible.
///
/// `DEFAULT_TERMINAL_ENV_VARS` are pass-through-if-set toggles (FORCE_COLOR
/// and NO_COLOR in particular are mutually exclusive and intentionally
/// unset on most hosts), so we skip them. Without this skip, every new
/// sandboxed session pops a warning dialog for env vars the user never
/// set on purpose.
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

/// Validate an env entry string and return a warning message when its key is
/// not a valid environment variable name (the entry is dropped at collection
/// time) or when it references a host variable that doesn't exist.
///
/// Entry formats:
/// - `KEY` (bare): pass through from host
/// - `KEY=$VAR`: resolve `$VAR` from host
/// - `KEY=literal` (no `$`): always valid
/// - `KEY=$$...`: escaped literal `$`, always valid
pub fn validate_env_entry(entry: &str) -> Option<String> {
    let key = entry.split_once('=').map(|(key, _)| key).unwrap_or(entry);
    if !is_valid_env_key(key) {
        return Some(format!(
            "Warning: invalid environment key '{}'; skipping",
            key
        ));
    }
    if let Some((_, value)) = entry.split_once('=') {
        if value.starts_with("$$") {
            // Escaped literal $, always valid
            None
        } else if let Some(var_name) = value.strip_prefix('$') {
            if var_name.is_empty() {
                Some("Warning: bare '$' in value has no variable name".to_string())
            } else if resolve_env_value(value).is_none() {
                Some(format!(
                    "Warning: ${} is not set on the host, so the value will be empty in the container",
                    var_name
                ))
            } else {
                None
            }
        } else {
            // Literal value, always valid
            None
        }
    } else {
        // Bare key -- pass through from host
        if std::env::var(entry).is_err() {
            Some(format!(
                "Warning: {} is not set on the host, so the value will be empty in the container",
                entry
            ))
        } else {
            None
        }
    }
}

/// Collect all environment entries from defaults, global config, and per-session extras.
///
/// Each entry is either:
/// - `KEY` (no `=`) -- pass through from host (inherited, not in argv)
/// - `KEY=$VAR` -- read from host env (inherited, not in argv)
/// - `KEY=literal` -- literal value (appears in argv, safe for non-secrets)
///
/// Returns `EnvEntry` values that distinguish inherited-from-host entries
/// (which use Docker `-e KEY` to avoid leaking secrets in argv/ps) from
/// literal entries (which use `-e KEY=VALUE`).
///
/// Deduplicates by key (first wins).
pub(crate) fn collect_environment(
    sandbox_config: &SandboxConfig,
    sandbox_info: &SandboxInfo,
) -> Vec<EnvEntry> {
    let mut seen_keys = std::collections::HashSet::new();
    let mut result = Vec::new();

    // When per-session extra_env is present, it is the authoritative env list
    // (the TUI seeds it from config.sandbox.environment and the user may have
    // added, edited, or removed entries). Fall back to config only when no
    // per-session overrides exist.
    let entries: &[String] = sandbox_info
        .extra_env
        .as_deref()
        .unwrap_or(&sandbox_config.environment);

    // Always ensure the terminal defaults are present (pass-through from host)
    for &key in DEFAULT_TERMINAL_ENV_VARS {
        if seen_keys.insert(key.to_string()) {
            if let Ok(val) = std::env::var(key) {
                result.push(EnvEntry::Inherit {
                    key: key.to_string(),
                    value: val,
                });
            }
        }
    }

    // Auto-forward Vertex provider env vars when Vertex is enabled on the host.
    // Gating on the host flag keeps non-Vertex users' sandboxes unchanged.
    if host_vertex_enabled() {
        for &key in AUTO_FORWARD_VERTEX_ENV_VARS {
            if seen_keys.insert(key.to_string()) {
                if let Ok(val) = std::env::var(key) {
                    result.push(EnvEntry::Inherit {
                        key: key.to_string(),
                        value: val,
                    });
                }
            }
        }
    }

    // Host-minted `before_start` values are injected as inherited entries so the
    // value is passed to docker via the process environment, never in argv.
    // Placed before the configured entries so a freshly-minted secret wins over
    // any same-keyed `sandbox.environment` / `extra_env` entry (first-wins).
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
        let key = entry.split_once('=').map(|(key, _)| key).unwrap_or(entry);
        if !is_valid_env_key(key) {
            tracing::warn!(target: "session.create", "invalid sandbox environment key '{}'; skipping", key);
            continue;
        }
        if let Some((key, value)) = entry.split_once('=') {
            if seen_keys.insert(key.to_string()) {
                if let Some(rest) = value.strip_prefix("$$") {
                    // Escaped literal $, e.g. KEY=$$FOO -> KEY=$FOO
                    let literal = format!("${}", rest);
                    result.push(EnvEntry::Literal {
                        key: key.to_string(),
                        value: literal,
                    });
                } else if value.starts_with('$') {
                    // Host env reference, e.g. GH_TOKEN=$GH_TOKEN
                    if let Some(resolved) = resolve_env_value(value) {
                        result.push(EnvEntry::Inherit {
                            key: key.to_string(),
                            value: resolved,
                        });
                    }
                } else {
                    // Literal value, e.g. TERM=xterm-256color
                    result.push(EnvEntry::Literal {
                        key: key.to_string(),
                        value: value.to_string(),
                    });
                }
            }
        } else {
            // Bare key -- pass through from host
            if seen_keys.insert(entry.clone()) {
                match std::env::var(entry) {
                    Ok(val) => {
                        result.push(EnvEntry::Inherit {
                            key: entry.clone(),
                            value: val,
                        });
                    }
                    Err(_) => {
                        tracing::warn!(target: "session.create",
                            "Environment variable {} is not set on host, skipping",
                            entry
                        );
                    }
                }
            }
        }
    }

    // Git's safe-directory check fails when the container user (root) does not
    // match the file owner (host UID 1000, shown as "ubuntu" inside the
    // aoe-dev-sandbox image). Bind-mounted repos trigger:
    //   fatal: detected dubious ownership in repository at '...'
    // We inject safe.directory=* via Git's env-var config API (Git 2.31+),
    // which overrides the check without modifying any files.
    // Placed after the user entries loop so caller-provided GIT_CONFIG_*
    // values take precedence (first-wins deduplication via seen_keys).
    if seen_keys.insert("GIT_CONFIG_COUNT".to_string()) {
        result.push(EnvEntry::Literal {
            key: "GIT_CONFIG_COUNT".to_string(),
            value: "1".to_string(),
        });
    }
    if seen_keys.insert("GIT_CONFIG_KEY_0".to_string()) {
        result.push(EnvEntry::Literal {
            key: "GIT_CONFIG_KEY_0".to_string(),
            value: "safe.directory".to_string(),
        });
    }
    if seen_keys.insert("GIT_CONFIG_VALUE_0".to_string()) {
        result.push(EnvEntry::Literal {
            key: "GIT_CONFIG_VALUE_0".to_string(),
            value: "*".to_string(),
        });
    }

    result
}

/// Resolve the effective sandbox config by merging global + the given profile + repo.
/// An empty `profile` falls back to the user's globally configured default profile
/// via [`super::config::effective_profile`].
pub(crate) fn resolved_sandbox_config(
    profile: &str,
    project_path: &std::path::Path,
) -> super::config::SandboxConfig {
    let resolved = super::config::effective_profile(profile);
    super::repo_config::resolve_config_with_repo_or_warn(&resolved, project_path).sandbox
}

/// Result of building docker exec environment arguments.
///
/// Separates secret (inherited from host) env vars from literal (non-secret) ones.
/// Secret values are prepended to the tmux session command as `export` shell
/// builtins, followed by `exec` to replace the outer shell process. This keeps
/// secret values out of every long-lived process's argv/ps output. The docker
/// exec command then uses `-e KEY` (key only, no value) to inherit the exported
/// variable from the shell environment.
pub(crate) struct DockerExecEnv {
    /// Docker `-e` flags for the exec command line.
    /// Inherit entries use `-e KEY` (key only); Literal entries use `-e KEY=VALUE`.
    pub docker_args: String,
    /// Shell export statements for Inherit (secret) entries.
    /// Each entry is a complete `export KEY='escaped_value'` command ready
    /// to be prepended to the tmux session command.
    pub exports: Vec<String>,
}

/// Build docker exec environment flags from config and optional per-session extra entries.
/// Used for `docker exec` commands run inside tmux sessions.
///
/// Returns a [`DockerExecEnv`] that separates secret values (prepended as
/// `export` statements to the tmux session command) from literal values
/// (which are safe to include in the command line).
///
/// The `docker run` path (container creation) is protected separately via
/// `Command::env()` in `run_create`, which keeps secrets out of argv entirely.
pub(crate) fn build_docker_env_args(
    profile: &str,
    sandbox: &SandboxInfo,
    project_path: &std::path::Path,
) -> DockerExecEnv {
    let sandbox_config = resolved_sandbox_config(profile, project_path);

    tracing::debug!(target: "session.create",
        "build_docker_env_args: profile={:?}, config.sandbox.environment={:?}, extra_env={:?}",
        profile,
        sandbox_config.environment,
        sandbox.extra_env
    );

    let env_entries = collect_environment(&sandbox_config, sandbox);

    tracing::debug!(target: "session.create",
        "build_docker_env_args: resolved {} env entries",
        env_entries.len()
    );
    for entry in &env_entries {
        tracing::debug!(target: "session.create", "  env: {}=<set>", entry.key());
    }

    let mut docker_flag_parts: Vec<String> = Vec::new();
    let mut exports: Vec<String> = Vec::new();

    for entry in &env_entries {
        match entry {
            EnvEntry::Inherit { key, value } => {
                // Key only in docker args; value injected via shell export
                docker_flag_parts.push(format!("-e {}", key));
                exports.push(format!("export {}={}", key, shell_escape(value)));
            }
            EnvEntry::Literal { key, value } => {
                // Non-secret literal values are safe in argv
                docker_flag_parts.push(format!("-e {}={}", key, shell_escape(value)));
            }
        }
    }

    DockerExecEnv {
        docker_args: docker_flag_parts.join(" "),
        exports,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Default posture (`inherit_host_environment` off): only the desktop and
    /// session vars a graphical login sets, sorted, empty values dropped.
    #[test]
    fn test_inherited_host_env_desktop_only_by_default() {
        let result = inherited_host_env_from(
            owned(&[
                ("DISPLAY", ":0"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("XDG_SESSION_TYPE", "wayland"),
                ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
                // Empty desktop vars are dropped rather than forwarded as
                // `KEY=`, which would blank out a working value inherited from
                // the tmux server's frozen base env.
                ("WAYLAND_DISPLAY", ""),
                // Not desktop vars: unrelated to reaching the user's display,
                // and forwarding them is what the opt-in passthrough is for.
                ("PATH", "/usr/bin"),
                ("HOME", "/home/me"),
                ("GOPATH", "/home/me/go"),
                ("SECRET_TOKEN", "abc"),
            ]),
            false,
        );
        assert_eq!(
            result,
            owned(&[
                ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
                ("DISPLAY", ":0"),
                ("XDG_RUNTIME_DIR", "/run/user/1000"),
                ("XDG_SESSION_TYPE", "wayland"),
            ])
        );
        assert!(
            inherited_host_env_from(owned(&[("PATH", "/bin")]), false).is_empty(),
            "a process with no desktop env forwards nothing, which is the case \
             for a daemon launched without the operator's environment"
        );
    }

    /// With `inherit_host_environment` on, arbitrary operator vars ride along
    /// (the `GOPATH` case in #3262) while aoe's own wiring and tmux-owned
    /// `TERM` stay out.
    #[test]
    fn test_inherited_host_env_passthrough_keeps_custom_vars() {
        let result = inherited_host_env_from(
            owned(&[
                ("GOPATH", "/home/me/go"),
                ("DISPLAY", ":0"),
                ("PATH", "/usr/bin"),
                // aoe's own auth and per-process wiring must never reach an
                // agent, whatever the operator opted into.
                ("AOE_TOKEN", "secret"),
                ("AOE_DAEMON_TOKEN", "secret"),
                ("AOE_ACP_SOCKET", "/tmp/sock"),
                // Same, under aoe's older prefix: the detached ACP runner is an
                // `aoe` process, so this would switch on its debug logging.
                ("AGENT_OF_EMPIRES_DEBUG", "1"),
                // tmux owns the pane's terminal type; a daemon's TERM is
                // routinely absent or `dumb`.
                ("TERM", "dumb"),
            ]),
            true,
        );
        assert_eq!(
            result,
            owned(&[
                ("DISPLAY", ":0"),
                ("GOPATH", "/home/me/go"),
                ("PATH", "/usr/bin"),
            ])
        );
    }

    #[test]
    fn test_passthrough_denyreason() {
        for key in ["GOPATH", "DISPLAY", "PATH", "HOME", "MY_CUSTOM_VAR"] {
            assert!(
                passthrough_denyreason(key).is_none(),
                "{key} should pass through"
            );
        }
        for key in [
            "AOE_TOKEN",
            "AOE_ACP_SOCKET",
            "AGENT_OF_EMPIRES_DEBUG",
            "AGENT_OF_EMPIRES_PROFILE",
            "TERM",
            "",
            "1BAD",
            "HAS-DASH",
        ] {
            assert!(
                passthrough_denyreason(key).is_some(),
                "{key:?} should be refused"
            );
        }
    }

    /// The pure core above takes `passthrough` as a bool, so it cannot catch a
    /// resolver that reads the wrong config key or the wrong scope. Drive the
    /// real [`inherited_host_env`] against an on-disk `config.toml` so the
    /// setting's name, its `[session]` section, and its effect are all pinned.
    ///
    /// `#[serial]` because it mutates the process-wide env and `HOME`.
    #[test]
    #[serial_test::serial]
    fn test_inherited_host_env_reads_the_setting_from_config() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let _app_dir = crate::session::test_support::isolate_app_dir_at(tmp.path());
        let _env = crate::session::test_support::EnvGuard::set(&[
            ("DISPLAY", ":7"),
            ("ENVTEST_CUSTOM_VAR", "custom-value"),
        ]);

        let config_path = crate::session::config::config_path().expect("config path");
        std::fs::create_dir_all(config_path.parent().expect("app dir")).expect("app dir");

        // Default: the file does not mention the key, so only desktop vars.
        std::fs::write(&config_path, "").expect("write config");
        let default = inherited_host_env("");
        assert!(
            default.iter().any(|(k, _)| k == "DISPLAY"),
            "the desktop layer is unconditional, got {default:?}"
        );
        assert!(
            !default.iter().any(|(k, _)| k == "ENVTEST_CUSTOM_VAR"),
            "an ordinary var must stay out by default, got {default:?}"
        );

        // Opted in: the same var now rides along.
        std::fs::write(&config_path, "[session]\ninherit_host_environment = true\n")
            .expect("write config");
        let opted_in = inherited_host_env("");
        assert_eq!(
            opted_in
                .iter()
                .find(|(k, _)| k == "ENVTEST_CUSTOM_VAR")
                .map(|(_, v)| v.as_str()),
            Some("custom-value"),
            "inherit_host_environment must widen the layer, got {opted_in:?}"
        );
    }

    #[test]
    fn test_login_shell_command_adds_login_flag_for_known_shells() {
        assert_eq!(login_shell_command("/bin/zsh"), "'/bin/zsh' -l");
        assert_eq!(login_shell_command("/bin/bash"), "'/bin/bash' -l");
        assert_eq!(
            login_shell_command("/opt/homebrew/bin/fish"),
            "'/opt/homebrew/bin/fish' -l"
        );
    }

    #[test]
    fn test_login_shell_command_plain_for_non_login_shells() {
        // nu / pwsh do not take a POSIX `-l`; launch them plain.
        assert_eq!(login_shell_command("/usr/bin/nu"), "'/usr/bin/nu'");
        assert_eq!(login_shell_command("/usr/bin/pwsh"), "'/usr/bin/pwsh'");
    }

    /// Regression test: when an instance is created under a non-default profile and
    /// has no per-session `extra_env` overrides, the docker env args must come from
    /// THAT profile's `sandbox.environment`, not from the user's globally configured
    /// default profile. Pre-fix, the web flow surfaced this as "personal profile's
    /// GH_TOKEN was ignored when launching from the web app."
    #[test]
    #[serial_test::serial]
    fn test_build_docker_env_args_uses_passed_profile_not_global_default() {
        let temp_home = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        // Determine app dir layout (matches session::get_app_dir_path).
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let app_dir = temp_home
            .path()
            .join(".config")
            .join(crate::session::APP_DIR_NAME_XDG);
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let app_dir = temp_home.path().join(crate::session::APP_DIR_NAME_OTHER);

        let profiles_dir = app_dir.join("profiles");
        std::fs::create_dir_all(profiles_dir.join("default")).unwrap();
        std::fs::create_dir_all(profiles_dir.join("personal")).unwrap();

        // Global config sets the "currently active" default profile.
        std::fs::write(
            app_dir.join("config.toml"),
            r#"default_profile = "default""#,
        )
        .unwrap();

        // Two profiles with distinct env values; both use literal values so the
        // test does not depend on inherited host env vars.
        std::fs::write(
            profiles_dir.join("default").join("config.toml"),
            r#"
[sandbox]
environment = ["GH_TOKEN=read_only_token"]
"#,
        )
        .unwrap();
        std::fs::write(
            profiles_dir.join("personal").join("config.toml"),
            r#"
[sandbox]
environment = ["GH_TOKEN=write_token"]
"#,
        )
        .unwrap();

        // Sandbox info with no per-session overrides forces the fallback path
        // through `sandbox_config.environment`, which is the buggy path pre-fix.
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let project_path = temp_home.path().join("nonexistent_project");

        let result_personal = build_docker_env_args("personal", &sandbox, &project_path);
        assert!(
            result_personal
                .docker_args
                .contains("GH_TOKEN='write_token'"),
            "passing profile=\"personal\" should resolve personal profile's env, got: {}",
            result_personal.docker_args,
        );

        let result_default = build_docker_env_args("default", &sandbox, &project_path);
        assert!(
            result_default
                .docker_args
                .contains("GH_TOKEN='read_only_token'"),
            "passing profile=\"default\" should resolve default profile's env, got: {}",
            result_default.docker_args,
        );

        // Empty profile must fall back to the user's globally configured default,
        // preserving prior behavior for callers without a profile in hand.
        let result_empty = build_docker_env_args("", &sandbox, &project_path);
        assert!(
            result_empty
                .docker_args
                .contains("GH_TOKEN='read_only_token'"),
            "empty profile must fall back to global default, got: {}",
            result_empty.docker_args,
        );
    }

    #[test]
    fn test_redact_env_values_docker_flags() {
        let cmd = "docker exec -e GH_TOKEN='secret' -e TERM=xterm container claude";
        let redacted = redact_env_values(cmd);
        assert!(redacted.contains("GH_TOKEN=<redacted>"));
        assert!(redacted.contains("TERM=xterm")); // safe key, not redacted
        assert!(!redacted.contains("secret"));
    }

    #[test]
    fn test_redact_env_values_export_statements() {
        let cmd = "export GH_TOKEN='secret123'; export TERM='xterm'; exec docker exec -e GH_TOKEN container claude";
        let redacted = redact_env_values(cmd);
        assert!(redacted.contains("export GH_TOKEN=<redacted>"));
        assert!(redacted.contains("export TERM='xterm'")); // safe key, not redacted
        assert!(!redacted.contains("secret123"));
    }

    #[test]
    fn test_redact_env_values_mixed_exports_and_flags() {
        let cmd = "export API_KEY='sk-abc'; exec bash -lc 'exec env docker exec -e API_KEY -e FOO='bar' container claude'";
        let redacted = redact_env_values(cmd);
        assert!(redacted.contains("export API_KEY=<redacted>"));
        assert!(!redacted.contains("sk-abc"));
        // -e API_KEY (key only, no value) should pass through unchanged
        assert!(redacted.contains("-e API_KEY"));
        assert!(redacted.contains("FOO=<redacted>"));
    }

    #[test]
    fn test_shell_escape_quotes_and_metacharacters() {
        // Single-quoting makes every shell metacharacter literal, so the only
        // input needing real work is an apostrophe (closed, escaped, reopened).
        // Newlines and carriage returns become two-character escapes so the
        // result is always safe to paste on one command line.
        let cases = [
            ("hello", "'hello'"),
            // apostrophe: close, escape, reopen
            ("Don't do that", "'Don'\\''t do that'"),
            // double quotes are literal inside single quotes
            ("say \"hello\"", "'say \"hello\"'"),
            // backslashes are literal inside single quotes
            ("path\\to\\file", "'path\\to\\file'"),
            // no parameter expansion
            ("$HOME/path", "'$HOME/path'"),
            // no command substitution
            ("run `cmd`", "'run `cmd`'"),
            // no history expansion
            ("hello!", "'hello!'"),
            ("line1\nline2", "'line1\\nline2'"),
            ("line1\rline2", "'line1\\rline2'"),
            ("line1\r\nline2", "'line1\\r\\nline2'"),
            (
                "First instruction.\nSecond instruction.\nThird instruction.",
                "'First instruction.\\nSecond instruction.\\nThird instruction.'",
            ),
            (
                "Say \"hello\"\nRun `echo $HOME`",
                "'Say \"hello\"\\nRun `echo $HOME`'",
            ),
            // both apostrophes and double quotes
            ("He said \"don't\"", "'He said \"don'\\''t\"'"),
        ];
        for (input, expected) in cases {
            let escaped = shell_escape(input);
            assert_eq!(escaped, expected, "shell_escape({input:?})");
            assert!(
                !escaped.contains('\n') && !escaped.contains('\r'),
                "shell_escape({input:?}) must stay on one line, got {escaped:?}"
            );
        }
    }

    #[test]
    fn test_host_environment_prefix_literal_forms() {
        // Cases that depend only on the entry string, not on host env. The
        // `$VAR`-reading and bare-key forms need process env and live in the
        // serial tests below.
        let cases: &[(&[&str], &str)] = &[
            (&["FOO=bar"], "FOO='bar' "),
            (&[], ""),
            // No path-aware magic: `~` is passed through verbatim, matching
            // sandbox.environment behavior. Users who want home-relative paths
            // should either use absolute paths or pass `$HOME` (bare key) and
            // resolve in their agent invocation.
            (&["DIR=~/sub"], "DIR='~/sub' "),
            // `$$literal` emits a literal `$literal`.
            (&["MARKER=$$KEEP"], "MARKER='$KEEP' "),
            // Single-quote wrapping with `'\''` escape for the apostrophe.
            (&["X=a b'c$d"], "X='a b'\\''c$d' "),
            // A key is a bare shell name here and cannot be quoted, so an
            // invalid one is dropped instead of concatenated into the command.
            (&["FOO; touch /tmp/pwn; X=1", "GOOD=ok"], "GOOD='ok' "),
        ];
        for (entries, expected) in cases {
            let owned: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
            assert_eq!(host_environment_prefix(&owned), *expected, "{entries:?}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_host_environment_prefix_dollar_var_reads_host_env() {
        std::env::set_var("AOE_TEST_HOST_ENV_PREFIX", "from-host");
        let prefix = host_environment_prefix(&["FORWARDED=$AOE_TEST_HOST_ENV_PREFIX".to_string()]);
        std::env::remove_var("AOE_TEST_HOST_ENV_PREFIX");
        assert_eq!(prefix, "FORWARDED='from-host' ");
    }

    #[test]
    #[serial_test::serial]
    fn test_host_environment_prefix_dollar_var_missing_is_skipped() {
        std::env::remove_var("AOE_TEST_DEFINITELY_NOT_SET");
        let prefix = host_environment_prefix(&[
            "MISSING=$AOE_TEST_DEFINITELY_NOT_SET".to_string(),
            "PRESENT=ok".to_string(),
        ]);
        assert_eq!(prefix, "PRESENT='ok' ");
    }

    #[test]
    #[serial_test::serial]
    fn test_host_environment_prefix_bare_key_passthrough() {
        std::env::set_var("AOE_TEST_BARE_PASSTHROUGH", "v");
        let prefix = host_environment_prefix(&["AOE_TEST_BARE_PASSTHROUGH".to_string()]);
        std::env::remove_var("AOE_TEST_BARE_PASSTHROUGH");
        assert_eq!(prefix, "AOE_TEST_BARE_PASSTHROUGH='v' ");
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_host_environment_value_uses_last_resolved_entry() {
        std::env::remove_var("AOE_TEST_MISSING_HOST_ENV_VALUE");
        let entries = vec![
            "CODEX_HOME=/first".to_string(),
            "OTHER=value".to_string(),
            "CODEX_HOME=$AOE_TEST_MISSING_HOST_ENV_VALUE".to_string(),
            "CODEX_HOME=/second".to_string(),
        ];

        assert_eq!(
            resolve_host_environment_value(&entries, "CODEX_HOME"),
            Some("/second".to_string())
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_host_environment_value_matches_host_env_grammar() {
        std::env::set_var("AOE_TEST_CODEX_HOME_REF", "/from-host");
        let entries = vec!["CODEX_HOME=$AOE_TEST_CODEX_HOME_REF".to_string()];

        assert_eq!(
            resolve_host_environment_value(&entries, "CODEX_HOME"),
            Some("/from-host".to_string())
        );

        std::env::remove_var("AOE_TEST_CODEX_HOME_REF");
    }

    /// The pair resolver must speak the same entry grammar the terminal-view
    /// prefix does, so a `Config.environment` list means the same thing to a
    /// structured worker as it does to a tmux pane.
    #[cfg(feature = "serve")]
    #[test]
    #[serial_test::serial]
    fn test_resolve_host_environment_pairs_matches_prefix_grammar() {
        std::env::set_var("AOE_TEST_HOST_PAIRS_REF", "from-host");
        std::env::set_var("AOE_TEST_HOST_PAIRS_BARE", "bare-val");
        std::env::remove_var("AOE_TEST_HOST_PAIRS_MISSING");
        let entries = vec![
            "CODEX_HOME=/literal".to_string(),
            "FROM_HOST=$AOE_TEST_HOST_PAIRS_REF".to_string(),
            "ESCAPED=$$LIT".to_string(),
            "AOE_TEST_HOST_PAIRS_BARE".to_string(),
            "MISSING=$AOE_TEST_HOST_PAIRS_MISSING".to_string(), // unset ref: skipped
            "1BAD=x".to_string(),                               // invalid key: skipped
        ];
        assert_eq!(
            resolve_host_environment_pairs(&entries),
            vec![
                ("CODEX_HOME".to_string(), "/literal".to_string()),
                ("FROM_HOST".to_string(), "from-host".to_string()),
                ("ESCAPED".to_string(), "$LIT".to_string()),
                (
                    "AOE_TEST_HOST_PAIRS_BARE".to_string(),
                    "bare-val".to_string()
                ),
            ]
        );
        std::env::remove_var("AOE_TEST_HOST_PAIRS_REF");
        std::env::remove_var("AOE_TEST_HOST_PAIRS_BARE");
    }

    /// Duplicate keys resolve LAST-wins, matching the shell assignment order
    /// `host_environment_prefix` emits (and `resolve_host_environment_value`),
    /// not the first-wins rule the container path uses. An entry whose host
    /// reference is unset does not clobber an earlier resolved value.
    #[cfg(feature = "serve")]
    #[test]
    #[serial_test::serial]
    fn test_resolve_host_environment_pairs_last_entry_wins() {
        std::env::remove_var("AOE_TEST_HOST_PAIRS_UNSET");
        let entries = vec![
            "CODEX_HOME=/first".to_string(),
            "OTHER=keep".to_string(),
            "CODEX_HOME=/second".to_string(),
            "CODEX_HOME=$AOE_TEST_HOST_PAIRS_UNSET".to_string(),
        ];
        assert_eq!(
            resolve_host_environment_pairs(&entries),
            vec![
                ("OTHER".to_string(), "keep".to_string()),
                ("CODEX_HOME".to_string(), "/second".to_string()),
            ]
        );
    }

    /// An empty mint list is the overwhelmingly common case (no
    /// `before_session` configured) and must leave the entry list untouched,
    /// including its order.
    #[test]
    fn test_drop_shadowed_host_entries_no_mint_is_identity() {
        let entries = vec![
            "CLAUDE_CONFIG_DIR=/a".to_string(),
            "TERM".to_string(),
            "GH_TOKEN=$GH_TOKEN".to_string(),
        ];
        assert_eq!(
            drop_shadowed_host_entries(entries.clone(), &[]),
            entries,
            "no minted keys must not perturb the list"
        );
    }

    /// A minted key removes the static entry for that key regardless of which
    /// entry form declared it: `KEY=literal`, `KEY=$REF`, or a bare passthrough
    /// `KEY`. Unrelated entries keep their relative order.
    #[test]
    fn test_drop_shadowed_host_entries_removes_every_entry_form() {
        let entries = vec![
            "CLAUDE_CONFIG_DIR=/stale".to_string(),
            "KEEP_LITERAL=keep".to_string(),
            "ANTHROPIC_BASE_URL=$SOME_REF".to_string(),
            "TERM".to_string(),
            "KEEP_BARE".to_string(),
        ];
        let minted = vec![
            ("CLAUDE_CONFIG_DIR".to_string(), "/fresh".to_string()),
            ("ANTHROPIC_BASE_URL".to_string(), "http://x".to_string()),
            ("TERM".to_string(), "xterm".to_string()),
        ];
        assert_eq!(
            drop_shadowed_host_entries(entries, &minted),
            vec!["KEEP_LITERAL=keep".to_string(), "KEEP_BARE".to_string()]
        );
    }

    /// Only an exact key match shadows. A minted `FOO` must not remove `FOOBAR`
    /// or `FOO_BAR`, which a prefix-based filter would get wrong.
    #[test]
    fn test_drop_shadowed_host_entries_matches_whole_key_only() {
        let entries = vec![
            "FOO=1".to_string(),
            "FOOBAR=2".to_string(),
            "FOO_BAR=3".to_string(),
        ];
        let minted = vec![("FOO".to_string(), "minted".to_string())];
        assert_eq!(
            drop_shadowed_host_entries(entries, &minted),
            vec!["FOOBAR=2".to_string(), "FOO_BAR=3".to_string()]
        );
    }

    /// Helper to find an entry by key and check its value
    fn find_entry<'a>(entries: &'a [EnvEntry], key: &str) -> Option<&'a EnvEntry> {
        entries.iter().find(|e| e.key() == key)
    }

    #[test]
    fn test_collect_environment_rejects_invalid_config_extra_and_hook_keys() {
        let config = SandboxConfig {
            environment: vec![
                "CFG; touch /tmp/cfg_injected; #=secret".to_string(),
                "VALID_CONFIG=ok".to_string(),
            ],
            ..Default::default()
        };
        let base = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: vec![
                (
                    "HOOK$(touch /tmp/hook_injected)".to_string(),
                    "secret".to_string(),
                ),
                ("VALID_HOOK".to_string(), "minted".to_string()),
            ],
            container_workdir: None,
        };
        let configured = collect_environment(&config, &base);
        assert!(find_entry(&configured, "VALID_CONFIG").is_some());
        assert!(find_entry(&configured, "VALID_HOOK").is_some());
        assert!(!configured
            .iter()
            .any(|entry| { entry.key().contains("touch") || entry.key().contains(';') }));

        let mut extra = base;
        extra.extra_env = Some(vec![
            "EXTRA`touch /tmp/extra_injected`=secret".to_string(),
            "VALID_EXTRA=ok".to_string(),
        ]);
        let resolved_extra = collect_environment(&config, &extra);
        assert!(find_entry(&resolved_extra, "VALID_EXTRA").is_some());
        assert!(!resolved_extra
            .iter()
            .any(|entry| { entry.key().contains("touch") || entry.key().contains('`') }));
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_environment_passthrough() {
        std::env::set_var("AOE_TEST_ENV_PT", "test_value");
        let config = SandboxConfig {
            environment: vec!["AOE_TEST_ENV_PT".to_string()],
            ..Default::default()
        };
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let entry = find_entry(&result, "AOE_TEST_ENV_PT").expect("AOE_TEST_ENV_PT not found");
        assert_eq!(entry.value(), "test_value");
        assert!(matches!(entry, EnvEntry::Inherit { .. }));
        std::env::remove_var("AOE_TEST_ENV_PT");
    }

    #[test]
    #[serial_test::serial]
    fn test_resolve_hook_env_pairs_grammar() {
        std::env::set_var("AOE_TEST_HOST_PAIR_REF", "from_host");
        std::env::set_var("AOE_TEST_HOST_PAIR_BARE", "bare_val");
        std::env::remove_var("AOE_TEST_HOST_PAIR_MISSING");
        let entries = vec![
            "TEST_VAR=literal".to_string(),
            "FROM_HOST=$AOE_TEST_HOST_PAIR_REF".to_string(),
            "ESCAPED=$$LIT".to_string(),
            "AOE_TEST_HOST_PAIR_BARE".to_string(),
            "MISSING=$AOE_TEST_HOST_PAIR_MISSING".to_string(), // unset host ref: skipped
            "TEST_VAR=second".to_string(),                     // dup key: first wins
        ];
        let pairs = resolve_hook_env_pairs(&entries);
        assert_eq!(
            pairs,
            vec![
                ("TEST_VAR".to_string(), "literal".to_string()),
                ("FROM_HOST".to_string(), "from_host".to_string()),
                ("ESCAPED".to_string(), "$LIT".to_string()),
                (
                    "AOE_TEST_HOST_PAIR_BARE".to_string(),
                    "bare_val".to_string()
                ),
            ]
        );
        std::env::remove_var("AOE_TEST_HOST_PAIR_REF");
        std::env::remove_var("AOE_TEST_HOST_PAIR_BARE");
    }

    #[test]
    fn test_resolve_hook_env_pairs_skips_invalid_keys() {
        // Malformed keys (would fail at Command::envs) are dropped; valid ones
        // pass through.
        let entries = vec![
            "GOOD=1".to_string(),
            "1BAD=x".to_string(),      // starts with a digit
            "HAS SPACE=y".to_string(), // contains a space
            "=novalue".to_string(),    // empty key
            "_OK=2".to_string(),
        ];
        assert_eq!(
            resolve_hook_env_pairs(&entries),
            vec![
                ("GOOD".to_string(), "1".to_string()),
                ("_OK".to_string(), "2".to_string()),
            ]
        );
    }

    #[test]
    fn test_host_hook_entries_drops_repo_contributed() {
        // extra_env carries one user entry and two that came from config; the
        // repo-only one (in repo_aware but not trusted) is dropped, the one also
        // in the profile/global baseline is kept.
        let extra = vec![
            "TEST_VAR=foo".to_string(),  // user-typed
            "NODE_ENV=test".to_string(), // repo-contributed
            "SHARED=keep".to_string(),   // also in profile/global baseline
        ];
        let trusted = vec!["SHARED=keep".to_string()];
        let repo_aware = vec!["NODE_ENV=test".to_string(), "SHARED=keep".to_string()];
        assert_eq!(
            host_hook_entries(&extra, &trusted, &repo_aware),
            vec!["TEST_VAR=foo".to_string(), "SHARED=keep".to_string()],
        );
    }

    #[test]
    fn test_session_host_env_pairs_uses_extra_env() {
        // With a per-session extra_env and no repo config at the path, every
        // entry survives the repo filter and is resolved to a host pair.
        let tmp = tempfile::tempdir().unwrap();
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "img".to_string(),
            container_name: "ctr".to_string(),
            extra_env: Some(vec!["TEST_VAR=foo".to_string(), "OTHER=bar".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let pairs = session_host_env_pairs("any-profile", tmp.path(), &info);
        assert_eq!(
            pairs,
            vec![
                ("TEST_VAR".to_string(), "foo".to_string()),
                ("OTHER".to_string(), "bar".to_string()),
            ]
        );
    }

    #[test]
    fn test_collect_environment_before_start_is_inherited() {
        // before_start-minted values are emitted as Inherit entries (so the
        // value rides the process env, never argv) and win over a same-keyed
        // sandbox.environment literal.
        let config = SandboxConfig {
            environment: vec!["GH_TOKEN=stale_literal".to_string()],
            ..Default::default()
        };
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: vec![("GH_TOKEN".to_string(), "ghs_fresh".to_string())],
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let entries: Vec<_> = result.iter().filter(|e| e.key() == "GH_TOKEN").collect();
        assert_eq!(entries.len(), 1, "deduped to a single GH_TOKEN entry");
        assert_eq!(entries[0].value(), "ghs_fresh");
        assert!(
            matches!(entries[0], EnvEntry::Inherit { .. }),
            "before_start values must be Inherit (leak-safe), not Literal"
        );
    }

    #[test]
    fn test_collect_environment_key_value() {
        let config = SandboxConfig {
            environment: vec!["MY_KEY=my_value".to_string()],
            ..Default::default()
        };
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let entry = find_entry(&result, "MY_KEY").expect("MY_KEY not found");
        assert_eq!(entry.value(), "my_value");
        assert!(matches!(entry, EnvEntry::Literal { .. }));
    }

    #[test]
    fn test_collect_environment_includes_git_safe_directory() {
        let config = SandboxConfig::default();
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let count = find_entry(&result, "GIT_CONFIG_COUNT").expect("GIT_CONFIG_COUNT not found");
        assert_eq!(count.value(), "1");
        assert!(matches!(count, EnvEntry::Literal { .. }));

        let key = find_entry(&result, "GIT_CONFIG_KEY_0").expect("GIT_CONFIG_KEY_0 not found");
        assert_eq!(key.value(), "safe.directory");
        assert!(matches!(key, EnvEntry::Literal { .. }));

        let value =
            find_entry(&result, "GIT_CONFIG_VALUE_0").expect("GIT_CONFIG_VALUE_0 not found");
        assert_eq!(value.value(), "*");
        assert!(matches!(value, EnvEntry::Literal { .. }));
    }

    #[test]
    fn test_collect_environment_git_safe_directory_user_override() {
        // If the user already provides GIT_CONFIG_* entries (e.g. via
        // sandbox.environment or extra_env), their values must take
        // precedence over the built-in safe.directory defaults.
        let config = SandboxConfig::default();
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: Some(vec![
                "GIT_CONFIG_COUNT=2".to_string(),
                "GIT_CONFIG_KEY_0=safe.directory".to_string(),
                "GIT_CONFIG_VALUE_0=/workspace/custom".to_string(),
                "GIT_CONFIG_KEY_1=safe.directory".to_string(),
                "GIT_CONFIG_VALUE_1=/workspace/other".to_string(),
            ]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let count = find_entry(&result, "GIT_CONFIG_COUNT").expect("GIT_CONFIG_COUNT not found");
        assert_eq!(count.value(), "2");
        assert!(matches!(count, EnvEntry::Literal { .. }));

        let value0 =
            find_entry(&result, "GIT_CONFIG_VALUE_0").expect("GIT_CONFIG_VALUE_0 not found");
        assert_eq!(value0.value(), "/workspace/custom");
        assert!(matches!(value0, EnvEntry::Literal { .. }));

        let value1 =
            find_entry(&result, "GIT_CONFIG_VALUE_1").expect("GIT_CONFIG_VALUE_1 not found");
        assert_eq!(value1.value(), "/workspace/other");
        assert!(matches!(value1, EnvEntry::Literal { .. }));
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_environment_extra_env() {
        std::env::set_var("AOE_TEST_EXTRA", "extra_val");
        let config = SandboxConfig::default();
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: Some(vec!["AOE_TEST_EXTRA".to_string(), "FOO=bar".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let extra = find_entry(&result, "AOE_TEST_EXTRA").expect("AOE_TEST_EXTRA not found");
        assert_eq!(extra.value(), "extra_val");
        assert!(matches!(extra, EnvEntry::Inherit { .. }));
        let foo = find_entry(&result, "FOO").expect("FOO not found");
        assert_eq!(foo.value(), "bar");
        assert!(matches!(foo, EnvEntry::Literal { .. }));
        std::env::remove_var("AOE_TEST_EXTRA");
    }

    #[test]
    fn test_collect_environment_extra_env_is_authoritative() {
        let config = SandboxConfig {
            environment: vec!["DUP_KEY=from_config".to_string()],
            ..Default::default()
        };
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: Some(vec!["DUP_KEY=from_session".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let dup_entries: Vec<_> = result.iter().filter(|e| e.key() == "DUP_KEY").collect();
        assert_eq!(dup_entries.len(), 1);
        assert_eq!(dup_entries[0].value(), "from_session");
    }

    #[test]
    fn test_collect_environment_falls_back_to_config_when_no_extra() {
        let config = SandboxConfig {
            environment: vec!["CONFIG_KEY=config_val".to_string()],
            ..Default::default()
        };
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let entry = find_entry(&result, "CONFIG_KEY").expect("CONFIG_KEY not found");
        assert_eq!(entry.value(), "config_val");
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_environment_dollar_ref() {
        std::env::set_var("AOE_TEST_HOST_REF", "host_val");
        let config = SandboxConfig {
            environment: vec!["INJECTED=$AOE_TEST_HOST_REF".to_string()],
            ..Default::default()
        };
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let entry = find_entry(&result, "INJECTED").expect("INJECTED not found");
        assert_eq!(entry.value(), "host_val");
        assert!(matches!(entry, EnvEntry::Inherit { .. }));
        std::env::remove_var("AOE_TEST_HOST_REF");
    }

    #[test]
    fn test_collect_environment_dollar_dollar_escape() {
        let config = SandboxConfig {
            environment: vec!["ESCAPED=$$LITERAL".to_string()],
            ..Default::default()
        };
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let entry = find_entry(&result, "ESCAPED").expect("ESCAPED not found");
        assert_eq!(entry.value(), "$LITERAL");
        assert!(matches!(entry, EnvEntry::Literal { .. }));
    }

    #[test]
    #[serial_test::serial]
    fn test_validate_env_entry_bare_key_present() {
        std::env::set_var("AOE_TEST_VALIDATE_BARE", "exists");
        assert_eq!(validate_env_entry("AOE_TEST_VALIDATE_BARE"), None);
        std::env::remove_var("AOE_TEST_VALIDATE_BARE");
    }

    #[test]
    #[serial_test::serial]
    fn test_validate_env_entry_bare_key_missing() {
        std::env::remove_var("AOE_TEST_VALIDATE_MISSING_BARE");
        let result = validate_env_entry("AOE_TEST_VALIDATE_MISSING_BARE");
        assert!(result.is_some());
        assert!(result.unwrap().contains("AOE_TEST_VALIDATE_MISSING_BARE"));
    }

    #[test]
    #[serial_test::serial]
    fn test_validate_env_entry_key_dollar_var_present() {
        std::env::set_var("AOE_TEST_VALIDATE_REF", "value");
        assert_eq!(validate_env_entry("MY_KEY=$AOE_TEST_VALIDATE_REF"), None);
        std::env::remove_var("AOE_TEST_VALIDATE_REF");
    }

    #[test]
    #[serial_test::serial]
    fn test_validate_env_entry_key_dollar_var_missing() {
        std::env::remove_var("AOE_TEST_VALIDATE_MISSING_REF");
        let result = validate_env_entry("MY_KEY=$AOE_TEST_VALIDATE_MISSING_REF");
        assert!(result.is_some());
        assert!(result.unwrap().contains("AOE_TEST_VALIDATE_MISSING_REF"));
    }

    #[test]
    fn test_validate_env_entry_literal_value() {
        assert_eq!(validate_env_entry("MY_KEY=some_literal"), None);
    }

    #[test]
    fn test_validate_env_entry_escaped_dollar() {
        assert_eq!(validate_env_entry("MY_KEY=$$ESCAPED"), None);
    }

    #[test]
    #[serial_test::serial]
    fn test_validate_env_entries_returns_one_warning_per_missing_var() {
        // Use unique names to avoid collisions with other tests' env state.
        std::env::remove_var("AOE_TEST_BATCH_MISSING_A");
        std::env::remove_var("AOE_TEST_BATCH_MISSING_B");
        std::env::set_var("AOE_TEST_BATCH_PRESENT", "ok");

        let entries = vec![
            "GH_TOKEN=$AOE_TEST_BATCH_MISSING_A".to_string(),
            "OK=$AOE_TEST_BATCH_PRESENT".to_string(),
            "ALSO_BROKEN=$AOE_TEST_BATCH_MISSING_B".to_string(),
            "LITERAL=fine".to_string(),
        ];
        let warnings = validate_env_entries(&entries);
        assert_eq!(
            warnings.len(),
            2,
            "expected 2 warnings, got: {:?}",
            warnings
        );
        assert!(warnings
            .iter()
            .any(|w| w.contains("AOE_TEST_BATCH_MISSING_A")));
        assert!(warnings
            .iter()
            .any(|w| w.contains("AOE_TEST_BATCH_MISSING_B")));

        std::env::remove_var("AOE_TEST_BATCH_PRESENT");
    }

    #[test]
    fn test_validate_env_entries_empty_list() {
        assert!(validate_env_entries(Vec::<String>::new()).is_empty());
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_validate_env_entries_skips_default_terminal_vars_when_unset() {
        // Stash + remove the defaults so the test catches all four keys even
        // on CI hosts where TERM/COLORTERM are set. `serial(shell_env)` matches
        // the pattern used by other tests in this file that mutate globally-
        // shared env vars.
        let originals: Vec<(&&str, Option<String>)> = DEFAULT_TERMINAL_ENV_VARS
            .iter()
            .map(|k| (k, std::env::var(*k).ok()))
            .collect();
        for key in DEFAULT_TERMINAL_ENV_VARS {
            std::env::remove_var(key);
        }

        let entries: Vec<String> = DEFAULT_TERMINAL_ENV_VARS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let warnings = validate_env_entries(&entries);

        for (key, original) in originals {
            match original {
                Some(v) => std::env::set_var(*key, v),
                None => std::env::remove_var(*key),
            }
        }

        assert!(
            warnings.is_empty(),
            "expected no warnings for default terminal vars even when unset, got: {:?}",
            warnings
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_build_docker_env_args_inherit_uses_key_only_in_args() {
        // Inherited (secret) env vars must NOT have values in docker_args.
        // Values are in exports for injection via tmux send-keys.
        std::env::set_var("AOE_TEST_TOKEN", "secret123");
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: Some(vec!["AOE_TEST_TOKEN=$AOE_TEST_TOKEN".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let result = build_docker_env_args("", &sandbox, std::path::Path::new("/nonexistent"));
        // docker_args should have the key but NOT the secret value
        assert!(
            result.docker_args.contains("-e AOE_TEST_TOKEN"),
            "Expected -e AOE_TEST_TOKEN in docker_args: {}",
            result.docker_args
        );
        assert!(
            !result.docker_args.contains("secret123"),
            "Secret value must NOT appear in docker_args: {}",
            result.docker_args
        );
        // exports should have the value for tmux send-keys injection
        assert!(
            result
                .exports
                .iter()
                .any(|e| e.contains("AOE_TEST_TOKEN") && e.contains("secret123")),
            "Expected export with secret value in exports: {:?}",
            result.exports
        );
        std::env::remove_var("AOE_TEST_TOKEN");
    }

    #[test]
    #[serial_test::serial]
    fn test_build_docker_env_args_inherit_with_different_key() {
        std::env::set_var("AOE_TEST_SOURCE", "secret456");
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: Some(vec!["MY_MAPPED=$AOE_TEST_SOURCE".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let result = build_docker_env_args("", &sandbox, std::path::Path::new("/nonexistent"));
        assert!(
            result.docker_args.contains("-e MY_MAPPED"),
            "Expected -e MY_MAPPED in docker_args: {}",
            result.docker_args
        );
        assert!(
            !result.docker_args.contains("secret456"),
            "Secret value must NOT appear in docker_args: {}",
            result.docker_args
        );
        assert!(
            result
                .exports
                .iter()
                .any(|e| e.contains("MY_MAPPED") && e.contains("secret456")),
            "Expected export with value in exports: {:?}",
            result.exports
        );
        std::env::remove_var("AOE_TEST_SOURCE");
    }

    #[test]
    #[serial_test::serial]
    fn test_build_docker_env_args_bare_key_uses_export() {
        // Bare keys (pass-through from host) are Inherit entries,
        // so they must use exports, not inline values.
        std::env::set_var("AOE_TEST_BARE", "barevalue");
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: Some(vec!["AOE_TEST_BARE".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let result = build_docker_env_args("", &sandbox, std::path::Path::new("/nonexistent"));
        assert!(
            result.docker_args.contains("-e AOE_TEST_BARE"),
            "Expected -e AOE_TEST_BARE in docker_args: {}",
            result.docker_args
        );
        assert!(
            !result.docker_args.contains("barevalue"),
            "Secret value must NOT appear in docker_args: {}",
            result.docker_args
        );
        assert!(
            result
                .exports
                .iter()
                .any(|e| e.contains("AOE_TEST_BARE") && e.contains("barevalue")),
            "Expected export with value: {:?}",
            result.exports
        );
        std::env::remove_var("AOE_TEST_BARE");
    }

    #[test]
    fn test_build_docker_env_args_literal_stays_in_args() {
        // Literal (non-secret) entries should have values in docker_args
        // and should NOT produce exports.
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: Some(vec!["MY_LITERAL=some_value".to_string()]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let result = build_docker_env_args("", &sandbox, std::path::Path::new("/nonexistent"));
        assert!(
            result.docker_args.contains("MY_LITERAL="),
            "Expected MY_LITERAL=value in docker_args: {}",
            result.docker_args
        );
        assert!(
            result.docker_args.contains("some_value"),
            "Expected literal value in docker_args: {}",
            result.docker_args
        );
        // No exports for literal entries
        assert!(
            !result.exports.iter().any(|e| e.contains("MY_LITERAL")),
            "Literal entries must NOT produce exports: {:?}",
            result.exports
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_build_docker_env_args_mixed_inherit_and_literal() {
        std::env::set_var("AOE_TEST_SECRET", "mysecret");
        let sandbox = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: Some(vec![
                "AOE_TEST_SECRET=$AOE_TEST_SECRET".to_string(),
                "MY_LITERAL=public_val".to_string(),
            ]),
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };
        let result = build_docker_env_args("", &sandbox, std::path::Path::new("/nonexistent"));
        // Secret: key only in docker_args, value in exports
        assert!(result.docker_args.contains("-e AOE_TEST_SECRET"));
        assert!(!result.docker_args.contains("mysecret"));
        assert!(result
            .exports
            .iter()
            .any(|e| e.contains("AOE_TEST_SECRET") && e.contains("mysecret")));
        // Literal: key=value in docker_args, no export
        assert!(result.docker_args.contains("MY_LITERAL='public_val'"));
        assert!(!result.exports.iter().any(|e| e.contains("MY_LITERAL")));
        std::env::remove_var("AOE_TEST_SECRET");
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_user_shell_reads_env() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "/bin/zsh");
        assert_eq!(user_shell(), "/bin/zsh");
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_user_shell_fallback() {
        let original = std::env::var("SHELL").ok();
        std::env::remove_var("SHELL");
        assert_eq!(user_shell(), "bash");
        if let Some(v) = original {
            std::env::set_var("SHELL", v);
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_user_shell_empty_falls_back() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "  ");
        assert_eq!(user_shell(), "bash");
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_user_posix_shell_returns_posix() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "/bin/zsh");
        assert_eq!(user_posix_shell(), "/bin/zsh");
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_user_posix_shell_falls_back_for_fish() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "/usr/bin/fish");
        assert_eq!(user_posix_shell(), "bash");
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_user_posix_shell_falls_back_for_nu() {
        let original = std::env::var("SHELL").ok();
        std::env::set_var("SHELL", "/usr/bin/nu");
        assert_eq!(user_posix_shell(), "bash");
        match original {
            Some(v) => std::env::set_var("SHELL", v),
            None => std::env::remove_var("SHELL"),
        }
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_environment_auto_forwards_vertex_vars_when_enabled() {
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "my-proj");
        std::env::set_var("CLOUD_ML_REGION", "us-east5");
        let config = SandboxConfig::default();
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);

        let vertex_flag = find_entry(&result, "CLAUDE_CODE_USE_VERTEX")
            .expect("CLAUDE_CODE_USE_VERTEX not found");
        assert_eq!(vertex_flag.value(), "1");
        assert!(matches!(vertex_flag, EnvEntry::Inherit { .. }));

        let project = find_entry(&result, "ANTHROPIC_VERTEX_PROJECT_ID")
            .expect("ANTHROPIC_VERTEX_PROJECT_ID not found");
        assert_eq!(project.value(), "my-proj");

        let region = find_entry(&result, "CLOUD_ML_REGION").expect("CLOUD_ML_REGION not found");
        assert_eq!(region.value(), "us-east5");

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        std::env::remove_var("ANTHROPIC_VERTEX_PROJECT_ID");
        std::env::remove_var("CLOUD_ML_REGION");
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_environment_skips_vertex_vars_when_flag_unset() {
        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "my-proj");
        std::env::set_var("CLOUD_ML_REGION", "us-east5");
        let config = SandboxConfig::default();
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        assert!(
            find_entry(&result, "ANTHROPIC_VERTEX_PROJECT_ID").is_none(),
            "Vertex vars should not auto-forward when CLAUDE_CODE_USE_VERTEX is unset",
        );
        assert!(find_entry(&result, "CLOUD_ML_REGION").is_none());

        std::env::remove_var("ANTHROPIC_VERTEX_PROJECT_ID");
        std::env::remove_var("CLOUD_ML_REGION");
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_environment_skips_vertex_vars_when_flag_empty() {
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "");
        std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "my-proj");
        let config = SandboxConfig::default();
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        assert!(
            find_entry(&result, "ANTHROPIC_VERTEX_PROJECT_ID").is_none(),
            "Empty CLAUDE_CODE_USE_VERTEX must be treated as unset",
        );

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        std::env::remove_var("ANTHROPIC_VERTEX_PROJECT_ID");
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_environment_does_not_auto_forward_anthropic_api_key() {
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        std::env::set_var("ANTHROPIC_API_KEY", "sk-host-key");
        let config = SandboxConfig::default();
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        assert!(
            find_entry(&result, "ANTHROPIC_API_KEY").is_none(),
            "ANTHROPIC_API_KEY must not be auto-forwarded; users opt in via sandbox.environment",
        );

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    #[serial_test::serial]
    fn test_collect_environment_vertex_vars_not_duplicated() {
        std::env::set_var("CLAUDE_CODE_USE_VERTEX", "1");
        std::env::set_var("ANTHROPIC_VERTEX_PROJECT_ID", "my-proj");
        let config = SandboxConfig {
            environment: vec!["ANTHROPIC_VERTEX_PROJECT_ID".to_string()],
            ..Default::default()
        };
        let info = SandboxInfo {
            enabled: true,
            container_id: None,
            image: "test".to_string(),
            container_name: "test".to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        };

        let result = collect_environment(&config, &info);
        let matches: Vec<_> = result
            .iter()
            .filter(|e| e.key() == "ANTHROPIC_VERTEX_PROJECT_ID")
            .collect();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].value(), "my-proj");

        std::env::remove_var("CLAUDE_CODE_USE_VERTEX");
        std::env::remove_var("ANTHROPIC_VERTEX_PROJECT_ID");
    }
}
