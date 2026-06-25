//! Hook target enumeration and marker-presence walker shared by the live
//! install/uninstall lifecycle and the hook-rewrite migrations (v015, v017,
//! v018). Extracted from `src/hooks/mod.rs` as a pure file-split refactor
//! (#2188); no behavior change.

use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{
    agent_settings_path_in, codex_hooks_json_path_in, codex_inline_hook_handler_is_aoe,
    codex_toml_table_command, is_aoe_hook_command, CODEX_HOOK_EVENT_NAMES,
};

/// Per-agent dispatch shape for the hook target enumerator. Each kind picks a
/// different installer/uninstaller and a different marker-presence walker.
#[derive(Clone, Copy, Debug)]
pub(crate) enum HookTargetKind {
    /// Generic JSON settings (Claude/OpenCode/Cursor/Gemini/Qwen/Kiro):
    /// `hooks.<event>[].hooks[].command`.
    JsonSettings,
    /// Codex `config.toml`: file-locked, symlink-resolved, with a user-trust
    /// `[hooks.state]` block to preserve. Only the v018 legacy-cleanup
    /// migration constructs this variant; no agent declares
    /// `HookFormat::CodexToml`.
    CodexToml,
    /// Codex `hooks.json`: same JSON payload shape as `JsonSettings`, but
    /// the path is resolved through `codex_hooks_json_path_in`
    /// (`CODEX_HOME` aware).
    CodexJson,
    /// settl/hermes/kiro: a config format the JSON path cannot emit; install
    /// goes through the agent's bundled `SidecarHooks` function pointers.
    Sidecar(&'static crate::agents::SidecarHooks),
}

/// One reachable on-disk location where AoE may have written status hooks.
///
/// Produced by [`iter_hook_targets`] (single source of truth for both
/// `uninstall_all_hooks` and the v015 hook-rewrite migration).
#[derive(Debug)]
pub(crate) struct HookTarget {
    pub agent_name: &'static str,
    pub kind: HookTargetKind,
    pub path: PathBuf,
    /// Hook events to register; empty for `Sidecar` (sidecar installers carry
    /// their own static event tables internally).
    pub events: &'static [crate::agents::HookEvent],
}

/// Enumerate every hook target reachable from the running AoE process: the
/// home-relative default for each agent, plus every profile-overridden path
/// (CLAUDE_CONFIG_DIR / CODEX_HOME / etc.) from the global config and each
/// profile's `environment` list. Deduplicated by `(kind discriminant, path)`.
///
/// Used by [`uninstall_all_hooks`] (read-modify-write removal) and the v015
/// migration (read-modify-write rewrite). Both share this enumerator so a
/// future agent added to `crate::agents::AGENTS` automatically appears in
/// both flows.
pub(crate) fn iter_hook_targets() -> Vec<HookTarget> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Vec::new(),
    };
    iter_hook_targets_in(&home, &collect_env_lists_from_session())
}

/// Home-injectable variant of [`iter_hook_targets`]. Tests construct a
/// synthetic `home` and `env_lists` directly without touching the AoE
/// process env or `Config::load()`.
pub(crate) fn iter_hook_targets_in(home: &Path, env_lists: &[Vec<String>]) -> Vec<HookTarget> {
    let mut out: Vec<HookTarget> = Vec::new();
    for agent in crate::agents::AGENTS {
        if let Some(hook_cfg) = agent.hook_config.as_ref() {
            let mut paths: Vec<PathBuf> = Vec::new();
            let resolve = |env: &[String]| -> PathBuf {
                match hook_cfg.format {
                    crate::agents::HookFormat::JsonSettings => {
                        agent_settings_path_in(home, hook_cfg, env)
                    }
                    crate::agents::HookFormat::CodexJson => codex_hooks_json_path_in(home, env),
                }
            };
            push_unique_target_path(&mut paths, resolve(&[]));
            for env in env_lists {
                push_unique_target_path(&mut paths, resolve(env));
            }
            let kind = match hook_cfg.format {
                crate::agents::HookFormat::JsonSettings => HookTargetKind::JsonSettings,
                crate::agents::HookFormat::CodexJson => HookTargetKind::CodexJson,
            };
            for path in paths {
                out.push(HookTarget {
                    agent_name: agent.name,
                    kind,
                    path,
                    events: hook_cfg.events,
                });
            }
        }
        if let Some(sidecar) = agent.sidecar_hooks.as_ref() {
            out.push(HookTarget {
                agent_name: agent.name,
                kind: HookTargetKind::Sidecar(sidecar),
                path: home.join(sidecar.host_config_subpath),
                events: &[],
            });
        }
    }
    out
}

fn push_unique_target_path(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}

/// Best-effort env-list collector for the running AoE process: global config
/// `environment` plus each profile's `environment`. Both the global load and
/// the profile listing degrade to defaults on failure with a `tracing::warn!`,
/// matching the warn-and-continue convention used elsewhere in this module.
///
/// Missing `config.toml` is silent: `Config::load` maps `!path.exists()` to
/// `Ok(Config::default())`, so any `Err` reaching `load_or_warn` is a real
/// TOML parse or I/O failure that the operator should see surfaced.
pub(super) fn collect_env_lists_from_session() -> Vec<Vec<String>> {
    let mut out = Vec::new();
    out.push(crate::session::config::Config::load_or_warn().environment);
    match crate::session::list_profiles() {
        Ok(profiles) => {
            for p in profiles {
                out.push(crate::session::profile_config::resolve_config_or_warn(&p).environment);
            }
        }
        Err(e) => {
            tracing::warn!(target: "hooks", "Failed to list profiles: {}", e);
        }
    }
    out
}

/// Marker-presence gate for the v015 hook-rewrite migration.
///
/// Returns `true` iff the target's on-disk file already contains at least one
/// AoE-managed hook command (i.e. one whose `command` string contains
/// [`AOE_HOOK_MARKER`]). Fails closed on every I/O / parse / wrong-shape
/// error: a file we cannot identify is a file we must not rewrite.
///
/// Why this exists: the install functions (`install_hooks`,
/// `install_codex_hooks_with_preserved_state`, sidecar installers) create
/// the file when absent. Calling them unconditionally on every reachable
/// target would resurrect hooks for users who explicitly uninstalled. The
/// migration must only touch files it already owns.
pub(crate) fn has_aoe_marker(target: &HookTarget) -> bool {
    if !target.path.exists() {
        return false;
    }
    match target.kind {
        HookTargetKind::JsonSettings | HookTargetKind::CodexJson => {
            json_settings_has_aoe_marker(&target.path)
        }
        HookTargetKind::CodexToml => codex_config_has_aoe_marker(&target.path),
        HookTargetKind::Sidecar(sidecar) => match sidecar.format {
            crate::agents::SidecarFormat::SettlToml => settl_config_has_aoe_marker(&target.path),
            crate::agents::SidecarFormat::HermesYaml => hermes_config_has_aoe_marker(&target.path),
            crate::agents::SidecarFormat::KiroJson => kiro_config_has_aoe_marker(&target.path),
        },
    }
}

fn json_settings_has_aoe_marker(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    let Some(hooks_obj) = value.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    for matchers in hooks_obj.values() {
        let Some(arr) = matchers.as_array() else {
            continue;
        };
        for matcher in arr {
            let Some(hooks_arr) = matcher.get("hooks").and_then(|h| h.as_array()) else {
                continue;
            };
            for hook in hooks_arr {
                if let Some(cmd) = hook.get("command").and_then(|c| c.as_str()) {
                    if is_aoe_hook_command(cmd) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn codex_config_has_aoe_marker(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    let Some(hooks) = doc.as_table().get("hooks").and_then(|h| h.as_table_like()) else {
        return false;
    };
    for event_name in CODEX_HOOK_EVENT_NAMES {
        let Some(event_item) = hooks.get(event_name) else {
            continue;
        };
        if let Some(matchers) = event_item.as_array_of_tables() {
            for matcher in matchers.iter() {
                if codex_matcher_group_contains_aoe(matcher) {
                    return true;
                }
            }
        } else if let Some(matchers) = event_item.as_array() {
            for matcher in matchers.iter() {
                if let Some(group) = matcher.as_inline_table() {
                    if codex_inline_group_contains_aoe(group) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

fn codex_matcher_group_contains_aoe(group: &toml_edit::Table) -> bool {
    let Some(hooks_item) = group.get("hooks") else {
        return false;
    };
    if let Some(handlers) = hooks_item.as_array_of_tables() {
        return handlers
            .iter()
            .any(|handler| codex_toml_table_command(handler).is_some_and(is_aoe_hook_command));
    }
    if let Some(handlers) = hooks_item.as_array() {
        return handlers.iter().any(codex_inline_hook_handler_is_aoe);
    }
    false
}

fn codex_inline_group_contains_aoe(group: &toml_edit::InlineTable) -> bool {
    let Some(hooks_item) = group.get("hooks") else {
        return false;
    };
    let Some(handlers) = hooks_item.as_array() else {
        return false;
    };
    handlers.iter().any(codex_inline_hook_handler_is_aoe)
}

fn settl_config_has_aoe_marker(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = toml::from_str::<toml::Value>(&content) else {
        return false;
    };
    let Some(arr) = value.get("hooks").and_then(|h| h.as_array()) else {
        return false;
    };
    arr.iter().any(|hook| {
        hook.get("command")
            .and_then(|c| c.as_str())
            .is_some_and(is_aoe_hook_command)
    })
}

fn hermes_config_has_aoe_marker(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    if content.trim().is_empty() {
        return false;
    }
    let Ok(value) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
        return false;
    };
    let Some(hooks_map) = value
        .as_mapping()
        .and_then(|m| m.get(serde_yaml::Value::String("hooks".into())))
        .and_then(|h| h.as_mapping())
    else {
        return false;
    };
    for entries in hooks_map.values() {
        let Some(seq) = entries.as_sequence() else {
            continue;
        };
        for entry in seq {
            if let Some(cmd) = entry
                .as_mapping()
                .and_then(|m| m.get(serde_yaml::Value::String("command".into())))
                .and_then(|c| c.as_str())
            {
                if is_aoe_hook_command(cmd) {
                    return true;
                }
            }
        }
    }
    false
}

/// Kiro's per-agent JSON uses a flat `hooks.{event}: [{command, ...}]` shape
/// (no nested matcher group), unlike Claude/OpenCode-style settings which
/// nest under `hooks.{event}[].hooks[].command`. Kiro therefore needs its
/// own walker rather than reusing [`json_settings_has_aoe_marker`].
fn kiro_config_has_aoe_marker(path: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<Value>(&content) else {
        return false;
    };
    let Some(hooks_obj) = value.get("hooks").and_then(|h| h.as_object()) else {
        return false;
    };
    for entries in hooks_obj.values() {
        let Some(arr) = entries.as_array() else {
            continue;
        };
        for entry in arr {
            if let Some(cmd) = entry.get("command").and_then(|c| c.as_str()) {
                if is_aoe_hook_command(cmd) {
                    return true;
                }
            }
        }
    }
    false
}
