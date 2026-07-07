//! Agent hook management for status detection.
//!
//! AoE installs hooks into an agent's settings file that write session
//! status (`running`/`waiting`/`idle`) to a sidecar file. This provides a
//! hook-first status source; agent-specific code may still reconcile known
//! hook gaps from tmux pane content.
//!
//! Hook events are agent-specific and defined in `AgentHookConfig::events`.

mod dir_guard;
mod status_file;
mod targets;

#[cfg(test)]
pub(crate) mod test_support;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt as _;
use serde_json::Value;

#[cfg(test)]
pub(crate) use dir_guard::{clear_base_override_for_test, override_base_for_test, reset_for_test};
pub(crate) use dir_guard::{
    ensure_instance_dir_path, hook_base_path, unlink_session_id_via_guard,
    write_session_id_via_guard,
};
pub use status_file::{
    cleanup_hook_status_dir, hook_status_dir, read_hook_session_id, read_hook_status,
    read_hook_urgent,
};
pub(crate) use targets::{
    has_aoe_marker, iter_hook_targets, iter_hook_targets_in, HookTarget, HookTargetKind,
};

/// Single source of truth for the `aoe-hooks` identity token. Defined as a
/// `macro_rules!` rather than a `const &str` because `concat!` only accepts
/// string literals, not const refs; folding this through `concat!` gives the
/// derived constants below a compile-time link to one literal occurrence.
macro_rules! aoe_hook_marker {
    () => {
        "aoe-hooks"
    };
}

/// Fixed base path used inside the sandbox image, where the multi-tenant
/// threat does not apply (per-container, single-tenant). The host bind-mounts
/// `/tmp/aoe-hooks-<euid>/<id>` from `dir_guard::hook_base_path()` to this
/// fixed path inside the container so the sandbox shell can bake a single
/// canonical string regardless of the host's effective uid.
pub(crate) const HOOK_STATUS_BASE_IN_CONTAINER: &str = concat!("/tmp/", aoe_hook_marker!());

/// Marker token embedded by every AoE-emitted hook command in two structurally
/// distinct positions: as a `# AOE_HOOK_MARKER` trailing shell comment, and as
/// a path component in `HOOK_STATUS_BASE_IN_CONTAINER`. [`is_aoe_hook_command`]
/// anchors on those positions via [`AOE_HOOK_TRAILING_SENTINEL`] and
/// [`AOE_HOOK_PATH_SENTINEL`], not on bare substring presence, so user
/// commands that mention the literal `aoe-hooks` are not misclassified.
const AOE_HOOK_MARKER: &str = aoe_hook_marker!();

/// Trailing sentinel: `0 # {AOE_HOOK_MARKER}`. Every shipping status and
/// session-id emitter ends with `exit 0 # AOE_HOOK_MARKER`, so the leading
/// `0 ` digit binds the sentinel to the canonical `exit 0` trailer and rules
/// out incidental `# aoe-hooks` matches inside echo arguments or quoted
/// strings.
const AOE_HOOK_TRAILING_SENTINEL: &str = concat!("0 # ", aoe_hook_marker!());

/// Path-template sentinel: `{AOE_HOOK_MARKER}/$AOE_INSTANCE_ID`. Baked into
/// the sandbox session-id command body via [`HOOK_STATUS_BASE_IN_CONTAINER`].
/// The un-expanded `$AOE_INSTANCE_ID` makes this user-collision-proof.
const AOE_HOOK_PATH_SENTINEL: &str = concat!(aoe_hook_marker!(), "/$AOE_INSTANCE_ID");

/// Where an agent's settings file lives. Determines which shell command
/// `hook_command_session_id` emits.
///
/// `Host`: emits a call to the `aoe __extract-session-id` Rust subcommand,
/// which parses stdin with `serde_json` and writes the top-level
/// `session_id` string atomically through the hardened per-user dir guard.
/// `Sandbox`: emits a POSIX shell wrapper around `jq -r` doing the same
/// structural top-level extraction, because `aoe` is not installed inside
/// the sandbox image. `jq` ships in the default `aoe-sandbox` image; on a
/// custom image without `jq` the hook silent-fails (exit 0, no sidecar)
/// and pane-content reconciliation takes over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookInstallTarget {
    Host,
    Sandbox,
}

/// Resolve the host Codex config path.
///
/// Codex treats `CODEX_HOME` as the directory containing `config.toml`, falling
/// back to `~/.codex` when the variable is not set.
pub fn codex_config_path() -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(codex_config_path_in(&home, &[]))
}

/// Home-injectable variant of [`codex_config_path_for_host_environment`]:
/// resolves the Codex config under the given `home` directory, honoring an
/// explicit `CODEX_HOME` in `host_env` (or the AoE process env), then
/// falling back to `<home>/.codex/config.toml`.
pub(crate) fn codex_config_path_in(home: &Path, host_env: &[String]) -> PathBuf {
    if let Some(codex_home) =
        crate::session::environment::resolve_host_environment_value(host_env, "CODEX_HOME")
            .or_else(|| std::env::var("CODEX_HOME").ok())
            .filter(|v| !v.is_empty())
    {
        return PathBuf::from(codex_home).join("config.toml");
    }
    home.join(".codex").join("config.toml")
}

pub fn codex_config_path_display() -> String {
    std::env::var("CODEX_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(|codex_home| {
            PathBuf::from(codex_home)
                .join("config.toml")
                .display()
                .to_string()
        })
        .unwrap_or_else(|| "~/.codex/config.toml".to_string())
}

// `hooks.json` is the production target for codex hooks; these
// `config.toml` path helpers stay as test scaffolding for the
// empty-`CODEX_HOME` fallback assertions in this module's unit tests.
#[cfg(test)]
pub(crate) fn codex_config_path_for_host_environment(entries: &[String]) -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(codex_config_path_in(&home, entries))
}

#[cfg(test)]
pub(crate) fn codex_config_path_display_for_host_environment(entries: &[String]) -> String {
    crate::session::environment::resolve_host_environment_value(entries, "CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(|codex_home| {
            PathBuf::from(codex_home)
                .join("config.toml")
                .display()
                .to_string()
        })
        .unwrap_or_else(codex_config_path_display)
}

/// Home-injectable variant of [`codex_hooks_json_path_for_host_environment`]:
/// resolves Codex's `hooks.json` under the given `home` directory, honoring
/// an explicit `CODEX_HOME` in `host_env` (or the AoE process env), then
/// falling back to `<home>/.codex/hooks.json`.
pub(crate) fn codex_hooks_json_path_in(home: &Path, host_env: &[String]) -> PathBuf {
    if let Some(codex_home) =
        crate::session::environment::resolve_host_environment_value(host_env, "CODEX_HOME")
            .or_else(|| std::env::var("CODEX_HOME").ok())
            .filter(|v| !v.is_empty())
    {
        return PathBuf::from(codex_home).join("hooks.json");
    }
    home.join(".codex").join("hooks.json")
}

/// Process-env variant of [`codex_hooks_json_path_in`]: resolves Codex's
/// `hooks.json` against the live `dirs::home_dir()` so install paths
/// outside the migration boot loop share a single call shape.
pub(crate) fn codex_hooks_json_path_for_host_environment(entries: &[String]) -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(codex_hooks_json_path_in(&home, entries))
}

/// Display-string variant of [`codex_hooks_json_path_for_host_environment`]
/// used by the TUI consent dialog; falls back to the literal
/// `~/.codex/hooks.json` string when the home directory cannot be resolved.
pub(crate) fn codex_hooks_json_path_display_for_host_environment(entries: &[String]) -> String {
    crate::session::environment::resolve_host_environment_value(entries, "CODEX_HOME")
        .filter(|v| !v.is_empty())
        .map(|codex_home| {
            PathBuf::from(codex_home)
                .join("hooks.json")
                .display()
                .to_string()
        })
        .unwrap_or_else(|| {
            std::env::var("CODEX_HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|codex_home| {
                    PathBuf::from(codex_home)
                        .join("hooks.json")
                        .display()
                        .to_string()
                })
                .unwrap_or_else(|| "~/.codex/hooks.json".to_string())
        })
}

/// Resolve the host settings-file path for an agent whose config directory may
/// be overridden by an environment variable (e.g. Claude's `CLAUDE_CONFIG_DIR`).
///
/// When the agent declares a `config_dir_env_var` and that variable is set in
/// the session's host environment (or, failing that, in AoE's own process env),
/// the settings file lives directly under that directory using the basename of
/// `settings_rel_path` (the env var replaces the whole `~/.claude`-style dir,
/// matching how the agents themselves interpret it). Otherwise it falls back to
/// the home-relative `settings_rel_path`.
pub(crate) fn agent_settings_path_for_host_environment(
    hook_cfg: &crate::agents::AgentHookConfig,
    host_env: &[String],
) -> Result<PathBuf> {
    let home =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?;
    Ok(agent_settings_path_in(&home, hook_cfg, host_env))
}

/// Home-injectable variant of [`agent_settings_path_for_host_environment`].
/// Resolves to `<config_dir_env>/<basename>` when the agent's config-dir env
/// var is set in `host_env` (or the AoE process env), otherwise to
/// `<home>/<settings_rel_path>`.
pub(crate) fn agent_settings_path_in(
    home: &Path,
    hook_cfg: &crate::agents::AgentHookConfig,
    host_env: &[String],
) -> PathBuf {
    if let Some(var) = hook_cfg.config_dir_env_var {
        if let Some(dir) = resolve_config_dir_override(var, host_env) {
            if let Some(file) = Path::new(hook_cfg.settings_rel_path).file_name() {
                return PathBuf::from(dir).join(file);
            }
        }
    }
    home.join(hook_cfg.settings_rel_path)
}

/// Display variant of [`agent_settings_path_for_host_environment`] for UI
/// consent dialogs. Returns the absolute override path when a config-dir env
/// var is set, otherwise the `~/`-relative default so the displayed path
/// matches where hooks are actually written.
pub(crate) fn agent_settings_path_display_for_host_environment(
    hook_cfg: &crate::agents::AgentHookConfig,
    host_env: &[String],
) -> String {
    if let Some(var) = hook_cfg.config_dir_env_var {
        if let Some(dir) = resolve_config_dir_override(var, host_env) {
            if let Some(file) = Path::new(hook_cfg.settings_rel_path).file_name() {
                return PathBuf::from(dir).join(file).display().to_string();
            }
        }
    }
    format!("~/{}", hook_cfg.settings_rel_path)
}

/// Resolve a config-dir override env var, preferring an explicit value in the
/// session's host environment list and falling back to AoE's own env so a var
/// exported in the shell that launched `aoe` (and thus inherited by the agent)
/// is honored too. Empty values are treated as unset.
fn resolve_config_dir_override(var: &str, host_env: &[String]) -> Option<String> {
    crate::session::environment::resolve_host_environment_value(host_env, var)
        .or_else(|| std::env::var(var).ok())
        .filter(|v| !v.is_empty())
}

/// Build the shell command for a hook that writes a status value.
///
/// The command must never exit non-zero, otherwise the agent treats the hook
/// as a blocking failure and refuses to run further tool calls. Every reject
/// path is `exit 0`; at worst the status file is one tick stale and the next
/// hook call recovers.
///
/// Per `HookInstallTarget`:
/// - `Host`: bakes `/tmp/aoe-hooks-<euid>` (per-user) and adds an
///   `id -u` ownership self-check (defence-in-depth, the Rust-side
///   `dir_guard` is the authoritative gate).
/// - `Sandbox`: bakes `/tmp/aoe-hooks` (fixed inside the container; the
///   host bind-mounts `/tmp/aoe-hooks-<euid>/<id>` -> `/tmp/aoe-hooks/<id>`)
///   and drops the uid check because the in-container UID is unpredictable
///   and the bind-mount source has already been validated host-side.
///
/// Both variants share the SELinux/ACL/xattr-tolerant mode pattern
/// (`d*------|d*------.|d*------+|d*------@`) and an environment-pinning
/// preamble (`unset IFS; set -f; umask 077; LC_ALL=C ls -ldn`). The
/// `set -f` glob disable closes the `set -- $LS` pathname-expansion vector
/// (no field could expand today, but a future change to the path format
/// could re-expose it).
fn hook_command(status: &str, target: HookInstallTarget) -> String {
    let base = match target {
        HookInstallTarget::Host => dir_guard::hook_base_path().display().to_string(),
        HookInstallTarget::Sandbox => HOOK_STATUS_BASE_IN_CONTAINER.to_string(),
    };
    hook_command_with_base(status, &base, target)
}

#[cfg(test)]
pub(crate) fn canonical_status_command(status: &str, target: HookInstallTarget) -> String {
    hook_command(status, target)
}

fn hook_command_with_base(status: &str, base: &str, target: HookInstallTarget) -> String {
    let parent_check = match target {
        HookInstallTarget::Host => {
            // mkdir -p $B is the wipe-recovery primitive for the
            // systemd-tmpfiles / manual /tmp reaper case. Safe because:
            //   - umask 077 (set above) makes a created $B mode 0700.
            //   - /tmp has the sticky bit, so cross-uid attackers cannot
            //     rename or unlink $B once we own it.
            //   - The LS check that follows still rejects squatted bases
            //     (mode != drwx------ or wrong owner uid), so mkdir -p
            //     hitting an existing-but-hostile dir does not lower the
            //     bar.
            // If the base ever moves outside /tmp (e.g., honoring
            // XDG_RUNTIME_DIR), this snippet must be re-audited because it
            // relies on the sticky-bit invariant of the parent.
            "\
             mkdir -p \"$B\" 2>/dev/null || exit 0; \
             LS=$(LC_ALL=C ls -ldn \"$B\" 2>/dev/null) || exit 0; \
             set -- $LS; M=\"$1\"; \
             case \"$M\" in drwx------|drwx------.|drwx------+|drwx------@) ;; *) exit 0 ;; esac; \
             ME=$(id -u 2>/dev/null) || exit 0; \
             [ \"$3\" = \"$ME\" ] || exit 0; "
        }
        HookInstallTarget::Sandbox => "",
    };
    let owner_recheck = match target {
        HookInstallTarget::Host => "[ \"$3\" = \"$ME\" ] || exit 0; ",
        HookInstallTarget::Sandbox => "",
    };
    format!(
        "sh -c 'unset IFS; set -f; umask 077; \
         [ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
         B={base}; {parent_check}\
         D=\"$B/$AOE_INSTANCE_ID\"; \
         mkdir -p \"$D\" 2>/dev/null; \
         LS=$(LC_ALL=C ls -ldn \"$D\" 2>/dev/null) || exit 0; \
         set -- $LS; M=\"$1\"; \
         case \"$M\" in drwx------|drwx------.|drwx------+|drwx------@) ;; *) exit 0 ;; esac; \
         {owner_recheck}\
         printf {status} > \"$D/status\" 2>/dev/null; \
         exit 0 # {AOE_HOOK_MARKER}'"
    )
}

/// Build the shell command for a hook that extracts `session_id` from the
/// agent's stdin JSON payload and writes it to a sidecar file.
///
/// Both variants must exit 0 even on failure: a non-zero hook blocks the
/// agent's tool calls. Both variants end with the trailing
/// `# AOE_HOOK_MARKER` shell comment so [`is_aoe_hook_command`] recognises
/// them via the anchored `# aoe-hooks` sentinel; the sandbox variant
/// additionally carries the legacy `aoe-hooks/$AOE_INSTANCE_ID` path
/// substring so hooks installed before the trailing marker was added stay
/// detectable on uninstall.
///
/// Host-variant silent-failure modes (acceptable, equivalent to a `jq`
/// absence in the sandbox variant): `aoe` not on PATH at hook-exec time,
/// or a stale `aoe` on PATH that predates `__extract-session-id`. Both
/// yield no sidecar without surfacing an error; session resume falls
/// back to the filesystem scan.
fn hook_command_session_id(target: HookInstallTarget) -> String {
    match target {
        HookInstallTarget::Host => hook_command_session_id_host(),
        HookInstallTarget::Sandbox => {
            hook_command_session_id_sandbox(HOOK_STATUS_BASE_IN_CONTAINER)
        }
    }
}

/// Test-only sibling of [`canonical_status_command`] for the
/// `session_id_capture` branch, which emits a different shape than
/// `hook_command(status)` (no `case` allowlist guard, just a trailing
/// `# aoe-hooks` marker).
#[cfg(test)]
pub(crate) fn canonical_session_id_command(target: HookInstallTarget) -> String {
    hook_command_session_id(target)
}

fn hook_command_session_id_host() -> String {
    format!(
        "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         command -v aoe >/dev/null 2>&1 || exit 0; \
         aoe __extract-session-id 2>/dev/null; exit 0 # {AOE_HOOK_MARKER}'"
    )
}

fn hook_command_session_id_sandbox(base: &str) -> String {
    // `jq -r` does the structural top-level extraction symmetric with the
    // host's `serde_json` path. POSIX `case` enforces the UUID shape after
    // jq returns, because `case` has no `{N}` quantifier we expand the
    // 8-4-4-4-12 layout literally over the `$H` hex-class abbreviation.
    // The `'\''` close-escape-reopen is the standard shell idiom for a
    // literal single-quote inside a single-quoted `sh -c '...'`; in Rust
    // source that is `'\\''`.
    format!(
        "sh -c 'unset IFS; set -f; umask 077; \
         [ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
         D=\"{base}/$AOE_INSTANCE_ID\"; mkdir -p \"$D\" 2>/dev/null; \
         LS=$(LC_ALL=C ls -ldn \"$D\" 2>/dev/null) || exit 0; \
         set -- $LS; M=\"$1\"; \
         case \"$M\" in drwx------|drwx------.|drwx------+|drwx------@) ;; *) exit 0 ;; esac; \
         command -v jq >/dev/null 2>&1 || exit 0; \
         SID=$(jq -r '\\''if (.session_id|type)==\"string\" then .session_id else empty end'\\'' 2>/dev/null); \
         H=[0-9a-fA-F]; \
         case \"$SID\" in $H$H$H$H$H$H$H$H-$H$H$H$H-$H$H$H$H-$H$H$H$H-$H$H$H$H$H$H$H$H$H$H$H$H) ;; *) exit 0 ;; esac; \
         printf \"%s\" \"$SID\" > \"$D/.session_id.$$.tmp\" 2>/dev/null && mv \"$D/.session_id.$$.tmp\" \"$D/session_id\" 2>/dev/null; \
         exit 0 # {AOE_HOOK_MARKER}'"
    )
}

/// Recognises an AoE-managed hook command. Accepts iff one of two structural
/// sentinels is present:
///
/// - [`AOE_HOOK_TRAILING_SENTINEL`] (`0 # aoe-hooks`) appears in trailing
///   position. Anchored: the predicate strips trailing whitespace and the
///   shell-string terminators `'` / `"` produced by the `sh -c '…'` wrapper,
///   then requires the sentinel at the end. The leading `0 ` digit binds the
///   match to the canonical `exit 0 # AOE_HOOK_MARKER` trailer baked by every
///   shipping emitter, ruling out incidental `# aoe-hooks` matches inside
///   echo arguments, quoted strings, or user-written trailing comments.
/// - [`AOE_HOOK_PATH_SENTINEL`] (`aoe-hooks/$AOE_INSTANCE_ID`) appears
///   anywhere. Substring match: the un-expanded `$AOE_INSTANCE_ID` is
///   user-collision-proof (a real script would have already expanded the
///   variable). This sentinel is load-bearing for backwards-compat with
///   legacy emitter forms that have no trailing-comment marker: the
///   pre-#2168 `hook_command_with_base` (host status) bakes
///   `mkdir -p "/tmp/aoe-hooks/$AOE_INSTANCE_ID"` in the body, and the
///   sandbox session-id command bakes `D="/tmp/aoe-hooks/$AOE_INSTANCE_ID"`.
///
/// Residual false-positive surface (accepted): a user hook whose stored
/// command literally contains the un-expanded `$AOE_INSTANCE_ID` reference
/// adjacent to `aoe-hooks/`. The out-of-band marker scheme proposed in
/// issue #2191 (Option B) is the future-proof fix.
pub(super) fn is_aoe_hook_command(cmd: &str) -> bool {
    let trimmed_tail = cmd.trim_end_matches(|c: char| c == '\'' || c == '"' || c.is_whitespace());
    trimmed_tail.ends_with(AOE_HOOK_TRAILING_SENTINEL) || cmd.contains(AOE_HOOK_PATH_SENTINEL)
}

/// Build the AoE hooks JSON structure from agent-defined events.
///
/// For each event, emit one entry per active behaviour:
/// - `event.session_id_capture` → session-id-extractor command (placed
///   first so it gets stdin first if the agent only delivers stdin to the
///   leading command in a matcher block).
/// - `event.status.is_some()` → status-writer command (does not read
///   stdin).
///
/// An event with both produces two `hooks` array entries under the same
/// matcher block. An event with neither is skipped.
///
/// Multiple events may share a `name` with different matchers (e.g. Claude's
/// `Notification` splits `permission_prompt|elicitation_dialog` → waiting from
/// `idle_prompt` → idle). Each becomes its own matcher block appended to that
/// event name's array, so they coexist instead of the later one clobbering the
/// earlier.
fn build_aoe_hooks(
    events: impl AsRef<[crate::agents::ResolvedHookEvent]>,
    target: HookInstallTarget,
) -> Value {
    let mut hooks_obj = serde_json::Map::new();
    for event in events.as_ref() {
        let mut commands: Vec<String> = Vec::new();
        if event.session_id_capture {
            commands.push(hook_command_session_id(target));
        }
        if let Some(status) = event.status {
            commands.push(hook_command(status.as_str(), target));
        }
        if commands.is_empty() {
            continue;
        }

        let mut entry = serde_json::Map::new();
        if let Some(m) = &event.matcher {
            entry.insert("matcher".to_string(), Value::String(m.clone()));
        }
        let hook_entries: Vec<Value> = commands
            .into_iter()
            .map(|cmd| {
                serde_json::json!({
                    "type": "command",
                    "command": cmd,
                })
            })
            .collect();
        entry.insert("hooks".to_string(), Value::Array(hook_entries));
        match hooks_obj
            .entry(event.name.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            Value::Array(groups) => groups.push(Value::Object(entry)),
            // or_insert_with only ever seeds an Array, so this arm is unreachable.
            _ => unreachable!("hook event group is always a JSON array"),
        }
    }

    Value::Object(hooks_obj)
}

/// Drop a matcher group only when EVERY hook in it is AoE-marked. A
/// hand-merged user+AoE group is left intact, so a stale pre-#1803 AoE
/// entry inside such a group is NOT rewritten by v015. Distinguishing
/// "legacy AoE in a mixed group" from "user copy-pasted an AoE-shaped
/// hook" would need historical-bytes comparison; PR #1803's launch-gate
/// validates `AOE_INSTANCE_ID` before any hook fires, so the legacy
/// command's blast radius is bounded. Locked by
/// `mixed_user_aoe_matcher_group_documents_double_firing` in v015 tests.
fn remove_aoe_entries(matchers: &mut Vec<Value>) {
    matchers.retain(|matcher| {
        let Some(hooks_arr) = matcher.get("hooks").and_then(|h| h.as_array()) else {
            return true;
        };
        // Keep the matcher group only if it has at least one non-AoE hook
        !hooks_arr.iter().all(|hook| {
            hook.get("command")
                .and_then(|c| c.as_str())
                .is_some_and(is_aoe_hook_command)
        })
    });
}

/// Install AoE status hooks into an agent's `settings.json` file.
///
/// Merges AoE hook entries into the existing hooks configuration, preserving
/// any user-defined hooks. Existing AoE hooks are replaced (idempotent).
///
/// Idempotent on disk: when the on-disk file already encodes the same
/// AoE-managed hook subtree, it is not rewritten (same inode, same bytes).
///
/// If the file doesn't exist, it will be created with just the hooks.
pub fn install_hooks(
    settings_path: &Path,
    events: impl AsRef<[crate::agents::ResolvedHookEvent]>,
    target: HookInstallTarget,
) -> Result<()> {
    with_config_lock(settings_path, "json.lock", || {
        let mut settings: Value = if settings_path.exists() {
            let content = std::fs::read_to_string(settings_path)?;
            serde_json::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(target: "hooks.install", "Failed to parse {}: {}", settings_path.display(), e);
                serde_json::json!({})
            })
        } else {
            serde_json::json!({})
        };

        let before = settings.clone();

        let aoe_hooks = build_aoe_hooks(events.as_ref(), target);

        if !settings.get("hooks").is_some_and(|h| h.is_object()) {
            settings
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("Settings file root is not a JSON object"))?
                .insert("hooks".to_string(), serde_json::json!({}));
        }

        let settings_hooks = settings
            .get_mut("hooks")
            .and_then(|h| h.as_object_mut())
            .ok_or_else(|| anyhow::anyhow!("hooks key is not a JSON object"))?;

        let aoe_hooks_obj = aoe_hooks
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Internal error: built hooks is not a JSON object"))?;
        let empty_events: Vec<String> = settings_hooks
            .iter_mut()
            .filter_map(|(event_name, existing)| {
                let arr = existing.as_array_mut()?;
                remove_aoe_entries(arr);
                arr.is_empty().then(|| event_name.clone())
            })
            .collect();
        for event_name in empty_events {
            settings_hooks.remove(&event_name);
        }

        for (event_name, aoe_matchers) in aoe_hooks_obj {
            if let Some(existing) = settings_hooks
                .get_mut(event_name)
                .and_then(|existing| existing.as_array_mut())
            {
                if let Some(new_arr) = aoe_matchers.as_array() {
                    existing.extend(new_arr.iter().cloned());
                }
            } else {
                settings_hooks.insert(event_name.clone(), aoe_matchers.clone());
            }
        }

        if settings == before {
            tracing::debug!(target: "hooks.install",
                "AoE hooks in {} already up to date; skipping write",
                settings_path.display());
            return Ok(());
        }

        if let Some(parent) = settings_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let formatted = serde_json::to_string_pretty(&settings)?;
        crate::session::atomic_write_following_symlinks(settings_path, formatted.as_bytes())?;

        tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", settings_path.display());
        Ok(())
    })
}

pub(super) const CODEX_HOOK_EVENT_NAMES: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
    "PreCompact",
    "PostCompact",
];

/// Install AoE status hooks into Codex's `config.toml`.
///
/// Codex also stores hook trust state in this file. Keep every AoE mutation
/// behind the lock and atomic replace below so repeated launches cannot leave
/// duplicated hook blocks or torn TOML.
///
/// Idempotent on disk: when the on-disk file already encodes the same
/// AoE-managed hook subtree, it is not rewritten (same inode, same bytes).
#[cfg(test)]
pub(crate) fn install_codex_hooks(
    config_path: &Path,
    events: impl AsRef<[crate::agents::ResolvedHookEvent]>,
    target: HookInstallTarget,
) -> Result<()> {
    install_codex_hooks_with_preserved_state(config_path, events, None, target)
}

/// Read `[hooks.state]` from `config_path` under the codex config lock and
/// return it for later restoration. Inverse of [`restore_codex_hooks_state`];
/// returns `Ok(None)` when the file is absent so callers can no-op the
/// snapshot/restore bracket.
pub(crate) fn snapshot_codex_hooks_state(config_path: &Path) -> Result<Option<toml_edit::Item>> {
    if !config_path.exists() {
        return Ok(None);
    }

    with_codex_config_lock(config_path, || {
        let config = read_codex_config(config_path)?;
        Ok(config
            .get("hooks")
            .and_then(|hooks| hooks.as_table_like())
            .and_then(|hooks| hooks.get("state"))
            .cloned())
    })
}

/// Write `state` back into the `[hooks.state]` table of `config_path`,
/// taking the codex config lock. Inverse of [`snapshot_codex_hooks_state`];
/// together they bracket destructive rewrites of `config.toml` (e.g. the
/// host-to-sandbox refresh in [`crate::session::container_config`]) so
/// Codex's user-trust block survives. Unconditionally overwrites any
/// `state` already present at `config_path`; pair only with a fresh
/// snapshot from the authoritative source.
pub(crate) fn restore_codex_hooks_state(config_path: &Path, state: toml_edit::Item) -> Result<()> {
    with_codex_config_lock(config_path, || {
        let mut config = read_codex_config(config_path)?;
        let hooks = ensure_codex_hooks_table(&mut config)?;
        hooks.insert("state", state);
        write_codex_config(config_path, &config)?;
        Ok(())
    })
}

pub(crate) fn install_codex_hooks_with_preserved_state(
    config_path: &Path,
    events: impl AsRef<[crate::agents::ResolvedHookEvent]>,
    preserved_state: Option<toml_edit::Item>,
    target: HookInstallTarget,
) -> Result<()> {
    with_codex_config_lock(config_path, || {
        let mut config = read_codex_config(config_path)?;
        if codex_hooks_feature_is_disabled(&config, config_path) {
            return Ok(());
        }

        let before = config.to_string();

        if let Some(state) = preserved_state {
            let hooks = ensure_codex_hooks_table(&mut config)?;
            if !hooks.contains_key("state") {
                hooks.insert("state", state);
            }
        }
        remove_codex_aoe_hooks(&mut config)?;
        merge_codex_hooks(&mut config, events.as_ref(), target)?;

        if config.to_string() == before {
            tracing::debug!(target: "hooks.install",
                "AoE hooks in {} already up to date; skipping write",
                config_path.display());
            return Ok(());
        }

        write_codex_config(config_path, &config)?;
        tracing::info!(target: "hooks.install",
            "Installed AoE hooks in {}", config_path.display());
        Ok(())
    })
}

fn with_codex_config_lock<T>(config_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_base_path = crate::session::resolve_symlink_chain(config_path)?;
    with_config_lock(&lock_base_path, "toml.lock", f)
}

/// Generic advisory-lock helper for hook settings files. Holds an exclusive
/// `flock` on `<path>.<lock_extension>` while `f` runs, so concurrent
/// installers (typical pattern: two `aoe` instances booting at the same
/// time) cannot interleave a stale read with a fresh write on the same
/// settings file. Lock-on-error releases via the match arm below.
fn with_config_lock<T>(
    path: &Path,
    lock_extension: &str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock_path = path.with_extension(lock_extension);
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&lock_path)
        .with_context(|| format!("Failed to open config lock {}", lock_path.display()))?;

    lock_file
        .lock_exclusive()
        .with_context(|| format!("Failed to lock config {}", path.display()))?;

    let result = f();
    let unlock_result = fs2::FileExt::unlock(&lock_file);
    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => {
            Err(error).with_context(|| format!("Failed to unlock {}", lock_path.display()))
        }
    }
}

fn write_codex_config(config_path: &Path, config: &toml_edit::DocumentMut) -> Result<()> {
    crate::session::atomic_write_following_symlinks(config_path, config.to_string().as_bytes())
}

fn read_codex_config(config_path: &Path) -> Result<toml_edit::DocumentMut> {
    if config_path.exists() {
        let content = std::fs::read_to_string(config_path)?;
        content
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("Failed to parse {}", config_path.display()))
    } else {
        Ok(toml_edit::DocumentMut::new())
    }
}

fn ensure_codex_hooks_table(config: &mut toml_edit::DocumentMut) -> Result<&mut toml_edit::Table> {
    let root = config.as_table_mut();
    if !root.contains_key("hooks") {
        root.insert("hooks", toml_edit::Item::Table(toml_edit::Table::new()));
    }

    let hooks_item = root
        .get_mut("hooks")
        .ok_or_else(|| anyhow::anyhow!("hooks key was not created"))?;
    if !hooks_item.is_table() {
        let old_item = std::mem::take(hooks_item);
        match old_item.into_table() {
            Ok(table) => {
                *hooks_item = toml_edit::Item::Table(table);
            }
            Err(old_item) => {
                *hooks_item = old_item;
                return Err(anyhow::anyhow!("Codex hooks key is not a TOML table"));
            }
        }
    }

    hooks_item
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("Codex hooks key is not a TOML table"))
}

fn ensure_codex_event_array<'a>(
    hooks: &'a mut toml_edit::Table,
    event_name: &str,
) -> Result<&'a mut toml_edit::ArrayOfTables> {
    if !hooks.contains_key(event_name) {
        hooks.insert(
            event_name,
            toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
        );
    }

    let event_item = hooks
        .get_mut(event_name)
        .ok_or_else(|| anyhow::anyhow!("hooks.{event_name} was not created"))?;
    if !event_item.is_array_of_tables() {
        if event_item.as_array().is_some_and(|arr| arr.is_empty()) {
            *event_item = toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new());
        } else {
            let old_item = std::mem::take(event_item);
            match old_item.into_array_of_tables() {
                Ok(array) => {
                    *event_item = toml_edit::Item::ArrayOfTables(array);
                }
                Err(old_item) => {
                    *event_item = old_item;
                    return Err(anyhow::anyhow!(
                        "Codex hooks.{event_name} is not an array of matcher groups"
                    ));
                }
            }
        }
    }

    event_item.as_array_of_tables_mut().ok_or_else(|| {
        anyhow::anyhow!("Codex hooks.{event_name} is not an array of matcher groups")
    })
}

fn merge_codex_hooks(
    config: &mut toml_edit::DocumentMut,
    events: &[crate::agents::ResolvedHookEvent],
    target: HookInstallTarget,
) -> Result<()> {
    let hooks = ensure_codex_hooks_table(config)?;

    for event in events {
        let Some(status) = event.status else {
            continue;
        };

        let event_array = ensure_codex_event_array(hooks, &event.name)?;
        event_array.push(codex_matcher_group(event, status, target));
    }

    Ok(())
}

fn codex_matcher_group(
    event: &crate::agents::ResolvedHookEvent,
    status: crate::agents::HookStatus,
    target: HookInstallTarget,
) -> toml_edit::Table {
    let mut group = toml_edit::Table::new();
    if let Some(matcher) = &event.matcher {
        group.insert("matcher", toml_edit::value(matcher.as_str()));
    }

    let mut handler = toml_edit::Table::new();
    handler.insert("type", toml_edit::value("command"));
    handler.insert(
        "command",
        toml_edit::value(hook_command(status.as_str(), target)),
    );

    let mut handlers = toml_edit::ArrayOfTables::new();
    handlers.push(handler);
    group.insert("hooks", toml_edit::Item::ArrayOfTables(handlers));
    group
}

fn remove_codex_aoe_hooks(config: &mut toml_edit::DocumentMut) -> Result<bool> {
    let Some(hooks_item) = config.as_table_mut().get_mut("hooks") else {
        return Ok(false);
    };
    let Some(hooks_table) = hooks_item.as_table_like_mut() else {
        return Err(anyhow::anyhow!("Codex hooks key is not a TOML table"));
    };

    let mut modified = false;
    for event_name in CODEX_HOOK_EVENT_NAMES {
        let Some(event_item) = hooks_table.get_mut(event_name) else {
            continue;
        };

        if let Some(matchers) = event_item.as_array_of_tables_mut() {
            let before = matchers.len();
            matchers.retain(|matcher| !codex_matcher_group_is_all_aoe(matcher));
            if matchers.len() != before {
                modified = true;
            }
            if matchers.is_empty() {
                hooks_table.remove(event_name);
            }
        } else if let Some(matchers) = event_item.as_array_mut() {
            let before = matchers.len();
            matchers.retain(|matcher| !codex_inline_matcher_group_is_all_aoe(matcher));
            if matchers.len() != before {
                modified = true;
            }
            if matchers.is_empty() {
                hooks_table.remove(event_name);
            }
        }
    }

    let remove_hooks_table = config
        .as_table()
        .get("hooks")
        .and_then(|item| item.as_table_like())
        .is_some_and(|hooks| hooks.is_empty());
    if remove_hooks_table {
        config.as_table_mut().remove("hooks");
        modified = true;
    }

    Ok(modified)
}

fn codex_matcher_group_is_all_aoe(group: &toml_edit::Table) -> bool {
    let Some(hooks_item) = group.get("hooks") else {
        return false;
    };

    if let Some(handlers) = hooks_item.as_array_of_tables() {
        return !handlers.is_empty()
            && handlers
                .iter()
                .all(|handler| codex_toml_table_command(handler).is_some_and(is_aoe_hook_command));
    }

    if let Some(handlers) = hooks_item.as_array() {
        return !handlers.is_empty() && handlers.iter().all(codex_inline_hook_handler_is_aoe);
    }

    false
}

fn codex_inline_matcher_group_is_all_aoe(group: &toml_edit::Value) -> bool {
    let Some(group) = group.as_inline_table() else {
        return false;
    };
    let Some(hooks_item) = group.get("hooks") else {
        return false;
    };
    let Some(handlers) = hooks_item.as_array() else {
        return false;
    };

    !handlers.is_empty() && handlers.iter().all(codex_inline_hook_handler_is_aoe)
}

pub(super) fn codex_inline_hook_handler_is_aoe(handler: &toml_edit::Value) -> bool {
    handler
        .as_inline_table()
        .and_then(|handler| codex_toml_table_command(handler))
        .is_some_and(is_aoe_hook_command)
}

pub(super) fn codex_toml_table_command(table: &dyn toml_edit::TableLike) -> Option<&str> {
    table.get("command").and_then(toml_edit::Item::as_str)
}

fn codex_hooks_feature_is_disabled(config: &toml_edit::DocumentMut, config_path: &Path) -> bool {
    let disabled = config
        .get("features")
        .and_then(|features| {
            let features = features.as_table_like()?;
            features
                .get("hooks")
                .or_else(|| features.get("codex_hooks"))
        })
        .and_then(toml_edit::Item::as_bool)
        .is_some_and(|enabled| !enabled);

    if disabled {
        tracing::warn!(target: "hooks.install",
            "Codex hooks are explicitly disabled in {}; skipping AoE status hooks",
            config_path.display()
        );
    }

    disabled
}

/// Remove AoE status hooks from Codex's `config.toml`.
pub fn uninstall_codex_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }

    let modified = with_codex_config_lock(config_path, || {
        let mut config = read_codex_config(config_path)?;
        if !remove_codex_aoe_hooks(&mut config)? {
            return Ok(false);
        }

        write_codex_config(config_path, &config)?;
        Ok(true)
    })?;
    if modified {
        tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", config_path.display());
    }
    Ok(modified)
}

/// Remove all AoE hooks from an agent's `settings.json` file.
///
/// Strips AoE hook entries while preserving user-defined hooks. If an event
/// ends up with no matchers after removal, the event key is removed entirely.
/// If the hooks object becomes empty, the `hooks` key is removed from settings.
///
/// Returns `Ok(true)` if the file was modified, `Ok(false)` if no AoE hooks were found.
pub fn uninstall_hooks(settings_path: &Path) -> Result<bool> {
    if !settings_path.exists() {
        return Ok(false);
    }

    with_config_lock(settings_path, "json.lock", || {
        let content = std::fs::read_to_string(settings_path)?;
        let mut settings: Value = serde_json::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(target: "hooks.uninstall", "Failed to parse {}: {}", settings_path.display(), e);
            serde_json::json!({})
        });

        let Some(hooks_obj) = settings.get_mut("hooks").and_then(|h| h.as_object_mut()) else {
            return Ok(false);
        };

        let mut modified = false;
        let event_names: Vec<String> = hooks_obj.keys().cloned().collect();

        for event_name in event_names {
            if let Some(matchers) = hooks_obj
                .get_mut(&event_name)
                .and_then(|v| v.as_array_mut())
            {
                let before = matchers.len();
                remove_aoe_entries(matchers);
                if matchers.len() != before {
                    modified = true;
                }
            }
        }

        if !modified {
            return Ok(false);
        }

        let empty_events: Vec<String> = hooks_obj
            .iter()
            .filter(|(_, v)| v.as_array().is_some_and(|a| a.is_empty()))
            .map(|(k, _)| k.clone())
            .collect();
        for key in empty_events {
            hooks_obj.remove(&key);
        }

        if hooks_obj.is_empty() {
            if let Some(obj) = settings.as_object_mut() {
                obj.remove("hooks");
            }
        }

        let formatted = serde_json::to_string_pretty(&settings)?;
        crate::session::atomic_write_following_symlinks(settings_path, formatted.as_bytes())?;

        tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", settings_path.display());
        Ok(true)
    })
}

fn default_sidecar_events(agent_name: &str) -> Vec<crate::agents::ResolvedHookEvent> {
    let agent = crate::agents::get_agent(agent_name).expect("sidecar agent is registered");
    crate::agents::resolved_sidecar_hook_events(agent, &crate::session::config::Config::default())
        .expect("default sidecar hook events resolve")
}

/// Install AoE status hooks into a settl TOML config file (typically
/// `~/.settl/config.toml`).
///
/// settl uses TOML config with `[[hooks]]` array entries instead of JSON
/// settings files. This function reads the existing config, removes any
/// previous AoE-managed hooks (identified by the marker), and adds hooks
/// for the three status transitions: TurnStarted->running,
/// WaitingForHuman->waiting, GameWon->idle.
///
/// Idempotent on disk: when the on-disk file already encodes the same
/// AoE-managed hook subtree, it is not rewritten (same inode, same bytes).
pub fn install_settl_hooks(config_path: &Path, target: HookInstallTarget) -> Result<()> {
    let events = default_sidecar_events("settl");
    install_settl_hooks_with_events(config_path, target, &events)
}

pub fn install_settl_hooks_with_events(
    config_path: &Path,
    target: HookInstallTarget,
    events: &[crate::agents::ResolvedHookEvent],
) -> Result<()> {
    with_config_lock(config_path, "toml.lock", || {
        let mut config: toml::Value = if config_path.exists() {
            let content = std::fs::read_to_string(config_path)?;
            toml::from_str(&content).unwrap_or_else(|e| {
                tracing::warn!(target: "hooks.install", "Failed to parse {}: {}", config_path.display(), e);
                toml::Value::Table(toml::map::Map::new())
            })
        } else {
            toml::Value::Table(toml::map::Map::new())
        };

        let before = config.clone();

        let table = config
            .as_table_mut()
            .ok_or_else(|| anyhow::anyhow!("Config root is not a TOML table"))?;

        let hooks = table
            .entry("hooks")
            .or_insert_with(|| toml::Value::Array(Vec::new()));
        let hooks_arr = hooks
            .as_array_mut()
            .ok_or_else(|| anyhow::anyhow!("hooks key is not a TOML array"))?;

        hooks_arr.retain(|hook| {
            !hook
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(is_aoe_hook_command)
        });

        for event in events {
            let Some(status) = event.status else {
                continue;
            };
            let mut entry = toml::map::Map::new();
            entry.insert("event".into(), toml::Value::String(event.name.clone()));
            entry.insert(
                "command".into(),
                toml::Value::String(hook_command(status.as_str(), target)),
            );
            hooks_arr.push(toml::Value::Table(entry));
        }

        if config == before {
            tracing::debug!(target: "hooks.install",
                "AoE hooks in {} already up to date; skipping write",
                config_path.display());
            return Ok(());
        }

        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let formatted = toml::to_string_pretty(&config)?;
        crate::session::atomic_write_following_symlinks(config_path, formatted.as_bytes())?;

        tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", config_path.display());
        Ok(())
    })
}

/// Remove AoE hooks from a settl TOML config file (typically
/// `~/.settl/config.toml`).
pub fn uninstall_settl_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }

    with_config_lock(config_path, "toml.lock", || {
        let content = std::fs::read_to_string(config_path)?;
        let mut config: toml::Value = toml::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(target: "hooks.uninstall", "Failed to parse {}: {}", config_path.display(), e);
            toml::Value::Table(toml::map::Map::new())
        });

        let Some(hooks_arr) = config.get_mut("hooks").and_then(|h| h.as_array_mut()) else {
            return Ok(false);
        };

        let before = hooks_arr.len();
        hooks_arr.retain(|hook| {
            !hook
                .get("command")
                .and_then(|c| c.as_str())
                .is_some_and(is_aoe_hook_command)
        });

        if hooks_arr.len() == before {
            return Ok(false);
        }

        let formatted = toml::to_string_pretty(&config)?;
        crate::session::atomic_write_following_symlinks(config_path, formatted.as_bytes())?;
        tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", config_path.display());
        Ok(true)
    })
}

/// Install AoE status hooks into Hermes's `config.yaml`.
///
/// Reads the existing YAML, removes any prior AoE-managed hook entries
/// (identified by the `aoe-hooks` marker in the command string), and inserts
/// our status-writing hooks under the configured events. Also pre-populates
/// `<config_dir>/shell-hooks-allowlist.json` so Hermes registers the hooks
/// without prompting for first-use consent.
///
/// **Atomicity caveat.** The two writes (config.yaml then allowlist.json)
/// are sequential, not atomic. The `with_config_lock` wrapper around this
/// function eliminates the cross-process interleaving leg of the caveat
/// (two `aoe` processes can no longer race on the pair); only an
/// in-process crash between the two writes can still leave config.yaml
/// in the hardened shape with the allowlist not yet updated. Hermes
/// itself tolerates a missing/stale allowlist by re-prompting for
/// consent, which is recoverable. Hardening to atomic-write across both
/// files is tracked as a follow-up.
///
/// Idempotent on disk: when the YAML config and the allowlist already
/// encode the same AoE state, neither is rewritten (same inode, same bytes).
pub fn install_hermes_hooks(config_path: &Path, target: HookInstallTarget) -> Result<()> {
    let events = default_sidecar_events("hermes");
    install_hermes_hooks_with_events(config_path, target, &events)
}

pub fn install_hermes_hooks_with_events(
    config_path: &Path,
    target: HookInstallTarget,
    events: &[crate::agents::ResolvedHookEvent],
) -> Result<()> {
    with_config_lock(config_path, "yaml.lock", || {
        let mut config: serde_yaml::Value = if config_path.exists() {
            let content = std::fs::read_to_string(config_path)?;
            if content.trim().is_empty() {
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
            } else {
                serde_yaml::from_str(&content)
                    .with_context(|| format!("Failed to parse {}", config_path.display()))?
            }
        } else {
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new())
        };

        let yaml_before = config.clone();

        let root = config
            .as_mapping_mut()
            .ok_or_else(|| anyhow::anyhow!("Hermes config root is not a YAML mapping"))?;

        let hooks_key = serde_yaml::Value::String("hooks".to_string());
        let hooks_value = root
            .entry(hooks_key.clone())
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
        if !hooks_value.is_mapping() {
            *hooks_value = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        }
        let hooks_map = hooks_value.as_mapping_mut().expect("ensured mapping above");

        for event in events {
            let Some(status) = event.status else {
                continue;
            };
            let event_key = serde_yaml::Value::String(event.name.clone());
            let entries = hooks_map
                .entry(event_key)
                .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));
            if !entries.is_sequence() {
                *entries = serde_yaml::Value::Sequence(Vec::new());
            }
            let arr = entries.as_sequence_mut().expect("ensured sequence above");

            arr.retain(|hook| {
                !hook
                    .as_mapping()
                    .and_then(|m| m.get(serde_yaml::Value::String("command".into())))
                    .and_then(|c| c.as_str())
                    .is_some_and(is_aoe_hook_command)
            });

            let mut entry = serde_yaml::Mapping::new();
            entry.insert(
                serde_yaml::Value::String("command".into()),
                serde_yaml::Value::String(hook_command(status.as_str(), target)),
            );
            arr.push(serde_yaml::Value::Mapping(entry));
        }

        let yaml_changed = config != yaml_before;

        let config_dir = config_path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
        let (allowlist_path, allowlist_formatted) =
            render_hermes_allowlist(config_dir, target, events)?;
        // Byte-compare (vs the structural YAML compare above) is sound:
        // render_hermes_allowlist preserves approved_at on (event, command)
        // collision and serde_json::to_string_pretty is deterministic, so
        // a clean reinstall is byte-identical from the second install onward.
        let allowlist_changed = if allowlist_path.exists() {
            std::fs::read(&allowlist_path)? != allowlist_formatted.as_bytes()
        } else {
            true
        };

        if !yaml_changed && !allowlist_changed {
            tracing::debug!(target: "hooks.install",
                "AoE hooks in {} already up to date; skipping write",
                config_path.display());
            return Ok(());
        }

        if yaml_changed {
            if let Some(parent) = config_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let formatted = serde_yaml::to_string(&config)?;
            crate::session::atomic_write_following_symlinks(config_path, formatted.as_bytes())?;
        }

        if allowlist_changed {
            if let Some(parent) = allowlist_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            crate::session::atomic_write_following_symlinks(
                &allowlist_path,
                allowlist_formatted.as_bytes(),
            )?;
        }

        tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", config_path.display());
        Ok(())
    })
}

/// Remove AoE hooks from Hermes's `config.yaml`.
pub fn uninstall_hermes_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }

    with_config_lock(config_path, "yaml.lock", || {
        let content = std::fs::read_to_string(config_path)?;
        let mut config: serde_yaml::Value = if content.trim().is_empty() {
            return Ok(false);
        } else {
            serde_yaml::from_str(&content)
                .with_context(|| format!("Failed to parse {}", config_path.display()))?
        };

        let Some(root) = config.as_mapping_mut() else {
            return Ok(false);
        };
        let hooks_key = serde_yaml::Value::String("hooks".to_string());
        let Some(hooks_value) = root.get_mut(&hooks_key) else {
            return Ok(false);
        };
        let Some(hooks_map) = hooks_value.as_mapping_mut() else {
            return Ok(false);
        };

        let mut modified = false;
        let event_keys: Vec<serde_yaml::Value> = hooks_map.keys().cloned().collect();
        for event_key in event_keys {
            if let Some(arr) = hooks_map
                .get_mut(&event_key)
                .and_then(|v| v.as_sequence_mut())
            {
                let before = arr.len();
                arr.retain(|hook| {
                    !hook
                        .as_mapping()
                        .and_then(|m| m.get(serde_yaml::Value::String("command".into())))
                        .and_then(|c| c.as_str())
                        .is_some_and(is_aoe_hook_command)
                });
                if arr.len() != before {
                    modified = true;
                }
            }
        }

        if !modified {
            return Ok(false);
        }

        let empty_events: Vec<serde_yaml::Value> = hooks_map
            .iter()
            .filter(|(_, v)| v.as_sequence().is_some_and(|a| a.is_empty()))
            .map(|(k, _)| k.clone())
            .collect();
        for key in empty_events {
            hooks_map.remove(&key);
        }
        if hooks_map.is_empty() {
            root.remove(&hooks_key);
        }

        let formatted = serde_yaml::to_string(&config)?;
        crate::session::atomic_write_following_symlinks(config_path, formatted.as_bytes())?;
        tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", config_path.display());
        Ok(true)
    })
}

/// Pre-populate Hermes's per-user shell-hook allowlist so registration runs
/// without prompting on the first session. Hermes keys consent on the exact
/// `(event, command)` pair, so we add one entry per status we install.
///
/// `approved_at` is preserved on `(event, command)` collision: a re-install
/// with the same canonical command keeps the original first-approval
/// timestamp, only freshly-introduced entries get `Utc::now()`. This makes
/// the install path (and the v015 hook-rewrite migration that reuses it)
/// byte-idempotent for users whose canonical command is already current.
fn render_hermes_allowlist(
    config_dir: &Path,
    target: HookInstallTarget,
    events: &[crate::agents::ResolvedHookEvent],
) -> Result<(std::path::PathBuf, String)> {
    let allowlist_path = config_dir.join("shell-hooks-allowlist.json");
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

    let mut data: Value = if allowlist_path.exists() {
        let content = std::fs::read_to_string(&allowlist_path)?;
        serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse {}", allowlist_path.display()))?
    } else {
        serde_json::json!({"approvals": []})
    };

    let approvals = data
        .as_object_mut()
        .and_then(|o| {
            o.entry("approvals")
                .or_insert(Value::Array(Vec::new()))
                .as_array_mut()
        })
        .ok_or_else(|| anyhow::anyhow!("allowlist root is not a JSON object with approvals[]"))?;

    for event in events {
        let Some(status) = event.status else {
            continue;
        };
        let cmd = hook_command(status.as_str(), target);
        // Preserve the original `approved_at` when an entry with the same
        // (event, command) already exists; only fresh entries get `now`.
        // A `null` value is preserved verbatim: the field records
        // first-approval time, so a null stays a null on re-render.
        let preserved = approvals.iter().find_map(|entry| {
            let same = entry.get("event").and_then(|v| v.as_str()) == Some(event.name.as_str())
                && entry.get("command").and_then(|v| v.as_str()) == Some(&cmd);
            if same {
                entry.get("approved_at").cloned()
            } else {
                None
            }
        });
        approvals.retain(|entry| {
            !(entry.get("event").and_then(|v| v.as_str()) == Some(event.name.as_str())
                && entry.get("command").and_then(|v| v.as_str()) == Some(&cmd))
        });
        approvals.push(serde_json::json!({
            "event": event.name,
            "command": cmd,
            "approved_at": preserved.unwrap_or_else(|| Value::String(now.clone())),
            "script_mtime_at_approval": Value::Null,
        }));
    }

    let formatted = serde_json::to_string_pretty(&data)?;
    Ok((allowlist_path, formatted))
}

/// Single source of truth for the `aoe-hooks` Kiro agent identifier. Used
/// both as the agent name (passed to `kiro-cli agent set-default`) and as the
/// stem of [`KIRO_HOOKS_AGENT_FILE`]. Defined as a `macro_rules!` so the file
/// path can fold it through `concat!`. Distinct from [`AOE_HOOK_MARKER`]:
/// renaming the agent is a user-visible breaking change and must not couple
/// to the internal hook-detection token.
macro_rules! kiro_hooks_agent_name {
    () => {
        "aoe-hooks"
    };
}

const KIRO_HOOKS_AGENT_NAME: &str = kiro_hooks_agent_name!();

/// Default agent config path for Kiro CLI: `~/.kiro/agents/aoe-hooks.json`.
/// We use a dedicated agent config file rather than modifying the user's
/// default agent, so AoE hooks are isolated and easy to remove.
pub const KIRO_HOOKS_AGENT_FILE: &str = concat!(".kiro/agents/", kiro_hooks_agent_name!(), ".json");

/// Install AoE status hooks into a Kiro CLI agent config file.
///
/// Writes a minimal agent config with hooks that write status to the
/// AoE sidecar file. This function is pure file IO and is safe to call
/// from any context (host install, sandbox provisioning, tests). To make
/// the agent the active default on the host, call
/// [`set_kiro_default_agent_if_builtin`] after this returns.
///
/// Idempotent on disk: when the on-disk file already encodes the same
/// AoE-managed hook subtree, it is not rewritten (same inode, same bytes).
pub fn install_kiro_hooks(agent_config_path: &Path, target: HookInstallTarget) -> Result<()> {
    let events = default_sidecar_events("kiro");
    install_kiro_hooks_with_events(agent_config_path, target, &events)
}

pub fn install_kiro_hooks_with_events(
    agent_config_path: &Path,
    target: HookInstallTarget,
    events: &[crate::agents::ResolvedHookEvent],
) -> Result<()> {
    with_config_lock(agent_config_path, "json.lock", || {
        let mut config: serde_json::Map<String, Value> = if agent_config_path.exists() {
            let content = std::fs::read_to_string(agent_config_path)?;
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::Map::new())
        } else {
            serde_json::Map::new()
        };

        let before = config.clone();

        let default_name = agent_config_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(KIRO_HOOKS_AGENT_NAME);
        config
            .entry("name".to_string())
            .or_insert_with(|| Value::String(default_name.to_string()));
        config
            .entry("tools".to_string())
            .or_insert_with(|| serde_json::json!(["*"]));

        let mut hooks_obj: serde_json::Map<String, Value> = config
            .get("hooks")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        for event in events {
            let Some(status) = event.status else {
                continue;
            };
            let entries = hooks_obj
                .entry(event.name.clone())
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Some(arr) = entries.as_array_mut() {
                arr.retain(|hook| {
                    !hook
                        .get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(is_aoe_hook_command)
                });
                arr.push(serde_json::json!({ "command": hook_command(status.as_str(), target) }));
            }
        }

        config.insert("hooks".to_string(), Value::Object(hooks_obj));

        if config == before {
            tracing::debug!(target: "hooks.install",
                "AoE hooks in {} already up to date; skipping write",
                agent_config_path.display());
            return Ok(());
        }

        if let Some(parent) = agent_config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let formatted = serde_json::to_string_pretty(&Value::Object(config))?;
        crate::session::atomic_write_following_symlinks(agent_config_path, formatted.as_bytes())?;

        tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", agent_config_path.display());
        Ok(())
    })
}

/// Resolve which file under `agents_dir` holds the agent Kiro loads for
/// `--agent <name>`, returning the path AoE installs its status hooks into.
///
/// Kiro resolves `--agent <name>` by the `name` field inside each
/// `<agents_dir>/*.json`, not by the filename stem. Generators (plugin/managed
/// agent tooling) render files as `<prefix>-<name>.json`, so filename and
/// logical name diverge; assuming `filename == name` installs into a file Kiro
/// never loads and status detection silently fails. Matching the `name` field
/// is therefore mandatory, not a nicety.
///
/// Falls back to `<agents_dir>/<name>.json` when no file declares that name:
/// the correct create-path for a brand-new agent, which Kiro then reads `name`
/// from. The caller validates `name` (rejecting path separators and `..`, see
/// [`crate::agents::parse_selected_agent`]) so the fallback stays inside
/// `agents_dir`.
pub fn resolve_kiro_agent_file(agents_dir: &Path, name: &str) -> PathBuf {
    if let Some(path) = find_kiro_agent_file_by_name(agents_dir, name) {
        return path;
    }
    agents_dir.join(format!("{name}.json"))
}

/// First `<agents_dir>/*.json` whose top-level `name` equals `name`, or `None`.
/// Sorted so a duplicate-name tie resolves deterministically; unreadable or
/// non-object files are skipped rather than failing the scan. Two files
/// declaring the same `name` is a user misconfiguration (Kiro warns about it
/// too): we pick the lexicographically-first and `warn!` so the divergence is
/// debuggable, since installing into the wrong one silently breaks detection.
fn find_kiro_agent_file_by_name(agents_dir: &Path, name: &str) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(agents_dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    entries.sort();
    let mut matches = entries.into_iter().filter(|path| {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|content| serde_json::from_str::<Value>(&content).ok())
            .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(|n| n == name))
            .unwrap_or(false)
    });
    let first = matches.next()?;
    if let Some(second) = matches.next() {
        tracing::warn!(target: "hooks.install",
            "multiple Kiro agent files in {} declare name '{}' (e.g. {} and {}); \
             installing hooks into the first. Remove the duplicate to avoid ambiguity.",
            agents_dir.display(), name, first.display(), second.display());
    }
    Some(first)
}

/// Make `aoe-hooks` the active default Kiro agent if the user is still on
/// Kiro's built-in default. Skipped when a user has chosen a custom default
/// so we never silently override their preference. Best-effort: any failure
/// (kiro-cli missing, unexpected output, command error) is logged and ignored.
///
/// Uses `kiro-cli settings chat.defaultAgent --format json` for structured
/// output: returns `null` when unset, `"kiro_default"` for the built-in, or
/// `"custom-name"` for a user-chosen agent.
pub fn set_kiro_default_agent_if_builtin() {
    let output = std::process::Command::new("kiro-cli")
        .args(["settings", "chat.defaultAgent", "--format", "json"])
        .output();
    let current_default = output
        .as_ref()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout.clone()).ok())
        .unwrap_or_default();
    // With --format json, unset returns "null", set returns "\"agent-name\""
    let trimmed = current_default.trim();
    let is_builtin_default =
        trimmed.is_empty() || trimmed == "null" || trimmed == "\"kiro_default\"";

    if is_builtin_default {
        let set_result = std::process::Command::new("kiro-cli")
            .args(["agent", "set-default", KIRO_HOOKS_AGENT_NAME])
            .output();
        match set_result {
            Ok(o) if o.status.success() => {
                tracing::info!(target: "hooks.install", "Set {KIRO_HOOKS_AGENT_NAME} as default Kiro agent for status detection");
            }
            Ok(o) => {
                tracing::debug!(target: "hooks.install",
                    "kiro-cli agent set-default failed (non-fatal): {}",
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Err(e) => {
                tracing::debug!(target: "hooks.install", "kiro-cli not available for set-default: {}", e);
            }
        }
    } else {
        tracing::info!(target: "hooks.install",
            "Kiro has a custom default agent; skipping set-default. \
             Run `kiro-cli agent set-default {KIRO_HOOKS_AGENT_NAME}` to enable status detection."
        );
    }
}

/// Remove AoE hooks from a Kiro CLI agent config file.
/// Returns true if hooks were removed, false if nothing to do.
pub fn uninstall_kiro_hooks(agent_config_path: &Path) -> Result<bool> {
    if !agent_config_path.exists() {
        return Ok(false);
    }

    with_config_lock(agent_config_path, "json.lock", || {
        let content = std::fs::read_to_string(agent_config_path)?;
        let mut config: serde_json::Map<String, Value> =
            serde_json::from_str(&content).unwrap_or_else(|_| serde_json::Map::new());

        let Some(hooks_value) = config.get_mut("hooks") else {
            return Ok(false);
        };
        let Some(hooks_obj) = hooks_value.as_object_mut() else {
            return Ok(false);
        };

        let mut modified = false;
        let keys: Vec<String> = hooks_obj.keys().cloned().collect();
        for key in keys {
            if let Some(arr) = hooks_obj.get_mut(&key).and_then(|v| v.as_array_mut()) {
                let before = arr.len();
                arr.retain(|hook| {
                    !hook
                        .get("command")
                        .and_then(|c| c.as_str())
                        .is_some_and(is_aoe_hook_command)
                });
                if arr.len() != before {
                    modified = true;
                }
            }
        }

        if !modified {
            return Ok(false);
        }

        hooks_obj.retain(|_, v| !v.as_array().is_some_and(|a| a.is_empty()));
        if hooks_obj.is_empty() {
            config.remove("hooks");
        }

        if config.is_empty() {
            std::fs::remove_file(agent_config_path)?;
        } else {
            let formatted = serde_json::to_string_pretty(&Value::Object(config))?;
            crate::session::atomic_write_following_symlinks(
                agent_config_path,
                formatted.as_bytes(),
            )?;
        }

        tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", agent_config_path.display());
        Ok(true)
    })
}

/// Remove all AoE hooks from all known agent settings files and clean up
/// the hook status base directory. Called during `aoe uninstall`.
pub fn uninstall_all_hooks() {
    for target in iter_hook_targets() {
        let result = match target.kind {
            HookTargetKind::JsonSettings | HookTargetKind::CodexJson => {
                uninstall_hooks(&target.path)
            }
            HookTargetKind::CodexToml => uninstall_codex_hooks(&target.path),
            HookTargetKind::Sidecar(sidecar) => (sidecar.uninstall)(&target.path),
        };
        match result {
            Ok(true) => println!("Removed AoE hooks from {}", target.path.display()),
            Ok(false) => {}
            Err(e) => tracing::warn!(target: "hooks.uninstall",
                "Failed to remove {} hooks from {}: {}",
                target.agent_name,
                target.path.display(),
                e
            ),
        }
    }

    let base = dir_guard::hook_base_path();
    if base.exists() {
        // Modern std::fs::remove_dir_all uses openat + O_NOFOLLOW (CVE-2022-21658
        // fixed in 1.70). The path itself is computed from `hook_base_path()`
        // which is invariant for the running process, so the read-then-remove
        // sequence cannot race a path swap.
        if let Err(e) = std::fs::remove_dir_all(&base) {
            tracing::warn!(target: "hooks.uninstall", "Failed to remove {}: {}", base.display(), e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::targets::collect_env_lists_from_session;
    use super::*;
    use tempfile::TempDir;

    fn claude_events() -> Vec<crate::agents::ResolvedHookEvent> {
        let agent = crate::agents::get_agent("claude").unwrap();
        crate::agents::resolved_hook_events(agent, &crate::session::config::Config::default())
            .unwrap()
    }

    fn codex_events() -> Vec<crate::agents::ResolvedHookEvent> {
        let agent = crate::agents::get_agent("codex").unwrap();
        crate::agents::resolved_hook_events(agent, &crate::session::config::Config::default())
            .unwrap()
    }

    fn resolved_events_with_override(
        agent_name: &str,
        event_name: &str,
        status: crate::agents::HookStatus,
    ) -> Vec<crate::agents::ResolvedHookEvent> {
        let mut config = crate::session::config::Config::default();
        config
            .agents
            .entry(agent_name.to_string())
            .or_default()
            .status_map
            .insert(event_name.to_string(), status);
        let agent = crate::agents::get_agent(agent_name).unwrap();
        if agent.hook_config.is_some() {
            crate::agents::resolved_hook_events(agent, &config).unwrap()
        } else {
            crate::agents::resolved_sidecar_hook_events(agent, &config).unwrap()
        }
    }

    fn sidecar_default_events(agent_name: &str) -> &'static [crate::agents::SidecarHookEvent] {
        crate::agents::get_agent(agent_name)
            .unwrap()
            .sidecar_hooks
            .as_ref()
            .unwrap()
            .events
    }

    struct CodexHomeGuard(Option<String>);
    impl CodexHomeGuard {
        fn set(path: &Path) -> Self {
            let prev = std::env::var("CODEX_HOME").ok();
            std::env::set_var("CODEX_HOME", path);
            Self(prev)
        }

        fn unset() -> Self {
            let prev = std::env::var("CODEX_HOME").ok();
            std::env::remove_var("CODEX_HOME");
            Self(prev)
        }
    }
    impl Drop for CodexHomeGuard {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("CODEX_HOME", v),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
    }

    fn claude_hook_config() -> &'static crate::agents::AgentHookConfig {
        crate::agents::get_agent("claude")
            .unwrap()
            .hook_config
            .as_ref()
            .unwrap()
    }

    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }

        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_agent_settings_path_defaults_to_home_relative() {
        let _guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");
        let path = agent_settings_path_for_host_environment(claude_hook_config(), &[]).unwrap();
        let expected = dirs::home_dir().unwrap().join(".claude/settings.json");
        assert_eq!(path, expected);
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_agent_settings_path_honors_host_env_override() {
        let _guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");
        let host_env = vec!["CLAUDE_CONFIG_DIR=/home/me/.claude-work".to_string()];
        let path =
            agent_settings_path_for_host_environment(claude_hook_config(), &host_env).unwrap();
        // The env var replaces the whole ~/.claude dir; only the basename of
        // settings_rel_path is appended.
        assert_eq!(path, PathBuf::from("/home/me/.claude-work/settings.json"));
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_agent_settings_path_host_env_takes_precedence_over_process_env() {
        // When both are set, the session's profile env wins over AoE's own env.
        let _guard = EnvGuard::set("CLAUDE_CONFIG_DIR", "/from/process/env");
        let host_env = vec!["CLAUDE_CONFIG_DIR=/from/host/env".to_string()];
        let path =
            agent_settings_path_for_host_environment(claude_hook_config(), &host_env).unwrap();
        assert_eq!(path, PathBuf::from("/from/host/env/settings.json"));
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_agent_settings_path_falls_back_to_process_env() {
        // Not present in the host env list at all, but set in AoE's own env:
        // the launched agent inherits it, so hooks must follow.
        let _guard = EnvGuard::set("CLAUDE_CONFIG_DIR", "/tmp/claude-proc");
        let path = agent_settings_path_for_host_environment(claude_hook_config(), &[]).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/claude-proc/settings.json"));
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_agent_settings_path_display_matches_resolution() {
        let _guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");

        // Default: tilde-relative, matching how the path is shown elsewhere.
        assert_eq!(
            agent_settings_path_display_for_host_environment(claude_hook_config(), &[]),
            "~/.claude/settings.json"
        );

        // Override: absolute path the user will actually see hooks land in.
        let host_env = vec!["CLAUDE_CONFIG_DIR=/home/me/.claude-work".to_string()];
        assert_eq!(
            agent_settings_path_display_for_host_environment(claude_hook_config(), &host_env),
            "/home/me/.claude-work/settings.json"
        );
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn test_agent_settings_path_empty_override_is_ignored() {
        let _guard = EnvGuard::unset("CLAUDE_CONFIG_DIR");
        let host_env = vec!["CLAUDE_CONFIG_DIR=".to_string()];
        let path =
            agent_settings_path_for_host_environment(claude_hook_config(), &host_env).unwrap();
        let expected = dirs::home_dir().unwrap().join(".claude/settings.json");
        assert_eq!(path, expected);
    }

    #[test]
    fn test_install_hooks_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join(".claude").join("settings.json");

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let hooks = content.get("hooks").unwrap().as_object().unwrap();

        assert!(hooks.contains_key("PreToolUse"));
        assert!(hooks.contains_key("UserPromptSubmit"));
        assert!(hooks.contains_key("Stop"));
        assert!(hooks.contains_key("Notification"));
        assert!(hooks.contains_key("ElicitationResult"));
    }

    #[test]
    fn test_install_hooks_uses_resolved_status_override() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let events = resolved_events_with_override(
            "claude",
            "PreToolUse",
            crate::agents::HookStatus::Waiting,
        );

        install_hooks(&settings_path, &events, HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let cmd = content["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains("printf waiting"), "got command: {cmd}");
    }

    #[test]
    fn test_install_hooks_removes_stale_custom_event() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let custom_events =
            resolved_events_with_override("claude", "PreCompact", crate::agents::HookStatus::Idle);

        install_hooks(&settings_path, &custom_events, HookInstallTarget::Host).unwrap();
        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(content["hooks"].get("PreCompact").is_none());
    }

    #[test]
    fn test_install_hooks_preserves_existing_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        let existing = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "echo user-hook"}]
                    }
                ]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let pre_tool = content["hooks"]["PreToolUse"].as_array().unwrap();

        // Should have both user hook and AoE hook
        assert_eq!(pre_tool.len(), 2);

        // User hook preserved
        let user_hook = &pre_tool[0];
        assert_eq!(user_hook["matcher"], "Bash");

        // AoE hook added
        let aoe_hook = &pre_tool[1];
        let cmd = aoe_hook["hooks"][0]["command"].as_str().unwrap();
        assert!(is_aoe_hook_command(cmd));
    }

    #[test]
    fn test_install_hooks_idempotent() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();
        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let pre_tool = content["hooks"]["PreToolUse"].as_array().unwrap();

        // Should have exactly one AoE entry, not duplicates
        assert_eq!(pre_tool.len(), 1);
    }

    #[test]
    fn test_install_hooks_preserves_non_hook_settings() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        let existing = serde_json::json!({
            "apiKey": "test-key",
            "model": "opus",
            "hooks": {}
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(content["apiKey"], "test-key");
        assert_eq!(content["model"], "opus");
    }

    #[test]
    fn test_install_codex_hooks_writes_config_toml() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        let config_path = codex_dir.join("config.toml");

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert!(config["hooks"]["SessionStart"].is_array());
        assert!(config["hooks"]["UserPromptSubmit"].is_array());
        assert!(config["hooks"]["PreToolUse"].is_array());
        assert!(config["hooks"]["PermissionRequest"].is_array());
        assert!(config["hooks"]["PostToolUse"].is_array());
        assert!(config["hooks"]["Stop"].is_array());
        assert!(!codex_dir.join("hooks.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn test_install_codex_hooks_preserves_symlinked_config() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        let dotfiles_dir = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::create_dir_all(&dotfiles_dir).unwrap();

        let target_path = dotfiles_dir.join("codex-config.toml");
        std::fs::write(&target_path, "model = \"gpt-5.3-codex\"\n").unwrap();
        let config_path = codex_dir.join("config.toml");
        symlink("../dotfiles/codex-config.toml", &config_path).unwrap();

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        assert!(
            std::fs::symlink_metadata(&config_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "Codex config path must remain a symlink"
        );
        let config_text = std::fs::read_to_string(&target_path).unwrap();
        assert!(config_text.contains("model = \"gpt-5.3-codex\""));
        let config: toml::Value = toml::from_str(&config_text).unwrap();
        assert!(config["hooks"]["SessionStart"].is_array());
        assert!(config["hooks"]["UserPromptSubmit"].is_array());
        assert!(target_path.with_extension("toml.lock").exists());
        assert!(!config_path.with_extension("toml.lock").exists());

        uninstall_codex_hooks(&config_path).unwrap();
        assert!(
            std::fs::symlink_metadata(&config_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "Codex config path must remain a symlink after uninstall"
        );
        let after = std::fs::read_to_string(&target_path).unwrap();
        assert!(
            after.contains("model = \"gpt-5.3-codex\""),
            "user content must survive uninstall"
        );
        let after_doc: toml::Value = toml::from_str(&after).unwrap();
        assert!(
            after_doc.get("hooks").is_none(),
            "AoE hooks must be removed from target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_install_hooks_preserves_symlinked_settings() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let claude_dir = tmp.path().join(".claude");
        let dotfiles_dir = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&claude_dir).unwrap();
        std::fs::create_dir_all(&dotfiles_dir).unwrap();

        let target_path = dotfiles_dir.join("claude-settings.json");
        std::fs::write(&target_path, "{\"apiKey\":\"keep-me\"}\n").unwrap();
        let settings_path = claude_dir.join("settings.json");
        symlink("../dotfiles/claude-settings.json", &settings_path).unwrap();

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        assert!(
            std::fs::symlink_metadata(&settings_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "settings path must remain a symlink"
        );
        let settings: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target_path).unwrap()).unwrap();
        assert_eq!(settings["apiKey"], "keep-me");
        assert!(settings["hooks"]["SessionStart"].is_array());

        uninstall_hooks(&settings_path).unwrap();
        assert!(
            std::fs::symlink_metadata(&settings_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "settings path must remain a symlink after uninstall"
        );
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&target_path).unwrap()).unwrap();
        assert_eq!(after["apiKey"], "keep-me");
        assert!(after.get("hooks").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn test_install_hermes_hooks_preserves_symlinked_config() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let hermes_dir = tmp.path().join(".hermes");
        let dotfiles_dir = tmp.path().join("dotfiles");
        std::fs::create_dir_all(&hermes_dir).unwrap();
        std::fs::create_dir_all(&dotfiles_dir).unwrap();

        let config_target = dotfiles_dir.join("hermes-config.yaml");
        let allowlist_target = dotfiles_dir.join("hermes-allowlist.json");
        std::fs::write(&config_target, "user_field: keep-me\n").unwrap();
        std::fs::write(&allowlist_target, "{\"approvals\":[]}\n").unwrap();

        let config_path = hermes_dir.join("config.yaml");
        let allowlist_path = hermes_dir.join("shell-hooks-allowlist.json");
        symlink("../dotfiles/hermes-config.yaml", &config_path).unwrap();
        symlink("../dotfiles/hermes-allowlist.json", &allowlist_path).unwrap();

        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();

        for path in [&config_path, &allowlist_path] {
            assert!(
                std::fs::symlink_metadata(path)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{} must remain a symlink",
                path.display()
            );
        }
        let config: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&config_target).unwrap()).unwrap();
        assert_eq!(
            config
                .as_mapping()
                .unwrap()
                .get(serde_yaml::Value::String("user_field".into()))
                .and_then(|v| v.as_str()),
            Some("keep-me")
        );
        let hooks = config
            .as_mapping()
            .unwrap()
            .get(serde_yaml::Value::String("hooks".into()))
            .unwrap()
            .as_mapping()
            .unwrap();
        for event in sidecar_default_events("hermes") {
            assert!(
                hooks
                    .get(serde_yaml::Value::String(event.name.into()))
                    .is_some(),
                "event {} missing on dotfile target",
                event.name
            );
        }
        let allowlist: Value =
            serde_json::from_str(&std::fs::read_to_string(&allowlist_target).unwrap()).unwrap();
        assert_eq!(
            allowlist["approvals"].as_array().unwrap().len(),
            sidecar_default_events("hermes").len()
        );

        uninstall_hermes_hooks(&config_path).unwrap();
        assert!(
            std::fs::symlink_metadata(&config_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "config symlink must remain after uninstall"
        );
        let after: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&config_target).unwrap()).unwrap();
        assert_eq!(
            after
                .as_mapping()
                .unwrap()
                .get(serde_yaml::Value::String("user_field".into()))
                .and_then(|v| v.as_str()),
            Some("keep-me")
        );
        assert!(after
            .as_mapping()
            .unwrap()
            .get(serde_yaml::Value::String("hooks".into()))
            .is_none());
        assert!(
            std::fs::symlink_metadata(&allowlist_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "allowlist symlink must remain untouched after uninstall"
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_config_path_respects_codex_home() {
        let tmp = TempDir::new().unwrap();
        let _guard = CodexHomeGuard::set(tmp.path());

        assert_eq!(codex_config_path().unwrap(), tmp.path().join("config.toml"));
        assert_eq!(
            codex_config_path_display(),
            tmp.path().join("config.toml").display().to_string()
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_config_path_for_host_environment_ignores_empty_codex_home() {
        let tmp = TempDir::new().unwrap();
        let _guard = CodexHomeGuard::unset();
        std::env::set_var("HOME", tmp.path());

        // An empty `CODEX_HOME=` must not resolve to a bare relative
        // `config.toml`; it should fall back to the home-relative default.
        let entries = vec!["CODEX_HOME=".to_string()];

        let path = codex_config_path_for_host_environment(&entries).unwrap();
        assert_eq!(path, tmp.path().join(".codex").join("config.toml"));
        assert!(path.is_absolute());
        assert_ne!(path, PathBuf::from("config.toml"));

        assert_eq!(
            codex_config_path_display_for_host_environment(&entries),
            codex_config_path_display()
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_codex_config_path_ignores_empty_process_codex_home() {
        let tmp = TempDir::new().unwrap();
        // An empty `CODEX_HOME` in AoE's own process env must fall back to the
        // home-relative default rather than a bare relative `config.toml`.
        let _guard = CodexHomeGuard::set(Path::new(""));
        std::env::set_var("HOME", tmp.path());

        let path = codex_config_path().unwrap();
        assert_eq!(path, tmp.path().join(".codex").join("config.toml"));
        assert!(path.is_absolute());
        assert_ne!(path, PathBuf::from("config.toml"));

        assert_eq!(codex_config_path_display(), "~/.codex/config.toml");
    }

    #[test]
    #[serial_test::serial]
    fn test_iter_hook_targets_includes_profile_codex_home() {
        let tmp = TempDir::new().unwrap();
        let _guard = CodexHomeGuard::unset();
        std::env::set_var("HOME", tmp.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));

        let codex_home = tmp.path().join("profile-codex-home");
        let profile_dir = crate::session::get_profile_dir("codex-profile").unwrap();
        std::fs::write(
            profile_dir.join("config.toml"),
            format!("environment = [\"CODEX_HOME={}\"]\n", codex_home.display()),
        )
        .unwrap();

        let codex_paths: Vec<_> = iter_hook_targets()
            .into_iter()
            .filter(|t| matches!(t.kind, HookTargetKind::CodexJson))
            .map(|t| t.path)
            .collect();

        assert!(codex_paths.contains(&tmp.path().join(".codex").join("hooks.json")));
        assert!(codex_paths.contains(&codex_home.join("hooks.json")));
    }

    #[test]
    fn test_install_codex_hooks_preserves_disabled_flag_and_skips_install() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            "# keep this comment\nmodel = \"gpt-5.3-codex\"\n\n[features]\nweb_search = true\nhooks = false\n",
        )
        .unwrap();

        let config_path = codex_dir.join("config.toml");
        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(config.contains("# keep this comment"));
        assert!(config.contains("model = \"gpt-5.3-codex\""));
        assert!(config.contains("web_search = true"));
        assert!(config.contains("hooks = false"));
        assert!(!config.contains("hooks = true"));
        assert!(!config.contains("aoe-hooks"));
    }

    #[test]
    fn test_install_codex_hooks_preserves_inline_user_hooks_state_and_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"model = "gpt-5.3-codex"
hooks = { PreToolUse = [{ matcher = "Bash", hooks = [{ type = "command", command = "echo user-hook" }] }], state = { user = { enabled = true, trusted_hash = "keep" } } }
"#,
        )
        .unwrap();

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();
        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        let config_text = std::fs::read_to_string(config_path).unwrap();
        let config: toml::Value = toml::from_str(&config_text).unwrap();
        let pre_tool = config["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 2);
        assert_eq!(
            pre_tool[0]["hooks"][0]["command"].as_str(),
            Some("echo user-hook")
        );
        assert_eq!(
            config["hooks"]["state"]["user"]["trusted_hash"].as_str(),
            Some("keep")
        );
        assert_eq!(config_text.matches("sh -c").count(), codex_events().len());
    }

    #[test]
    fn test_install_codex_hooks_preserves_hooks_state_on_existing_aoe_hooks() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                r#"[hooks.state]
existing = {{ enabled = true, trusted_hash = "hook-trust" }}

[[hooks.PreToolUse]]

[[hooks.PreToolUse.hooks]]
type = "command"
command = {:?}
"#,
                hook_command("running", HookInstallTarget::Host)
            ),
        )
        .unwrap();

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();
        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        let config_text = std::fs::read_to_string(config_path).unwrap();
        let config: toml::Value = toml::from_str(&config_text).unwrap();
        let pre_tool = config["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(
            config["hooks"]["state"]["existing"]["trusted_hash"].as_str(),
            Some("hook-trust")
        );
        assert_eq!(config_text.matches("sh -c").count(), codex_events().len());
    }

    #[test]
    fn test_install_codex_hooks_does_not_overwrite_newer_hooks_state() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"[hooks.state.current]
enabled = true
trusted_hash = "new"
"#,
        )
        .unwrap();

        let mut stale_state = toml_edit::Table::new();
        stale_state.insert("enabled", toml_edit::value(true));
        stale_state.insert("trusted_hash", toml_edit::value("old"));
        let mut preserved_state = toml_edit::Table::new();
        preserved_state.insert("stale", toml_edit::Item::Table(stale_state));
        let preserved_state = toml_edit::Item::Table(preserved_state);

        install_codex_hooks_with_preserved_state(
            &config_path,
            codex_events(),
            Some(preserved_state),
            HookInstallTarget::Host,
        )
        .unwrap();

        let config_text = std::fs::read_to_string(config_path).unwrap();
        let config: toml::Value = toml::from_str(&config_text).unwrap();
        assert_eq!(
            config["hooks"]["state"]["current"]["trusted_hash"].as_str(),
            Some("new")
        );
        assert!(config["hooks"]["state"].get("stale").is_none());
    }

    #[test]
    fn test_install_codex_hooks_collapses_duplicated_aoe_blocks() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        let installed_once = format!(
            r#"[hooks]

[[hooks.SessionStart]]

[[hooks.SessionStart.hooks]]
type = "command"
command = {:?}

[[hooks.PreToolUse]]

[[hooks.PreToolUse.hooks]]
type = "command"
command = {:?}

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = {:?}

[[hooks.SessionStart]]

[[hooks.SessionStart.hooks]]
type = "command"
command = {:?}

[[hooks.PreToolUse]]

[[hooks.PreToolUse.hooks]]
type = "command"
command = {:?}

[[hooks.Stop]]

[[hooks.Stop.hooks]]
type = "command"
command = {:?}

[hooks.state.trusted]
enabled = true
trusted_hash = "sha256:keep"

[projects."/tmp/aoe-project"]
trust_level = "trusted"
"#,
            hook_command("idle", HookInstallTarget::Host),
            hook_command("running", HookInstallTarget::Host),
            hook_command("idle", HookInstallTarget::Host),
            hook_command("idle", HookInstallTarget::Host),
            hook_command("running", HookInstallTarget::Host),
            hook_command("idle", HookInstallTarget::Host)
        );
        std::fs::write(&config_path, installed_once).unwrap();

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        let config_text = std::fs::read_to_string(config_path).unwrap();
        let config: toml::Value = toml::from_str(&config_text).unwrap();
        for event in codex_events() {
            assert_eq!(config["hooks"][event.name].as_array().unwrap().len(), 1);
        }
        assert_eq!(
            config["hooks"]["state"]["trusted"]["trusted_hash"].as_str(),
            Some("sha256:keep")
        );
        assert_eq!(
            config["projects"]["/tmp/aoe-project"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(config_text.matches("sh -c").count(), codex_events().len());
    }

    #[test]
    fn test_install_codex_hooks_concurrent_rewrites_keep_valid_toml() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"model = "gpt-5.3-codex"

[projects."/tmp/aoe-project"]
trust_level = "trusted"
"#,
        )
        .unwrap();

        let workers = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
        let mut handles = Vec::new();
        for _ in 0..workers {
            let barrier = barrier.clone();
            let config_path = config_path.clone();
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..8 {
                    install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host)
                        .unwrap();
                    let config_text = std::fs::read_to_string(&config_path).unwrap();
                    config_text.parse::<toml_edit::DocumentMut>().unwrap();
                }
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        let config_text = std::fs::read_to_string(config_path).unwrap();
        let config: toml::Value = toml::from_str(&config_text).unwrap();
        for event in codex_events() {
            assert_eq!(config["hooks"][event.name].as_array().unwrap().len(), 1);
        }
        assert_eq!(
            config["projects"]["/tmp/aoe-project"]["trust_level"].as_str(),
            Some("trusted")
        );
        assert_eq!(config_text.matches("sh -c").count(), codex_events().len());
    }

    #[test]
    fn test_install_codex_hooks_preserves_inline_disabled_flag_and_skips_install() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            r#"model = "gpt-5.3-codex"
features = { web_search = true, hooks = false }
"#,
        )
        .unwrap();

        let config_path = codex_dir.join("config.toml");
        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        let config = std::fs::read_to_string(&config_path).unwrap();
        assert!(config.contains("model = \"gpt-5.3-codex\""));
        assert!(config.contains("web_search = true"));
        assert!(config.contains("hooks = false"));
        assert!(!config.contains("aoe-hooks"));
    }

    #[test]
    fn test_install_codex_hooks_respects_deprecated_disabled_alias() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        std::fs::write(
            codex_dir.join("config.toml"),
            r#"model = "gpt-5.3-codex"
features = { web_search = true, codex_hooks = false }
"#,
        )
        .unwrap();

        let config_path = codex_dir.join("config.toml");
        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        let config = std::fs::read_to_string(config_path).unwrap();
        assert!(!config.contains("aoe-hooks"));
    }

    #[test]
    fn test_uninstall_codex_hooks_removes_toml_entries() {
        let tmp = TempDir::new().unwrap();
        let codex_dir = tmp.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let config_path = codex_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"[hooks.state]
user = { enabled = true, trusted_hash = "keep" }

[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo user-hook"
"#,
        )
        .unwrap();

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();
        let modified = uninstall_codex_hooks(&config_path).unwrap();
        assert!(modified);

        let config = std::fs::read_to_string(config_path).unwrap();
        assert!(config.contains("echo user-hook"));
        assert!(config.contains("trusted_hash = \"keep\""));
        assert!(!config.contains("aoe-hooks"));
    }

    #[test]
    fn test_hook_command_format() {
        let cmd = hook_command("running", HookInstallTarget::Host);
        assert!(cmd.contains(AOE_HOOK_MARKER));
        assert!(cmd.contains("printf running"));
    }

    #[test]
    fn test_hook_command_contains_instance_id_guard() {
        let cmd = hook_command("idle", HookInstallTarget::Host);
        assert!(cmd.contains("AOE_INSTANCE_ID"));
        assert!(cmd.contains("printf idle"));
    }

    #[test]
    fn test_hook_command_tolerates_unwritable_base_dir() {
        // Regression for #1390 (issue title quotes the pre-#1844 path
        // `/tmp/aoe-hooks/<id>`; the modern path is `/tmp/aoe-hooks-<euid>/<id>`):
        // if the per-instance hook dir disappears mid-session (OS /tmp
        // cleanup, transient FS hiccup, external tooling), the hook must
        // still exit 0 so the agent doesn't treat it as blocking and
        // freeze further tool calls.
        use std::process::Command;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("aoe-hooks-blocked");
        // Pre-create base as a regular file so mkdir -p can never succeed.
        std::fs::write(&base, "i am a file, not a dir").unwrap();

        let cmd =
            hook_command_with_base("running", base.to_str().unwrap(), HookInstallTarget::Host);

        let output = Command::new("sh")
            .args(["-c", &cmd])
            .env("AOE_INSTANCE_ID", "regression_1390")
            .output()
            .expect("spawn sh");

        assert!(
            output.status.success(),
            "hook must exit 0 even when its dir cannot be created: {:?}",
            output
        );
    }

    #[test]
    fn test_hook_command_writes_status_on_happy_path() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("aoe-hooks");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();

        let cmd =
            hook_command_with_base("waiting", base.to_str().unwrap(), HookInstallTarget::Host);

        let output = Command::new("sh")
            .args(["-c", &cmd])
            .env("AOE_INSTANCE_ID", "happy_path")
            .output()
            .expect("spawn sh");

        assert!(output.status.success(), "happy-path hook should exit 0");
        let status_path = base.join("happy_path").join("status");
        assert_eq!(std::fs::read_to_string(&status_path).unwrap(), "waiting");
    }

    #[test]
    fn test_notification_hook_has_matcher() {
        let hooks = build_aoe_hooks(claude_events(), HookInstallTarget::Sandbox);
        let notification = hooks["Notification"].as_array().unwrap();
        // Two matcher groups: permission/elicitation → waiting, idle_prompt → idle.
        assert_eq!(notification.len(), 2);

        let waiting = notification
            .iter()
            .find(|g| {
                g["matcher"]
                    .as_str()
                    .is_some_and(|m| m.contains("permission_prompt"))
            })
            .expect("waiting matcher group present");
        let waiting_matcher = waiting["matcher"].as_str().unwrap();
        assert!(waiting_matcher.contains("permission_prompt"));
        assert!(waiting_matcher.contains("elicitation_dialog"));
        assert!(!waiting_matcher.contains("idle_prompt"));
        assert!(
            waiting["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("printf waiting"),
            "permission/elicitation notification should write waiting"
        );

        let idle = notification
            .iter()
            .find(|g| g["matcher"].as_str() == Some("idle_prompt"))
            .expect("idle_prompt matcher group present");
        assert!(
            idle["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("printf idle"),
            "idle_prompt notification should write idle"
        );
    }

    #[test]
    fn test_stop_hook_writes_idle() {
        let hooks = build_aoe_hooks(claude_events(), HookInstallTarget::Sandbox);
        let stop = hooks["Stop"].as_array().unwrap();
        let cmd = stop[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(
            cmd.contains("printf idle"),
            "Stop hook should write idle status: {}",
            cmd
        );
    }

    #[test]
    fn test_stop_failure_hook_writes_idle() {
        // A turn killed by an API error fires StopFailure, not Stop, so this is
        // the only thing that clears the trailing `running` write in that path.
        let hooks = build_aoe_hooks(claude_events(), HookInstallTarget::Sandbox);
        let stop_failure = hooks["StopFailure"].as_array().unwrap();
        assert_eq!(stop_failure.len(), 1);
        let cmd = stop_failure[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(
            cmd.contains("printf idle"),
            "StopFailure hook should write idle status: {}",
            cmd
        );
    }

    #[test]
    fn test_elicitation_result_hook_writes_running() {
        let hooks = build_aoe_hooks(claude_events(), HookInstallTarget::Sandbox);
        let er = hooks["ElicitationResult"].as_array().unwrap();
        assert_eq!(er.len(), 1);
        let cmd = er[0]["hooks"][0]["command"].as_str().unwrap();
        assert!(
            cmd.contains("printf running"),
            "ElicitationResult hook should write running status: {}",
            cmd
        );
    }

    #[test]
    fn test_hooks_are_synchronous() {
        let hooks = build_aoe_hooks(claude_events(), HookInstallTarget::Sandbox);
        for (_, matchers) in hooks.as_object().unwrap() {
            for matcher in matchers.as_array().unwrap() {
                for hook in matcher["hooks"].as_array().unwrap() {
                    assert!(
                        hook.get("async").is_none(),
                        "Hooks should be synchronous (no async field): {:?}",
                        hook
                    );
                }
            }
        }
    }

    #[test]
    fn test_uninstall_hooks_removes_aoe_entries() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(!content
            .get("hooks")
            .unwrap()
            .as_object()
            .unwrap()
            .is_empty());

        let modified = uninstall_hooks(&settings_path).unwrap();
        assert!(modified);

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert!(content.get("hooks").is_none());
    }

    #[test]
    fn test_uninstall_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        let existing = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "echo user-hook"}]
                    }
                ]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();
        let modified = uninstall_hooks(&settings_path).unwrap();
        assert!(modified);

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let pre_tool = content["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(pre_tool[0]["matcher"], "Bash");
        assert!(content["hooks"].get("Stop").is_none());
    }

    #[test]
    fn test_uninstall_hooks_nonexistent_file() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("nonexistent.json");
        let modified = uninstall_hooks(&settings_path).unwrap();
        assert!(!modified);
    }

    #[test]
    fn test_uninstall_hooks_no_aoe_hooks() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        let existing = serde_json::json!({
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [{"type": "command", "command": "echo user-hook"}]
                    }
                ]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        let modified = uninstall_hooks(&settings_path).unwrap();
        assert!(!modified);
    }

    #[test]
    fn test_remove_aoe_entries_keeps_user_hooks() {
        let mut matchers = vec![
            serde_json::json!({
                "matcher": "Bash",
                "hooks": [{"type": "command", "command": "echo user"}]
            }),
            serde_json::json!({
                "hooks": [{"type": "command",
                           "command": "sh -c 'printf running > /dev/null; exit 0 # aoe-hooks'"}]
            }),
        ];

        remove_aoe_entries(&mut matchers);
        assert_eq!(matchers.len(), 1);
        assert_eq!(matchers[0]["matcher"], "Bash");
    }

    #[test]
    fn test_install_replaces_existing_hooks() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        let old_hooks = serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{
                        "type": "command",
                        "command": "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; mkdir -p /tmp/aoe-hooks/$AOE_INSTANCE_ID && printf running > /tmp/aoe-hooks/$AOE_INSTANCE_ID/status'"
                    }]
                }]
            }
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&old_hooks).unwrap(),
        )
        .unwrap();

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        let pre_tool = &content["hooks"]["PreToolUse"];
        let all_cmds: Vec<String> = pre_tool
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|m| m["hooks"].as_array().unwrap())
            .filter_map(|h| h["command"].as_str().map(|s| s.to_string()))
            .collect();
        assert_eq!(
            all_cmds.len(),
            1,
            "Expected exactly 1 hook after reinstall, got: {:?}",
            all_cmds
        );
    }

    #[test]
    fn test_install_settl_hooks_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join(".settl").join("config.toml");

        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        let hooks = config["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 3);
        assert_eq!(hooks[0]["event"].as_str().unwrap(), "TurnStarted");
        assert_eq!(hooks[1]["event"].as_str().unwrap(), "WaitingForHuman");
        assert_eq!(hooks[2]["event"].as_str().unwrap(), "GameWon");

        for hook in hooks {
            assert!(hook["command"].as_str().unwrap().contains("aoe-hooks"));
        }
    }

    #[test]
    fn test_install_settl_hooks_idempotent() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join(".settl").join("config.toml");

        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();
        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        let hooks = config["hooks"].as_array().unwrap();
        assert_eq!(
            hooks.len(),
            3,
            "Should have exactly 3 hooks, not duplicates"
        );
    }

    #[test]
    fn test_install_settl_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join(".settl");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[[hooks]]
event = "GameWon"
command = "echo user-hook"
"#,
        )
        .unwrap();

        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        let hooks = config["hooks"].as_array().unwrap();
        // 1 user hook + 3 AoE hooks = 4
        assert_eq!(hooks.len(), 4);
        assert_eq!(hooks[0]["command"].as_str().unwrap(), "echo user-hook");
    }

    #[test]
    fn test_uninstall_settl_hooks_removes_aoe_entries() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join(".settl").join("config.toml");

        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();
        let modified = uninstall_settl_hooks(&config_path).unwrap();

        assert!(modified);
        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        let hooks = config["hooks"].as_array().unwrap();
        assert!(hooks.is_empty());
    }

    #[test]
    fn test_uninstall_settl_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join(".settl");
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_path = config_dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[[hooks]]
event = "GameWon"
command = "echo user-hook"
"#,
        )
        .unwrap();

        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();
        let modified = uninstall_settl_hooks(&config_path).unwrap();

        assert!(modified);
        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        let hooks = config["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["command"].as_str().unwrap(), "echo user-hook");
    }

    #[test]
    fn test_settl_hook_commands_write_correct_status() {
        for event in sidecar_default_events("settl") {
            let expected_status = event.status.as_str();
            let cmd = hook_command(expected_status, HookInstallTarget::Host);
            assert!(
                cmd.contains(&format!("printf {}", expected_status)),
                "Hook for {} should write '{}': {}",
                event.name,
                expected_status,
                cmd
            );
            assert!(cmd.contains("aoe-hooks"), "Hook should contain marker");
        }
    }

    #[test]
    fn test_install_hermes_hooks_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join(".hermes").join("config.yaml");

        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        let hooks = config
            .as_mapping()
            .unwrap()
            .get(serde_yaml::Value::String("hooks".into()))
            .unwrap()
            .as_mapping()
            .unwrap();

        for event in sidecar_default_events("hermes") {
            let entries = hooks
                .get(serde_yaml::Value::String(event.name.into()))
                .unwrap_or_else(|| panic!("event {} missing", event.name))
                .as_sequence()
                .unwrap();
            assert_eq!(
                entries.len(),
                1,
                "event {} should have one entry",
                event.name
            );
            let cmd = entries[0]
                .as_mapping()
                .and_then(|m| m.get(serde_yaml::Value::String("command".into())))
                .and_then(|c| c.as_str())
                .unwrap();
            assert!(is_aoe_hook_command(cmd));
        }

        // Allowlist should be pre-populated alongside the config
        let allowlist = tmp
            .path()
            .join(".hermes")
            .join("shell-hooks-allowlist.json");
        assert!(allowlist.exists(), "shell-hooks-allowlist.json missing");
        let raw = std::fs::read_to_string(&allowlist).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let approvals = parsed["approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), sidecar_default_events("hermes").len());
    }

    #[test]
    fn test_install_hermes_hooks_uses_resolved_status_override() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        let events = resolved_events_with_override(
            "hermes",
            "pre_approval_request",
            crate::agents::HookStatus::Error,
        );

        install_hermes_hooks_with_events(&config_path, HookInstallTarget::Host, &events).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        let cmd = config["hooks"]["pre_approval_request"][0]["command"]
            .as_str()
            .unwrap();
        assert!(cmd.contains("printf error"), "got command: {cmd}");

        let allowlist: Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("shell-hooks-allowlist.json")).unwrap(),
        )
        .unwrap();
        let approval_cmd = allowlist["approvals"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["event"].as_str() == Some("pre_approval_request"))
            .and_then(|entry| entry["command"].as_str())
            .unwrap();
        assert!(
            approval_cmd.contains("printf error"),
            "got command: {approval_cmd}"
        );
    }

    #[test]
    fn test_install_hermes_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        std::fs::write(
            &config_path,
            r#"hooks:
  pre_tool_call:
    - command: "echo user-hook"
      matcher: "terminal"
hooks_auto_accept: false
"#,
        )
        .unwrap();

        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();

        // Non-hook keys preserved
        assert_eq!(
            config["hooks_auto_accept"].as_bool(),
            Some(false),
            "hooks_auto_accept should remain false"
        );

        let pre_tool = config["hooks"]["pre_tool_call"].as_sequence().unwrap();
        // 1 user hook + 1 AoE hook = 2
        assert_eq!(pre_tool.len(), 2);
        assert_eq!(pre_tool[0]["command"].as_str().unwrap(), "echo user-hook");
        assert!(is_aoe_hook_command(
            pre_tool[1]["command"].as_str().unwrap()
        ));
    }

    #[test]
    fn test_install_hermes_hooks_rejects_invalid_yaml_without_overwrite() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        let original = "hooks:\n  pre_tool_call: [\n";
        std::fs::write(&config_path, original).unwrap();

        let result = install_hermes_hooks(&config_path, HookInstallTarget::Host);

        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), original);
        assert!(!tmp.path().join("shell-hooks-allowlist.json").exists());
    }

    #[test]
    fn test_install_hermes_hooks_rejects_invalid_allowlist_without_overwrite() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        let allowlist_path = tmp.path().join("shell-hooks-allowlist.json");
        let original_config = "model: claude-opus\n";
        let original_allowlist = "{ invalid json";
        std::fs::write(&config_path, original_config).unwrap();
        std::fs::write(&allowlist_path, original_allowlist).unwrap();

        let result = install_hermes_hooks(&config_path, HookInstallTarget::Host);

        assert!(result.is_err());
        assert_eq!(
            std::fs::read_to_string(&config_path).unwrap(),
            original_config
        );
        assert_eq!(
            std::fs::read_to_string(&allowlist_path).unwrap(),
            original_allowlist
        );
    }

    #[test]
    fn test_install_hermes_hooks_idempotent() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");

        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();
        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        let pre_tool = config["hooks"]["pre_tool_call"].as_sequence().unwrap();
        assert_eq!(pre_tool.len(), 1, "reinstall should not duplicate");

        // Allowlist also dedupes
        let allowlist = tmp.path().join("shell-hooks-allowlist.json");
        let raw = std::fs::read_to_string(&allowlist).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let approvals = parsed["approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), sidecar_default_events("hermes").len());
    }

    #[test]
    fn test_uninstall_hermes_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        std::fs::write(
            &config_path,
            "hooks:\n  pre_tool_call:\n    - command: \"echo user-hook\"\n",
        )
        .unwrap();

        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();
        let modified = uninstall_hermes_hooks(&config_path).unwrap();
        assert!(modified);

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        let pre_tool = config["hooks"]["pre_tool_call"].as_sequence().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(pre_tool[0]["command"].as_str().unwrap(), "echo user-hook");
        // Other AoE-only events should be gone entirely
        assert!(config["hooks"].get("post_llm_call").is_none());
    }

    #[test]
    fn test_uninstall_hermes_hooks_nonexistent_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        let modified = uninstall_hermes_hooks(&config_path).unwrap();
        assert!(!modified);
    }

    #[test]
    fn test_hermes_hook_commands_write_correct_status() {
        for event in sidecar_default_events("hermes") {
            let expected_status = event.status.as_str();
            let cmd = hook_command(expected_status, HookInstallTarget::Host);
            assert!(
                cmd.contains(&format!("printf {}", expected_status)),
                "Hook for {} should write '{}': {}",
                event.name,
                expected_status,
                cmd
            );
            assert!(cmd.contains("aoe-hooks"), "Hook should contain marker");
        }
    }

    #[test]
    fn test_hermes_approval_request_writes_waiting() {
        let mapped: Vec<&str> = sidecar_default_events("hermes")
            .iter()
            .filter(|event| event.name == "pre_approval_request")
            .map(|event| event.status.as_str())
            .collect();
        assert_eq!(
            mapped,
            vec!["waiting"],
            "pre_approval_request must map to waiting status"
        );
    }

    #[test]
    fn test_install_kiro_hooks_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp
            .path()
            .join(".kiro")
            .join("agents")
            .join("aoe-hooks.json");

        install_kiro_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        let hooks = config["hooks"].as_object().unwrap();

        for event in sidecar_default_events("kiro") {
            let entries = hooks
                .get(event.name)
                .unwrap_or_else(|| panic!("event {} missing", event.name))
                .as_array()
                .unwrap();
            assert_eq!(
                entries.len(),
                1,
                "event {} should have one entry",
                event.name
            );
            let cmd = entries[0]["command"].as_str().unwrap();
            assert!(is_aoe_hook_command(cmd));
        }
    }

    #[test]
    fn test_install_kiro_hooks_uses_resolved_status_override() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("aoe-hooks.json");
        let events =
            resolved_events_with_override("kiro", "stop", crate::agents::HookStatus::Error);

        install_kiro_hooks_with_events(&config_path, HookInstallTarget::Host, &events).unwrap();

        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        let cmd = config["hooks"]["stop"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("printf error"), "got command: {cmd}");
    }

    #[test]
    fn test_install_kiro_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("aoe-hooks.json");
        std::fs::write(
            &config_path,
            r#"{"hooks": {"preToolUse": [{"command": "echo user-hook", "matcher": "shell"}]}}"#,
        )
        .unwrap();

        install_kiro_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        let pre_tool = config["hooks"]["preToolUse"].as_array().unwrap();
        // 1 user hook + 1 AoE hook = 2
        assert_eq!(pre_tool.len(), 2);
        assert_eq!(pre_tool[0]["command"].as_str().unwrap(), "echo user-hook");
        assert!(is_aoe_hook_command(
            pre_tool[1]["command"].as_str().unwrap()
        ));
    }

    #[test]
    fn test_install_kiro_hooks_idempotent() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("aoe-hooks.json");

        install_kiro_hooks(&config_path, HookInstallTarget::Host).unwrap();
        install_kiro_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        for event in sidecar_default_events("kiro") {
            let entries = config["hooks"][event.name].as_array().unwrap();
            assert_eq!(
                entries.len(),
                1,
                "event {} should still have exactly one AoE entry after double install",
                event.name
            );
        }
    }

    #[test]
    fn test_uninstall_kiro_hooks_removes_aoe_entries() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("aoe-hooks.json");

        install_kiro_hooks(&config_path, HookInstallTarget::Host).unwrap();
        let modified = uninstall_kiro_hooks(&config_path).unwrap();
        assert!(modified);
        // File still exists (has name/tools fields) but hooks are gone
        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        assert!(config.get("hooks").is_none());
    }

    #[test]
    fn test_uninstall_kiro_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("aoe-hooks.json");
        std::fs::write(
            &config_path,
            r#"{"hooks": {"preToolUse": [{"command": "echo user-hook"}]}}"#,
        )
        .unwrap();

        install_kiro_hooks(&config_path, HookInstallTarget::Host).unwrap();
        let modified = uninstall_kiro_hooks(&config_path).unwrap();
        assert!(modified);

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        let pre_tool = config["hooks"]["preToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(pre_tool[0]["command"].as_str().unwrap(), "echo user-hook");
    }

    #[test]
    fn test_uninstall_kiro_hooks_nonexistent_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("nonexistent.json");
        let modified = uninstall_kiro_hooks(&config_path).unwrap();
        assert!(!modified);
    }

    #[test]
    fn test_resolve_kiro_agent_file_falls_back_to_name_json_when_dir_absent() {
        // Brand-new agent, nothing on disk yet: resolve to `<dir>/<name>.json`,
        // the path Kiro will read the name from once the file is written.
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("agents");
        assert_eq!(
            resolve_kiro_agent_file(&missing, "custom-agent"),
            missing.join("custom-agent.json")
        );
    }

    #[test]
    fn test_resolve_kiro_agent_file_falls_back_when_no_name_matches() {
        // The directory exists and has files, but none declares the selected
        // name. Fall back to the create-path so a new agent can be added.
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("something-else.json"),
            r#"{"name":"something-else"}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_kiro_agent_file(dir.path(), "custom-agent"),
            dir.path().join("custom-agent.json")
        );
    }

    #[test]
    fn test_resolve_kiro_agent_file_matches_name_field_not_filename() {
        // Regression: a generator renders the `custom-agent` agent under a
        // prefixed filename, so the file Kiro loads for `--agent custom-agent`
        // is name-matched, not stem-matched. The resolver must return it and
        // ignore a decoy whose stem matches but whose `name` does not.
        let dir = TempDir::new().unwrap();
        let managed_file = dir.path().join("TeamAgents-custom-agent.json");
        std::fs::write(
            &managed_file,
            r#"{"name":"custom-agent","hooks":{"agentSpawn":[{"command":"team-tool emit"}]}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("custom-agent.json"),
            r#"{"name":"aoe-hooks"}"#,
        )
        .unwrap();

        assert_eq!(
            resolve_kiro_agent_file(dir.path(), "custom-agent"),
            managed_file,
            "must resolve by the JSON name field, not the filename stem"
        );
    }

    #[test]
    fn test_resolve_kiro_agent_file_ignores_non_json_and_invalid_files() {
        // Unreadable-as-JSON and non-.json files in the dir must not break the
        // scan or produce a false match.
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("notes.txt"), "name: custom-agent").unwrap();
        std::fs::write(dir.path().join("broken.json"), "{ not json").unwrap();
        let good = dir.path().join("AcmePkg-custom-agent.json");
        std::fs::write(&good, r#"{"name":"custom-agent"}"#).unwrap();
        assert_eq!(resolve_kiro_agent_file(dir.path(), "custom-agent"), good);
    }

    #[test]
    fn test_resolve_kiro_agent_file_duplicate_names_pick_lexicographic_first() {
        // Two files declaring the same name is a misconfiguration; resolution
        // must be deterministic (lexicographically-first filename) so the
        // install target does not flip between launches.
        let dir = TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("ZZZ-custom.json"),
            r#"{"name":"custom-agent"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("AAA-custom.json"),
            r#"{"name":"custom-agent"}"#,
        )
        .unwrap();
        assert_eq!(
            resolve_kiro_agent_file(dir.path(), "custom-agent"),
            dir.path().join("AAA-custom.json"),
        );
    }

    #[test]
    fn test_install_into_selected_kiro_agent_creates_file() {
        // No pre-existing agent file: installing into the selected agent's path
        // creates it with AoE hooks, just like the dedicated aoe-hooks agent.
        // Mirrors how `install_sidecar_host_hooks` resolves the path then calls
        // the sidecar installer.
        let agents_dir = TempDir::new().unwrap();
        let path = resolve_kiro_agent_file(agents_dir.path(), "custom-agent");
        install_kiro_hooks(&path, HookInstallTarget::Host).unwrap();

        let config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The created file's name must match the selected agent, not the
        // standalone "aoe-hooks" default, so Kiro loads it for --agent custom-agent.
        assert_eq!(config["name"].as_str(), Some("custom-agent"));
        for event in sidecar_default_events("kiro") {
            let entries = config["hooks"][event.name].as_array().unwrap();
            assert_eq!(
                entries.len(),
                1,
                "event {} should have one AoE entry",
                event.name
            );
            assert!(is_aoe_hook_command(entries[0]["command"].as_str().unwrap()));
        }
    }

    #[test]
    fn test_install_into_selected_kiro_agent_preserves_user_config() {
        // A real user agent has its own name/prompt/tools/hooks. Installing must
        // keep all of that and only add AoE hook entries. The file uses the
        // `<prefix>-<name>.json` generator convention to prove the resolver
        // targets it by `name` rather than the filename stem.
        let agents_dir = TempDir::new().unwrap();
        std::fs::write(
            agents_dir.path().join("CustomPkg-custom-agent.json"),
            r#"{"name":"custom-agent","prompt":"custom helper","tools":["read","shell"],"hooks":{"preToolUse":[{"command":"echo mine"}]}}"#,
        )
        .unwrap();
        let path = resolve_kiro_agent_file(agents_dir.path(), "custom-agent");
        assert_eq!(
            path,
            agents_dir.path().join("CustomPkg-custom-agent.json"),
            "resolver must target the name-matched file, not custom-agent.json"
        );

        install_kiro_hooks(&path, HookInstallTarget::Host).unwrap();

        let config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // User fields untouched.
        assert_eq!(config["name"].as_str().unwrap(), "custom-agent");
        assert_eq!(config["prompt"].as_str().unwrap(), "custom helper");
        assert_eq!(config["tools"].as_array().unwrap().len(), 2);
        // User's own preToolUse hook is preserved, AoE's appended after it.
        let pre = config["hooks"]["preToolUse"].as_array().unwrap();
        assert_eq!(pre[0]["command"].as_str().unwrap(), "echo mine");
        assert!(pre
            .iter()
            .any(|h| is_aoe_hook_command(h["command"].as_str().unwrap())));
        // And it's reversible without harming the user's hook.
        assert!(uninstall_kiro_hooks(&path).unwrap());
        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let pre_after = after["hooks"]["preToolUse"].as_array().unwrap();
        assert_eq!(pre_after.len(), 1);
        assert_eq!(pre_after[0]["command"].as_str().unwrap(), "echo mine");
    }

    fn run_session_id_hook(payload: &str, instance_id: &str, base: &Path) -> std::process::Output {
        let cmd = hook_command_session_id_sandbox(base.to_str().unwrap());
        let mut child = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .env("AOE_INSTANCE_ID", instance_id)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        use std::io::Write;
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(payload.as_bytes())
            .unwrap();
        child.wait_with_output().expect("wait sh")
    }

    /// Skip end-to-end sandbox shell tests when `jq` is absent. The
    /// default `aoe-sandbox` image bundles `jq`; minimal CI runners or
    /// stripped dev environments may not. Without it the body silent-fails
    /// at the `command -v jq` gate and writes no sidecar, which is the
    /// contract, not a test failure.
    ///
    /// Probes through `sh -c "command -v jq ..."` rather than
    /// `Command::new("jq")` so the gate asks the same question the
    /// installed hook asks at runtime (Claude spawns the hook via
    /// `sh -c <body>`; `command -v` resolves against the inherited
    /// `$PATH`). Switching to a direct `Command::new("jq")` probe would
    /// test a different code path with the same answer on common systems.
    fn skip_if_no_jq() -> bool {
        let absent = std::process::Command::new("sh")
            .args(["-c", "command -v jq >/dev/null 2>&1"])
            .status()
            .map(|s| !s.success())
            .unwrap_or(true);
        if absent {
            eprintln!("skipping: jq not on PATH");
        }
        absent
    }

    #[test]
    fn test_hook_command_session_id_extracts_from_compact_payload() {
        if skip_if_no_jq() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = format!(r#"{{"session_id":"{uuid}","cwd":"/x"}}"#);
        let output = run_session_id_hook(&payload, "extract_compact", tmp.path());
        assert!(output.status.success());
        let written =
            std::fs::read_to_string(tmp.path().join("extract_compact").join("session_id"))
                .expect("sidecar file");
        assert_eq!(written, uuid);
    }

    #[test]
    fn test_hook_command_session_id_ignores_user_prompt_injection() {
        if skip_if_no_jq() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let real = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let fake = "11111111-2222-3333-4444-555555555555";
        let payload = format!(r#"{{"session_id":"{real}","prompt":"\"session_id\":\"{fake}\""}}"#);
        let output = run_session_id_hook(&payload, "prompt_injection", tmp.path());
        assert!(output.status.success());
        let written =
            std::fs::read_to_string(tmp.path().join("prompt_injection").join("session_id"))
                .expect("sidecar file");
        assert_eq!(written, real);
    }

    /// Top-level `session_id` wins even when a nested `session_id` appears
    /// textually earlier in the payload. Locks the fix for issue #1760:
    /// the previous shell pipeline's `[{,]` regex anchor picked the
    /// textually-first match, so a payload of the shape
    /// `{"context":{"session_id":NESTED},"session_id":TOP}` wrote NESTED.
    #[test]
    fn test_hook_command_session_id_sandbox_extracts_top_level_over_nested() {
        if skip_if_no_jq() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let nested = "11111111-2222-3333-4444-555555555555";
        let top_level = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload =
            format!(r#"{{"context":{{"session_id":"{nested}"}},"session_id":"{top_level}"}}"#);
        let output = run_session_id_hook(&payload, "sandbox_top_wins", tmp.path());
        assert!(output.status.success());
        let written =
            std::fs::read_to_string(tmp.path().join("sandbox_top_wins").join("session_id"))
                .expect("sidecar file");
        assert_eq!(
            written, top_level,
            "structural top-level extraction must beat a textually-earlier nested `session_id`"
        );
    }

    #[test]
    fn test_hook_command_session_id_extracts_from_multi_line_payload() {
        if skip_if_no_jq() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload = format!("{{\n  \"session_id\":\"{uuid}\",\n  \"cwd\":\"/x\"\n}}");
        let output = run_session_id_hook(&payload, "multi_line", tmp.path());
        assert!(output.status.success());
        let written = std::fs::read_to_string(tmp.path().join("multi_line").join("session_id"))
            .expect("sidecar file");
        assert_eq!(written, uuid);
    }

    #[test]
    fn test_hook_command_session_id_accepts_uppercase_uuid() {
        if skip_if_no_jq() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let uuid = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";
        let payload = format!(r#"{{"session_id":"{uuid}"}}"#);
        let output = run_session_id_hook(&payload, "uppercase_uuid", tmp.path());
        assert!(output.status.success());
        let written = std::fs::read_to_string(tmp.path().join("uppercase_uuid").join("session_id"))
            .expect("sidecar file");
        assert_eq!(written, uuid);
    }

    #[test]
    fn test_hook_command_session_id_skips_when_no_session_id() {
        if skip_if_no_jq() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"cwd":"/x","other":"value"}"#;
        let output = run_session_id_hook(payload, "no_sid", tmp.path());
        assert!(output.status.success());
        let path = tmp.path().join("no_sid").join("session_id");
        assert!(!path.exists());
    }

    /// End-to-end check against a Claude `PreToolUse` shape: pretty-printed
    /// JSON, nested `tool_input` carrying its own fields, a user `prompt`
    /// containing a literal `"session_id":"<uuid>"` substring (which would
    /// have fooled the legacy grep pipeline), and the real `session_id`
    /// sitting at the top level.
    #[test]
    fn test_hook_command_session_id_sandbox_extracts_from_real_claude_payload() {
        if skip_if_no_jq() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let real = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let decoy = "11111111-2222-3333-4444-555555555555";
        let payload = format!(
            r##"{{
  "hook_event_name": "PreToolUse",
  "transcript_path": "/tmp/t.jsonl",
  "cwd": "/workspace",
  "tool_name": "Bash",
  "tool_input": {{
    "command": "echo hi",
    "session_id": "{decoy}"
  }},
  "prompt": "please do \"session_id\":\"{decoy}\" thing",
  "session_id": "{real}"
}}"##
        );
        let output = run_session_id_hook(&payload, "real_claude_payload", tmp.path());
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let written =
            std::fs::read_to_string(tmp.path().join("real_claude_payload").join("session_id"))
                .expect("sidecar file");
        assert_eq!(
            written, real,
            "top-level `session_id` must win over a same-shape value nested under \
             `tool_input` and over a literal substring inside a `prompt` field"
        );
    }

    #[test]
    fn test_hook_command_session_id_host_invokes_aoe_subcommand() {
        let cmd = hook_command_session_id(HookInstallTarget::Host);
        assert!(
            cmd.contains("aoe __extract-session-id"),
            "host hook should invoke the Rust subcommand, got: {cmd}"
        );
        assert!(
            cmd.contains("command -v aoe"),
            "host hook should guard on `aoe` being on PATH, got: {cmd}"
        );
        assert!(
            cmd.contains(AOE_HOOK_MARKER),
            "host hook must carry the AoE marker so uninstall can find it, got: {cmd}"
        );
        assert!(
            !cmd.contains("grep -oE"),
            "host hook must not use the legacy GNU/BSD grep pipeline, got: {cmd}"
        );
    }

    #[test]
    fn test_hook_command_session_id_sandbox_invokes_jq_extractor() {
        let cmd = hook_command_session_id(HookInstallTarget::Sandbox);
        assert!(
            cmd.contains("jq -r"),
            "sandbox hook must invoke `jq -r` for structural top-level extraction, got: {cmd}"
        );
        assert!(
            cmd.contains("command -v jq"),
            "sandbox hook must guard on `jq` being on PATH, got: {cmd}"
        );
        assert!(
            !cmd.contains("grep -oE"),
            "sandbox hook must not use the legacy GNU/BSD grep pipeline, got: {cmd}"
        );
        assert!(
            !cmd.contains("aoe __extract-session-id"),
            "sandbox hook must not invoke the Rust subcommand (not in the image), got: {cmd}"
        );
        assert!(
            cmd.contains(AOE_HOOK_MARKER),
            "sandbox hook must carry the AoE marker, got: {cmd}"
        );
    }

    #[test]
    fn test_build_aoe_hooks_emits_session_id_capture_for_session_start() {
        let events = claude_events();
        let hooks = build_aoe_hooks(events, HookInstallTarget::Sandbox);
        let session_start = hooks
            .get("SessionStart")
            .expect("SessionStart matcher block")
            .as_array()
            .unwrap();
        assert_eq!(session_start.len(), 1);
        let entries = session_start[0]["hooks"].as_array().unwrap();
        assert_eq!(entries.len(), 1, "SessionStart should emit 1 hook command");
        let cmd = entries[0]["command"].as_str().unwrap();
        assert!(cmd.contains("session_id"));
        assert!(cmd.contains(AOE_HOOK_MARKER));
    }

    #[test]
    fn test_build_aoe_hooks_emits_both_for_user_prompt_submit() {
        let events = claude_events();
        let hooks = build_aoe_hooks(events, HookInstallTarget::Sandbox);
        let user_prompt = hooks
            .get("UserPromptSubmit")
            .expect("UserPromptSubmit matcher block")
            .as_array()
            .unwrap();
        let entries = user_prompt[0]["hooks"].as_array().unwrap();
        assert_eq!(
            entries.len(),
            2,
            "UserPromptSubmit should emit status + session_id_capture"
        );
        let commands: Vec<&str> = entries
            .iter()
            .map(|e| e["command"].as_str().unwrap())
            .collect();
        assert!(commands.iter().any(|c| c.contains("printf running")));
        assert!(commands.iter().any(|c| c.contains("session_id")));
    }

    #[test]
    fn test_build_aoe_hooks_status_only_events_unchanged() {
        let events = claude_events();
        let hooks = build_aoe_hooks(events, HookInstallTarget::Sandbox);
        for event_name in &["PreToolUse", "Stop", "Notification", "ElicitationResult"] {
            let block = hooks
                .get(*event_name)
                .unwrap_or_else(|| panic!("expected {event_name}"))
                .as_array()
                .unwrap();
            let entries = block[0]["hooks"].as_array().unwrap();
            assert_eq!(
                entries.len(),
                1,
                "status-only event {event_name} should emit 1 hook"
            );
        }
    }

    #[test]
    fn hook_command_with_base_quotes_and_guards() {
        let cmd = hook_command_with_base("running", "/tmp/aoe-hooks", HookInstallTarget::Host);
        assert!(
            cmd.contains("case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac"),
            "missing instance-id allowlist: {cmd}"
        );
        assert!(cmd.contains("unset IFS"), "missing IFS pin: {cmd}");
        assert!(cmd.contains("set -f"), "missing globbing pin: {cmd}");
        assert!(cmd.contains("umask 077"), "missing umask pin: {cmd}");
        assert!(
            cmd.contains("LC_ALL=C ls -ldn"),
            "missing locale-pinned ls: {cmd}"
        );
        assert!(
            cmd.contains("drwx------|drwx------.|drwx------+|drwx------@"),
            "missing strict 0700 mode pattern: {cmd}"
        );
        assert!(
            cmd.contains("ME=$(id -u 2>/dev/null)"),
            "host hook MUST include id-u uid check: {cmd}"
        );
        assert!(
            cmd.contains("B=/tmp/aoe-hooks"),
            "base must be baked: {cmd}"
        );
        assert!(
            cmd.contains("D=\"$B/$AOE_INSTANCE_ID\""),
            "instance dir must be baked under base: {cmd}"
        );
        assert!(
            cmd.contains("printf running > \"$D/status\""),
            "status writer must target the per-instance subdir: {cmd}"
        );
        assert!(
            cmd.contains(&format!("# {AOE_HOOK_MARKER}")),
            "marker substring must be present: {cmd}"
        );
    }

    #[test]
    fn hook_command_session_id_sandbox_quotes_and_guards() {
        let cmd = hook_command_session_id_sandbox("/tmp/aoe-hooks");
        assert!(
            cmd.contains("case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac"),
            "missing instance-id allowlist: {cmd}"
        );
        assert!(cmd.contains("unset IFS"), "missing IFS pin: {cmd}");
        assert!(cmd.contains("set -f"), "missing globbing pin: {cmd}");
        assert!(cmd.contains("umask 077"), "missing umask pin: {cmd}");
        assert!(
            cmd.contains("LC_ALL=C ls -ldn"),
            "missing locale-pinned ls: {cmd}"
        );
        assert!(
            cmd.contains("drwx------|drwx------.|drwx------+|drwx------@"),
            "missing strict 0700 mode pattern: {cmd}"
        );
        assert!(
            cmd.contains("D=\"/tmp/aoe-hooks/$AOE_INSTANCE_ID\""),
            "missing instance dir construction: {cmd}"
        );
        assert!(
            cmd.contains("command -v jq >/dev/null 2>&1 || exit 0"),
            "missing jq presence gate: {cmd}"
        );
        assert!(cmd.contains("jq -r "), "missing jq invocation: {cmd}");
        assert!(
            cmd.contains(".session_id|type"),
            "missing jq string-type gate: {cmd}"
        );
        assert!(
            cmd.contains("H=[0-9a-fA-F]"),
            "missing hex-class abbreviation for UUID case: {cmd}"
        );
        assert!(
            cmd.contains(".session_id.$$.tmp"),
            "missing atomic .tmp + mv write: {cmd}"
        );
        assert!(
            cmd.contains(&format!("# {AOE_HOOK_MARKER}")),
            "missing trailing AoE marker: {cmd}"
        );
        assert!(
            !cmd.contains("grep -oE"),
            "legacy grep pipeline must be gone: {cmd}"
        );
    }

    #[test]
    fn sandbox_shell_byte_equality() {
        let cmd = canonical_status_command("running", HookInstallTarget::Sandbox);
        for token in [
            "unset IFS",
            "set -f",
            "umask 077",
            "case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*)",
            "B=/tmp/aoe-hooks;",
            "D=\"$B/$AOE_INSTANCE_ID\"",
            "LC_ALL=C ls -ldn",
            "drwx------|drwx------.|drwx------+|drwx------@",
            "printf running",
        ] {
            assert!(cmd.contains(token), "missing token {token:?}: {cmd}");
        }
        for forbidden in ["ME=$(id -u", "[ \"$3\" = \"$ME\" ]", "/tmp/aoe-hooks-"] {
            assert!(
                !cmd.contains(forbidden),
                "sandbox snippet must NOT contain {forbidden:?}: {cmd}"
            );
        }
        assert!(cmd.contains(&format!("# {AOE_HOOK_MARKER}")));
    }

    #[test]
    #[serial_test::serial(hook_base)]
    fn host_shell_bakes_per_user_base_byte_stable_across_mocks() {
        // Drop guard so the thread-local override clears even if an
        // assertion below panics, preventing leakage into other
        // serial(hook_base) tests.
        struct ClearOnDrop;
        impl Drop for ClearOnDrop {
            fn drop(&mut self) {
                crate::hooks::dir_guard::clear_base_override_for_test();
            }
        }
        let _g = ClearOnDrop;
        for euid in [1000u32, 65534u32, 0u32] {
            let mock_base = std::path::PathBuf::from(format!("/tmp/aoe-hooks-{euid}"));
            crate::hooks::dir_guard::override_base_for_test(mock_base.clone());
            let cmd = canonical_status_command("running", HookInstallTarget::Host);
            let want = format!("B=/tmp/aoe-hooks-{euid};");
            assert!(
                cmd.contains(&want),
                "euid {euid}: expected {want:?} in: {cmd}"
            );
            assert!(
                cmd.contains("ME=$(id -u 2>/dev/null)"),
                "host hook must include id-u uid check: {cmd}"
            );
        }
    }

    #[test]
    fn shell_guard_actually_rejects_traversal() {
        // Nesting <tmp>/level1/base + canary file: any escape (one or
        // two levels) or in-place mkdir/write surfaces in one of the
        // three read_dir assertions below.
        let tmp = tempfile::tempdir().unwrap();
        let level1 = tmp.path().join("level1");
        std::fs::create_dir(&level1).unwrap();
        let base = level1.join("base");
        std::fs::create_dir(&base).unwrap();
        let canary_name = ".canary-deadbeef";
        std::fs::write(base.join(canary_name), b"do not delete").unwrap();
        let cmd =
            hook_command_with_base("running", base.to_str().unwrap(), HookInstallTarget::Host);

        for poisoned in ["..", "../../escape", "/etc", "foo/bar", "; rm -rf /;", ""] {
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .env("AOE_INSTANCE_ID", poisoned)
                .status()
                .unwrap();
            assert!(status.success(), "hook MUST exit 0 (id={poisoned:?})");
        }

        let base_entries: Vec<_> = std::fs::read_dir(&base)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            base_entries,
            vec![std::ffi::OsString::from(canary_name)],
            "shell guard must prevent any mkdir/write under {:?}",
            base
        );

        let level1_entries: Vec<_> = std::fs::read_dir(&level1)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            level1_entries,
            vec![std::ffi::OsString::from("base")],
            "shell guard must prevent one-level escape into {:?}",
            level1
        );

        let tmp_entries: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            tmp_entries,
            vec![std::ffi::OsString::from("level1")],
            "shell guard must prevent two-level escape into {:?}",
            tmp.path()
        );
    }

    fn dash_available() -> bool {
        std::process::Command::new("dash")
            .arg("-c")
            .arg("exit 0")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    #[test]
    fn host_shell_in_dash_refuses_wrong_mode() {
        if !dash_available() {
            eprintln!("skipping: dash not available");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("aoe-hooks-mode");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cmd =
            hook_command_with_base("running", base.to_str().unwrap(), HookInstallTarget::Host);
        let output = std::process::Command::new("dash")
            .args(["-c", &cmd])
            .env("AOE_INSTANCE_ID", "dash_wrong_mode")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "hook must exit 0 even on rejection"
        );
        assert!(
            !base.join("dash_wrong_mode").exists(),
            "dash must reject 0o755 parent and refuse to mkdir under it"
        );
    }

    #[test]
    fn host_shell_handles_hostile_ifs() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("aoe-hooks-ifs");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cmd =
            hook_command_with_base("running", base.to_str().unwrap(), HookInstallTarget::Host);
        let output = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .env("AOE_INSTANCE_ID", "ifs_hostile")
            .env("IFS", "d")
            .output()
            .unwrap();
        assert!(output.status.success());
        let status_path = base.join("ifs_hostile").join("status");
        assert_eq!(
            std::fs::read_to_string(&status_path).unwrap(),
            "running",
            "snippet must reset IFS and write status despite hostile inherited IFS=d"
        );
    }

    #[test]
    fn host_shell_handles_hostile_umask() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("aoe-hooks-umask");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cmd =
            hook_command_with_base("running", base.to_str().unwrap(), HookInstallTarget::Host);
        let output = std::process::Command::new("sh")
            .args(["-c", &format!("umask 022; {cmd}")])
            .env("AOE_INSTANCE_ID", "umask_hostile")
            .output()
            .unwrap();
        assert!(output.status.success());
        let inst = base.join("umask_hostile");
        assert!(inst.exists(), "instance dir must be created");
        let mode = std::fs::metadata(&inst).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "snippet must override caller umask 022 to mkdir 0o700; got {mode:o}"
        );
    }

    #[test]
    fn host_shell_set_f_blocks_glob_expansion_in_cwd() {
        // Belt-and-suspenders functional test: even though `set -- $LS`
        // would never word-split a glob character today (uid/gid are
        // integers, the mode glyphs and date format are fixed by
        // LC_ALL=C, the path is controlled), we plant decoy files in
        // cwd that any glob expansion would match. With `set -f` in the
        // preamble, no decoy is touched. A future regression that drops
        // `set -f` AND introduces a glob vector would surface as a
        // missing decoy or a hung process.
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("aoe-hooks-glob");
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        let cwd = tmp.path().join("cwd_with_decoys");
        std::fs::create_dir(&cwd).unwrap();
        for name in [
            "glob-decoy-1",
            "glob-decoy-2",
            "drwxrwxrwx",
            "1000",
            "65534",
        ] {
            std::fs::write(cwd.join(name), b"untouched").unwrap();
        }
        let cmd =
            hook_command_with_base("running", base.to_str().unwrap(), HookInstallTarget::Host);
        let output = std::process::Command::new("sh")
            .args(["-c", &cmd])
            .current_dir(&cwd)
            .env("AOE_INSTANCE_ID", "glob_bait")
            .output()
            .unwrap();
        assert!(output.status.success(), "snippet must exit 0");
        let status_path = base.join("glob_bait").join("status");
        assert_eq!(
            std::fs::read_to_string(&status_path).unwrap(),
            "running",
            "status file must be written despite cwd full of glob bait"
        );
        for name in [
            "glob-decoy-1",
            "glob-decoy-2",
            "drwxrwxrwx",
            "1000",
            "65534",
        ] {
            assert_eq!(
                std::fs::read_to_string(cwd.join(name)).unwrap(),
                "untouched",
                "decoy {name} must be untouched"
            );
        }
    }

    #[test]
    fn install_hooks_under_8_threads_byte_canonical_final_state() {
        // Eight installers race on the same settings.json. The advisory
        // lock must serialise read-modify-write so the final state is
        // byte-equal to a single sequential install.
        use std::sync::{Arc, Barrier};
        let tmp = tempfile::tempdir().unwrap();
        let settings_path = tmp.path().join("settings.json");
        let events = claude_events();

        install_hooks(&settings_path, &events, HookInstallTarget::Host).unwrap();
        let canonical = std::fs::read_to_string(&settings_path).unwrap();
        std::fs::remove_file(&settings_path).unwrap();

        let barrier = Arc::new(Barrier::new(8));
        std::thread::scope(|s| {
            for _ in 0..8 {
                let path = settings_path.clone();
                let b = barrier.clone();
                let events = &events;
                s.spawn(move || {
                    b.wait();
                    for _ in 0..100 {
                        install_hooks(&path, events, HookInstallTarget::Host).unwrap();
                    }
                });
            }
        });
        let final_state = std::fs::read_to_string(&settings_path).unwrap();
        assert_eq!(
            final_state, canonical,
            "concurrent installs must converge to the same canonical bytes as a single sequential install"
        );
    }

    #[test]
    fn install_lock_drops_on_panic_then_acquires_again() {
        // The advisory lock must release when its owning closure panics,
        // otherwise a single buggy install would deadlock every later
        // installer in the same process. Lock release rides the
        // `lock_file` drop on unwind because `with_config_lock` keeps
        // `lock_file` on the stack across the `f()` call.
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("panicky.json");
        let panicked = std::panic::catch_unwind(|| {
            with_config_lock(&target, "json.lock", || -> Result<()> {
                panic!("boom");
            })
        });
        assert!(panicked.is_err(), "closure panic must propagate");
        let started = std::time::Instant::now();
        with_config_lock(&target, "json.lock", || -> Result<()> { Ok(()) })
            .expect("second acquire must succeed after the first panicked");
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "lock should release immediately on panic, took {elapsed:?}"
        );
    }

    #[derive(Clone)]
    struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = CaptureWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture_warns<F: FnOnce()>(f: F) -> String {
        use tracing_subscriber::layer::SubscriberExt;
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let writer = CaptureWriter(buf.clone());
        let subscriber = tracing_subscriber::Registry::default().with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false)
                .with_target(true),
        );
        tracing::subscriber::with_default(subscriber, f);
        let bytes = buf.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[test]
    #[serial_test::serial(shell_env)]
    fn collect_env_lists_warns_on_corrupt_global_config_but_not_on_missing() {
        let tmp = TempDir::new().unwrap();
        let _codex = CodexHomeGuard::unset();
        let _home = EnvGuard::set("HOME", tmp.path().to_str().unwrap());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let _xdg = EnvGuard::set(
            "XDG_CONFIG_HOME",
            tmp.path().join(".config").to_str().unwrap(),
        );

        let app_dir = crate::session::get_app_dir().unwrap();
        let config_path = app_dir.join("config.toml");

        let logs_missing = capture_warns(|| {
            assert!(!config_path.exists(), "test precondition: no config.toml");
            let _ = collect_env_lists_from_session();
        });
        assert!(
            !logs_missing.contains("Failed to load global config"),
            "missing config.toml must not warn; captured: {logs_missing}"
        );

        std::fs::write(&config_path, "this = is = not = toml\n").unwrap();
        let logs_parse = capture_warns(|| {
            let _ = collect_env_lists_from_session();
        });
        let warn_count = logs_parse.matches("Failed to load global config").count();
        assert_eq!(
            warn_count, 1,
            "malformed config.toml must surface exactly one warn; captured: {logs_parse}"
        );
        assert!(
            logs_parse.contains("session.store"),
            "warn must carry the load_or_warn target; captured: {logs_parse}"
        );
    }

    #[test]
    fn is_aoe_hook_command_distinguishes_ours_from_user_commands() {
        let aoe_emitted = [
            canonical_status_command("running", HookInstallTarget::Host),
            canonical_status_command("waiting", HookInstallTarget::Sandbox),
            canonical_session_id_command(HookInstallTarget::Host),
            canonical_session_id_command(HookInstallTarget::Sandbox),
        ];
        for cmd in &aoe_emitted {
            assert!(
                is_aoe_hook_command(cmd),
                "every shipping AoE emitter must match: {cmd}"
            );
        }

        let user_commands = [
            "ls /tmp/aoe-hooks",
            "echo 'cleaning aoe-hooks dir'",
            "rm -rf /tmp/aoe-hooks-1000",
            "cat /var/log/aoe-hooks.log",
            "sh -c 'aoe-hooks stuff'",
            "aoe-hooks --foo",
            "echo aoe-hooks",
            "echo \" # aoe-hooks comment\"",
            "bash -c \"cd ~ && # aoe-hooks placeholder\nls\"",
            "# aoe-hooks: clean up tmp dir",
            "echo '# aoe-hooks'",
            "echo \"# aoe-hooks\"",
            "X='hidden # aoe-hooks'",
            "say 'task done: # aoe-hooks'",
        ];
        for cmd in &user_commands {
            assert!(
                !is_aoe_hook_command(cmd),
                "user command containing the marker substring must not match: {cmd}"
            );
        }

        let legacy_sandbox_session_id = "sh -c 'unset IFS; set -f; umask 077; \
             [ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
             D=\"/tmp/aoe-hooks/$AOE_INSTANCE_ID\"; mkdir -p \"$D\" 2>/dev/null; \
             exit 0'";
        assert!(
            is_aoe_hook_command(legacy_sandbox_session_id),
            "legacy sandbox session-id (no trailing comment) must match via path sentinel"
        );
        let pre_2168_host_status = "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
             case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
             mkdir -p \"/tmp/aoe-hooks/$AOE_INSTANCE_ID\" 2>/dev/null; \
             printf running > \"/tmp/aoe-hooks/$AOE_INSTANCE_ID/status\" 2>/dev/null; \
             exit 0'";
        assert!(
            is_aoe_hook_command(pre_2168_host_status),
            "pre-#2168 host status (no trailing comment) must match via path sentinel"
        );

        let sandbox_sid = canonical_session_id_command(HookInstallTarget::Sandbox);
        assert!(
            sandbox_sid.contains(AOE_HOOK_PATH_SENTINEL),
            "sandbox session-id keeps the path-template sentinel"
        );
        assert!(
            sandbox_sid.contains(AOE_HOOK_TRAILING_SENTINEL),
            "sandbox session-id gains the trailing comment sentinel"
        );
    }

    #[test]
    fn every_emitter_is_recognised_by_is_aoe_hook_command() {
        let emitters = [
            (
                "host status (running)",
                canonical_status_command("running", HookInstallTarget::Host),
            ),
            (
                "host status (idle)",
                canonical_status_command("idle", HookInstallTarget::Host),
            ),
            (
                "sandbox status (waiting)",
                canonical_status_command("waiting", HookInstallTarget::Sandbox),
            ),
            (
                "host session-id",
                canonical_session_id_command(HookInstallTarget::Host),
            ),
            (
                "sandbox session-id",
                canonical_session_id_command(HookInstallTarget::Sandbox),
            ),
        ];
        for (label, cmd) in &emitters {
            assert!(
                is_aoe_hook_command(cmd),
                "{label} must be recognised by is_aoe_hook_command: {cmd}"
            );
        }
    }

    #[test]
    fn aoe_hook_sentinels_embed_marker() {
        assert!(
            AOE_HOOK_TRAILING_SENTINEL.contains(AOE_HOOK_MARKER),
            "trailing sentinel must embed AOE_HOOK_MARKER verbatim"
        );
        assert!(
            AOE_HOOK_PATH_SENTINEL.contains(AOE_HOOK_MARKER),
            "path sentinel must embed AOE_HOOK_MARKER verbatim"
        );
        assert!(
            HOOK_STATUS_BASE_IN_CONTAINER.contains(AOE_HOOK_MARKER),
            "container base path must embed AOE_HOOK_MARKER verbatim"
        );
    }

    #[test]
    fn test_install_hooks_preserves_statusline_foreign_key() {
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        let existing = serde_json::json!({
            "statusLine": {"type": "command", "command": "my-status"},
            "model": "opus",
            "hooks": {}
        });
        std::fs::write(
            &settings_path,
            serde_json::to_string_pretty(&existing).unwrap(),
        )
        .unwrap();

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();

        let content: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings_path).unwrap()).unwrap();
        assert_eq!(content["statusLine"]["type"], "command");
        assert_eq!(content["statusLine"]["command"], "my-status");
        assert_eq!(content["model"], "opus");
        assert!(
            content["hooks"].is_object(),
            "AoE hooks subtree must be present"
        );
        assert!(
            content["hooks"]["SessionStart"].is_array(),
            "AoE SessionStart hook must be installed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_install_hooks_no_rewrite_when_unchanged() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let settings_path = tmp.path().join("settings.json");

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();
        let ino_before = std::fs::metadata(&settings_path).unwrap().ino();
        let bytes_before = std::fs::read(&settings_path).unwrap();

        install_hooks(&settings_path, claude_events(), HookInstallTarget::Host).unwrap();
        assert_eq!(
            std::fs::metadata(&settings_path).unwrap().ino(),
            ino_before,
            "second install must not replace the inode"
        );
        assert_eq!(
            std::fs::read(&settings_path).unwrap(),
            bytes_before,
            "second install must leave bytes byte-identical"
        );
    }

    #[test]
    fn test_install_settl_hooks_preserves_foreign_root_key() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(&config_path, "model = \"settl-pro\"\n").unwrap();

        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let config: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(config["model"].as_str(), Some("settl-pro"));
        assert!(config["hooks"].is_array(), "AoE hooks must be installed");
    }

    #[cfg(unix)]
    #[test]
    fn test_install_settl_hooks_no_rewrite_when_unchanged() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();
        let ino_before = std::fs::metadata(&config_path).unwrap().ino();
        let bytes_before = std::fs::read(&config_path).unwrap();

        install_settl_hooks(&config_path, HookInstallTarget::Host).unwrap();
        assert_eq!(
            std::fs::metadata(&config_path).unwrap().ino(),
            ino_before,
            "second install must not replace the inode"
        );
        assert_eq!(
            std::fs::read(&config_path).unwrap(),
            bytes_before,
            "second install must leave bytes byte-identical"
        );
    }

    #[test]
    fn test_install_hermes_hooks_preserves_foreign_yaml_and_allowlist_keys() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        std::fs::write(
            &config_path,
            "model: hermes-pro\nhooks_auto_accept: false\n",
        )
        .unwrap();
        let allowlist_path = tmp.path().join("shell-hooks-allowlist.json");
        std::fs::write(&allowlist_path, "{\"version\":7,\"approvals\":[]}").unwrap();

        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();

        let yaml: serde_yaml::Value =
            serde_yaml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        assert_eq!(
            yaml.get("model").and_then(|v| v.as_str()),
            Some("hermes-pro")
        );
        assert_eq!(
            yaml.get("hooks_auto_accept").and_then(|v| v.as_bool()),
            Some(false)
        );
        assert!(yaml.get("hooks").is_some());

        let allowlist: Value =
            serde_json::from_str(&std::fs::read_to_string(&allowlist_path).unwrap()).unwrap();
        assert_eq!(allowlist["version"], 7);
    }

    #[cfg(unix)]
    #[test]
    fn test_install_hermes_hooks_no_rewrite_when_unchanged() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.yaml");
        let allowlist_path = tmp.path().join("shell-hooks-allowlist.json");

        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();
        let yaml_ino_before = std::fs::metadata(&config_path).unwrap().ino();
        let yaml_bytes_before = std::fs::read(&config_path).unwrap();
        let allowlist_ino_before = std::fs::metadata(&allowlist_path).unwrap().ino();
        let allowlist_bytes_before = std::fs::read(&allowlist_path).unwrap();

        install_hermes_hooks(&config_path, HookInstallTarget::Host).unwrap();

        assert_eq!(
            std::fs::metadata(&config_path).unwrap().ino(),
            yaml_ino_before,
            "config.yaml inode must be stable on a second clean install"
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), yaml_bytes_before);
        assert_eq!(
            std::fs::metadata(&allowlist_path).unwrap().ino(),
            allowlist_ino_before,
            "shell-hooks-allowlist.json inode must be stable on a second clean install"
        );
        assert_eq!(
            std::fs::read(&allowlist_path).unwrap(),
            allowlist_bytes_before
        );
    }

    #[test]
    fn test_install_kiro_hooks_preserves_foreign_root_keys() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("aoe-hooks.json");
        std::fs::write(&path, "{\"description\":\"keep me\",\"version\":3}").unwrap();

        install_kiro_hooks(&path, HookInstallTarget::Host).unwrap();

        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["description"], "keep me");
        assert_eq!(v["version"], 3);
        assert!(v["hooks"].is_object());
        assert!(
            v["hooks"]["preToolUse"].is_array(),
            "AoE preToolUse hook must be installed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_install_kiro_hooks_no_rewrite_when_unchanged() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("aoe-hooks.json");

        install_kiro_hooks(&path, HookInstallTarget::Host).unwrap();
        let ino_before = std::fs::metadata(&path).unwrap().ino();
        let bytes_before = std::fs::read(&path).unwrap();

        install_kiro_hooks(&path, HookInstallTarget::Host).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            ino_before,
            "second install must not replace the inode"
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes_before);
    }

    #[test]
    fn test_install_codex_hooks_preserves_foreign_root_key_and_comment() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        let original = "# user comment\n\
                        model = \"gpt-5.3-codex\"\n\
                        approval_policy = \"on-failure\"\n";
        std::fs::write(&config_path, original).unwrap();

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();

        let after = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            after.contains("# user comment"),
            "user comment must survive toml_edit round-trip; got:\n{after}"
        );
        assert!(after.contains("model = \"gpt-5.3-codex\""));
        assert!(after.contains("approval_policy = \"on-failure\""));
        let parsed: toml::Value = toml::from_str(&after).unwrap();
        assert!(parsed["hooks"]["SessionStart"].is_array());
    }

    #[cfg(unix)]
    #[test]
    fn test_install_codex_hooks_no_rewrite_when_unchanged() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();
        let ino_before = std::fs::metadata(&config_path).unwrap().ino();
        let bytes_before = std::fs::read(&config_path).unwrap();

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();
        assert_eq!(
            std::fs::metadata(&config_path).unwrap().ino(),
            ino_before,
            "second install must not replace the inode"
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), bytes_before);
    }

    #[cfg(unix)]
    #[test]
    fn test_install_codex_with_preserved_state_no_rewrite_when_unchanged() {
        use std::os::unix::fs::MetadataExt;
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");

        install_codex_hooks(&config_path, codex_events(), HookInstallTarget::Host).unwrap();
        let preserved = snapshot_codex_hooks_state(&config_path).unwrap();
        let ino_before = std::fs::metadata(&config_path).unwrap().ino();
        let bytes_before = std::fs::read(&config_path).unwrap();

        install_codex_hooks_with_preserved_state(
            &config_path,
            codex_events(),
            preserved,
            HookInstallTarget::Host,
        )
        .unwrap();

        assert_eq!(
            std::fs::metadata(&config_path).unwrap().ino(),
            ino_before,
            "preserved-state install must not replace the inode when state and events are clean"
        );
        assert_eq!(std::fs::read(&config_path).unwrap(), bytes_before);
    }
}
