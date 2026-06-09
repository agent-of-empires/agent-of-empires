//! Agent hook management for status detection.
//!
//! AoE installs hooks into an agent's settings file that write session
//! status (`running`/`waiting`/`idle`) to a sidecar file. This provides a
//! hook-first status source; agent-specific code may still reconcile known
//! hook gaps from tmux pane content.
//!
//! Hook events are agent-specific and defined in `AgentHookConfig::events`.

mod status_file;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use fs2::FileExt as _;
use serde_json::Value;

pub use status_file::{
    cleanup_hook_status_dir, hook_status_dir, read_hook_session_id, read_hook_status,
    read_hook_urgent,
};

/// Base directory for all AoE hook status files.
pub(crate) const HOOK_STATUS_BASE: &str = "/tmp/aoe-hooks";

/// Marker substring used to identify AoE-managed hooks in settings.json.
/// Any hook command containing this string is considered ours.
const AOE_HOOK_MARKER: &str = "aoe-hooks";

/// Where an agent's settings file lives. Determines which shell command
/// `hook_command_session_id` emits.
///
/// `Host`: emits a call to the `aoe __extract-session-id` Rust subcommand.
/// `Sandbox`: emits a POSIX shell pipeline because `aoe` is not installed
/// inside the sandbox image. The pipeline keeps a known schema-ordering
/// quirk: a textually-earlier nested `session_id` wins over the top-level
/// one, accepted because Claude does not emit such payloads.
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
    if let Some(codex_home) = std::env::var("CODEX_HOME").ok().filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(codex_home).join("config.toml"));
    }

    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(".codex")
        .join("config.toml"))
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

pub(crate) fn codex_config_path_for_host_environment(entries: &[String]) -> Result<PathBuf> {
    if let Some(codex_home) =
        crate::session::environment::resolve_host_environment_value(entries, "CODEX_HOME")
            .filter(|v| !v.is_empty())
    {
        return Ok(PathBuf::from(codex_home).join("config.toml"));
    }

    codex_config_path()
}

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
    if let Some(var) = hook_cfg.config_dir_env_var {
        if let Some(dir) = resolve_config_dir_override(var, host_env) {
            if let Some(file) = Path::new(hook_cfg.settings_rel_path).file_name() {
                return Ok(PathBuf::from(dir).join(file));
            }
        }
    }
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("Cannot determine home directory"))?
        .join(hook_cfg.settings_rel_path))
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

/// Enumerate every settings path AoE might have written hooks to for an agent
/// whose config dir is env-overridable, so uninstall cleans up all of them:
/// the home-relative default plus the resolution under the global config env
/// and each profile's env. Mirrors `codex_config_paths_for_uninstall`.
fn agent_settings_paths_for_uninstall(hook_cfg: &crate::agents::AgentHookConfig) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if let Some(home) = dirs::home_dir() {
        push_unique_path(&mut paths, Ok(home.join(hook_cfg.settings_rel_path)));
    }

    if hook_cfg.config_dir_env_var.is_some() {
        if let Ok(config) = crate::session::config::Config::load() {
            push_unique_path(
                &mut paths,
                agent_settings_path_for_host_environment(hook_cfg, &config.environment),
            );
        }
        match crate::session::list_profiles() {
            Ok(profiles) => {
                for profile in profiles {
                    let environment =
                        crate::session::profile_config::resolve_config_or_warn(&profile)
                            .environment;
                    push_unique_path(
                        &mut paths,
                        agent_settings_path_for_host_environment(hook_cfg, &environment),
                    );
                }
            }
            Err(e) => {
                tracing::warn!(target: "hooks.uninstall", "Failed to list profiles for {} hook cleanup: {}", hook_cfg.settings_rel_path, e)
            }
        }
    }

    paths
}

/// Build the shell command for a hook that writes a status value.
///
/// The command must never exit non-zero, otherwise the agent treats the hook
/// as a blocking failure and refuses to run further tool calls. `/tmp/aoe-hooks/<id>`
/// can disappear mid-session (OS /tmp cleanup, transient FS hiccup, external
/// tooling), so both mkdir and printf must tolerate a missing parent dir. We
/// swallow stderr and force a final `exit 0`: at worst the status file is one
/// tick stale and the next hook call recreates the dir.
fn hook_command(status: &str) -> String {
    hook_command_with_base(status, HOOK_STATUS_BASE)
}

fn hook_command_with_base(status: &str, base: &str) -> String {
    // `[ -n ]` is load-bearing: `*[!...]*` does not match the empty
    // string. `exit 0` on rejection: a non-zero hook exit blocks the
    // agent's tool calls.
    format!(
        "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
         mkdir -p \"{base}/$AOE_INSTANCE_ID\" 2>/dev/null; \
         printf {status} > \"{base}/$AOE_INSTANCE_ID/status\" 2>/dev/null; \
         exit 0'"
    )
}

/// Build the shell command for a hook that extracts `session_id` from the
/// agent's stdin JSON payload and writes it to a sidecar file.
///
/// Both variants must exit 0 even on failure: a non-zero hook blocks the
/// agent's tool calls. The trailing `# AOE_HOOK_MARKER` on the host
/// variant is load-bearing: `is_aoe_hook_command` recognises AoE hooks by
/// substring; the sandbox variant gets the marker via its baked-in
/// `HOOK_STATUS_BASE` path.
///
/// Host-variant silent-failure modes (acceptable, equivalent to a regex
/// miss in the sandbox variant): `aoe` not on PATH at hook-exec time, or
/// a stale `aoe` on PATH that predates `__extract-session-id`. Both yield
/// no sidecar without surfacing an error; session resume falls back to
/// the filesystem scan.
fn hook_command_session_id(target: HookInstallTarget) -> String {
    match target {
        HookInstallTarget::Host => hook_command_session_id_host(),
        HookInstallTarget::Sandbox => hook_command_session_id_sandbox(HOOK_STATUS_BASE),
    }
}

fn hook_command_session_id_host() -> String {
    format!(
        "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         command -v aoe >/dev/null 2>&1 || exit 0; \
         aoe __extract-session-id 2>/dev/null; exit 0 # {AOE_HOOK_MARKER}'"
    )
}

fn hook_command_session_id_sandbox(base: &str) -> String {
    format!(
        "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; \
         case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac; \
         D=\"{base}/$AOE_INSTANCE_ID\"; mkdir -p \"$D\" 2>/dev/null; \
         SID=$(tr -d \"\\n\" | grep -oE \"[{{,][[:space:]]*\\\"session_id\\\"[[:space:]]*:[[:space:]]*\\\"[0-9a-fA-F]{{8}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{12}}\\\"\" | head -1 | grep -oE \"[0-9a-fA-F]{{8}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{4}}-[0-9a-fA-F]{{12}}\"); \
         [ -n \"$SID\" ] && printf \"%s\" \"$SID\" > \"$D/.session_id.$$.tmp\" 2>/dev/null && mv \"$D/.session_id.$$.tmp\" \"$D/session_id\" 2>/dev/null; \
         exit 0'"
    )
}

fn is_aoe_hook_command(cmd: &str) -> bool {
    cmd.contains(AOE_HOOK_MARKER)
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
fn build_aoe_hooks(events: &[crate::agents::HookEvent], target: HookInstallTarget) -> Value {
    let mut hooks_obj = serde_json::Map::new();
    for event in events {
        let mut commands: Vec<String> = Vec::new();
        if event.session_id_capture {
            commands.push(hook_command_session_id(target));
        }
        if let Some(status) = event.status {
            commands.push(hook_command(status));
        }
        if commands.is_empty() {
            continue;
        }

        let mut entry = serde_json::Map::new();
        if let Some(m) = event.matcher {
            entry.insert("matcher".to_string(), Value::String(m.to_string()));
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
        hooks_obj.insert(
            event.name.to_string(),
            Value::Array(vec![Value::Object(entry)]),
        );
    }

    Value::Object(hooks_obj)
}

/// Remove any existing AoE hooks from an event's matcher array.
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
/// If the file doesn't exist, it will be created with just the hooks.
pub fn install_hooks(
    settings_path: &Path,
    events: &[crate::agents::HookEvent],
    target: HookInstallTarget,
) -> Result<()> {
    let mut settings: Value = if settings_path.exists() {
        let content = std::fs::read_to_string(settings_path)?;
        serde_json::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(target: "hooks.install", "Failed to parse {}: {}", settings_path.display(), e);
            serde_json::json!({})
        })
    } else {
        serde_json::json!({})
    };

    let aoe_hooks = build_aoe_hooks(events, target);

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
    for (event_name, aoe_matchers) in aoe_hooks_obj {
        if let Some(existing) = settings_hooks.get_mut(event_name) {
            if let Some(arr) = existing.as_array_mut() {
                // Remove old AoE entries, then append new ones
                remove_aoe_entries(arr);
                if let Some(new_arr) = aoe_matchers.as_array() {
                    arr.extend(new_arr.iter().cloned());
                }
            }
        } else {
            settings_hooks.insert(event_name.clone(), aoe_matchers.clone());
        }
    }

    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let formatted = serde_json::to_string_pretty(&settings)?;
    std::fs::write(settings_path, formatted)?;

    tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", settings_path.display());
    Ok(())
}

const CODEX_HOOK_EVENT_NAMES: &[&str] = &[
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
pub fn install_codex_hooks(config_path: &Path, events: &[crate::agents::HookEvent]) -> Result<()> {
    install_codex_hooks_with_preserved_state(config_path, events, None)
}

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

pub(crate) fn install_codex_hooks_with_preserved_state(
    config_path: &Path,
    events: &[crate::agents::HookEvent],
    preserved_state: Option<toml_edit::Item>,
) -> Result<()> {
    with_codex_config_lock(config_path, || {
        let mut config = read_codex_config(config_path)?;
        if codex_hooks_feature_is_disabled(&config, config_path) {
            return Ok(());
        }

        if let Some(state) = preserved_state {
            let hooks = ensure_codex_hooks_table(&mut config)?;
            if !hooks.contains_key("state") {
                hooks.insert("state", state);
            }
        }
        remove_codex_aoe_hooks(&mut config)?;
        merge_codex_hooks(&mut config, events)?;
        write_codex_config(config_path, &config)?;
        Ok(())
    })?;

    tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", config_path.display());
    Ok(())
}

fn with_codex_config_lock<T>(config_path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let lock_base_path = codex_config_write_path(config_path)?;

    if let Some(parent) = lock_base_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let lock_path = lock_base_path.with_extension("toml.lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .with_context(|| format!("Failed to open Codex config lock {}", lock_path.display()))?;

    lock_file
        .lock_exclusive()
        .with_context(|| format!("Failed to lock Codex config {}", config_path.display()))?;

    let result = f();
    let unlock_result = fs2::FileExt::unlock(&lock_file);
    match (result, unlock_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error)
            .with_context(|| format!("Failed to unlock Codex config lock {}", lock_path.display())),
    }
}

fn write_codex_config(config_path: &Path, config: &toml_edit::DocumentMut) -> Result<()> {
    let write_path = codex_config_write_path(config_path)?;
    crate::session::atomic_write(&write_path, config.to_string().as_bytes())
}

fn codex_config_write_path(config_path: &Path) -> Result<PathBuf> {
    let mut write_path = config_path.to_path_buf();

    for _ in 0..32 {
        let metadata = match std::fs::symlink_metadata(&write_path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(write_path),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Failed to inspect Codex config {}", write_path.display())
                });
            }
        };

        if !metadata.file_type().is_symlink() {
            return Ok(write_path);
        }

        let target = std::fs::read_link(&write_path).with_context(|| {
            format!(
                "Failed to read Codex config symlink {}",
                write_path.display()
            )
        })?;
        write_path = if target.is_absolute() {
            target
        } else {
            write_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(target)
        };
    }

    Err(anyhow::anyhow!(
        "Codex config symlink chain is too deep: {}",
        config_path.display()
    ))
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
    events: &[crate::agents::HookEvent],
) -> Result<()> {
    let hooks = ensure_codex_hooks_table(config)?;

    for event in events {
        let Some(status) = event.status else {
            continue;
        };

        let event_array = ensure_codex_event_array(hooks, event.name)?;
        event_array.push(codex_matcher_group(event, status));
    }

    Ok(())
}

fn codex_matcher_group(event: &crate::agents::HookEvent, status: &str) -> toml_edit::Table {
    let mut group = toml_edit::Table::new();
    if let Some(matcher) = event.matcher {
        group.insert("matcher", toml_edit::value(matcher));
    }

    let mut handler = toml_edit::Table::new();
    handler.insert("type", toml_edit::value("command"));
    handler.insert("command", toml_edit::value(hook_command(status)));

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

fn codex_inline_hook_handler_is_aoe(handler: &toml_edit::Value) -> bool {
    handler
        .as_inline_table()
        .and_then(|handler| codex_toml_table_command(handler))
        .is_some_and(is_aoe_hook_command)
}

fn codex_toml_table_command(table: &dyn toml_edit::TableLike) -> Option<&str> {
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
    std::fs::write(settings_path, formatted)?;

    tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", settings_path.display());
    Ok(true)
}

/// settl hook events and the AoE status they map to.
const SETTL_HOOKS: &[(&str, &str)] = &[
    ("TurnStarted", "running"),
    ("WaitingForHuman", "waiting"),
    ("GameWon", "idle"),
];

/// Install AoE status hooks into settl's `~/.settl/config.toml`.
///
/// settl uses TOML config with `[[hooks]]` array entries instead of JSON
/// settings files. This function reads the existing config, removes any
/// previous AoE-managed hooks (identified by the marker), and adds hooks
/// for the three status transitions: TurnStarted->running,
/// WaitingForHuman->waiting, GameWon->idle.
pub fn install_settl_hooks() -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    let config_path = home.join(".settl").join("config.toml");

    // Parse existing config or start fresh
    let mut config: toml::Value = if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)?;
        toml::from_str(&content).unwrap_or_else(|e| {
            tracing::warn!(target: "hooks.install", "Failed to parse {}: {}", config_path.display(), e);
            toml::Value::Table(toml::map::Map::new())
        })
    } else {
        toml::Value::Table(toml::map::Map::new())
    };

    let table = config
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("Config root is not a TOML table"))?;

    // Get or create the hooks array
    let hooks = table
        .entry("hooks")
        .or_insert_with(|| toml::Value::Array(Vec::new()));
    let hooks_arr = hooks
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("hooks key is not a TOML array"))?;

    // Remove existing AoE hooks
    hooks_arr.retain(|hook| {
        !hook
            .get("command")
            .and_then(|c| c.as_str())
            .is_some_and(is_aoe_hook_command)
    });

    // Add one hook per status transition
    for (event, status) in SETTL_HOOKS {
        let mut entry = toml::map::Map::new();
        entry.insert("event".into(), toml::Value::String((*event).into()));
        entry.insert("command".into(), toml::Value::String(hook_command(status)));
        hooks_arr.push(toml::Value::Table(entry));
    }

    // Write back
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let formatted = toml::to_string_pretty(&config)?;
    std::fs::write(&config_path, formatted)?;

    tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", config_path.display());
    Ok(())
}

/// Remove AoE hooks from settl's `~/.settl/config.toml`.
pub fn uninstall_settl_hooks() -> Result<bool> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("No home directory"))?;
    let config_path = home.join(".settl").join("config.toml");

    if !config_path.exists() {
        return Ok(false);
    }

    let content = std::fs::read_to_string(&config_path)?;
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
    std::fs::write(&config_path, formatted)?;
    tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", config_path.display());
    Ok(true)
}

/// Hermes hook events and the AoE status they map to. Hermes uses an
/// event-keyed YAML schema (`hooks: { event_name: [ {command, ...} ] }`),
/// not the flat array settl uses.
const HERMES_HOOKS: &[(&str, &str)] = &[
    ("pre_llm_call", "running"),
    ("pre_tool_call", "running"),
    ("post_llm_call", "idle"),
    ("pre_approval_request", "waiting"),
    ("post_approval_response", "running"),
    ("on_session_end", "idle"),
];

/// Install AoE status hooks into Hermes's `config.yaml`.
///
/// Reads the existing YAML, removes any prior AoE-managed hook entries
/// (identified by the `aoe-hooks` marker in the command string), and inserts
/// our status-writing hooks under the configured events. Also pre-populates
/// `<config_dir>/shell-hooks-allowlist.json` so Hermes registers the hooks
/// without prompting for first-use consent.
pub fn install_hermes_hooks(config_path: &Path) -> Result<()> {
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

    for (event, status) in HERMES_HOOKS {
        let event_key = serde_yaml::Value::String((*event).to_string());
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
            serde_yaml::Value::String(hook_command(status)),
        );
        arr.push(serde_yaml::Value::Mapping(entry));
    }

    let formatted = serde_yaml::to_string(&config)?;
    let config_dir = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?;
    let (allowlist_path, allowlist_formatted) = render_hermes_allowlist(config_dir)?;

    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(config_path, formatted)?;

    if let Some(parent) = allowlist_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&allowlist_path, allowlist_formatted)?;

    tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", config_path.display());
    Ok(())
}

/// Remove AoE hooks from Hermes's `config.yaml`.
pub fn uninstall_hermes_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }

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
    std::fs::write(config_path, formatted)?;
    tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", config_path.display());
    Ok(true)
}

/// Pre-populate Hermes's per-user shell-hook allowlist so registration runs
/// without prompting on the first session. Hermes keys consent on the exact
/// `(event, command)` pair, so we add one entry per status we install.
fn render_hermes_allowlist(config_dir: &Path) -> Result<(std::path::PathBuf, String)> {
    let allowlist_path = config_dir.join("shell-hooks-allowlist.json");
    let approved_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);

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

    for (event, status) in HERMES_HOOKS {
        let cmd = hook_command(status);
        approvals.retain(|entry| {
            !(entry.get("event").and_then(|v| v.as_str()) == Some(*event)
                && entry.get("command").and_then(|v| v.as_str()) == Some(&cmd))
        });
        approvals.push(serde_json::json!({
            "event": *event,
            "command": cmd,
            "approved_at": approved_at,
            "script_mtime_at_approval": Value::Null,
        }));
    }

    let formatted = serde_json::to_string_pretty(&data)?;
    Ok((allowlist_path, formatted))
}

/// Kiro CLI hook events. Kiro uses lowercase camelCase event names and a flat
/// `[{"command": "..."}]` structure in its agent config JSON.
const KIRO_HOOKS: &[(&str, &str)] = &[
    ("preToolUse", "running"),
    ("userPromptSubmit", "running"),
    ("stop", "idle"),
];

/// Default agent config path for Kiro CLI: `~/.kiro/agents/aoe-hooks.json`.
/// We use a dedicated agent config file rather than modifying the user's
/// default agent, so AoE hooks are isolated and easy to remove.
pub const KIRO_HOOKS_AGENT_FILE: &str = ".kiro/agents/aoe-hooks.json";

/// Install AoE status hooks into a Kiro CLI agent config file.
///
/// Writes a minimal agent config with hooks that write status to the
/// AoE sidecar file. This function is pure file IO and is safe to call
/// from any context (host install, sandbox provisioning, tests). To make
/// the agent the active default on the host, call
/// [`set_kiro_default_agent_if_builtin`] after this returns.
pub fn install_kiro_hooks(agent_config_path: &Path) -> Result<()> {
    let mut config: serde_json::Map<String, Value> = if agent_config_path.exists() {
        let content = std::fs::read_to_string(agent_config_path)?;
        serde_json::from_str(&content).unwrap_or_else(|_| serde_json::Map::new())
    } else {
        serde_json::Map::new()
    };

    // Kiro requires a name field for valid agent configs
    config
        .entry("name".to_string())
        .or_insert_with(|| Value::String("aoe-hooks".to_string()));
    // Wildcard tools so preToolUse hooks fire for all tool invocations
    config
        .entry("tools".to_string())
        .or_insert_with(|| serde_json::json!(["*"]));

    let mut hooks_obj: serde_json::Map<String, Value> = config
        .get("hooks")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    for (event, status) in KIRO_HOOKS {
        let entries = hooks_obj
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Some(arr) = entries.as_array_mut() {
            arr.retain(|hook| {
                !hook
                    .get("command")
                    .and_then(|c| c.as_str())
                    .is_some_and(is_aoe_hook_command)
            });
            arr.push(serde_json::json!({ "command": hook_command(status) }));
        }
    }

    config.insert("hooks".to_string(), Value::Object(hooks_obj));

    if let Some(parent) = agent_config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let formatted = serde_json::to_string_pretty(&Value::Object(config))?;
    std::fs::write(agent_config_path, formatted)?;

    tracing::info!(target: "hooks.install", "Installed AoE hooks in {}", agent_config_path.display());
    Ok(())
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
            .args(["agent", "set-default", "aoe-hooks"])
            .output();
        match set_result {
            Ok(o) if o.status.success() => {
                tracing::info!(target: "hooks.install", "Set aoe-hooks as default Kiro agent for status detection");
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
             Run `kiro-cli agent set-default aoe-hooks` to enable status detection."
        );
    }
}

/// Remove AoE hooks from a Kiro CLI agent config file.
/// Returns true if hooks were removed, false if nothing to do.
pub fn uninstall_kiro_hooks(agent_config_path: &Path) -> Result<bool> {
    if !agent_config_path.exists() {
        return Ok(false);
    }

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

    // Remove empty event arrays
    hooks_obj.retain(|_, v| !v.as_array().is_some_and(|a| a.is_empty()));
    if hooks_obj.is_empty() {
        config.remove("hooks");
    }

    // If the file is now just `{}`, remove it entirely
    if config.is_empty() {
        std::fs::remove_file(agent_config_path)?;
    } else {
        let formatted = serde_json::to_string_pretty(&Value::Object(config))?;
        std::fs::write(agent_config_path, formatted)?;
    }

    tracing::info!(target: "hooks.uninstall", "Removed AoE hooks from {}", agent_config_path.display());
    Ok(true)
}

/// Remove all AoE hooks from all known agent settings files and clean up
/// the hook status base directory. Called during `aoe uninstall`.
pub fn uninstall_all_hooks() {
    // Remove settl TOML hooks
    match uninstall_settl_hooks() {
        Ok(true) => println!("Removed AoE hooks from ~/.settl/config.toml"),
        Ok(false) => {}
        Err(e) => tracing::warn!(target: "hooks.uninstall", "Failed to remove settl hooks: {}", e),
    }

    let home = dirs::home_dir();
    if let Some(home) = &home {
        // Remove Hermes YAML hooks
        let hermes_config = home.join(".hermes").join("config.yaml");
        match uninstall_hermes_hooks(&hermes_config) {
            Ok(true) => println!("Removed AoE hooks from {}", hermes_config.display()),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(target: "hooks.uninstall", "Failed to remove hermes hooks: {}", e)
            }
        }

        // Remove Kiro CLI agent config hooks
        let kiro_config = home.join(KIRO_HOOKS_AGENT_FILE);
        match uninstall_kiro_hooks(&kiro_config) {
            Ok(true) => println!("Removed AoE hooks from {}", kiro_config.display()),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(target: "hooks.uninstall", "Failed to remove kiro hooks: {}", e)
            }
        }
    }

    for agent in crate::agents::AGENTS {
        if let Some(hook_cfg) = &agent.hook_config {
            let resolved_paths = if agent.name == "codex" {
                codex_config_paths_for_uninstall()
            } else {
                agent_settings_paths_for_uninstall(hook_cfg)
            };
            for settings_path in resolved_paths {
                let result = if agent.name == "codex" {
                    uninstall_codex_hooks(&settings_path)
                } else {
                    uninstall_hooks(&settings_path)
                };
                match result {
                    Ok(true) => println!("Removed AoE hooks from {}", settings_path.display()),
                    Ok(false) => {}
                    Err(e) => tracing::warn!(target: "hooks.uninstall",
                        "Failed to remove hooks from {}: {}",
                        settings_path.display(),
                        e
                    ),
                }
            }
        }
    }

    // Clean up the entire hook status base directory
    let base = std::path::Path::new(HOOK_STATUS_BASE);
    if base.exists() {
        if let Err(e) = std::fs::remove_dir_all(base) {
            tracing::warn!(target: "hooks.uninstall", "Failed to remove {}: {}", base.display(), e);
        }
    }
}

fn codex_config_paths_for_uninstall() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    push_unique_path(&mut paths, codex_config_path());

    if let Ok(config) = crate::session::config::Config::load() {
        push_unique_path(
            &mut paths,
            codex_config_path_for_host_environment(&config.environment),
        );
    }

    match crate::session::list_profiles() {
        Ok(profiles) => {
            for profile in profiles {
                let environment =
                    crate::session::profile_config::resolve_config_or_warn(&profile).environment;
                push_unique_path(
                    &mut paths,
                    codex_config_path_for_host_environment(&environment),
                );
            }
        }
        Err(e) => {
            tracing::warn!(target: "hooks.uninstall", "Failed to list profiles for Codex hook cleanup: {}", e)
        }
    }

    paths
}

fn push_unique_path(paths: &mut Vec<PathBuf>, path: Result<PathBuf>) {
    match path {
        Ok(path) if !paths.contains(&path) => paths.push(path),
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(target: "hooks.uninstall", "Failed to resolve Codex config path: {}", e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn claude_events() -> &'static [crate::agents::HookEvent] {
        crate::agents::get_agent("claude")
            .unwrap()
            .hook_config
            .as_ref()
            .unwrap()
            .events
    }

    fn codex_events() -> &'static [crate::agents::HookEvent] {
        crate::agents::get_agent("codex")
            .unwrap()
            .hook_config
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

        install_codex_hooks(&config_path, codex_events()).unwrap();

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

        install_codex_hooks(&config_path, codex_events()).unwrap();

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
    fn test_codex_config_paths_for_uninstall_include_profile_codex_home() {
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

        let paths = codex_config_paths_for_uninstall();

        assert!(paths.contains(&tmp.path().join(".codex").join("config.toml")));
        assert!(paths.contains(&codex_home.join("config.toml")));
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
        install_codex_hooks(&config_path, codex_events()).unwrap();

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

        install_codex_hooks(&config_path, codex_events()).unwrap();
        install_codex_hooks(&config_path, codex_events()).unwrap();

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
                hook_command("running")
            ),
        )
        .unwrap();

        install_codex_hooks(&config_path, codex_events()).unwrap();
        install_codex_hooks(&config_path, codex_events()).unwrap();

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
            hook_command("idle"),
            hook_command("running"),
            hook_command("idle"),
            hook_command("idle"),
            hook_command("running"),
            hook_command("idle")
        );
        std::fs::write(&config_path, installed_once).unwrap();

        install_codex_hooks(&config_path, codex_events()).unwrap();

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
                    install_codex_hooks(&config_path, codex_events()).unwrap();
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
        install_codex_hooks(&config_path, codex_events()).unwrap();

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
        install_codex_hooks(&config_path, codex_events()).unwrap();

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

        install_codex_hooks(&config_path, codex_events()).unwrap();
        let modified = uninstall_codex_hooks(&config_path).unwrap();
        assert!(modified);

        let config = std::fs::read_to_string(config_path).unwrap();
        assert!(config.contains("echo user-hook"));
        assert!(config.contains("trusted_hash = \"keep\""));
        assert!(!config.contains("aoe-hooks"));
    }

    #[test]
    fn test_hook_command_format() {
        let cmd = hook_command("running");
        assert!(cmd.contains(AOE_HOOK_MARKER));
        assert!(cmd.contains("printf running"));
    }

    #[test]
    fn test_hook_command_contains_instance_id_guard() {
        let cmd = hook_command("idle");
        assert!(cmd.contains("AOE_INSTANCE_ID"));
        assert!(cmd.contains("printf idle"));
    }

    #[test]
    fn test_hook_command_tolerates_unwritable_base_dir() {
        // Regression for #1390: if /tmp/aoe-hooks/<id> disappears mid-session
        // (OS /tmp cleanup, transient FS hiccup, external tooling), the hook
        // must still exit 0 so the agent doesn't treat it as blocking and
        // freeze further tool calls.
        use std::process::Command;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("aoe-hooks-blocked");
        // Pre-create base as a regular file so mkdir -p can never succeed.
        std::fs::write(&base, "i am a file, not a dir").unwrap();

        let cmd = hook_command_with_base("running", base.to_str().unwrap());

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
        use std::process::Command;

        let tmp = TempDir::new().unwrap();
        let base = tmp.path().join("aoe-hooks");

        let cmd = hook_command_with_base("waiting", base.to_str().unwrap());

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
        assert_eq!(notification.len(), 1);
        let matcher = notification[0]["matcher"].as_str().unwrap();
        assert!(matcher.contains("permission_prompt"));
        assert!(matcher.contains("elicitation_dialog"));
        assert!(!matcher.contains("idle_prompt"));
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
                "hooks": [{"type": "command", "command": "sh -c 'aoe-hooks stuff'"}]
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
    #[serial_test::serial]
    fn test_install_settl_hooks_creates_new_file() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join(".settl").join("config.toml");

        // Override HOME so install_settl_hooks writes to our temp dir
        std::env::set_var("HOME", tmp.path());
        install_settl_hooks().unwrap();
        std::env::remove_var("HOME");

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
    #[serial_test::serial]
    fn test_install_settl_hooks_idempotent() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        install_settl_hooks().unwrap();
        install_settl_hooks().unwrap();
        std::env::remove_var("HOME");

        let config_path = tmp.path().join(".settl").join("config.toml");
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
    #[serial_test::serial]
    fn test_install_settl_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join(".settl");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            r#"
[[hooks]]
event = "GameWon"
command = "echo user-hook"
"#,
        )
        .unwrap();

        std::env::set_var("HOME", tmp.path());
        install_settl_hooks().unwrap();
        std::env::remove_var("HOME");

        let content = std::fs::read_to_string(config_dir.join("config.toml")).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        let hooks = config["hooks"].as_array().unwrap();
        // 1 user hook + 3 AoE hooks = 4
        assert_eq!(hooks.len(), 4);
        assert_eq!(hooks[0]["command"].as_str().unwrap(), "echo user-hook");
    }

    #[test]
    #[serial_test::serial]
    fn test_uninstall_settl_hooks_removes_aoe_entries() {
        let tmp = TempDir::new().unwrap();
        std::env::set_var("HOME", tmp.path());
        install_settl_hooks().unwrap();

        let modified = uninstall_settl_hooks().unwrap();
        std::env::remove_var("HOME");

        assert!(modified);
        let config_path = tmp.path().join(".settl").join("config.toml");
        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        let hooks = config["hooks"].as_array().unwrap();
        assert!(hooks.is_empty());
    }

    #[test]
    #[serial_test::serial]
    fn test_uninstall_settl_hooks_preserves_user_hooks() {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join(".settl");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            r#"
[[hooks]]
event = "GameWon"
command = "echo user-hook"
"#,
        )
        .unwrap();

        std::env::set_var("HOME", tmp.path());
        install_settl_hooks().unwrap();
        let modified = uninstall_settl_hooks().unwrap();
        std::env::remove_var("HOME");

        assert!(modified);
        let content = std::fs::read_to_string(config_dir.join("config.toml")).unwrap();
        let config: toml::Value = toml::from_str(&content).unwrap();
        let hooks = config["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0]["command"].as_str().unwrap(), "echo user-hook");
    }

    #[test]
    fn test_settl_hook_commands_write_correct_status() {
        for (event, expected_status) in SETTL_HOOKS {
            let cmd = hook_command(expected_status);
            assert!(
                cmd.contains(&format!("printf {}", expected_status)),
                "Hook for {} should write '{}': {}",
                event,
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

        install_hermes_hooks(&config_path).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        let hooks = config
            .as_mapping()
            .unwrap()
            .get(serde_yaml::Value::String("hooks".into()))
            .unwrap()
            .as_mapping()
            .unwrap();

        for (event, _) in HERMES_HOOKS {
            let entries = hooks
                .get(serde_yaml::Value::String((*event).into()))
                .unwrap_or_else(|| panic!("event {} missing", event))
                .as_sequence()
                .unwrap();
            assert_eq!(entries.len(), 1, "event {} should have one entry", event);
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
        assert_eq!(approvals.len(), HERMES_HOOKS.len());
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

        install_hermes_hooks(&config_path).unwrap();

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

        let result = install_hermes_hooks(&config_path);

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

        let result = install_hermes_hooks(&config_path);

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

        install_hermes_hooks(&config_path).unwrap();
        install_hermes_hooks(&config_path).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: serde_yaml::Value = serde_yaml::from_str(&content).unwrap();
        let pre_tool = config["hooks"]["pre_tool_call"].as_sequence().unwrap();
        assert_eq!(pre_tool.len(), 1, "reinstall should not duplicate");

        // Allowlist also dedupes
        let allowlist = tmp.path().join("shell-hooks-allowlist.json");
        let raw = std::fs::read_to_string(&allowlist).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        let approvals = parsed["approvals"].as_array().unwrap();
        assert_eq!(approvals.len(), HERMES_HOOKS.len());
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

        install_hermes_hooks(&config_path).unwrap();
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
        for (event, expected_status) in HERMES_HOOKS {
            let cmd = hook_command(expected_status);
            assert!(
                cmd.contains(&format!("printf {}", expected_status)),
                "Hook for {} should write '{}': {}",
                event,
                expected_status,
                cmd
            );
            assert!(cmd.contains("aoe-hooks"), "Hook should contain marker");
        }
    }

    #[test]
    fn test_hermes_approval_request_writes_waiting() {
        let mapped: Vec<&str> = HERMES_HOOKS
            .iter()
            .filter(|(e, _)| *e == "pre_approval_request")
            .map(|(_, s)| *s)
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

        install_kiro_hooks(&config_path).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        let hooks = config["hooks"].as_object().unwrap();

        for (event, _) in KIRO_HOOKS {
            let entries = hooks
                .get(*event)
                .unwrap_or_else(|| panic!("event {} missing", event))
                .as_array()
                .unwrap();
            assert_eq!(entries.len(), 1, "event {} should have one entry", event);
            let cmd = entries[0]["command"].as_str().unwrap();
            assert!(is_aoe_hook_command(cmd));
        }
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

        install_kiro_hooks(&config_path).unwrap();

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

        install_kiro_hooks(&config_path).unwrap();
        install_kiro_hooks(&config_path).unwrap();

        let content = std::fs::read_to_string(&config_path).unwrap();
        let config: Value = serde_json::from_str(&content).unwrap();
        for (event, _) in KIRO_HOOKS {
            let entries = config["hooks"][event].as_array().unwrap();
            assert_eq!(
                entries.len(),
                1,
                "event {} should still have exactly one AoE entry after double install",
                event
            );
        }
    }

    #[test]
    fn test_uninstall_kiro_hooks_removes_aoe_entries() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("aoe-hooks.json");

        install_kiro_hooks(&config_path).unwrap();
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

        install_kiro_hooks(&config_path).unwrap();
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

    #[test]
    fn test_hook_command_session_id_extracts_from_compact_payload() {
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

    #[test]
    fn test_hook_command_session_id_sandbox_pins_nested_first_quirk() {
        let tmp = TempDir::new().unwrap();
        let nested = "11111111-2222-3333-4444-555555555555";
        let top_level = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let payload =
            format!(r#"{{"context":{{"session_id":"{nested}"}},"session_id":"{top_level}"}}"#);
        let output = run_session_id_hook(&payload, "sandbox_nested_first", tmp.path());
        assert!(output.status.success());
        let written =
            std::fs::read_to_string(tmp.path().join("sandbox_nested_first").join("session_id"))
                .expect("sidecar file");
        assert_eq!(
            written, nested,
            "the sandbox shell pipeline's `[{{,]` regex anchor cannot \
             distinguish a nested object literal from the top-level field; \
             a textually-earlier nested `session_id` wins. The host variant \
             fixes this via `serde_json`. Documented limitation; pinned so \
             a regex tweak does not silently change ordering semantics."
        );
    }

    #[test]
    fn test_hook_command_session_id_extracts_from_multi_line_payload() {
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
        let tmp = TempDir::new().unwrap();
        let payload = r#"{"cwd":"/x","other":"value"}"#;
        let output = run_session_id_hook(payload, "no_sid", tmp.path());
        assert!(output.status.success());
        let path = tmp.path().join("no_sid").join("session_id");
        assert!(!path.exists());
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
    fn test_hook_command_session_id_sandbox_keeps_shell_pipeline() {
        let cmd = hook_command_session_id(HookInstallTarget::Sandbox);
        assert!(
            cmd.contains("grep -oE"),
            "sandbox hook must keep the POSIX pipeline since `aoe` is not in the image, got: {cmd}"
        );
        assert!(
            !cmd.contains("aoe __extract-session-id"),
            "sandbox hook must not invoke the Rust subcommand, got: {cmd}"
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
        let cmd = hook_command_with_base("running", "/tmp/aoe-hooks");
        assert!(
            cmd.contains("case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*) exit 0 ;; esac"),
            "missing shell guard: {cmd}"
        );
        assert!(cmd.contains("\"/tmp/aoe-hooks/$AOE_INSTANCE_ID\""));
        assert!(cmd.contains("\"/tmp/aoe-hooks/$AOE_INSTANCE_ID/status\""));
    }

    #[test]
    fn hook_command_session_id_sandbox_quotes_and_guards() {
        let cmd = hook_command_session_id_sandbox("/tmp/aoe-hooks");
        assert!(cmd.contains("case \"$AOE_INSTANCE_ID\" in *[!0-9a-zA-Z_-]*"));
        assert!(cmd.contains("D=\"/tmp/aoe-hooks/$AOE_INSTANCE_ID\""));
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
        let cmd = hook_command_with_base("running", base.to_str().unwrap());

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
}
