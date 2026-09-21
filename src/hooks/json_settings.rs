//! Claude-style `settings.json` hooks (`hooks.<event>[].hooks[].command`) and
//! Cursor's flat `hooks.json` (`hooks.<event>[].command`).

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value};

use crate::agents::ResolvedHookEvent;

use super::command::{hook_command_session_id, json_command_is_aoe, status_command_for_event};
use super::config_io::{
    log_installed, log_removed, log_unchanged, parse_json_or_empty, with_config_lock, write_json,
};
use super::HookInstallTarget;

/// Commands for one event: the identity extractor first (it must see stdin
/// first), then the status writer.
fn event_commands(event: &ResolvedHookEvent, target: HookInstallTarget) -> Vec<String> {
    let identity = event
        .identity_field
        .map(|field| hook_command_session_id(target, field));
    let status = event
        .status
        .map(|status| status_command_for_event(status, &event.waiting_tools, target));
    identity.into_iter().chain(status).collect()
}

/// One matcher group per event, appended under the event name so events that
/// share a name with different matchers coexist.
fn build_aoe_hooks(events: &[ResolvedHookEvent], target: HookInstallTarget) -> Map<String, Value> {
    let mut hooks = Map::new();
    for event in events {
        let commands = event_commands(event, target);
        if commands.is_empty() {
            continue;
        }
        let mut group = Map::new();
        if let Some(m) = &event.matcher {
            group.insert("matcher".to_string(), Value::String(m.clone()));
        }
        let entries = commands
            .into_iter()
            .map(|cmd| serde_json::json!({ "type": "command", "command": cmd }))
            .collect();
        group.insert("hooks".to_string(), Value::Array(entries));
        if let Value::Array(groups) = hooks
            .entry(event.name.clone())
            .or_insert_with(|| Value::Array(Vec::new()))
        {
            groups.push(Value::Object(group));
        }
    }
    hooks
}

/// Drops matcher groups whose every hook is AoE's. A mixed user+AoE group is
/// kept intact (locked by v015's `mixed_user_aoe_matcher_group_documents_double_firing`).
fn remove_aoe_entries(groups: &mut Vec<Value>) {
    groups.retain(|group| {
        let Some(hooks) = group.get("hooks").and_then(Value::as_array) else {
            return true;
        };
        !hooks.iter().all(json_command_is_aoe)
    });
}

/// Removes events whose array is empty. `Map::remove` swaps entries, which
/// keeps the key order earlier releases wrote.
fn remove_empty_events(hooks: &mut Map<String, Value>) {
    let empty: Vec<String> = hooks
        .iter()
        .filter(|(_, groups)| groups.as_array().is_some_and(Vec::is_empty))
        .map(|(name, _)| name.clone())
        .collect();
    for name in empty {
        hooks.remove(&name);
    }
}

/// Merge AoE hooks into an agent's `settings.json`, replacing earlier AoE
/// entries and keeping user hooks. Unchanged content is not rewritten.
pub fn install_hooks(
    settings_path: &Path,
    events: impl AsRef<[ResolvedHookEvent]>,
    target: HookInstallTarget,
) -> Result<()> {
    if events.as_ref().is_empty() && !settings_path.exists() {
        return Ok(());
    }
    with_config_lock(settings_path, "json.lock", || {
        let content = settings_path
            .exists()
            .then(|| std::fs::read_to_string(settings_path))
            .transpose()?;
        let mut settings = parse_json_or_empty(content, settings_path);
        let before = settings.clone();

        let root = settings
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Settings file root is not a JSON object"))?;
        if !root.get("hooks").is_some_and(Value::is_object) {
            root.insert("hooks".to_string(), serde_json::json!({}));
        }
        let settings_hooks = root
            .get_mut("hooks")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| anyhow::anyhow!("hooks key is not a JSON object"))?;

        for groups in settings_hooks.values_mut().filter_map(Value::as_array_mut) {
            remove_aoe_entries(groups);
        }
        remove_empty_events(settings_hooks);
        for (event_name, aoe_groups) in build_aoe_hooks(events.as_ref(), target) {
            match settings_hooks
                .get_mut(&event_name)
                .and_then(Value::as_array_mut)
            {
                Some(existing) => {
                    existing.extend(aoe_groups.as_array().into_iter().flatten().cloned())
                }
                None => {
                    settings_hooks.insert(event_name, aoe_groups);
                }
            }
        }

        if settings == before {
            log_unchanged(settings_path);
            return Ok(());
        }
        write_json(settings_path, &settings)?;
        log_installed(settings_path);
        Ok(())
    })
}

/// Remove AoE hooks from `settings.json`, dropping emptied events and an
/// emptied `hooks` key. Returns whether the file changed.
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
        let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else {
            return Ok(false);
        };

        let mut modified = false;
        for groups in hooks.values_mut().filter_map(Value::as_array_mut) {
            let before = groups.len();
            remove_aoe_entries(groups);
            modified |= groups.len() != before;
        }
        if !modified {
            return Ok(false);
        }
        remove_empty_events(hooks);
        if hooks.is_empty() {
            if let Some(obj) = settings.as_object_mut() {
                obj.remove("hooks");
            }
        }

        crate::session::atomic_write(
            settings_path,
            serde_json::to_string_pretty(&settings)?.as_bytes(),
        )?;
        log_removed(settings_path);
        Ok(true)
    })
}

/// Drops AoE entries from flat `hooks.<event>[].command` arrays; returns whether any were removed.
pub(super) fn remove_flat_aoe_entries(hooks: &mut Map<String, Value>) -> bool {
    let mut removed = false;
    for entries in hooks.values_mut().filter_map(Value::as_array_mut) {
        let before = entries.len();
        entries.retain(|entry| !json_command_is_aoe(entry));
        removed |= entries.len() != before;
    }
    removed
}

pub(super) fn drop_empty_events(hooks: &mut Map<String, Value>) {
    hooks.retain(|_, entries| !entries.as_array().is_some_and(Vec::is_empty));
}

fn read_cursor_hooks(config_path: &Path) -> Result<Value> {
    serde_json::from_str(&std::fs::read_to_string(config_path)?)
        .with_context(|| format!("parsing Cursor hooks at {}", config_path.display()))
}

/// Install Cursor's version-1 flat hooks.json schema while preserving user hooks.
pub fn install_cursor_hooks_with_events(
    config_path: &Path,
    target: HookInstallTarget,
    events: &[ResolvedHookEvent],
) -> Result<()> {
    if events.is_empty() && !config_path.exists() {
        return Ok(());
    }
    with_config_lock(config_path, "json.lock", || {
        let mut config = if config_path.exists() {
            read_cursor_hooks(config_path)?
        } else {
            serde_json::json!({})
        };
        let before = config.clone();
        let root = config
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Cursor hooks root is not a JSON object"))?;
        root.insert("version".to_string(), Value::from(1));
        let hooks = root
            .entry("hooks")
            .or_insert_with(|| serde_json::json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("Cursor hooks key is not a JSON object"))?;
        remove_flat_aoe_entries(hooks);
        drop_empty_events(hooks);

        for event in events {
            let entries = hooks
                .entry(event.name.clone())
                .or_insert_with(|| Value::Array(Vec::new()))
                .as_array_mut()
                .ok_or_else(|| anyhow::anyhow!("Cursor hook event is not an array"))?;
            entries.extend(
                event_commands(event, target)
                    .into_iter()
                    .map(|command| serde_json::json!({ "command": command })),
            );
        }

        if config == before {
            return Ok(());
        }
        write_json(config_path, &config)
    })
}

/// Remove only AoE commands from Cursor's flat hooks.json schema.
pub fn uninstall_cursor_hooks(config_path: &Path) -> Result<bool> {
    if !config_path.exists() {
        return Ok(false);
    }
    with_config_lock(config_path, "json.lock", || {
        let mut config = read_cursor_hooks(config_path)?;
        let before = config.clone();
        if let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) {
            remove_flat_aoe_entries(hooks);
            drop_empty_events(hooks);
        }
        if config == before {
            return Ok(false);
        }
        write_json(config_path, &config)?;
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{HookIdentityField, HookStatus};
    use crate::hooks::command::is_aoe_hook_command;
    use crate::hooks::test_support::{agent_events, read_json};
    use tempfile::TempDir;

    fn commands(group: &Value) -> Vec<&str> {
        group["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["command"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn claude_events_map_to_hook_groups() {
        let hooks = Value::Object(build_aoe_hooks(
            &agent_events("claude", &[]),
            HookInstallTarget::Sandbox,
        ));
        for (event, fragments) in [
            ("SessionStart", &["session_id"][..]),
            ("UserPromptSubmit", &["session_id", "printf running"]),
            (
                "PreToolUse",
                &[r#"S=running; case "$IN" in *\"tool_name\":\"AskUserQuestion\"*) S=waiting"#][..],
            ),
            ("Stop", &["printf idle"]),
            // A turn killed by an API error fires StopFailure, not Stop.
            ("StopFailure", &["printf idle"]),
            ("ElicitationResult", &["printf running"]),
        ] {
            let groups = hooks[event].as_array().unwrap();
            assert_eq!(groups.len(), 1, "{event}");
            let cmds = commands(&groups[0]);
            assert_eq!(cmds.len(), fragments.len(), "{event}");
            for (cmd, fragment) in cmds.iter().zip(fragments) {
                assert!(cmd.contains(fragment), "{event}: {cmd}");
            }
        }

        let post = hooks["PostToolUse"].as_array().unwrap();
        assert_eq!(post.len(), 1);
        assert_eq!(post[0]["matcher"], "AskUserQuestion");
        assert!(commands(&post[0])[0].contains("printf running"));

        let notification = hooks["Notification"].as_array().unwrap();
        assert_eq!(notification.len(), 2);
        let group = |needle: &str| {
            notification
                .iter()
                .find(|g| g["matcher"].as_str().unwrap().contains(needle))
                .unwrap()
        };
        let waiting = group("permission_prompt");
        let matcher = waiting["matcher"].as_str().unwrap();
        for m in ["elicitation_dialog", "agent_needs_input"] {
            assert!(matcher.contains(m), "{matcher}");
        }
        assert!(!matcher.contains("idle_prompt"));
        assert!(commands(waiting)[0].contains("printf waiting"));
        let idle = group("idle_prompt");
        assert!(idle["matcher"]
            .as_str()
            .unwrap()
            .contains("agent_completed"));
        assert!(commands(idle)[0].contains("printf idle"));

        for groups in hooks.as_object().unwrap().values() {
            for group in groups.as_array().unwrap() {
                for hook in group["hooks"].as_array().unwrap() {
                    assert!(hook.get("async").is_none(), "hooks must be synchronous");
                }
            }
        }
    }

    #[test]
    fn install_hooks_merges_with_user_settings_and_uninstalls_cleanly() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let user_group = serde_json::json!({
            "matcher": "Bash",
            "hooks": [{"type": "command", "command": "echo user-hook"}]
        });
        let legacy = serde_json::json!({"hooks": [{"type": "command", "command":
            "sh -c '[ -n \"$AOE_INSTANCE_ID\" ] || exit 0; mkdir -p /tmp/aoe-hooks/$AOE_INSTANCE_ID && printf running > /tmp/aoe-hooks/$AOE_INSTANCE_ID/status'"}]});
        let existing = serde_json::json!({
            "apiKey": "test-key",
            "statusLine": {"type": "command", "command": "my-status"},
            "hooks": {"PreToolUse": [user_group, legacy]}
        });
        std::fs::write(&path, existing.to_string()).unwrap();

        install_hooks(&path, agent_events("claude", &[]), HookInstallTarget::Host).unwrap();

        let content = read_json(&path);
        assert_eq!(content["apiKey"], "test-key");
        assert_eq!(content["statusLine"]["command"], "my-status");
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "Stop",
            "Notification",
            "ElicitationResult",
        ] {
            assert!(content["hooks"][event].is_array(), "{event}");
        }
        let pre_tool = content["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 2, "legacy AoE group replaced: {pre_tool:?}");
        assert_eq!(pre_tool[0]["matcher"], "Bash");
        assert!(is_aoe_hook_command(commands(&pre_tool[1])[0]));

        assert!(uninstall_hooks(&path).unwrap());
        let content = read_json(&path);
        let pre_tool = content["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre_tool.len(), 1);
        assert_eq!(pre_tool[0]["matcher"], "Bash");
        assert!(content["hooks"].get("Stop").is_none());
        assert!(!uninstall_hooks(&path).unwrap(), "no AoE hooks left");

        let fresh = tmp.path().join("fresh/settings.json");
        assert!(!uninstall_hooks(&fresh).unwrap());
        install_hooks(&fresh, agent_events("claude", &[]), HookInstallTarget::Host).unwrap();
        assert!(uninstall_hooks(&fresh).unwrap());
        assert!(read_json(&fresh).get("hooks").is_none());
    }

    #[test]
    fn install_hooks_follows_status_overrides_and_drops_stale_events() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");

        let events = agent_events("claude", &[("PreToolUse", HookStatus::Waiting)]);
        install_hooks(&path, &events, HookInstallTarget::Host).unwrap();
        let cmd = read_json(&path)["hooks"]["PreToolUse"][0]["hooks"][0]["command"].clone();
        assert!(cmd.as_str().unwrap().contains("printf waiting"), "{cmd}");

        let custom = agent_events("claude", &[("PreCompact", HookStatus::Idle)]);
        install_hooks(&path, &custom, HookInstallTarget::Host).unwrap();
        assert!(read_json(&path)["hooks"]["PreCompact"].is_array());
        install_hooks(&path, agent_events("claude", &[]), HookInstallTarget::Host).unwrap();
        assert!(read_json(&path)["hooks"].get("PreCompact").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn install_hooks_preserves_symlinked_settings() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::create_dir_all(tmp.path().join("dotfiles")).unwrap();
        let target = tmp.path().join("dotfiles/claude-settings.json");
        std::fs::write(&target, "{\"apiKey\":\"keep-me\"}\n").unwrap();
        let link = tmp.path().join(".claude/settings.json");
        std::os::unix::fs::symlink("../dotfiles/claude-settings.json", &link).unwrap();
        let is_link = || {
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        };

        install_hooks(&link, agent_events("claude", &[]), HookInstallTarget::Host).unwrap();
        assert!(is_link());
        assert_eq!(read_json(&target)["apiKey"], "keep-me");
        assert!(read_json(&target)["hooks"]["SessionStart"].is_array());

        uninstall_hooks(&link).unwrap();
        assert!(is_link());
        assert_eq!(read_json(&target)["apiKey"], "keep-me");
        assert!(read_json(&target).get("hooks").is_none());
    }

    #[test]
    fn concurrent_installs_converge_on_sequential_bytes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("settings.json");
        let events = agent_events("claude", &[]);
        install_hooks(&path, &events, HookInstallTarget::Host).unwrap();
        let canonical = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();

        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    barrier.wait();
                    for _ in 0..100 {
                        install_hooks(&path, &events, HookInstallTarget::Host).unwrap();
                    }
                });
            }
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), canonical);
    }

    #[test]
    fn cursor_hooks_use_flat_schema_and_preserve_user_entries() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("hooks.json");
        std::fs::write(
            &path,
            r#"{"version":1,"hooks":{"beforeSubmitPrompt":[{"command":"user-hook"}]}}"#,
        )
        .unwrap();
        let events = [ResolvedHookEvent {
            name: "beforeSubmitPrompt".to_string(),
            matcher: None,
            status: Some(HookStatus::Running),
            identity_field: Some(HookIdentityField::ConversationIdOrSessionId),
            waiting_tools: Vec::new(),
        }];

        install_cursor_hooks_with_events(&path, HookInstallTarget::Host, &events).unwrap();
        let value = read_json(&path);
        assert_eq!(value["version"], 1);
        let entries = value["hooks"]["beforeSubmitPrompt"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["command"], "user-hook");
        assert!(entries[1].get("hooks").is_none());
        assert!(entries[1]["command"]
            .as_str()
            .unwrap()
            .contains("__extract-session-id"));

        assert!(uninstall_cursor_hooks(&path).unwrap());
        assert_eq!(
            read_json(&path)["hooks"]["beforeSubmitPrompt"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
}
